#!/usr/bin/env python3
"""Run benchmark tasks against taria-demo through one or more arms.

Each run gets a fresh demo in its own tmux server and temp dir (see
harness.DemoRun for the layout), and one of three MCP servers, all registered
as `tui`, so the agent sees `mcp__tui__<tool>` whichever it is:

  taria    the real bridge, target/debug/taria-mcp --app taria-demo (the tree as JSON)
  outline  the same bridge with --tree-format outline (the same tree, one line per node)
  tmux     bench/tmux_mcp.py, screen-scraping the same pane

A run is graded on the demo's state dump by the task's check (bench/tasks.py),
never on the screen or the tree, and on the demo process still running.

Two ways to drive a run:

  dry-run   a scripted, non-LLM solver (bench/solvers.py) drives each task
            through the same server an agent gets. Free; proves the harness.
  subagent  real Claude Code subagents, spawned by an orchestrating Claude
            Code session through its Agent tool. Python never calls a model:

              run.py agents --slots K --dir D   once: the agent files
              run.py plan --batch B ...         once: the shuffled job list
              run.py next --batch B --slot K    prints one job as JSON
              Agent(subagent_type=<agent>, model=<model>, prompt=<prompt>)
              run.py next --batch B --slot K --agent-id <id>
                                                grades that agent's run and
                                                prints the slot's next job

            Up to K slots run at once. `next` prepares a demo in the slot and
            writes results/slots/<K>.json; the agent file for that arm and
            slot (bench/agents.py) starts bench/mcp_launch.py, which reads the
            slot file and execs the arm's server against that demo, once per
            job (results/slots/<K>.<job>.launched). When the agent finishes,
            `next --agent-id` copies its transcript and .meta.json into
            results/<batch>/transcripts/, reads it (bench/transcript.py),
            grades the demo, records the row and tears the demo down. It
            refuses (exit 2, nothing recorded, the slot kept) what is the
            orchestrator's mistake rather than a run: an agent id the batch
            already recorded, an id with no transcript, a transcript with no
            .meta.json or whose meta names another slot's agent, and a
            transcript whose first prompt is not the job's task or that
            started before the slot was prepared.

  regrade   grades every row of a finished batch again with the current
            code, from what persists (the recorded state and demo_alive, the
            transcript re-read: the batch's archived copy, else the live one
            under ~/.claude/projects), after copying runs.jsonl to
            runs.orig-<utc>.jsonl. For after a change to the grading logic,
            so one batch keeps one logic for all its rows.

  archive   copies the live transcript of every row of a batch that has no
            archived copy yet into results/<batch>/transcripts/. Claude Code
            deletes transcripts after cleanupPeriodDays (default 30), and
            regrade needs them; `next` archives as it grades, so this is for
            a batch graded before it did.

`plan --tolerate-attachment TYPE` (repeatable; only remote_session_change,
Claude Code's remote-control attribution note) records the type in the
plan's tolerated_attachments, and grading (next and regrade) reads every
transcript of the batch with it: such an attachment in the shape it is known
in is recorded in the row's context.tolerated_attachments instead of making
the run a harness error. With one orchestrating session every arm of the
batch gets the same note, so its arms still compare with each other, but
its absolute token counts include the note, and it is not a clean batch
(summarize.py marks it).

taria and outline serve the same tool names, and a Claude Code session keeps
the first definitions it sees for a tool name, so the two never share an
orchestrating session: `plan` refuses a batch with both (its arms default to
taria,tmux), and `next` refuses to hand out a job of a batch whose arm shares
tool names with another arm the same session was already handed, in any
batch, telling the operator to restart the session. It knows the session by
CLAUDE_PID, which Claude Code sets for its Bash commands, and records each
hand-out in results/slots/handouts.jsonl; outside Claude Code it cannot
tell, and only the procedure (one session per arm family, restarted between)
protects the batch.

Every run ends in exactly one status:

  passed         the check passed and the demo is still running
  failed         the check said no, or the demo exited
  max_turns      the agent was stopped at the batch's max turns (see
                 transcript.ending): a failure, whatever the dump says
                 (check_passed is recorded)
  harness_error  infrastructure, not the agent: a failed setup, a server or
                 solver failure, an unreadable transcript, the transcript
                 guard refusing how the agent was set up or context that
                 reached it (an attachment it has no rule for, say), or a
                 run that did
                 not end on its own (Claude Code's own error message last, an
                 explicit stop other than end_turn, or a tool call under
                 max_turns). A final text reply whose closing line Claude
                 Code never wrote ended on its own (transcript.ending; the
                 row's end_inferred). Out of the pass rate.

Rows go to results/<batch>/runs.jsonl, one JSON object per run, with the
same keys in both modes (ROW_KEYS; null where a mode has no value).

Exit codes: 0 ok (dry-run: every run passed); 1 dry-run: a run did not
pass; 2 a harness error (a stale build, bad arguments, slot misuse, a
graded run that was a harness error, after which `next` claims nothing, or
an `archive` that found a row's transcript neither archived nor live).

Python 3 stdlib only. Never builds, never calls a model.

Usage:  python3 bench/run.py dry-run [--tasks T1,T3] [--arms taria,tmux,outline] [--runs N] [--seed S]
                                     [--sabotage] [--batch B]
        python3 bench/run.py agents  --slots K [--dir D] [--effort E] [--max-turns N] [--remove]
        python3 bench/run.py plan    --batch B [--tasks ..] [--arms ..] [--models sonnet,haiku]
                                     [--runs N] [--seed S] [--effort E] [--max-turns N]
                                     [--tolerate-attachment remote_session_change]
        python3 bench/run.py next    --batch B --slot K [--agent-id ID] [--agents-dir D]
        python3 bench/run.py status  --batch B
        python3 bench/run.py abort   --batch B --slot K [--reason TEXT]
        python3 bench/run.py regrade --batch B
        python3 bench/run.py archive --batch B
"""

import argparse
import fcntl
import glob
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import time
import traceback
import uuid
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
import agents  # noqa: E402
import mcp_launch  # noqa: E402
from harness import (  # noqa: E402
    ARM_TOOLS,
    ARM_TREE_FORMAT,
    ARMS,
    REPO,
    SERVER_KEY,
    SERVER_NAMES,
    DemoRun,
    HarnessError,
    McpStdio,
    ToolError,
    require_fresh_build,
    server_entry,
    wait_ready,
)
from solvers import CountingSession, SolverError, solve  # noqa: E402
from tasks import TASKS  # noqa: E402

RESULTS_DIR = os.path.join(BENCH, "results")
# The Agent tool's model choices; a plan names model families from these.
MODELS = ("sonnet", "opus", "haiku", "fable")
DEFAULT_MODELS = "sonnet"
STATUSES = ("passed", "failed", "max_turns", "harness_error")
EXIT_OK, EXIT_FAILED, EXIT_HARNESS = 0, 1, 2
BATCH_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$")
AGENT_ID_RE = re.compile(r"^[0-9A-Za-z_-]+$")
# A plan's arms when --arms is not given: two that share no tool name (see
# colliding_arms). The dry run, which spawns no agent, runs every arm.
PLAN_DEFAULT_ARMS = ("taria", "tmux")
# How `next` tells which orchestrating session runs it: Claude Code 2.1.294
# puts its own pid in CLAUDE_PID for the commands its Bash tool runs (and its
# session id in CLAUDE_CODE_SESSION_ID, recorded for the reader).
SESSION_PID_ENV = "CLAUDE_PID"
SESSION_ID_ENV = "CLAUDE_CODE_SESSION_ID"

# One row per run, in this order, in both modes. From an agent's transcript:
# model_resolved (the models it ran on), efforts_resolved (the effort Claude
# Code recorded on its turns, which proves the effort used), claude_versions
# (and claude_version, the same joined into one string): read, never asked of
# `claude`, so they name what ran the agent rather than what is installed now.
ROW_KEYS = (
    "ts", "batch", "job", "task", "arm", "run", "slot", "seed", "mode", "solver",
    "model", "model_resolved", "effort", "efforts_resolved", "max_turns", "agent_id",
    "session_id", "git_sha", "git_dirty", "claude_version", "claude_versions",
    "status", "passed", "check_passed", "reason", "error", "guard_problems",
    "end_inferred", "notes",
    "demo_alive", "demo_exit", "state",
    "turns", "tool_calls", "tool_calls_total", "toolsearch_calls", "tokens",
    "output_tokens_lower_bound", "unclosed_messages",
    "wall_ms", "final_text", "context",
)


