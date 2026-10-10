#!/usr/bin/env python3
"""Run one benchmark task against taria-demo through one arm.

Each run gets a fresh demo in its own tmux session (`taria-bench-<uuid8>`,
120x40, on a private tmux server) with `TARIA_SOCK` and `TARIA_DEMO_STATE`
pointing into a fresh temp dir, and an MCP config for the arm written next to
them:

  taria  the real bridge, target/debug/taria-mcp --app taria-demo
  tmux   bench/tmux_mcp.py, screen-scraping the same pane

Both arms register their server under the same name, so the agent sees
`mcp__tui__<tool>` either way. When the agent stops, the run is graded on the
demo's state dump by the task's check (bench/tasks.py), never on the screen or
the tree, and one JSON line is printed and appended to bench/results/.

Modes:
  --dry-run   a scripted, non-LLM solver drives the task through the same MCP
              server the agent would get, spawned from the same config entry.
              Free, deterministic, and what proves the harness end to end.
  --spend     the real agent run with `claude -p`. Costs money, so it is not
              wired up in this slice and refuses to run (see spend_run).

Python 3 stdlib only. Never builds: a target/debug older than the sources is
refused by the same freshness gate scripts/e2e.py uses.

Usage:  python3 bench/run.py --dry-run --task T3 --arm taria|tmux [--sabotage]
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from collections import Counter
from datetime import datetime, timezone

BENCH = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(BENCH)
sys.path.insert(0, os.path.join(REPO, "scripts"))
sys.path.insert(0, BENCH)
from e2e import (  # noqa: E402
    DEMO_BIN,
    MCP_BIN,
    McpClient,
    StepFailure,
    ToolError,
    find,
    flatten,
    has_action,
    parse_result,
    require_fresh_binaries,
)
from tasks import TASKS  # noqa: E402

TMUX_MCP = os.path.join(BENCH, "tmux_mcp.py")
RESULTS_DIR = os.path.join(BENCH, "results")
SERVER_KEY = "tui"  # one name for both arms, so tool names do not name the arm
COLS, ROWS = 120, 40
READY_TIMEOUT = 5.0
ARMS = ("taria", "tmux")


class HarnessError(Exception):
    """The run could not be carried out; nothing about the arm is measured."""


class SolverError(Exception):
    """The scripted solver saw something it did not expect."""


# --- The demo under test ------------------------------------------------------


class DemoRun:
    """One taria-demo in its own tmux session and temp dir."""

    def __init__(self):
        self.name = f"taria-bench-{uuid.uuid4().hex[:8]}"
        self.tmpdir = tempfile.mkdtemp(prefix=f"{self.name}-")
        self.sock = os.path.join(self.tmpdir, "demo.sock")
        self.state_path = os.path.join(self.tmpdir, "state.json")
        # A private server (-L) with no config file (-f): the user's tmux
        # sessions and settings stay out of the measurement, and tearing the
        # session down cannot touch anything but this run.
        self.tmux = ["tmux", "-L", self.name, "-f", os.devnull]
        self.tmux_socket = None

    def _tmux(self, *args, check=True):
        env = {k: v for k, v in os.environ.items() if k != "TMUX"}
        result = subprocess.run(
            self.tmux + list(args), capture_output=True, text=True, env=env, timeout=10
        )
        if check and result.returncode != 0:
            raise HarnessError(f"tmux {args[0]} failed: {result.stderr.strip()}")
        return result

    def start(self):
        self._tmux(
            "new-session", "-d", "-s", self.name, "-x", str(COLS), "-y", str(ROWS),
            "env", f"TARIA_SOCK={self.sock}", f"TARIA_DEMO_STATE={self.state_path}",
            DEMO_BIN,
        )
        self.tmux_socket = self._tmux("display-message", "-p", "#{socket_path}").stdout.strip()
        deadline = time.monotonic() + READY_TIMEOUT
        while time.monotonic() < deadline:
            if os.path.exists(self.sock) and os.path.exists(self.state_path):
                break
            time.sleep(0.02)
        else:
            raise HarnessError(
                f"the demo never came up (socket {os.path.exists(self.sock)}, "
                f"state dump {os.path.exists(self.state_path)}); pane:\n{self.capture()}"
            )
        # The first frame is drawn right after the first dump; wait for it to
        # hold still so neither arm's first look races the first draw.
        last = None
        while time.monotonic() < deadline:
            screen = self.capture()
            if screen.strip() and screen == last:
                return
            last = screen
            time.sleep(0.05)
        raise HarnessError("the demo's screen never settled")

    def alive(self):
        return self._tmux("has-session", "-t", self.name, check=False).returncode == 0

    def capture(self):
        return self._tmux("capture-pane", "-p", "-t", self.name, check=False).stdout

    def send(self, *keys):
        self._tmux("send-keys", "-t", self.name, "--", *keys)

    def read_state(self):
        with open(self.state_path) as f:
            return json.load(f)

    def settled_state(self, cap=1.0):
        """The dump once two reads 100 ms apart agree (or `cap` runs out)."""
        deadline = time.monotonic() + cap
        last = self.read_state()
        while time.monotonic() < deadline:
            time.sleep(0.1)
            current = self.read_state()
            if current == last:
                return current
            last = current
        return last

    def quit(self):
        """Quit the demo the way a person would, whatever state it was left in.

        Esc closes a dialog, Esc leaves the input, q quits from the list. Sent
        apart, because an Esc arriving glued to the next key reads as alt+key.
        Returns True if the demo exited on its own.
        """
        for key in ("Escape", "Escape", "q"):
            if not self.alive():
                break
            self.send(key)
            time.sleep(0.15)
        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            if not self.alive():
                return True
            time.sleep(0.05)
        return False

    def close(self):
        if self.alive():
            self._tmux("kill-session", "-t", self.name, check=False)
        if self.tmux_socket and os.path.basename(self.tmux_socket) == self.name:
            try:
                os.unlink(self.tmux_socket)
            except OSError:
                pass
        shutil.rmtree(self.tmpdir, ignore_errors=True)


def mcp_config(arm, run):
    """The `mcpServers` config an agent for `arm` is given, and the dry run uses."""
    if arm == "taria":
        entry = {
            "type": "stdio",
            "command": MCP_BIN,
            "args": ["--app", "taria-demo"],
            "env": {"TARIA_SOCK": run.sock},
        }
    else:
        entry = {
            "type": "stdio",
            "command": sys.executable,
            "args": [TMUX_MCP, "--session", run.name, "--socket-name", run.name],
        }
    return {"mcpServers": {SERVER_KEY: entry}}


# --- An MCP server, spawned the way the agent's client would spawn it ---------


class StdioServer(McpClient):
    """e2e.py's MCP client, pointed at a config entry instead of taria-mcp.

    Only the spawn differs; request framing, reads and timeouts are e2e's.
    The attributes set here are the ones McpClient.__init__ sets.
    """

    def __init__(self, entry, cwd):
        env = dict(os.environ)
        env.update(entry.get("env", {}))
        self.proc = subprocess.Popen(
            [entry["command"], *entry.get("args", [])],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self._next_id = 0
        self._buf = b""
        self._stderr = bytearray()
        self._errdrain = threading.Thread(target=self._drain_stderr, daemon=True)
        self._errdrain.start()


class CountingSession:
    """Every tools/call the solver makes goes through here and is counted."""

    def __init__(self, client):
        self.client = client
        self.calls = Counter()

    def call(self, tool, arguments=None):
        self.calls[tool] += 1
        return self.client.call_raw(tool, arguments)


# --- Scripted solvers (dry run only) ------------------------------------------


def taria_delete(session, title):
    """Delete the task labelled `title` the way the tree says to."""
    tree = json.loads(session.call("read_tree"))
    items = [n for n in flatten(tree["root"]) if n.get("role") == "list_item"]
    target = next((n for n in items if n.get("label") == title), None)
    if target is None:
        raise SolverError(f"no list item labelled {title!r} in the tree")
    if not has_action(target, "delete"):
        raise SolverError(f"{target['id']} does not advertise delete")
    kind, tree = parse_result(session.call("act", {"node": target["id"], "action": "delete"}))
    if kind != "tree" or find(tree, "dialog-confirm") is None:
        raise SolverError(f"delete on {target['id']} answered {kind!r} with no dialog")
    kind, tree = parse_result(
        session.call("act", {"node": "dialog-confirm", "action": "activate"})
    )
    if kind != "tree" or find(tree, target["id"]) is not None:
        raise SolverError(f"confirming answered {kind!r}; {target['id']} still listed")


# One task row as the demo draws it: two spaces, the cursor marker or two more
# spaces, the checkbox, the title.
ROW_RE = re.compile(r"^  (>| ) \[([ x])\] (.*\S)\s*$")
CONFIRM_RE = re.compile(r"\[(\w)\] Delete")


def screen_rows(screen):
    """[(has_cursor, title)] for every task row on screen, top to bottom."""
    rows = []
    for line in screen.splitlines():
        match = ROW_RE.match(line)
        if match:
            rows.append((match.group(1) == ">", match.group(3)))
    return rows


def tmux_delete(session, title):
    """Delete the task titled `title` by reading the screen and pressing keys."""
    rows = screen_rows(session.call("screen"))
    titles = [t for _, t in rows]
    if title not in titles:
        raise SolverError(f"no row titled {title!r} on screen: {titles}")
    cursor = next((i for i, (has, _) in enumerate(rows) if has), None)
    if cursor is None:
        raise SolverError("no cursor row on screen")
    steps = titles.index(title) - cursor
    screen = None
    if steps:
        key = "Down" if steps > 0 else "Up"
        screen = session.call("send_keys", {"keys": " ".join([key] * abs(steps))})
        rows = screen_rows(screen)
        if (True, title) not in rows:
            raise SolverError(f"cursor did not reach {title!r}: {rows}")
    screen = session.call("send_keys", {"keys": "d"})
    if f"Delete task '{title}'?" not in screen:
        raise SolverError(f"d did not open a dialog for {title!r}:\n{screen}")
    confirm = CONFIRM_RE.search(screen)
    if confirm is None:
        raise SolverError(f"no confirm key on the dialog:\n{screen}")
    screen = session.call("send_keys", {"keys": confirm.group(1)})
    if title in [t for _, t in screen_rows(screen)]:
        raise SolverError(f"{title!r} still on screen after confirming")


# What each task's scripted solver does, and the wrong-target variant that
# proves the check can fail.
PLANS = {
    "T3": {"action": "delete", "title": "Wire up the MCP bridge",
           "sabotage_title": "Write the taria README"},
}
SOLVERS = {("delete", "taria"): taria_delete, ("delete", "tmux"): tmux_delete}


def wait_for_server(arm, client):
    """Hold the solver until the server can answer, as a real client's startup does.

    Claude Code starts its MCP servers before the model's first turn, so the
    agent never meets a bridge that has not connected yet. These calls are
    the harness's, not the solver's, and are not counted.
    """
    if arm == "taria":
        client.read_tree()


def dry_run(task, arm, run, config, sabotage):
    plan = PLANS[task.id]
    title = plan["sabotage_title"] if sabotage else plan["title"]
    solver = SOLVERS[(plan["action"], arm)]
    client = StdioServer(config["mcpServers"][SERVER_KEY], cwd=run.tmpdir)
    session = CountingSession(client)
    solver_error = None
    try:
        client.initialize()
        wait_for_server(arm, client)
        started = time.monotonic()
        try:
            solver(session, title)
        except (SolverError, StepFailure, ToolError, TimeoutError) as err:
            solver_error = f"{type(err).__name__}: {getattr(err, 'message', err)}"
        wall_ms = round((time.monotonic() - started) * 1000)
    finally:
        client.close()
    return {"tool_calls": dict(session.calls), "wall_ms": wall_ms, "solver_error": solver_error}


def spend_run(task, arm, run, config_path, model):
    """The paid arm. Deliberately not wired up in the pilot slice.

    TODO(spend): run the agent with cwd=run.tmpdir, so no project CLAUDE.md is
    in reach:

        claude -p <task.prompt> \\
            --strict-mcp-config --mcp-config <config_path> \\
            --tools "" --allowedTools mcp__tui \\
            --output-format stream-json --verbose \\
            --no-session-persistence --max-budget-usd 1 --model <model>

    then parse the stream: the final {"type":"result"} event carries `usage`
    (input, output and cache tokens), `total_cost_usd` and `num_turns`, and
    the tool_use blocks in the assistant events give tool_calls by name
    (strip the mcp__tui__ prefix). Grade with task.check on
    run.settled_state() exactly as the dry run does.
    """
    raise NotImplementedError("spend runs are not wired up in the pilot slice")


# --- Runner ---------------------------------------------------------------------


def results_path(path):
    if path:
        return path
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return os.path.join(RESULTS_DIR, f"{stamp}.jsonl")


def main():
    parser = argparse.ArgumentParser(
        description="Run one taria benchmark task through one arm."
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true", help="scripted solver, no model")
    mode.add_argument("--spend", action="store_true", help="real claude -p run (costs money)")
    parser.add_argument("--task", required=True, choices=sorted(TASKS))
    parser.add_argument("--arm", required=True, choices=ARMS)
    parser.add_argument(
        "--sabotage", action="store_true",
        help="dry run only: the solver deletes the wrong task, so the check must fail",
    )
    parser.add_argument("--results", help="append to this file instead of a new one")
    args = parser.parse_args()

    if args.spend:
        print(
            "run.py: --spend runs a paid model and is not wired up in the pilot "
            "slice; see the TODO in spend_run. Use --dry-run.",
            file=sys.stderr,
        )
        sys.exit(2)
    if shutil.which("tmux") is None:
        print("run.py: tmux is not on PATH", file=sys.stderr)
        sys.exit(2)
    require_fresh_binaries()

    task = TASKS[args.task]
    run = DemoRun()
    record = {
        "ts": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "task": task.id,
        "arm": args.arm,
        "mode": "dry-run",
        "solver": "scripted-sabotage" if args.sabotage else "scripted",
    }
    try:
        run.start()
        config = mcp_config(args.arm, run)
        config_path = os.path.join(run.tmpdir, "mcp.json")
        with open(config_path, "w") as f:
            json.dump(config, f, indent=2)
        if task.setup is not None:
            task.setup(run)
        outcome = dry_run(task, args.arm, run, config, args.sabotage)
        state = run.settled_state()
        passed, reason = task.check(state)
        exited = run.quit()
        final = run.read_state()
        record.update(
            passed=passed,
            reason=reason,
            tool_calls=outcome["tool_calls"],
            tool_calls_total=sum(outcome["tool_calls"].values()),
            wall_ms=outcome["wall_ms"],
            tokens=None,
            cost_usd=None,
            turns=None,
            model=None,
            solver_error=outcome["solver_error"],
            demo_exit="clean" if exited and final.get("quit") else "killed",
            state=state,
        )
    except (
        HarnessError, ToolError, StepFailure, OSError, ValueError, subprocess.SubprocessError
    ) as err:
        print(f"run.py: harness error: {err}", file=sys.stderr)
        sys.exit(2)
    finally:
        run.close()

    line = json.dumps(record)
    print(line)
    path = results_path(args.results)
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    with open(path, "a") as f:
        f.write(line + "\n")
    sys.exit(0 if record["passed"] else 1)


if __name__ == "__main__":
    main()
