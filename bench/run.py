#!/usr/bin/env python3
"""Run benchmark tasks against taria-demo through one or both arms.

Each run gets a fresh demo in its own tmux server and temp dir (see
harness.DemoRun for the layout) and an MCP config for its arm:

  taria  the real bridge, target/debug/taria-mcp --app taria-demo
  tmux   bench/tmux_mcp.py, screen-scraping the same pane

Both arms register their server under the same name, so the agent sees
`mcp__tui__<tool>` either way. When the solver or agent stops, the run is
graded on the demo's state dump by the task's check (bench/tasks.py), never
on the screen or the tree, and one JSON line is printed and appended to
bench/results/.

Modes:
  --dry-run   a scripted, non-LLM solver (bench/solvers.py) drives the task
              through the same MCP server the agent would get, spawned from
              the same config entry. Free, and what proves the harness.
  --spend     one real agent per run with `claude -p`. Costs money; needs
              ANTHROPIC_API_KEY (`--bare` reads no other credential).

Jobs (task x arm x run) are shuffled with --seed, so neither arm always runs
first or warm.

Exit codes: 0 every run passed; 1 a task failed; 2 a harness error (stale or
missing build, a server crash, a timeout, a failed setup, a signal), which
wins over 1.

Python 3 stdlib only. Never builds.

Usage:  python3 bench/run.py --dry-run [--tasks T1,T3] [--arms taria,tmux] [--runs N] [--sabotage]
        python3 bench/run.py --spend   [--tasks ...] [--arms ...] [--runs N] [--model M]
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
from datetime import datetime, timezone

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import (  # noqa: E402
    ARM_TOOLS,
    ARMS,
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
DEFAULT_BUDGET_USD = 1.0
DEFAULT_TIMEOUT_S = 600

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
    """The agent was not given exactly its arm's tools. Systematic, so the
    whole batch stops rather than spending on runs that measure nothing."""


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
            solver_error = f"{type(err).__name__}: {getattr(err, 'message', err)}"
        wall_ms = round((time.monotonic() - started) * 1000)
    finally:
        client.close()
    return {
        "tool_calls": dict(session.calls),
        "wall_ms": wall_ms,
        "solver_error": solver_error,
    }


# --- Spend run ---------------------------------------------------------------------


def spend_command(prompt, config_path, budget_usd, model):
    """The agent's command line. Every flag checked against `claude --help`.

    The prompt goes straight after -p, before the variadic options
    (--mcp-config, --tools, --allowedTools), so none of them can swallow it.
    """
    return [
        "claude", "-p", prompt,
        "--bare",  # no hooks, plugins, auto-memory or CLAUDE.md discovery
        "--strict-mcp-config", "--mcp-config", config_path,
        "--tools", "",  # no built-in tools
        "--allowedTools", f"mcp__{SERVER_KEY}",  # the arm's tools, unprompted
        "--disable-slash-commands",
        "--output-format", "stream-json", "--verbose",
        "--no-session-persistence",
        "--max-budget-usd", f"{budget_usd:g}",
        "--model", model,
    ]


class StreamStats:
    """What a `claude -p --output-format stream-json` run did, event by event.

    The guard lives here: the first event must be system/init, and its tool
    list must be exactly the arm's tools, or GuardError stops the run before
    the agent's first turn is spent.
    """

    def __init__(self, arm):
        self.arm = arm
        self.expected_tools = sorted(f"mcp__{SERVER_KEY}__{t}" for t in ARM_TOOLS[arm])
        self.init = None
        self.result = None
        self.tool_calls = {}
        self._seen_tool_ids = set()

    def feed(self, event):
        kind = event.get("type")
        if kind == "system" and event.get("subtype") == "init":
            tools = sorted(event.get("tools") or [])
            if tools != self.expected_tools:
                raise GuardError(
                    f"the agent was given tools {tools}; the {self.arm} arm allows exactly "
                    f"{self.expected_tools} (mcp_servers: {event.get('mcp_servers')})"
                )
            self.init = event
            return
        if self.init is None and kind in ("assistant", "user", "result"):
            raise GuardError(f"a {kind} event arrived before system/init; cannot vet the tools")
        if kind == "assistant":
            for block in (event.get("message") or {}).get("content") or []:
                if not isinstance(block, dict) or block.get("type") != "tool_use":
                    continue
                if block.get("id") in self._seen_tool_ids:
                    continue
                self._seen_tool_ids.add(block.get("id"))
                name = block.get("name", "?").removeprefix(f"mcp__{SERVER_KEY}__")
                self.tool_calls[name] = self.tool_calls.get(name, 0) + 1
        elif kind == "result":
            self.result = event

    def summary(self):
        result = self.result or {}
        usage = result.get("usage") or {}
        parts = {
            "input": usage.get("input_tokens"),
            "output": usage.get("output_tokens"),
            "cache_read": usage.get("cache_read_input_tokens"),
            "cache_creation": usage.get("cache_creation_input_tokens"),
        }
        known = [v for v in parts.values() if isinstance(v, int)]
        return {
            "result_subtype": result.get("subtype"),
            "is_error": result.get("is_error"),
            "turns": result.get("num_turns"),
            "duration_ms": result.get("duration_ms"),
            "duration_api_ms": result.get("duration_api_ms"),
            "cost_usd": result.get("total_cost_usd"),
            "usage": parts if known else None,
            "tokens": sum(known) if known else None,
            "agent_model": (self.init or {}).get("model"),
            # Nonzero means --allowedTools did not approve a call: a harness
            # problem showing up as a task failure, so it is kept in view.
            "permission_denials": len(result.get("permission_denials") or []),
            "tool_calls": dict(self.tool_calls),
        }


def _kill_group(proc):
    """Stop the agent and the MCP servers it spawned (its own process group)."""
    for sig, wait in ((signal.SIGTERM, 5), (signal.SIGKILL, 5)):
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
    # The leader may be gone while a server it spawned lingers in the group.
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except OSError:
        pass


def spend_run(task, arm, run, args):
    cmd = spend_command(task.prompt, run.config_path, args.budget_usd, args.model)
    stats = StreamStats(arm)
    stderr = bytearray()
    started = time.monotonic()
    # Its own session, so the group can be killed whole, and so a signal
    # aimed at this script's group does not reach it before cleanup does.
    proc = subprocess.Popen(
        cmd, cwd=run.workdir, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True,
    )

    def drain():
        for line in proc.stderr:
            stderr.extend(line)
            del stderr[:-4096]

    threading.Thread(target=drain, daemon=True).start()
    error = None
    try:
        deadline = started + args.timeout
        fd = proc.stdout.fileno()
        buf = b""
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                error = f"the agent timed out after {args.timeout} s"
                break
            ready, _, _ = select.select([fd], [], [], min(remaining, 1.0))
            if not ready:
                continue
            chunk = os.read(fd, 65536)
            if not chunk:
                break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if not line.strip():
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue  # not an event; stream-json lines are all JSON
                if isinstance(event, dict):
                    stats.feed(event)
        if error is None:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                error = "the agent closed its output but did not exit"
    finally:
        _kill_group(proc)
    wall_ms = round((time.monotonic() - started) * 1000)
    if error is None and stats.result is None:
        tail = stderr.decode(errors="replace").strip()[-800:]
        error = f"claude exited {proc.returncode} without a result event; stderr: {tail or '(empty)'}"
    return {**stats.summary(), "wall_ms": wall_ms, "error": error}


# --- One run -----------------------------------------------------------------------


def run_one(task, arm, args, run_index):
    record = {
        "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "task": task.id,
        "arm": arm,
        "run": run_index,
        "seed": args.seed,
        "mode": "spend" if args.spend else "dry-run",
        "solver": "agent" if args.spend else ("scripted-sabotage" if args.sabotage else "scripted"),
        "model": args.model if args.spend else None,
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
            outcome.update(tokens=None, usage=None, cost_usd=None, turns=None)
        state = run.settled_state()
        passed, reason = task.check(state)
        exited = run.quit()
        final = run.read_state()
        error = outcome.pop("error", None)
        record.update(outcome)
        record.update(
            status="harness_error" if error else ("passed" if passed else "failed"),
            passed=None if error else passed,
            reason=reason,
            error=error,
            tool_calls_total=sum(outcome["tool_calls"].values()),
            demo_exit="clean" if exited and final.get("quit") else "killed",
            state=state,
        )
    except GuardError:
        raise
    except (HarnessError, OSError, ValueError, subprocess.SubprocessError) as err:
        record.update(status="harness_error", passed=None, error=str(err))
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
        "--budget-usd", type=float, default=DEFAULT_BUDGET_USD,
        help=f"spend only: --max-budget-usd per run (default {DEFAULT_BUDGET_USD:g})",
    )
    parser.add_argument(
        "--timeout", type=int, default=DEFAULT_TIMEOUT_S,
        help=f"spend only: wall-clock seconds per run (default {DEFAULT_TIMEOUT_S})",
    )
    parser.add_argument("--results", help="append to this file instead of a new one")
    args = parser.parse_args(argv)
    if args.sabotage and args.spend:
        parser.error("--sabotage is for --dry-run only")
    if args.runs < 1:
        parser.error("--runs must be at least 1")
    args.tasks = list(dict.fromkeys(args.tasks or task_ids))
    args.arms = list(dict.fromkeys(args.arms or arms))
    if args.seed is None:
        args.seed = random.SystemRandom().randrange(2**31)
    return args


def results_path(path):
    if path:
        return path
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return os.path.join(RESULTS_DIR, f"{stamp}.jsonl")


def run_all(args):
    if args.spend and not os.environ.get("ANTHROPIC_API_KEY"):
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
    path = results_path(args.results)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    if args.spend:
        print(
            f"run.py: {len(jobs)} paid run(s), model {args.model}, at most "
            f"${args.budget_usd:g} each (${args.budget_usd * len(jobs):g} in all)",
            file=sys.stderr,
        )
    statuses = []
    for task_id, arm, run_index in jobs:
        try:
            record = run_one(TASKS[task_id], arm, args, run_index)
        except GuardError as err:
            print(f"run.py: aborted before the agent's first turn: {err}", file=sys.stderr)
            return EXIT_HARNESS
        line = json.dumps(record)
        print(line, flush=True)
        with open(path, "a") as f:
            f.write(line + "\n")
        if record["status"] == "harness_error":
            print(f"run.py: harness error in {task_id}/{arm}: {record['error']}", file=sys.stderr)
        statuses.append(record["status"])
    print(f"run.py: results in {path}", file=sys.stderr)
    if "harness_error" in statuses:
        return EXIT_HARNESS
    return EXIT_FAILED if "failed" in statuses else EXIT_PASSED


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
    sys.exit(code)


if __name__ == "__main__":
    main()