class CommandError(Exception):
    """A subcommand refused: bad arguments, a stale build, slot misuse. Exit 2."""


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


def now(timespec="seconds"):
    return datetime.now(timezone.utc).isoformat(timespec=timespec)


def parse_time(value):
    """An aware datetime from an ISO 8601 string (`Z` or an offset; none is
    UTC), or None."""
    if not isinstance(value, str):
        return None
    try:
        stamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return stamp if stamp.tzinfo is not None else stamp.replace(tzinfo=timezone.utc)


def new_row(**fields):
    unknown = set(fields) - set(ROW_KEYS)
    if unknown:
        raise KeyError(f"not row keys: {sorted(unknown)}")
    return {key: fields.get(key) for key in ROW_KEYS}


# --- Provenance --------------------------------------------------------------------


def _git(*args):
    try:
        out = subprocess.run(
            ["git", "-C", REPO, *args], capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout if out.returncode == 0 else None


def git_provenance():
    sha = _git("rev-parse", "HEAD")
    status = _git("status", "--porcelain")
    return {
        "git_sha": sha.strip() if sha else None,
        "git_dirty": None if status is None else bool(status.strip()),
    }


def new_batch_id():
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return f"{stamp}-{uuid.uuid4().hex[:6]}"


# --- Where things live ---------------------------------------------------------------


class Paths:
    """results/<batch>/{plan.json,runs.jsonl,.lock,transcripts/},
    results/slots/<K>.json and results/slots/handouts.jsonl."""

    def __init__(self, results_dir, batch=None):
        self.results = os.path.abspath(results_dir)
        self.slots = os.path.join(self.results, "slots")
        # Which arms each orchestrating session was handed, across batches
        # (see claim_in_session).
        self.handouts = os.path.join(self.slots, "handouts.jsonl")
        self.handouts_lock = os.path.join(self.slots, ".handouts.lock")
        self.batch = batch
        if batch is not None:
            self.batch_dir = os.path.join(self.results, batch)
            self.plan = os.path.join(self.batch_dir, "plan.json")
            self.runs = os.path.join(self.batch_dir, "runs.jsonl")
            self.lock = os.path.join(self.batch_dir, ".lock")
            self.transcripts = os.path.join(self.batch_dir, "transcripts")

    def slot(self, k):
        return os.path.join(self.slots, f"{k}.json")

    def slot_lock(self, k):
        return os.path.join(self.slots, f".{k}.lock")


@contextmanager
def flocked(path, blocking=True):
    """An exclusive flock on `path` (created if need be) for the block.

    Non-blocking raises BlockingIOError when another process holds it.
    """
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        yield
    finally:
        os.close(fd)


def read_rows(paths):
    try:
        with open(paths.runs) as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        return []
    rows = []
    for line in lines:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict):
            rows.append(row)
    return rows


def has_rows(paths):
    """Whether the batch's runs.jsonl holds anything at all, a line that is
    not a row included."""
    try:
        with open(paths.runs, "rb") as f:
            return bool(f.read().strip())
    except FileNotFoundError:
        return False


def append_row(paths, row):
    os.makedirs(os.path.dirname(paths.runs), exist_ok=True)
    line = json.dumps(row) + "\n"
    with flocked(paths.lock):
        with open(paths.runs, "a") as f:
            f.write(line)


def load_plan(paths):
    try:
        with open(paths.plan) as f:
            return json.load(f)
    except FileNotFoundError:
        raise CommandError(f"no plan for batch {paths.batch} ({paths.plan}); run `run.py plan` first")
    except ValueError as err:
        raise CommandError(f"{paths.plan} is not JSON: {err}")


def read_slot(paths, k):
    try:
        with open(paths.slot(k)) as f:
            slot = json.load(f)
    except FileNotFoundError:
        return None
    except ValueError as err:
        raise CommandError(f"{paths.slot(k)} is not JSON ({err}); `run.py abort` cannot read it either")
    if not isinstance(slot, dict):
        raise CommandError(f"{paths.slot(k)} is not a slot file")
    return slot


def read_all_slots(paths):
    """{K: slot} for every readable slot file."""
    slots = {}
    for path in glob.glob(os.path.join(paths.slots, "*.json")):
        name = os.path.basename(path)[: -len(".json")]
        if not name.isdigit():
            continue
        try:
            with open(path) as f:
                slot = json.load(f)
        except (OSError, ValueError):
            continue
        if isinstance(slot, dict):
            slots[int(name)] = slot
    return slots


