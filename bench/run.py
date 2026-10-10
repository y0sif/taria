#!/usr/bin/env python3
"""Run benchmark tasks against taria-demo through one or both arms.

Each run gets a fresh demo in its own tmux server and temp dir (see
harness.DemoRun for the layout) and an MCP config for its arm:

  taria  the real bridge, target/debug/taria-mcp --app taria-demo
  tmux   bench/tmux_mcp.py, screen-scraping the same pane

Both arms register their server under the same name, so the agent sees
`mcp__tui__<tool>` either way. When the solver or agent stops, the run is
graded on the demo's state dump by the task's check (bench/tasks.py), never
on the screen or the tree, and on the demo process still running. One JSON
line is printed and appended to the batch's results file.

Modes:
  --dry-run   a scripted, non-LLM solver (bench/solvers.py) drives the task
              through the same MCP server the agent would get, spawned from
              the same config entry. Free, and what proves the harness.
  --spend     one real agent per run with `claude -p`. Costs money; needs
              ANTHROPIC_API_KEY (`--bare` reads no other credential).

Every run ends in exactly one status:

  passed         the check passed and the demo is still running
  failed         the check said no, or the demo exited
  timeout        the agent ran out of wall clock (--timeout)
  budget         the agent hit --max-budget-usd (result error_max_budget_usd)
  max_turns      the agent hit --max-turns (result error_max_turns)
  harness_error  infrastructure, not the agent: a failed setup, a server
                 crash, the tool guard, a solver or harness exception, an
                 API or auth error, a permission denial, claude exiting
                 without a result. Out of the pass rate, counted per row.

The three limits are one rule: an agent that hit any of them did not finish
on its own, so the run is a failure in the pass rate, counted in its own
column. Its dump is still graded and `check_passed` recorded, so the summary
can say how many cut-off runs had the right state when they were stopped.

Jobs (task x arm x run) are shuffled with --seed, so neither arm always runs
first or warm. Every record carries the batch id, git SHA and dirty flag,
`claude --version`, the seed, the model, effort and limits it ran under
(`timeout_s`, `budget_usd`, `max_turns`; null where not passed or for a dry
run), and the names of the environment variables the agent was not given.
That is every flag that changes a run; everything else is the code at that
SHA (the dirty flag says when it is not), so summarize.py never has to guess
what produced a record.

The batch stops at once on the tool guard or a permission denial: both mean
the agent is not set up as the arm requires, so every later run would be a
harness error too, and paid for.

Exit codes: 0 every run passed; 1 a run did not pass (failed, or cut off by
a limit); 2 a harness error (including a stale or missing build, the guard,
a signal), which wins over 1.

Python 3 stdlib only. Never builds.

Usage:  python3 bench/run.py --dry-run [--tasks T1,T3] [--arms taria,tmux] [--runs N] [--sabotage]
        python3 bench/run.py --spend   [--tasks ...] [--arms ...] [--runs N] [--model M] [--effort E]
                                       [--timeout S] [--budget-usd D] [--max-turns N]
"""

import argparse
import json
import os
import random
import select
import shutil
import signal
import subprocess
import sys
import threading
import time
import traceback
import uuid
from datetime import datetime, timezone

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import (  # noqa: E402
    ARM_TOOLS,
    ARMS,
    REPO,
    SERVER_KEY,
    SERVER_NAMES,
    DemoRun,
    HarnessError,
    McpStdio,
    ToolError,
    mcp_config,
    require_fresh_build,
    wait_ready,
)
from solvers import CountingSession, SolverError, solve  # noqa: E402
from tasks import TASKS  # noqa: E402