def create_slot(paths, k, slot):
    """Write a new slot file; refuses one that exists (the slot is held)."""
    os.makedirs(paths.slots, exist_ok=True)
    try:
        fd = os.open(paths.slot(k), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        raise CommandError(f"slot {k} is held ({paths.slot(k)} exists)")
    with os.fdopen(fd, "w") as f:
        json.dump(slot, f, indent=2)


def write_slot(paths, k, slot):
    tmp = f"{paths.slot(k)}.{os.getpid()}.tmp"
    with open(tmp, "w") as f:
        json.dump(slot, f, indent=2)
    os.replace(tmp, paths.slot(k))


def free_slot(paths, k):
    """Remove slot `k`'s file and its launch markers (mcp_launch.py)."""
    try:
        os.remove(paths.slot(k))
    except FileNotFoundError:
        pass
    mcp_launch.clear_markers(paths.slots, k)


def pid_alive(pid):
    if not isinstance(pid, int) or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def teardown(slot):
    """Close the demo a slot describes, if it describes one. Never raises."""
    demo = slot.get("demo")
    if not demo:
        return
    try:
        DemoRun.from_slot(demo).close()
    except HarnessError as err:
        print(f"run.py: could not tear down slot's demo: {err}", file=sys.stderr)


# --- One run, either mode ---------------------------------------------------------------


def prepare_demo(task, on_created=None):
    """A started demo, through the task's setup, ready for any arm's server.

    `on_created(run)` is called once the temp dir exists and before anything
    is started, so a caller can record what to tear down if this process dies.
    """
    run = DemoRun()
    try:
        if on_created is not None:
            on_created(run)
        run.start()
        if task.setup is not None:
            try:
                task.setup(run)
            except HarnessError as err:
                raise HarnessError(f"setup for {task.id} failed: {err}")
        run.wait_ready_for_agent()
    except BaseException:
        run.close()
        raise
    return run


def judge(task, state, alive):
    """The task's check on a dump, and whether the demo still ran."""
    check_passed, reason = task.check(state)
    if not alive:
        reason = "demo exited" if check_passed else f"demo exited; {reason}"
    return {"state": state, "demo_alive": alive, "check_passed": check_passed, "reason": reason}


def grade(task, run):
    """The task's check on the settled dump, and whether the demo still runs."""
    return judge(task, run.settled_state(), run.demo_alive())


def classify(graded, error=None, cut_off=False):
    """(status, passed) for one run.

    A harness error wins; a run cut off at max turns failed whatever its dump
    says; otherwise it passed only if the check passed and the demo runs.
    """
    if error:
        return "harness_error", None
    if cut_off:
        return "max_turns", False
    passed = bool(graded["check_passed"] and graded["demo_alive"])
    return ("passed" if passed else "failed"), passed


def quit_demo(run, alive):
    """How the demo ended: clean (it quit when asked, or had quit before
    grading, the agent having quit it: its dump says `quit: true`), killed
    (still running when asked, it did not quit), or crashed (gone before
    grading, its dump not saying it quit)."""
    if not alive:
        return "clean" if run.read_state().get("quit") is True else "crashed"
    exited = run.quit()
    final = run.read_state()
    return "clean" if exited and final.get("quit") else "killed"


# --- Dry run -------------------------------------------------------------------------


def drive_scripted(task, arm, run, sabotage):
    name = SERVER_NAMES[arm]
    client = McpStdio(server_entry(arm, run), cwd=run.workdir, name=name)
    try:
        client.initialize()
        tools = client.tools_list()
        if sorted(tools) != sorted(ARM_TOOLS[arm]):
            raise HarnessError(
                f"{name} lists tools {sorted(tools)}, but grading expects "
                f"{sorted(ARM_TOOLS[arm])}; update ARM_TOOLS in bench/harness.py"
            )
        check_tree_format(arm, name, client.tools)
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
    return dict(session.calls), wall_ms, solver_error


def check_tree_format(arm, name, tools):
    """A bridge arm's server must describe the tree format the arm is for:
    taria and outline list the same tools, and the read_tree description is
    what tells them apart (transcript.guard holds an agent's run to it)."""
    want = ARM_TREE_FORMAT.get(arm)
    if want is None:
        return
    described = load_transcript_module().tree_format_of(
        next((t.get("description") for t in tools if t.get("name") == "read_tree"), None)
    )
    if described != want:
        raise HarnessError(
            f"{name} describes read_tree as showing the tree as {described or 'nothing known'}, "
            f"but the {arm} arm needs {want}: is target/debug/taria-mcp current?"
        )


def dry_run_one(task, arm, run_index, job_id, batch, seed, sabotage, prov):
    row = new_row(
        ts=now(), batch=batch, job=job_id, task=task.id, arm=arm, run=run_index, seed=seed,
        mode="dry-run", solver="scripted-sabotage" if sabotage else "scripted", **prov,
    )
    run = None
    try:
        run = prepare_demo(task)
        calls, wall_ms, solver_error = drive_scripted(task, arm, run, sabotage)
        row.update(tool_calls=calls, tool_calls_total=sum(calls.values()), wall_ms=wall_ms)
        graded = grade(task, run)
        error = f"the scripted solver failed: {solver_error}" if solver_error else None
        status, passed = classify(graded, error)
        row.update(
            status=status, passed=passed, check_passed=graded["check_passed"],
            reason=graded["reason"], error=error, demo_alive=graded["demo_alive"],
            state=graded["state"],
        )
        row["demo_exit"] = quit_demo(run, graded["demo_alive"])
    except Exception as err:  # noqa: BLE001 - anything here is the harness's
        traceback.print_exc(limit=8, file=sys.stderr)
        row.update(status="harness_error", passed=None, error=f"{type(err).__name__}: {err}")
    finally:
        if run is not None:
            run.close()
    return row


def preflight():
    if shutil.which("tmux") is None:
        raise CommandError("tmux not on PATH")
    try:
        require_fresh_build()
    except HarnessError as err:
        raise CommandError(str(err))


def cmd_dry_run(args):
    paths = Paths(args.results_dir, args.batch)
    if os.path.exists(paths.plan):
        raise CommandError(f"batch {args.batch} is a subagent batch ({paths.plan}); name another")
    preflight()
    jobs = [(t, a, r) for t in args.tasks for a in args.arms for r in range(args.runs)]
    random.Random(args.seed).shuffle(jobs)
    prov = {**git_provenance(), "claude_version": None}
    statuses, passes = [], []
    for number, (task_id, arm, run_index) in enumerate(jobs, 1):
        row = dry_run_one(
            TASKS[task_id], arm, run_index, f"j{number:03d}", args.batch, args.seed,
            args.sabotage, prov,
        )
        append_row(paths, row)
        print(json.dumps(row), flush=True)
        statuses.append(row["status"])
        passes.append(row["passed"] is True)
        if row["status"] == "harness_error":
            print(f"run.py: harness error in {task_id}/{arm}: {row['error']}", file=sys.stderr)
    counts = {s: statuses.count(s) for s in STATUSES if s in statuses}
    print(
        f"run.py: batch {args.batch}: {sum(passes)}/{len(jobs)} passed "
        f"({', '.join(f'{s} {n}' for s, n in counts.items())}); results in {paths.runs}",
        file=sys.stderr,
    )
    if "harness_error" in statuses:
        return EXIT_HARNESS
    return EXIT_OK if all(passes) else EXIT_FAILED


# --- Agents and plans -------------------------------------------------------------------


def cmd_agents(args):
    if args.remove:
        removed = agents.remove(args.dir)
        print(f"run.py: removed {len(removed)} agent file(s) from {args.dir}")
        return EXIT_OK
    if args.slots is None or args.slots < 1:
        raise CommandError("agents needs --slots K (at least 1), or --remove")
    slots_dir = Paths(args.results_dir).slots
    try:
        written = agents.write(args.slots, args.dir, slots_dir, args.effort, args.max_turns)
    except (FileExistsError, ValueError) as err:
        raise CommandError(str(err))
    print(
        f"run.py: wrote {len(written)} agent file(s) to {args.dir} (slots 1-{args.slots}, "
        f"{', '.join(ARMS)}; effort {args.effort}, maxTurns {args.max_turns}); "
        f"their launcher reads {slots_dir}"
    )
    return EXIT_OK


def cmd_plan(args):
    paths = Paths(args.results_dir, args.batch)
    collisions = colliding_arms(args.arms)
    if collisions:
        raise CommandError(
            f"{_collision(*collisions[0])}: plan them as separate batches, and run each from its own "
            "orchestrating session, restarting the session between them"
        )
    if os.path.exists(paths.plan):
        raise CommandError(f"{paths.plan} exists; a plan is never replaced, name a new batch")
    if has_rows(paths):
        # A dry run's job ids are a plan's (jNNN), and claim takes a job with
        # a row as done: the plan would skip those jobs without a word.
        raise CommandError(
            f"batch {args.batch} already has rows ({paths.runs}), from a dry run or an earlier "
            "batch; claim would take their job ids as done, so name a new batch"
        )
    combos = [
        (t, a, m, r) for t in args.tasks for a in args.arms for m in args.models for r in range(args.runs)
    ]
    random.Random(args.seed).shuffle(combos)
    jobs = [
        {"id": f"j{n:03d}", "task": t, "arm": a, "model": m, "run": r}
        for n, (t, a, m, r) in enumerate(combos, 1)
    ]
    plan = {
        "batch": args.batch,
        "created": now(),
        "tasks": args.tasks,
        "arms": args.arms,
        "models": args.models,
        "runs": args.runs,
        "seed": args.seed,
        "effort": args.effort,
        "max_turns": args.max_turns,
        # Attachment types the guard flags that this batch lets through,
        # recorded per run instead (see plan_tolerated): not a clean batch.
        "tolerated_attachments": sorted(set(args.tolerate_attachments)),
        "agent_body_sha256": agents.body_sha256(),
        **git_provenance(),
        "jobs": jobs,
    }
    os.makedirs(paths.batch_dir, exist_ok=True)
    try:
        fd = os.open(paths.plan, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    except FileExistsError:
        raise CommandError(f"{paths.plan} exists; a plan is never replaced, name a new batch")
    with os.fdopen(fd, "w") as f:
        json.dump(plan, f, indent=2)
    print(
        f"run.py: batch {args.batch}: {len(jobs)} jobs ({len(args.tasks)} tasks x "
        f"{len(args.arms)} arms x {len(args.models)} models x {args.runs} runs), seed "
        f"{args.seed}, effort {args.effort}, max {args.max_turns} turns{tolerance_note(plan)}; {paths.plan}"
    )
    return EXIT_OK


def plan_tolerated(plan):
    """The attachment types `plan` tolerates (`plan --tolerate-attachment`),
    sorted; [] for a plan from before the option. Raises CommandError when
    the plan names one a plan may not tolerate (a plan edited by hand), so
    nothing is graded under a tolerance the guard does not know."""
    try:
        return load_transcript_module().check_tolerated(plan.get("tolerated_attachments", []))
    except ValueError as err:
        raise CommandError(f"the plan of batch {plan.get('batch')} tolerates what no plan may: {err}")


def tolerance_note(plan):
    """`, tolerating T (not a clean batch)` for a plan that tolerates
    attachments, else nothing."""
    tolerated = plan.get("tolerated_attachments") or []
    return f", tolerating {', '.join(map(str, tolerated))} (not a clean batch)" if tolerated else ""


# --- Arms that share tool names ------------------------------------------------------------------
#
# taria and outline list the same tools (mcp__tui__read_tree, act, key,
# type_text) with different descriptions, and a Claude Code session keeps the
# first definitions it sees for a tool name for the rest of the session. In
# probe4, outline's j001 started first, and taria's j002 and j005 (j005
# spawned alone, after every other run had finished) were offered the
# outline's descriptions while their own bridge served JSON. So `plan`
# refuses a batch whose arms share a tool name, and `next` refuses to hand
# out a job of a batch with an arm that shares a tool name with another arm
# the same orchestrating session was already handed, in any batch
# (claim_in_session). tmux shares no name with either.


def _shared_tools(a, b):
    """The tool names arms `a` and `b` both list, sorted; [] for one arm."""
    return [] if a == b else sorted(set(ARM_TOOLS.get(a, ())) & set(ARM_TOOLS.get(b, ())))


def colliding_arms(arms):
    """(a, b, shared tool names) for each pair of different arms in `arms`
    whose tools share a name."""
    arms = list(dict.fromkeys(arms))
    pairs = [(a, b) for i, a in enumerate(arms) for b in arms[i + 1:]]
    return [(a, b, _shared_tools(a, b)) for a, b in pairs if _shared_tools(a, b)]


def _collision(a, b, shared):
    names = ", ".join(f"mcp__{SERVER_KEY}__{tool}" for tool in shared)
    return (f"arms {a} and {b} share tool names ({names}) with different descriptions, and a Claude Code session "
            "keeps the first definitions it sees for a tool name, so the agents of the arm spawned second are "
            "offered the other's")


def orchestrator(environ=None):
    """The Claude Code process this run.py runs under, as {"key", "pid",
    "session_id"}, or None when it cannot be told. Claude Code puts its pid
    in CLAUDE_PID for the commands its Bash tool runs; the key adds the
    process's start time from /proc, so a pid a later process reuses is
    another session. Without CLAUDE_PID (run outside Claude Code) or /proc,
    or with a pid that is gone, None: the session check is then skipped."""
    environ = os.environ if environ is None else environ
    pid = environ.get(SESSION_PID_ENV) or ""
    if not pid.isdigit():
        return None
    try:
        with open(f"/proc/{int(pid)}/stat") as f:
            # The fields after the command name's closing paren start at
            # field 3 (state); the start time is field 22.
            start = f.read().rpartition(")")[2].split()[19]
    except (OSError, IndexError):
        return None
    return {"key": f"{int(pid)}@{start}", "pid": int(pid), "session_id": environ.get(SESSION_ID_ENV)}


def session_handouts(paths, key):
    """{arm: its first hand-out record} for the orchestrating session `key`."""
    try:
        with open(paths.handouts) as f:
            lines = f.read().splitlines()
    except FileNotFoundError:
        return {}
    handed = {}
    for line in lines:
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict) and record.get("session") == key and isinstance(record.get("arm"), str):
            handed.setdefault(record["arm"], record)
    return handed


def handout_problem(paths, plan, k, session):
    """Why no job of `plan` may be handed out to the orchestrating
    `session` (orchestrator(); None skips the session check), or None.
    A plan whose own arms share tool names (one written before `plan`
    refused it, as probe4's was) is refused whoever runs it; otherwise an
    arm of the plan that shares tool names with another arm this session
    was already handed, in any batch."""
    collisions = colliding_arms(plan.get("arms") or [])
    if collisions:
        return (f"the plan of batch {plan['batch']} was written before `plan` refused it: {_collision(*collisions[0])}"
                "; plan them as separate batches, each run from its own orchestrating session")
    if session is None:
        return None
    handed = session_handouts(paths, session["key"])
    for arm in plan.get("arms") or []:
        for other, record in sorted(handed.items()):
            shared = _shared_tools(arm, other)
            if shared:
                return (f"this orchestrating session (Claude Code pid {session['pid']}) was already handed "
                        f"{other} jobs ({record.get('batch')} {record.get('job')}), and {_collision(arm, other, shared)}"
                        f"; restart the orchestrating session, then `run.py next --batch {plan['batch']} --slot {k}` "
                        "from the new one")
    return None


def claim_in_session(paths, plan, k, session):
    """claim(), unless handout_problem refuses this plan to `session`: then
    the slot is given back and CommandError raised, nothing claimed. A job
    claimed for a session is recorded in paths.handouts, so a later batch
    of an arm sharing its tool names is refused to the same session. Under
    one lock across batches, so two slots cannot hand out colliding arms at
    once. Nothing left to claim is never refused."""
    os.makedirs(paths.slots, exist_ok=True)
    with flocked(paths.handouts_lock):
        job, slot = claim(paths, plan, k)
        if job is None:
            return job, slot
        problem = handout_problem(paths, plan, k, session)
        if problem:
            free_slot(paths, k)
            raise CommandError(f"{problem}; claimed nothing")
        if session is not None:
            record = {"ts": now(), "session": session["key"], "claude_pid": session["pid"],
                      "claude_session_id": session["session_id"], "batch": plan["batch"], "job": job["id"],
                      "arm": job["arm"]}
            with open(paths.handouts, "a") as f:
                f.write(json.dumps(record) + "\n")
    return job, slot


# --- Subagent runs: next, status, abort ---------------------------------------------------


def job_of(slot):
    return f"{slot.get('job')} ({slot.get('task')}/{slot.get('arm')}/{slot.get('model')} run {slot.get('run')})"


def claim(paths, plan, k):
    """Claim the first job in plan order that has no row and no slot, into
    slot `k`. Returns (job, slot) or (None, None) when nothing is left.

    Under the batch lock, so two `next` commands never claim the same job.
    """
    with flocked(paths.lock):
        recorded = {row.get("job") for row in read_rows(paths)}
        held = {s.get("job") for s in read_all_slots(paths).values() if s.get("batch") == plan["batch"]}
        for job in plan["jobs"]:
            if job["id"] in recorded or job["id"] in held:
                continue
            slot = {
                "slot": k, "batch": plan["batch"], "job": job["id"], "task": job["task"],
                "arm": job["arm"], "model": job["model"], "run": job["run"], "state": "claimed",
                "pid": os.getpid(), "claimed": now(),
            }
            create_slot(paths, k, slot)
            return job, slot
    return None, None


def prepare_slot(paths, k, slot):
    """Start the slot's demo and mark the slot ready for its agent."""

    def created(run):
        slot.update(state="starting", demo=run.to_slot())
        write_slot(paths, k, slot)

    run = prepare_demo(TASKS[slot["task"]], on_created=created)
    mcp_launch.clear_markers(paths.slots, k)  # none can be this job's: the slot was never ready
    # To the millisecond: a transcript that starts before it did not drive this demo.
    slot.update(state="ready", demo=run.to_slot(), prepared=now("milliseconds"))
    write_slot(paths, k, slot)


def base_agent_row(plan, slot, agent_id, prov):
    return new_row(
        ts=now(), batch=plan["batch"], job=slot["job"], task=slot["task"], arm=slot["arm"],
        run=slot["run"], slot=slot.get("slot"), seed=plan["seed"], mode="subagent", solver="agent",
        model=slot["model"],
        effort=plan["effort"], max_turns=plan["max_turns"], agent_id=agent_id, **prov,
    )


def load_transcript_module():
    try:
        import transcript
    except ImportError as err:
        raise HarnessError(f"bench/transcript.py cannot be imported: {err}")
    return transcript


def _one(values):
    values = sorted({v for v in values or [] if isinstance(v, str)})
    return ", ".join(values) if values else None


def meta_path(transcript_path):
    """The meta file Claude Code writes beside a subagent transcript."""
    base = transcript_path[: -len(".jsonl")] if transcript_path.endswith(".jsonl") else transcript_path
    return base + ".meta.json"


def archived_transcript(paths, agent_id):
    """Where batch `paths` keeps its copy of agent `agent_id`'s transcript:
    transcripts/agent-<id>.jsonl, with its meta file beside it as Claude
    Code lays them out, so meta_path finds it."""
    if not isinstance(agent_id, str) or not AGENT_ID_RE.match(agent_id):
        raise ValueError(f"agent id {agent_id!r} is not [0-9A-Za-z_-]+")
    return os.path.join(paths.transcripts, f"agent-{agent_id}.jsonl")


def archive_transcript(paths, agent_id, source):
    """Copy the transcript at `source`, and its meta file when it has one,
    into the batch's transcripts/ dir. Returns the archived transcript's
    path. Claude Code deletes transcripts after cleanupPeriodDays (default
    30), and regrade needs them. Each file lands by a rename, the meta file
    first, so an archived transcript is never a partial copy and never lacks
    a meta file its source had. Raises OSError when a copy fails."""
    target = archived_transcript(paths, agent_id)
    os.makedirs(paths.transcripts, exist_ok=True)
    pairs = [(source, target)]
    if os.path.exists(meta_path(source)):
        pairs.insert(0, (meta_path(source), meta_path(target)))
    for src, dst in pairs:
        if os.path.abspath(src) == os.path.abspath(dst):
            continue
        tmp = f"{dst}.{os.getpid()}.tmp"
        try:
            shutil.copy2(src, tmp)
            os.replace(tmp, dst)
        finally:
            if os.path.exists(tmp):
                os.remove(tmp)
    return target


def agent_type_problems(transcript_path, expected):
    """The subagent's meta file names the agent type it was spawned as: it
    must be `expected` (this slot's agent; or, given a compiled pattern, any
    name it matches), or the run drove another slot's demo. A missing or
    unreadable meta file is a problem too: the type cannot be checked."""
    meta = meta_path(transcript_path)
    if not os.path.exists(meta):
        return [f"has no meta file ({meta}), so the agent type it was spawned as cannot be checked"]
    try:
        with open(meta) as f:
            kind = json.load(f).get("agentType")
    except (OSError, ValueError, AttributeError):
        return [f"its meta file {meta} cannot be read"]
    ok = expected.fullmatch(kind) is not None if isinstance(expected, re.Pattern) and isinstance(kind, str) \
        else kind == expected
    if not ok:
        want = expected.pattern if isinstance(expected, re.Pattern) else expected
        return [f"was spawned as {kind!r}, not this slot's agent {want!r}"]
    return []


# The row fields a transcript fills, which a regrade recomputes.
TRANSCRIPT_FIELDS = (
    "model_resolved", "session_id", "claude_version", "claude_versions", "efforts_resolved",
    "guard_problems", "end_inferred", "notes", "turns", "tool_calls", "tool_calls_total",
    "toolsearch_calls", "tokens", "output_tokens_lower_bound", "unclosed_messages", "wall_ms",
    "final_text", "context",
)


def transcript_verdict(transcript_mod, path, arm, model, effort, max_turns, extra_problems=(), tolerated=()):
    """(row fields, errors, cut_off) from the transcript at `path`: the stats
    a row records, the guard (plus `extra_problems`), and how the run ended.
    The transcript is read and guarded with the attachment types the plan
    tolerates (`tolerated`, plan_tolerated), which its context then records.
    The one grading logic for `next` and `regrade`. Raises when the
    transcript cannot be read."""
    stats = transcript_mod.read_transcript(path, tolerated=tolerated)
    problems = list(transcript_mod.guard(stats, arm, model, ARM_TOOLS[arm], effort=effort,
                                         tree_format=ARM_TREE_FORMAT.get(arm), tolerated=tolerated))
    problems += list(extra_problems)
    kind, detail = transcript_mod.ending(stats, max_turns)
    inferred = kind == "end_turn" and detail is not None
    fields = {
        "model_resolved": _one(stats.get("models")),
        "session_id": stats.get("session_id"),
        "claude_version": _one(stats.get("claude_versions")),
        "claude_versions": stats.get("claude_versions"),
        "efforts_resolved": stats.get("efforts"),
        "guard_problems": problems,
        "end_inferred": inferred,
        "notes": [detail] if inferred else [],
        "turns": stats.get("turns"),
        "tool_calls": stats.get("tool_calls"),
        "tool_calls_total": stats.get("tool_calls_total"),
        "toolsearch_calls": stats.get("toolsearch_calls"),
        "tokens": stats.get("tokens"),
        # A message Claude Code never closed keeps a placeholder output count,
        # mid-run as well as last: then tokens.output is a lower bound.
        "output_tokens_lower_bound": stats.get("output_tokens_lower_bound"),
        "unclosed_messages": stats.get("unclosed_messages"),
        "wall_ms": stats.get("wall_ms"),
        "final_text": stats.get("final_text"),
        "context": stats.get("context"),
    }
    errors = []
    if problems:
        errors.append("guard: " + "; ".join(problems))
    if kind == "harness_error":
        errors.append(detail)
    return fields, errors, kind == "max_turns"


def grade_agent_run(plan, slot, agent_id, run=None, transcript_mod=None, transcript_file=None, prov=None):
    """The row for the agent run in `slot`: its transcript, the guard, and the
    demo's dump. Quits the demo; does not close it or record the row. Who
    ran it (check_agent_identity) is settled before this, by refusing."""
    arm = slot["arm"]
    prov = prov if prov is not None else git_provenance()
    row = base_agent_row(plan, slot, agent_id, {**prov, "claude_version": None})
    errors, cut_off = [], False
    try:
        transcript_mod = transcript_mod or load_transcript_module()
        path = transcript_file or transcript_mod.find_transcript(agent_id)
        if path is None:
            errors.append(f"no transcript for agent {agent_id} under ~/.claude/projects")
        else:
            fields, found, cut_off = transcript_verdict(
                transcript_mod, path, arm, slot["model"], plan["effort"], plan["max_turns"],
                tolerated=plan.get("tolerated_attachments") or [],
            )
            row.update(fields)
            errors += found
    except Exception as err:  # noqa: BLE001 - an unreadable transcript is the harness's
        errors.append(f"reading the transcript failed: {type(err).__name__}: {err}")
    try:
        run = run if run is not None else DemoRun.from_slot(slot["demo"])
        graded = grade(TASKS[slot["task"]], run)
        error = "; ".join(errors) or None
        status, passed = classify(graded, error, cut_off)
        row.update(
            status=status, passed=passed, check_passed=graded["check_passed"],
            reason=graded["reason"], error=error, demo_alive=graded["demo_alive"],
            state=graded["state"],
        )
        row["demo_exit"] = quit_demo(run, graded["demo_alive"])
    except Exception as err:  # noqa: BLE001 - anything here is the harness's
        traceback.print_exc(limit=8, file=sys.stderr)
        row.update(
            status="harness_error", passed=None,
            error="; ".join(errors + [f"grading failed: {type(err).__name__}: {err}"]),
        )
    return row


def summary_line(row):
    bits = [f"{row['status']}"]
    if row.get("agent_id"):
        bits.append(f"agent {row['agent_id']}")
    if row.get("turns") is not None:
        bits.append(f"{row['turns']} turns")
    if row.get("tool_calls_total") is not None:
        bits.append(f"{row['tool_calls_total']} tool calls")
    detail = row.get("error") or row.get("reason")
    return (
        f"run.py: {row['job']} {row['task']}/{row['arm']}/{row['model']} run {row['run']}: "
        f"{', '.join(bits)}{'; ' + detail if detail else ''}"
    )


def identity_problems(slot, stats):
    """Why the transcript `stats` came from cannot be this slot's run: its
    first prompt is not the job's task, or it started before the slot's demo
    was ready. Empty when it can be."""
    problems = []
    want = TASKS[slot["task"]].prompt
    first = stats.get("first_prompt")
    # Stripped: a trailing newline the orchestrator's Agent call adds is not another task.
    if not isinstance(first, str) or first.strip() != want.strip():
        problems.append(f"its first prompt is {first!r}, not the task's {want!r}")
    prepared, started = parse_time(slot.get("prepared")), parse_time(stats.get("started"))
    if prepared is None:
        problems.append("the slot records no prepared time to hold it to")
    elif started is None:
        problems.append("its transcript has no timestamp")
    elif started < prepared:
        problems.append(f"it started at {stats.get('started')}, before the slot was prepared at {slot.get('prepared')}")
    return problems


def check_agent_identity(paths, plan, k, slot, agent_id, transcript_file=None, transcript_mod=None):
    """Refuse (CommandError, nothing recorded, the slot kept) an agent that
    cannot have run the job slot `k` holds. These are the orchestrator's
    mistakes, not runs: an id the batch already recorded, an id with no
    transcript, a transcript with no meta file or whose meta file names
    another slot's agent, or a transcript that is another run's (another
    task, or started before the slot was prepared). A transcript that exists
    but cannot be read is left to grading, which records the harness error.
    Returns the transcript's path."""
    reused = next((r for r in read_rows(paths) if r.get("agent_id") == agent_id), None)
    retry = (f"pass the id of the agent spawned for {slot['job']}, or "
             f"`run.py abort --batch {plan['batch']} --slot {k}`")

    def refuse(why):
        raise CommandError(f"{why}; nothing recorded: {retry}")

    if reused is not None:
        refuse(f"agent {agent_id} was already recorded for {reused.get('job')}, so it did not run "
               f"{job_of(slot)} in slot {k}")
    transcript_mod = transcript_mod or load_transcript_module()
    try:
        path = transcript_file or transcript_mod.find_transcript(agent_id)
    except ValueError as err:
        refuse(f"agent {agent_id}'s transcript cannot be told apart: {err}")
    if path is None or not os.path.isfile(path):
        where = path if path is not None else "~/.claude/projects"
        refuse(f"no transcript for agent {agent_id} ({where}), so it did not run {job_of(slot)} in slot {k}")
    problems = agent_type_problems(path, agents.agent_name(slot["arm"], slot["slot"]))
    if problems:
        refuse(f"agent {agent_id} {'; '.join(problems)}, so it is not the run of {job_of(slot)} in slot {k}")
    try:
        stats = transcript_mod.read_transcript(path)
    except Exception:  # noqa: BLE001 - grading reads it again and records why it failed
        return path
    if stats.get("started") is None and stats.get("first_prompt") is None:
        return path  # nothing in it to be another run's; grading records the harness error
    problems = identity_problems(slot, stats)
    if problems:
        refuse(f"agent {agent_id}'s transcript is not the run of {job_of(slot)} in slot {k}: {'; '.join(problems)}")
    return path


def finish_slot(paths, plan, k, slot, agent_id, transcript_file=None):
    """Grade the agent run slot `k` holds, record it, tear the demo down and
    free the slot. Returns the row (an existing one if the job was recorded
    before a crash left the slot behind). Refuses an agent that cannot have
    run this job (check_agent_identity) before recording anything.

    The transcript is archived into the batch (archive_transcript) and the
    archived copy graded, so the row and a later regrade read the same
    bytes. A failed copy is said on stderr and the live one graded;
    `run.py archive` copies it later."""
    existing = next((r for r in read_rows(paths) if r.get("job") == slot["job"]), None)
    if existing is not None:
        print(f"run.py: {slot['job']} was already recorded; freeing slot {k}", file=sys.stderr)
        teardown(slot)
        free_slot(paths, k)
        return existing
    path = check_agent_identity(paths, plan, k, slot, agent_id, transcript_file)
    try:
        path = archive_transcript(paths, agent_id, path)
    except (OSError, ValueError) as err:
        print(f"run.py: could not archive agent {agent_id}'s transcript ({err}); grading the live one. "
              f"`run.py archive --batch {plan['batch']}` copies it later", file=sys.stderr)
    row = grade_agent_run(plan, slot, agent_id, transcript_file=path)
    append_row(paths, row)  # before teardown: a crash from here on leaves a row, not a lost run
    teardown(slot)
    free_slot(paths, k)
    return row


def check_agent_files(plan, k, agents_dir, slots_dir):
    found = []
    for arm in plan["arms"]:
        path = os.path.join(agents_dir, f"{agents.agent_name(arm, k)}.md")
        found += agents.problems(
            path, arm, k, slots_dir, plan["effort"], plan["max_turns"], plan["agent_body_sha256"]
        )
    if found:
        raise CommandError("the agent files do not match the plan: " + "; ".join(found))


def cmd_next(args):
    paths = Paths(args.results_dir, args.batch)
    plan = load_plan(paths)
    plan_tolerated(plan)  # a tolerance the guard does not know grades and claims nothing
    if args.agent_id is not None and not AGENT_ID_RE.match(args.agent_id):
        raise CommandError(f"agent id {args.agent_id!r} is not [0-9A-Za-z_-]+")
    os.makedirs(paths.slots, exist_ok=True)
    try:
        with flocked(paths.slot_lock(args.slot), blocking=False):
            return _next_locked(args, paths, plan)
    except BlockingIOError:
        raise CommandError(f"slot {args.slot} is busy: another run.py is working on it")


def _next_locked(args, paths, plan):
    k = args.slot
    slot = read_slot(paths, k)
    if slot is not None and slot.get("state") != "ready":
        # A prepare runs under this slot's flock, which we now hold: whoever
        # wrote this unfinished slot is gone.
        print(f"run.py: slot {k}: a prepare of {job_of(slot)} died; tearing down what it left", file=sys.stderr)
        teardown(slot)
        free_slot(paths, k)
        slot = None
    if slot is not None:
        if slot.get("batch") != plan["batch"]:
            raise CommandError(f"slot {k} holds {job_of(slot)} of batch {slot.get('batch')}, not {plan['batch']}")
        if args.agent_id is None:
            raise CommandError(
                f"slot {k} holds {job_of(slot)}; pass --agent-id of the agent that ran it, "
                f"or `run.py abort --batch {plan['batch']} --slot {k}`"
            )
        row = finish_slot(paths, plan, k, slot, args.agent_id, args.transcript_file)
        print(summary_line(row), file=sys.stderr)
        if row["status"] == "harness_error":
            raise CommandError(
                f"{row['job']} was a harness error ({row.get('error')}); claimed nothing. "
                f"Fix the setup, then `run.py next --batch {plan['batch']} --slot {k}` resumes"
            )
    elif args.agent_id is not None:
        if not any(r.get("agent_id") == args.agent_id for r in read_rows(paths)):
            raise CommandError(f"slot {k} holds no job, so agent {args.agent_id} did not run from it")
        print(f"run.py: agent {args.agent_id} was already graded", file=sys.stderr)

    preflight()
    check_agent_files(plan, k, args.agents_dir, paths.slots)
    job, slot = claim_in_session(paths, plan, k, orchestrator())
    if job is None:
        print(json.dumps({"done": True}), flush=True)
        return EXIT_OK
    try:
        prepare_slot(paths, k, slot)
    except Exception as err:  # noqa: BLE001 - a failed setup is the harness's
        traceback.print_exc(limit=8, file=sys.stderr)
        row = base_agent_row(plan, slot, None, {**git_provenance(), "claude_version": None})
        row.update(status="harness_error", error=f"preparing the demo failed: {type(err).__name__}: {err}")
        append_row(paths, row)
        teardown(slot)  # prepare_demo closes its own failures; this covers a failed slot write
        free_slot(paths, k)
        raise CommandError(f"{job_of(slot)}: preparing the demo failed: {err}")
    except BaseException:
        teardown(slot)  # a signal mid-prepare: the job goes back to pending
        free_slot(paths, k)
        raise
    print(json.dumps({
        "job": job["id"],
        "slot": k,
        "agent": agents.agent_name(job["arm"], k),
        "model": job["model"],
        "prompt": TASKS[job["task"]].prompt,
    }), flush=True)
    return EXIT_OK


def _age(stamp):
    try:
        seconds = int(time.time() - datetime.fromisoformat(stamp).timestamp())
    except (TypeError, ValueError):
        return "?"
    return f"{seconds // 60}m{seconds % 60:02d}s"


def cmd_status(args):
    paths = Paths(args.results_dir, args.batch)
    plan = load_plan(paths)
    rows = read_rows(paths)
    recorded = {r.get("job") for r in rows}
    slots = {k: s for k, s in read_all_slots(paths).items() if s.get("batch") == plan["batch"]}
    held = {s.get("job") for s in slots.values()}
    pending = [j for j in plan["jobs"] if j["id"] not in recorded and j["id"] not in held]
    counts = {s: sum(1 for r in rows if r.get("status") == s) for s in STATUSES}
    print(
        f"batch {plan['batch']}: {len(plan['jobs'])} jobs; {len(recorded)} recorded ("
        + ", ".join(f"{s} {n}" for s, n in counts.items())
        + f"); {len(held)} held; {len(pending)} pending{tolerance_note(plan)}"
    )
    for k in sorted(slots):
        s = slots[k]
        state = s.get("state")
        if state != "ready" and not pid_alive(s.get("pid")):
            state = f"{state}, preparer gone"
        print(f"slot {k}: {job_of(s)}, {state}, {_age(s.get('prepared') or s.get('claimed'))}")
    return EXIT_OK


def cmd_abort(args):
    paths = Paths(args.results_dir, args.batch)
    plan = load_plan(paths)
    os.makedirs(paths.slots, exist_ok=True)
    try:
        with flocked(paths.slot_lock(args.slot), blocking=False):
            slot = read_slot(paths, args.slot)
            if slot is None:
                raise CommandError(f"slot {args.slot} holds no job")
            if slot.get("batch") != plan["batch"]:
                raise CommandError(
                    f"slot {args.slot} holds {job_of(slot)} of batch {slot.get('batch')}, not {plan['batch']}"
                )
            if not any(r.get("job") == slot["job"] for r in read_rows(paths)):
                row = base_agent_row(plan, slot, None, {**git_provenance(), "claude_version": None})
                row.update(status="harness_error", error=f"aborted: {args.reason}")
                if slot.get("demo"):
                    try:
                        run = DemoRun.from_slot(slot["demo"])
                        graded = grade(TASKS[slot["task"]], run)
                        row.update(
                            check_passed=graded["check_passed"], reason=graded["reason"],
                            demo_alive=graded["demo_alive"], state=graded["state"],
                        )
                    except Exception as err:  # noqa: BLE001 - the abort goes ahead regardless
                        row["error"] += f"; the dump could not be read: {type(err).__name__}: {err}"
                append_row(paths, row)
            teardown(slot)
            free_slot(paths, args.slot)
    except BlockingIOError:
        raise CommandError(f"slot {args.slot} is busy: another run.py is working on it")
    print(f"run.py: aborted {job_of(slot)} in slot {args.slot}: {args.reason}", file=sys.stderr)
    return EXIT_OK


# --- Regrade: one grading logic for every row of a batch ----------------------------------------

# Matched whole (agent_type_problems uses fullmatch), so `tui-outline-1` is
# never taken for `tui-taria-1` or the reverse.
ARM_AGENT_RE = {arm: re.compile(rf"tui-{re.escape(arm)}-[1-9][0-9]*") for arm in ARMS}


def locate_transcript(paths, agent_id, transcript_mod, projects_dir=None):
    """(path, source) of agent `agent_id`'s transcript: the batch's archived
    copy ("archive") when there is one, else the live one Claude Code wrote
    under the projects dir ("live"), else (None, None). Raises ValueError for
    an id that is not [0-9A-Za-z_-]+ or that matches several live ones."""
    archived = archived_transcript(paths, agent_id)
    if os.path.isfile(archived):
        return archived, "archive"
    live = (transcript_mod.find_transcript(agent_id) if projects_dir is None
            else transcript_mod.find_transcript(agent_id, projects_dir))
    return (live, "live") if live is not None else (None, None)


def regrade_row(row, plan, transcript_mod, projects_dir=None, paths=None):
    """`row` graded again with the current code, from what persists: its
    recorded state and demo_alive, and its transcript re-read (the archived
    copy in batch `paths` first, else the live one). Returns (a new dict,
    where the transcript came from: "archive", "live" or None); `row` is
    not changed. A dry-run row comes back as it was. A row that cannot be
    regraded (no agent id, no state, no transcript any more) keeps its stats
    and status, with a `regrade_note` saying why."""
    if row.get("mode") != "subagent":
        return dict(row), None
    new = dict(row)
    new["status_at_run"] = row.get("status_at_run", row.get("status"))
    new["regrade_note"] = None
    arm, task, agent_id, state = row.get("arm"), TASKS.get(row.get("task")), row.get("agent_id"), row.get("state")
    keep, path, source = None, None, None
    if not agent_id:
        keep = "it has no agent id (a failed prepare or an abort), so there is no transcript to re-read"
    elif task is None or arm not in ARM_TOOLS:
        keep = f"its task {row.get('task')!r} or arm {arm!r} is unknown to this code"
    elif not isinstance(state, dict):
        keep = "it has no recorded state to check again"
    else:
        try:
            path, source = locate_transcript(paths or Paths(RESULTS_DIR, plan["batch"]), agent_id,
                                             transcript_mod, projects_dir)
        except ValueError as err:
            keep = f"its transcript cannot be told apart: {err}"
        if path is None and keep is None:
            keep = f"no transcript for agent {agent_id} can be found any more, archived or live"
    if keep is not None:
        new["regrade_note"] = f"kept as recorded: {keep}"
        return new, None

    errors, cut_off, fields = [], False, {}
    slot = row.get("slot")
    expected = agents.agent_name(arm, slot) if isinstance(slot, int) else ARM_AGENT_RE[arm]
    extra = []
    if os.path.exists(meta_path(path)):
        extra = agent_type_problems(path, expected)
    else:
        new["regrade_note"] = "its meta file is gone, so the agent type was not checked again"
    try:
        fields, found, cut_off = transcript_verdict(
            transcript_mod, path, arm, row.get("model"), plan["effort"], plan["max_turns"], extra,
            tolerated=plan.get("tolerated_attachments") or [],
        )
        errors += found
    except Exception as err:  # noqa: BLE001 - an unreadable transcript is the harness's
        errors.append(f"reading the transcript failed: {type(err).__name__}: {err}")
    new.update(fields)
    try:
        graded = judge(task, state, bool(row.get("demo_alive")))
    except Exception as err:  # noqa: BLE001 - a check that falls over is the harness's
        errors.append(f"grading failed: {type(err).__name__}: {err}")
        graded = {"check_passed": row.get("check_passed"), "demo_alive": row.get("demo_alive"),
                  "reason": row.get("reason")}
    error = "; ".join(errors) or None
    status, passed = classify(graded, error, cut_off)
    new.update(status=status, passed=passed, check_passed=graded["check_passed"],
               reason=graded["reason"], error=error)
    return new, source


def change_reason(new):
    """Why a regraded row has its status, in a few words."""
    if new.get("error"):
        return new["error"]
    if new.get("notes"):
        return "; ".join(new["notes"])
    return new.get("reason") or "no harness error any more"


def backup_path(paths, stamp):
    """runs.orig-<stamp>.jsonl beside runs.jsonl, never one that exists."""
    base = os.path.join(paths.batch_dir, f"runs.orig-{stamp}")
    candidate, n = f"{base}.jsonl", 1
    while os.path.exists(candidate):
        n += 1
        candidate = f"{base}-{n}.jsonl"
    return candidate


def regrade(paths, transcript_mod=None, projects_dir=None):
    """Regrade every row of the batch at `paths` in place, after a backup.
    Returns (rows, changes, backup, sources): changes are (job, old, new,
    reason); sources counts where the regraded rows' transcripts came from."""
    plan = load_plan(paths)
    plan_tolerated(plan)  # a tolerance the guard does not know regrades nothing
    held = sorted(k for k, s in read_all_slots(paths).items() if s.get("batch") == plan["batch"])
    if held:
        raise CommandError(
            f"batch {plan['batch']} still holds slot(s) {', '.join(map(str, held))}; grade or abort "
            "them first, so every row is regraded once and none is written meanwhile"
        )
    if not os.path.exists(paths.runs):
        raise CommandError(f"batch {plan['batch']} has no rows ({paths.runs})")
    transcript_mod = transcript_mod or load_transcript_module()
    prov = git_provenance()
    stamp_iso = now()
    with flocked(paths.lock):
        with open(paths.runs, "rb") as f:
            original = f.read()
        out, rows, changes, sources = [], [], [], Counter()
        for raw in original.decode("utf-8", errors="surrogateescape").splitlines():
            try:
                row = json.loads(raw)
            except ValueError:
                row = None
            if not isinstance(row, dict):
                out.append(raw)  # not a row: kept byte for byte
                continue
            new, source = regrade_row(row, plan, transcript_mod, projects_dir, paths)
            if source is not None:
                sources[source] += 1
            if new.get("mode") == "subagent":
                new.update(regraded_at=stamp_iso, regrade_git_sha=prov["git_sha"],
                           regrade_git_dirty=prov["git_dirty"])
            if new.get("status") != row.get("status"):
                changes.append((row.get("job"), row.get("status"), new.get("status"), change_reason(new)))
            rows.append(new)
            out.append(json.dumps(new))
        backup = backup_path(paths, datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ"))
        fd = os.open(backup, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
        with os.fdopen(fd, "wb") as f:
            f.write(original)
        tmp = f"{paths.runs}.{os.getpid()}.tmp"
        with open(tmp, "w", encoding="utf-8", errors="surrogateescape") as f:
            f.write("".join(line + "\n" for line in out))
        os.replace(tmp, paths.runs)
    return rows, changes, backup, sources


def cmd_regrade(args):
    paths = Paths(args.results_dir, args.batch)
    rows, changes, backup, sources = regrade(paths, projects_dir=args.projects_dir)
    kept = sum(1 for r in rows if str(r.get("regrade_note") or "").startswith("kept as recorded"))
    dry = sum(1 for r in rows if r.get("mode") != "subagent")
    tolerating = tolerance_note(load_plan(paths))
    print(f"run.py: regraded batch {args.batch}{tolerating}: {len(rows)} rows ({dry} dry-run kept, {kept} kept as "
          f"recorded); {len(changes)} changed status; transcripts: {sources['archive']} archived, "
          f"{sources['live']} live; the original is {backup}")
    for job, old, new, why in changes:
        print(f"  {job}: {old} -> {new} ({why})")
    for row in rows:
        note = row.get("regrade_note")
        if note:
            print(f"  {row.get('job')}: note: {note}")
    if sources["live"]:
        print(f"  {sources['live']} transcript(s) were read live, which Claude Code deletes after "
              f"cleanupPeriodDays: `run.py archive --batch {args.batch}` keeps a copy")
    return EXIT_OK


# --- Archive: keep a batch's transcripts past Claude Code's cleanup -------------------------------


def archive_batch(paths, transcript_mod=None, projects_dir=None):
    """Copy the live transcript (and meta file) of every agent row of the
    batch at `paths` that has no archived copy yet. An archived copy is
    never replaced: it is what was graded. Returns (copied, already,
    missing, without_id): job ids, and (job, why) pairs for missing."""
    if not os.path.exists(paths.runs):
        raise CommandError(f"batch {paths.batch} has no rows ({paths.runs})")
    transcript_mod = transcript_mod or load_transcript_module()
    copied, already, missing, without_id, seen = [], [], [], [], set()
    for row in read_rows(paths):
        if row.get("mode") != "subagent":
            continue
        job, agent_id = row.get("job"), row.get("agent_id")
        if not agent_id:
            without_id.append(job)
            continue
        if agent_id in seen:
            continue
        seen.add(agent_id)
        try:
            path, source = locate_transcript(paths, agent_id, transcript_mod, projects_dir)
        except ValueError as err:
            missing.append((job, str(err)))
            continue
        if source == "archive":
            already.append(job)
        elif path is None:
            missing.append((job, f"no transcript for agent {agent_id} under "
                                 f"{projects_dir or '~/.claude/projects'} (deleted, or never written)"))
        else:
            try:
                archive_transcript(paths, agent_id, path)
                copied.append(job)
            except OSError as err:
                missing.append((job, f"copying {path} failed: {err}"))
    return copied, already, missing, without_id


def cmd_archive(args):
    paths = Paths(args.results_dir, args.batch)
    copied, already, missing, without_id = archive_batch(paths, projects_dir=args.projects_dir)
    print(f"run.py: archived batch {args.batch}: {len(copied)} copied, {len(already)} already archived, "
          f"{len(missing)} missing, {len(without_id)} row(s) without an agent id (an abort or a failed "
          f"prepare); in {paths.transcripts}")
    for job, why in missing:
        print(f"  {job}: {why}")
    return EXIT_HARNESS if missing else EXIT_OK


# --- Command line ---------------------------------------------------------------------------


def _list(value, choices, what):
    items = [v.strip() for v in value.split(",") if v.strip()]
    bad = [v for v in items if v not in choices]
    if bad or not items:
        raise argparse.ArgumentTypeError(
            f"unknown {what} {', '.join(bad) or '(none)'}; choose from {', '.join(choices)}"
        )
    return items


def _batch(value):
    if not BATCH_RE.match(value) or value == "slots":
        raise argparse.ArgumentTypeError(f"batch {value!r} must be [A-Za-z0-9._-], not 'slots'")
    return value


def _tolerable(value):
    """An attachment type a plan may tolerate (transcript.tolerable_problem)."""
    try:
        problem = load_transcript_module().tolerable_problem(value)
    except HarnessError as err:
        raise argparse.ArgumentTypeError(str(err))
    if problem:
        raise argparse.ArgumentTypeError(problem)
    return value


def _positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="Run taria benchmark tasks.")
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument(
        "--results-dir", default=RESULTS_DIR,
        help="where batches and slot files live (default bench/results)",
    )
    task_ids = sorted(TASKS)
    selection = argparse.ArgumentParser(add_help=False)
    selection.add_argument(
        "--tasks", "--task", dest="tasks", action="extend",
        type=lambda v: _list(v, task_ids, "task"),
        help=f"comma-separated task ids (default: all of {','.join(task_ids)})",
    )
    selection.add_argument(
        "--arms", "--arm", dest="arms", action="extend",
        type=lambda v: _list(v, list(ARMS), "arm"),
        help=f"comma-separated arms (default: dry-run {','.join(ARMS)}, plan {','.join(PLAN_DEFAULT_ARMS)}; "
             "a plan refuses arms that share tool names, taria and outline)",
    )
    selection.add_argument("--runs", type=_positive, default=1, help="runs per combination (default 1)")
    selection.add_argument("--seed", type=int, help="shuffle seed (default: random, recorded)")
    limits = argparse.ArgumentParser(add_help=False)
    limits.add_argument("--effort", default=agents.DEFAULT_EFFORT, choices=agents.EFFORTS,
                        help=f"the agents' effort (default {agents.DEFAULT_EFFORT})")
    limits.add_argument("--max-turns", type=_positive, default=agents.DEFAULT_MAX_TURNS,
                        help=f"the agents' maxTurns (default {agents.DEFAULT_MAX_TURNS})")

    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("dry-run", parents=[common, selection], help="scripted solver, no model")
    p.add_argument("--sabotage", action="store_true", help="a wrong plan per task: every check must fail")
    p.add_argument("--batch", type=_batch, help="batch id (default: time plus random)")

    p = sub.add_parser("agents", parents=[common, limits], help="write the subagent files")
    p.add_argument("--slots", type=_positive, help="how many slots (files per arm)")
    p.add_argument("--dir", default=agents.DEFAULT_DIR, help=f"where (default {agents.DEFAULT_DIR})")
    p.add_argument("--remove", action="store_true", help="delete this script's tui-*-*.md files from --dir")

    p = sub.add_parser("plan", parents=[common, selection, limits], help="write a batch's job list")
    p.add_argument("--batch", type=_batch, required=True)
    p.add_argument("--models", action="extend", type=lambda v: _list(v, list(MODELS), "model"),
                   help=f"comma-separated model families (default {DEFAULT_MODELS})")
    p.add_argument("--tolerate-attachment", dest="tolerate_attachments", action="append", default=[],
                   type=_tolerable, metavar="TYPE",
                   help="let the guard pass this flagged attachment type, recorded per run instead "
                        "(repeatable; only remote_session_change, the same note for every arm): "
                        "the batch is then not a clean batch")

    p = sub.add_parser("next", parents=[common], help="grade a slot's run and claim its next job")
    p.add_argument("--batch", type=_batch, required=True)
    p.add_argument("--slot", type=_positive, required=True)
    p.add_argument("--agent-id", help="the agent that ran the job the slot holds")
    p.add_argument("--agents-dir", default=agents.DEFAULT_DIR,
                   help=f"where the agent files are, checked against the plan (default {agents.DEFAULT_DIR})")
    p.add_argument("--transcript-file", help=argparse.SUPPRESS)  # test hook: skips find_transcript

    p = sub.add_parser("status", parents=[common], help="a batch's progress")
    p.add_argument("--batch", type=_batch, required=True)

    p = sub.add_parser("abort", parents=[common], help="tear a slot down, recording a harness error")
    p.add_argument("--batch", type=_batch, required=True)
    p.add_argument("--slot", type=_positive, required=True)
    p.add_argument("--reason", default="aborted by the operator")

    p = sub.add_parser("regrade", parents=[common], help="grade every row of a batch again with this code")
    p.add_argument("--batch", type=_batch, required=True)
    p.add_argument("--projects-dir", help=argparse.SUPPRESS)  # test hook: where find_transcript looks

    p = sub.add_parser("archive", parents=[common],
                       help="copy a batch's live transcripts into results/<batch>/transcripts")
    p.add_argument("--batch", type=_batch, required=True)
    p.add_argument("--projects-dir", help=argparse.SUPPRESS)  # test hook: where find_transcript looks

    args = parser.parse_args(argv)
    if args.command in ("dry-run", "plan"):
        args.tasks = list(dict.fromkeys(args.tasks or task_ids))
        args.arms = list(dict.fromkeys(args.arms or (ARMS if args.command == "dry-run" else PLAN_DEFAULT_ARMS)))
        if args.seed is None:
            args.seed = random.SystemRandom().randrange(2**31)
    if args.command == "plan":
        args.models = list(dict.fromkeys(args.models or DEFAULT_MODELS.split(",")))
    if args.command == "dry-run" and args.batch is None:
        args.batch = new_batch_id()
    return args


COMMANDS = {
    "dry-run": cmd_dry_run,
    "agents": cmd_agents,
    "plan": cmd_plan,
    "next": cmd_next,
    "status": cmd_status,
    "abort": cmd_abort,
    "regrade": cmd_regrade,
    "archive": cmd_archive,
}


def cli(argv=None):
    """Parse `argv`, run the subcommand, return its exit code."""
    try:
        args = parse_args(argv)
    except SystemExit as exit_:
        return EXIT_HARNESS if exit_.code else EXIT_OK
    try:
        return COMMANDS[args.command](args)
    except (CommandError, HarnessError) as err:
        print(f"run.py: {err}", file=sys.stderr)
        if args.command == "next":
            print(json.dumps({"error": str(err), "slot": args.slot}), flush=True)
        return EXIT_HARNESS


def main():
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, _on_signal)
    try:
        code = cli(sys.argv[1:])
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