RESULTS_DIR = os.path.join(BENCH, "results")
DEFAULT_MODEL = "claude-sonnet-5-5"
DEFAULT_EFFORT = "medium"
EFFORTS = ("low", "medium", "high", "xhigh", "max")  # `claude --help`, --effort
DEFAULT_BUDGET_USD = 1.0
DEFAULT_TIMEOUT_S = 600
# `claude --help` lists the --permission-mode choices; dontAsk denies whatever
# --allowedTools does not approve instead of prompting, which in -p mode
# nobody could answer. Pinned so a defaultMode in some settings file cannot
# change it, and checked against system/init by the guard.
PERMISSION_MODE = "dontAsk"
# Result subtypes that mean a limit stopped the agent (both carry is_error in
# the 2.1.294 binary), and the status each run gets.
CUTOFF_SUBTYPES = {"error_max_budget_usd": "budget", "error_max_turns": "max_turns"}
# The agent's environment is an allowlist: only these reach it, plus
# HARNESS_ENV. Anything else a shell exports can steer Claude Code (Claude
# Code's own session markers, CLAUDE_CODE_EFFORT_LEVEL overriding --effort,
# ANTHROPIC_MODEL, ANTHROPIC_BASE_URL, NODE_OPTIONS, a proxy), so it is
# dropped and its name, never its value, is recorded.
ENV_ALLOW = frozenset({
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
    "TMPDIR", "SHELL", "ANTHROPIC_API_KEY",
})
HARNESS_ENV = {}  # what the harness sets on the agent itself; nothing today

KILL_GRACE_S = 5.0  # SIGTERM to SIGKILL, for the agent's process group
RESULT_GRACE_S = 10.0  # how long claude may take to exit after its result

CUTOFFS = ("timeout", "budget", "max_turns")
STATUSES = ("passed", "failed", *CUTOFFS, "harness_error")
EXIT_PASSED, EXIT_FAILED, EXIT_HARNESS = 0, 1, 2


class Stopped(SystemExit):
    """SIGTERM or SIGHUP: unwinds like any exit, so every finally runs."""

    def __init__(self, signum):
        super().__init__(EXIT_HARNESS)
        self.signame = signal.Signals(signum).name


def _on_signal(signum, _frame):
    # Once is enough: a second signal must not cut the cleanup short.
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, signal.SIG_IGN)
    raise Stopped(signum)


class GuardError(Exception):
    """The agent was not set up as the arm requires. Systematic, so the whole
    batch stops rather than spending on runs that measure nothing."""


# --- Provenance --------------------------------------------------------------------


def _git(*args):
    try:
        out = subprocess.run(
            ["git", "-C", REPO, *args], capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout if out.returncode == 0 else None


def claude_version():
    """`claude --version`, or None when there is no claude on PATH. Free: no API call."""
    if shutil.which("claude") is None:
        return None
    try:
        out = subprocess.run(
            ["claude", "--version"], capture_output=True, text=True, timeout=30,
            env=spend_env()[0], stdin=subprocess.DEVNULL,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if out.returncode != 0:
        return None
    return out.stdout.strip() or None


def provenance(args):
    """What produced a batch: recorded on every one of its records."""
    sha = _git("rev-parse", "HEAD")
    status = _git("status", "--porcelain")
    return {
        "batch": args.batch,
        "git_sha": sha.strip() if sha else None,
        "git_dirty": None if status is None else bool(status.strip()),
        "claude_version": claude_version(),
    }


def new_batch_id():
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"{stamp}-{uuid.uuid4().hex[:6]}"


# --- Dry run -----------------------------------------------------------------------


def dry_run(task, arm, run, config, sabotage):
    name = SERVER_NAMES[arm]
    client = McpStdio(config["mcpServers"][SERVER_KEY], cwd=run.workdir, name=name)
    try:
        client.initialize()
        tools = client.tools_list()
        if sorted(tools) != sorted(ARM_TOOLS[arm]):
            raise HarnessError(
                f"{name} lists tools {sorted(tools)}, but the spend guard expects "
                f"{sorted(ARM_TOOLS[arm])}; update ARM_TOOLS in bench/harness.py"
            )
        wait_ready(arm, client)
        session = CountingSession(client)
        solver_error = None
        started = time.monotonic()
        try:
            solve(task.id, arm, session, sabotage)
        except (SolverError, ToolError) as err:
            # The scripted solver is part of the harness: when it trips, the
            # harness is in question, not the arm.
            solver_error = f"{type(err).__name__}: {getattr(err, 'message', err)}"
        wall_ms = round((time.monotonic() - started) * 1000)
    finally:
        client.close()
    return {
        "tool_calls": dict(session.calls),
        "wall_ms": wall_ms,
        "solver_error": solver_error,
        "harness_error": f"the scripted solver failed: {solver_error}" if solver_error else None,
    }


# --- Spend run ---------------------------------------------------------------------


def spend_command(prompt, config_path, budget_usd, model, effort=DEFAULT_EFFORT, max_turns=None):
    """The agent's command line. Every flag checked against `claude --help`
    (2.1.294), but --max-turns, which that version hides from --help and the
    binary still parses; what each one does that a free check cannot show is
    listed in bench/README.md.

    The prompt goes straight after -p, before the variadic options
    (--mcp-config, --tools, --allowedTools), so none of them can swallow it.
    """
    turns = [] if max_turns is None else ["--max-turns", str(max_turns)]
    return [
        "claude", "-p", prompt,
        "--bare",  # no hooks, plugin sync, auto-memory or CLAUDE.md discovery
        # No user, project or local settings files (an effortLevel, model,
        # outputStyle or enabledPlugins there would apply otherwise). The
        # binary parses an empty list as no sources; --restricted uses the
        # same path. Only managed (policy) settings still apply.
        "--setting-sources=",
        "--strict-mcp-config", "--mcp-config", config_path,
        "--tools", "",  # no built-in tools
        "--allowedTools", f"mcp__{SERVER_KEY}",  # the arm's tools, unprompted
        "--permission-mode", PERMISSION_MODE,
        "--disable-slash-commands",  # no skills
        "--output-format", "stream-json", "--verbose",
        "--no-session-persistence",
        "--max-budget-usd", f"{budget_usd:g}",
        "--model", model,
        "--effort", effort,
        *turns,
    ]


def spend_env():
    """The agent's environment: ENV_ALLOW from this one, plus HARNESS_ENV.

    Returns (env, the names dropped). Values are never recorded.
    """
    env = {k: v for k, v in os.environ.items() if k in ENV_ALLOW}
    env.update(HARNESS_ENV)
    return env, sorted(k for k in os.environ if k not in env)


# What system/init reports that bears on the measurement, recorded per run.
INIT_FIELDS = (
    "model", "tools", "mcp_servers", "plugins", "skills", "output_style",
    "permissionMode", "claude_code_version", "apiKeySource", "plugin_errors",
    "mcp_server_errors", "memory_paths",
)


def guard_problems(init, arm, permission_mode=PERMISSION_MODE):
    """Every way a system/init event says the agent is not set up as `arm` requires."""
    expected = sorted(f"mcp__{SERVER_KEY}__{t}" for t in ARM_TOOLS[arm])
    problems = []
    tools = sorted(init.get("tools") or [])
    if tools != expected:
        problems.append(f"the agent was given tools {tools}; the {arm} arm allows exactly {expected}")
    plugins = init.get("plugins") or []
    if plugins:
        names = [p.get("name", p) if isinstance(p, dict) else p for p in plugins]
        problems.append(f"plugins are loaded: {names}")
    skills = init.get("skills") or []
    if skills:
        problems.append(f"skills are loaded: {skills}")
    servers = [
        (s.get("name"), s.get("status")) if isinstance(s, dict) else (s, None)
        for s in init.get("mcp_servers") or []
    ]
    if servers != [(SERVER_KEY, "connected")]:
        problems.append(f"MCP servers are {servers}; expected only {SERVER_KEY!r}, connected")
    if init.get("permissionMode") != permission_mode:
        problems.append(
            f"permission mode is {init.get('permissionMode')!r}; expected {permission_mode!r}"
        )
    style = init.get("output_style")
    if style not in (None, "default"):
        problems.append(f"output style is {style!r}; expected 'default'")
    return problems


USAGE_KEYS = {
    "input": "input_tokens",
    "output": "output_tokens",
    "cache_read": "cache_read_input_tokens",
    "cache_creation": "cache_creation_input_tokens",
}


def usage_parts(usage):
    """{input, output, cache_read, cache_creation} from an API usage object."""
    return {short: usage.get(long) for short, long in USAGE_KEYS.items()}


class StreamStats:
    """What a `claude -p --output-format stream-json` run did, event by event.

    The guard lives here: the first event must be system/init, and it must
    pass guard_problems, or GuardError stops the run at that event. The init
    event and the first model request can overlap, so a stopped run may
    already have spent; it ends before any result event, so that spend is
    not known. Usage is kept twice: from the result event, which sums the
    session, and per assistant message as it streams, which is all a run cut
    off before its result leaves behind.
    """

    def __init__(self, arm, permission_mode=PERMISSION_MODE):
        self.arm = arm
        self.permission_mode = permission_mode
        self.init = None
        self.result = None
        self.tool_calls = {}
        self._seen_tool_ids = set()
        self._usage_by_message = {}

    def feed(self, event):
        kind = event.get("type")
        if kind == "system" and event.get("subtype") == "init":
            self.init = event
            try:
                problems = guard_problems(event, self.arm, self.permission_mode)
            except Exception as err:  # noqa: BLE001 - an init the guard cannot read is not vetted
                raise GuardError(f"could not vet system/init: {type(err).__name__}: {err}")
            if problems:
                raise GuardError("; ".join(problems))
            return
        if self.init is None and kind in ("assistant", "user", "result"):
            raise GuardError(f"a {kind} event arrived before system/init; cannot vet the agent")
        if kind == "assistant":
            message = event.get("message") or {}
            usage = message.get("usage")
            if isinstance(usage, dict):
                # One API message can stream as several events, each carrying
                # the message's usage so far: keep the latest per message.
                key = message.get("id") or f"anonymous-{len(self._usage_by_message)}"
                self._usage_by_message[key] = usage
            for block in message.get("content") or []:
                if not isinstance(block, dict) or block.get("type") != "tool_use":
                    continue
                if block.get("id") in self._seen_tool_ids:
                    continue
                self._seen_tool_ids.add(block.get("id"))
                name = block.get("name", "?").removeprefix(f"mcp__{SERVER_KEY}__")
                self.tool_calls[name] = self.tool_calls.get(name, 0) + 1
        elif kind == "result":
            self.result = event

    def usage(self):
        """(parts, source): from the result if one came, else summed from the stream."""
        result_usage = (self.result or {}).get("usage")
        if isinstance(result_usage, dict):
            parts = usage_parts(result_usage)
            if any(isinstance(v, int) for v in parts.values()):
                return parts, "result"
        if self._usage_by_message:
            totals = dict.fromkeys(USAGE_KEYS, 0)
            for usage in self._usage_by_message.values():
                for short, value in usage_parts(usage).items():
                    if isinstance(value, int):
                        totals[short] += value
            return totals, "stream"
        return None, None

    def agent_env(self):
        if self.init is None:
            return None
        return {field: self.init.get(field) for field in INIT_FIELDS}

    def summary(self):
        result = self.result or {}
        parts, source = self.usage()

        def part(name):
            value = (parts or {}).get(name)
            return value if isinstance(value, int) else None

        known = [v for v in (parts or {}).values() if isinstance(v, int)]
        tokens_in = [part(k) for k in ("input", "cache_read", "cache_creation")]
        denials = result.get("permission_denials") or []
        return {
            "result_subtype": result.get("subtype"),
            "is_error": result.get("is_error"),
            "turns": result.get("num_turns"),
            "duration_ms": result.get("duration_ms"),
            "duration_api_ms": result.get("duration_api_ms"),
            # Only a result event carries cost; a run cut off before one has
            # none to recover, and summarize.py says so.
            "cost_usd": result.get("total_cost_usd"),
            "usage": parts,
            "usage_source": source,
            "input_tokens": part("input"),
            "output_tokens": part("output"),
            "cache_read_tokens": part("cache_read"),
            "cache_creation_tokens": part("cache_creation"),
            "tokens_in": sum(v for v in tokens_in if v is not None) if any(v is not None for v in tokens_in) else None,
            "tokens_out": part("output"),
            "tokens": sum(known) if known else None,
            "agent_model": (self.init or {}).get("model"),
            "agent_env": self.agent_env(),
            "permission_denials": len(denials),
            "denied_tools": sorted({d.get("tool_name", "?") for d in denials if isinstance(d, dict)}),
            "tool_calls": dict(self.tool_calls),
        }


def _kill_group(proc, grace=None):
    """Stop the agent and everything it spawned (its own process group)."""
    grace = KILL_GRACE_S if grace is None else grace
    for sig, wait in ((signal.SIGTERM, grace), (signal.SIGKILL, 5)):
        if proc.poll() is not None:
            break
        try:
            os.killpg(proc.pid, sig)
        except OSError:
            break
        try:
            proc.wait(timeout=wait)
        except subprocess.TimeoutExpired:
            pass
    # The leader may be gone while something it spawned lingers in the group,
    # a child that ignored SIGTERM included.
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except OSError:
        pass


def _result_problem(result):
    """Why a result event is the harness's fault rather than the agent's, or None."""
    denials = result.get("permission_denials") or []
    if denials:
        names = sorted({d.get("tool_name", "?") for d in denials if isinstance(d, dict)})
        return (
            f"{len(denials)} permission denial(s) for {names}: the agent was refused "
            "a tool the arm should have approved"
        )
    if result.get("is_error") and result.get("subtype") not in CUTOFF_SUBTYPES:
        detail = result.get("result") or result.get("errors") or "(no detail)"
        return f"claude reported an error (subtype {result.get('subtype')}): {str(detail)[:400]}"
    return None


def spend_run(task, arm, run, args):
    cmd = spend_command(
        task.prompt, run.config_path, args.budget_usd, args.model, args.effort, args.max_turns
    )
    env, dropped = spend_env()
    stats = StreamStats(arm)
    stderr = bytearray()
    started = time.monotonic()
    # Its own session, so the group can be killed whole, and so a signal
    # aimed at this script's group does not reach it before cleanup does.
    proc = subprocess.Popen(
        cmd, cwd=run.workdir, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True, env=env,
    )

    def drain():
        for line in proc.stderr:
            stderr.extend(line)
            del stderr[:-4096]

    drainer = threading.Thread(target=drain, daemon=True)
    drainer.start()
    timed_out, guard, lingered, unreadable = False, None, False, None
    try:
        deadline = started + args.timeout
        fd = proc.stdout.fileno()
        buf = b""
        while guard is None and stats.result is None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                break
            ready, _, _ = select.select([fd], [], [], min(remaining, 1.0))
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf and guard is None and stats.result is None:
                line, buf = buf.split(b"\n", 1)
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue  # not an event; stream-json lines are all JSON
                if not isinstance(event, dict):
                    continue
                try:
                    stats.feed(event)
                except GuardError as err:
                    guard = str(err)
                except Exception as err:  # noqa: BLE001 - one odd event must not lose the run
                    # Reading on keeps the result event, and with it the cost.
                    if unreadable is None:
                        unreadable = f"{type(err).__name__}: {err} in {line[:200]!r}"
        if not timed_out and guard is None:
            try:
                proc.wait(timeout=RESULT_GRACE_S)
            except subprocess.TimeoutExpired:
                lingered = True  # killed below; the result, if any, stands
    finally:
        _kill_group(proc)
        drainer.join(2)  # the group is gone, so stderr is at EOF
        for pipe in (proc.stdout, proc.stderr):
            try:
                pipe.close()
            except (OSError, ValueError):
                pass
    wall_ms = round((time.monotonic() - started) * 1000)
    harness_error = None
    if guard is not None:
        harness_error = f"guard: {guard}"
    elif timed_out:
        pass  # the agent's outcome, not the harness's
    elif stats.result is None:
        tail = stderr.decode(errors="replace").strip()[-800:]
        harness_error = (
            f"claude exited {proc.returncode} without a result event; "
            f"stderr: {tail or '(empty)'}"
        )
    else:
        harness_error = _result_problem(stats.result)
    if harness_error is None and unreadable is not None:
        harness_error = f"a stream event could not be read: {unreadable}"
    return {
        **stats.summary(),
        "wall_ms": wall_ms,
        "timed_out": timed_out,
        "guard_failed": guard is not None,
        "harness_error": harness_error,
        "lingered": lingered,
        "exit_code": proc.returncode,
        "env_dropped": dropped,
    }


# --- One run -----------------------------------------------------------------------


def grade(task, run):
    """The task's check on the settled dump, and whether the demo still runs."""
    state = run.settled_state()
    alive = run.demo_alive()
    check_passed, reason = task.check(state)
    if not alive:
        reason = "demo exited" if check_passed else f"demo exited; {reason}"
    return {"state": state, "demo_alive": alive, "check_passed": check_passed, "reason": reason}


def classify(outcome, graded):
    """(status, passed, error) for one run, from what ran and how it graded.

    A run cut off by any limit failed, whatever its dump says: the agent did
    not finish on its own. Its `check_passed` is still recorded beside it.
    """
    error = outcome.get("harness_error")
    if error:
        return "harness_error", None, error
    if outcome.get("timed_out"):
        return "timeout", False, None
    cutoff = CUTOFF_SUBTYPES.get(outcome.get("result_subtype"))
    if cutoff:
        return cutoff, False, None
    passed = bool(graded["check_passed"] and graded["demo_alive"])
    return ("passed" if passed else "failed"), passed, None


def limits(args):
    """The limits a run is held to, recorded on every record: null for a dry
    run, which has none, and max_turns null when --max-turns was not passed."""
    if not args.spend:
        return {"timeout_s": None, "budget_usd": None, "max_turns": None}
    return {"timeout_s": args.timeout, "budget_usd": args.budget_usd, "max_turns": args.max_turns}


def run_one(task, arm, args, run_index, prov=None):
    prov = prov if prov is not None else provenance(args)
    record = {
        "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        **prov,
        "task": task.id,
        "arm": arm,
        "run": run_index,
        "seed": args.seed,
        "mode": "spend" if args.spend else "dry-run",
        "solver": "agent" if args.spend else ("scripted-sabotage" if args.sabotage else "scripted"),
        "model": args.model if args.spend else None,
        "effort": args.effort if args.spend else None,
        **limits(args),
    }
    run = None
    try:
        run = DemoRun()
        run.start()
        config = mcp_config(arm, run)
        with open(run.config_path, "w") as f:
            json.dump(config, f, indent=2)
        if task.setup is not None:
            try:
                task.setup(run)
            except HarnessError as err:
                raise HarnessError(f"setup for {task.id} failed: {err}")
        if args.spend:
            outcome = spend_run(task, arm, run, args)
        else:
            outcome = dry_run(task, arm, run, config, args.sabotage)
        # On the record before grading, so a failure from here on still
        # leaves the run's cost, usage and tool calls behind.
        record.update(outcome)
        record["tool_calls_total"] = sum(outcome["tool_calls"].values())
        graded = grade(task, run)
        status, passed, error = classify(outcome, graded)
        record.update(
            status=status,
            passed=passed,
            check_passed=graded["check_passed"],
            reason=graded["reason"],
            error=error,
            demo_alive=graded["demo_alive"],
            state=graded["state"],
        )
        exited = run.quit() if graded["demo_alive"] else False
        final = run.read_state()
        record["demo_exit"] = (
            "crashed" if not graded["demo_alive"]
            else "clean" if exited and final.get("quit") else "killed"
        )
    except Exception as err:  # noqa: BLE001 - anything here is the harness's
        record.update(
            status="harness_error",
            passed=None,
            error=f"{type(err).__name__}: {err}",
            traceback=traceback.format_exc(limit=8),
        )
    finally:
        if run is not None:
            run.close()
    return record


# --- Runner ------------------------------------------------------------------------


def _list(value, choices, what):
    items = [v.strip() for v in value.split(",") if v.strip()]
    bad = [v for v in items if v not in choices]
    if bad or not items:
        raise argparse.ArgumentTypeError(
            f"unknown {what} {', '.join(bad) or '(none)'}; choose from {', '.join(choices)}"
        )
    return items


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="Run taria benchmark tasks.")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true", help="scripted solver, no model")
    mode.add_argument("--spend", action="store_true", help="real claude -p runs (costs money)")
    task_ids, arms = sorted(TASKS), list(ARMS)
    parser.add_argument(
        "--tasks", "--task", dest="tasks", action="extend",
        type=lambda v: _list(v, task_ids, "task"),
        help=f"comma-separated task ids (default: all of {','.join(task_ids)})",
    )
    parser.add_argument(
        "--arms", "--arm", dest="arms", action="extend",
        type=lambda v: _list(v, arms, "arm"),
        help="comma-separated arms (default: taria,tmux)",
    )
    parser.add_argument("--runs", type=int, default=1, help="runs per task and arm (default 1)")
    parser.add_argument("--seed", type=int, help="shuffle seed (default: random, recorded)")
    parser.add_argument(
        "--sabotage", action="store_true",
        help="dry run only: the solver follows a wrong plan, so every check must fail",
    )
    parser.add_argument("--model", default=DEFAULT_MODEL, help=f"spend only (default {DEFAULT_MODEL})")
    parser.add_argument(
        "--effort", default=DEFAULT_EFFORT, choices=EFFORTS,
        help=f"spend only: claude --effort, always passed (default {DEFAULT_EFFORT})",
    )
    parser.add_argument(
        "--budget-usd", type=float, default=DEFAULT_BUDGET_USD,
        help=f"spend only: --max-budget-usd per run (default {DEFAULT_BUDGET_USD:g})",
    )
    parser.add_argument(
        "--timeout", type=float, default=DEFAULT_TIMEOUT_S,
        help=f"spend only: wall-clock seconds per run (default {DEFAULT_TIMEOUT_S})",
    )
    parser.add_argument(
        "--max-turns", type=int,
        help="spend only: claude --max-turns per run (default: not passed, no turn cap)",
    )
    parser.add_argument("--batch", help="batch id recorded on every run (default: time plus random)")
    parser.add_argument("--results", help="append to this file (default: bench/results/<batch>.jsonl)")
    args = parser.parse_args(argv)
    if args.sabotage and args.spend:
        parser.error("--sabotage is for --dry-run only")
    if args.runs < 1:
        parser.error("--runs must be at least 1")
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if args.budget_usd <= 0:
        parser.error("--budget-usd must be positive")
    if args.max_turns is not None and args.max_turns < 1:
        parser.error("--max-turns must be at least 1")
    args.tasks = list(dict.fromkeys(args.tasks or task_ids))
    args.arms = list(dict.fromkeys(args.arms or arms))
    if args.seed is None:
        args.seed = random.SystemRandom().randrange(2**31)
    if args.batch is None:
        args.batch = new_batch_id()
    return args


def results_path(path, batch):
    return path or os.path.join(RESULTS_DIR, f"{batch}.jsonl")


def has_api_key():
    return bool(os.environ.get("ANTHROPIC_API_KEY"))


def exit_code(statuses, passes):
    """2 on any harness error; else 1 if any run did not pass (a cut-off run
    never passes); else 0."""
    if "harness_error" in statuses:
        return EXIT_HARNESS
    return EXIT_PASSED if all(passes) else EXIT_FAILED


def run_all(args):
    if args.spend and not has_api_key():
        print(
            "run.py: --spend needs ANTHROPIC_API_KEY: it runs claude with --bare, which "
            "reads no other credential. A subscription (OAuth) run is not wired yet.",
            file=sys.stderr,
        )
        return EXIT_HARNESS
    missing = [tool for tool in ["tmux"] + (["claude"] if args.spend else []) if shutil.which(tool) is None]
    if missing:
        print(f"run.py: {', '.join(missing)} not on PATH", file=sys.stderr)
        return EXIT_HARNESS
    try:
        require_fresh_build()
    except HarnessError as err:
        print(f"run.py: {err}", file=sys.stderr)
        return EXIT_HARNESS

    jobs = [(t, a, r) for t in args.tasks for a in args.arms for r in range(args.runs)]
    random.Random(args.seed).shuffle(jobs)
    path = results_path(args.results, args.batch)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    prov = provenance(args)
    if args.spend:
        turns = "no turn cap" if args.max_turns is None else f"at most {args.max_turns} turns"
        print(
            f"run.py: batch {args.batch}: {len(jobs)} paid run(s), model {args.model}, "
            f"effort {args.effort}, {args.timeout:g}s and {turns} each, at most "
            f"${args.budget_usd:g} each (${args.budget_usd * len(jobs):g} in all)",
            file=sys.stderr,
        )
    statuses, passes = [], []
    for done, (task_id, arm, run_index) in enumerate(jobs, 1):
        record = run_one(TASKS[task_id], arm, args, run_index, prov)
        line = json.dumps(record)
        print(line, flush=True)
        with open(path, "a") as f:
            f.write(line + "\n")
        statuses.append(record["status"])
        passes.append(record.get("passed") is True)
        if record["status"] == "harness_error":
            print(f"run.py: harness error in {task_id}/{arm}: {record['error']}", file=sys.stderr)
        stop = None
        if record.get("guard_failed"):
            stop = "the guard refused the agent's setup at its init event"
        elif record.get("permission_denials"):
            stop = (
                f"the agent was refused {record.get('denied_tools')}, so --allowedTools "
                "does not approve the arm's tools"
            )
        if stop:
            print(
                f"run.py: stopped the batch: {stop}; {len(jobs) - done} run(s) not started",
                file=sys.stderr,
            )
            break
    print(f"run.py: batch {args.batch}, results in {path}", file=sys.stderr)
    return exit_code(statuses, passes)


def main():
    args = parse_args()
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, _on_signal)
    try:
        code = run_all(args)
    except Stopped as stop:
        print(f"run.py: stopped by {stop.signame}; the run's demo, tmux server and temp dir are gone", file=sys.stderr)
        code = EXIT_HARNESS
    except KeyboardInterrupt:
        print("run.py: interrupted; the run's demo, tmux server and temp dir are gone", file=sys.stderr)
        code = EXIT_HARNESS
    except Exception as err:  # noqa: BLE001 - a harness bug is a harness error
        traceback.print_exc()
        print(f"run.py: harness error: {type(err).__name__}: {err}", file=sys.stderr)
        code = EXIT_HARNESS
    sys.exit(code)


if __name__ == "__main__":
    main()
