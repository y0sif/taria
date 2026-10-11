#!/usr/bin/env python3
"""Start one arm's MCP server against the demo a slot holds.

A subagent's file (bench/agents.py) names this script as its inline MCP
server. Claude Code runs it when the agent starts; it reads the slot's state
file, written by `run.py next`, and execs the arm's real server exactly as
harness.server_entry configures it for that demo:

  taria  target/debug/taria-mcp --app taria-demo, with TARIA_SOCK=<demo socket>
  tmux   python3 bench/tmux_mcp.py --session <name> --socket-path <tmux socket>

exec, not a proxy: the server inherits this process's stdio, so the MCP
stream passes straight through and nothing here can alter or delay it.
Nothing is written to stdout before the exec.

A missing slot file, a slot still being prepared, or a slot whose job is for
the other arm (the wrong agent was spawned for the slot) is one line on
stderr and exit 1, so the agent gets no tools rather than another arm's.

One launch per job: before the exec it creates `<slots-dir>/<K>.<job>.launched`
with O_CREAT|O_EXCL, and refuses (exit 1) when that file exists, so a second
agent cannot drive a demo another agent already used. `run.py` removes the
marker when it frees the slot (graded, aborted, or a dead prepare). A refused
agent is offered no tools, which the transcript guard reports as a harness
error.

Usage:  python3 bench/mcp_launch.py --arm taria|tmux --slot K [--slots-dir D]

Python 3 stdlib only.
"""

import argparse
import glob
import json
import os
import re
import sys
from datetime import datetime, timezone

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import ARMS, DemoRun, HarnessError, server_entry  # noqa: E402

DEFAULT_SLOTS_DIR = os.path.join(BENCH, "results", "slots")
JOB_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def marker_path(slots_dir, slot, job):
    """The file whose existence says job `job` in slot `slot` was launched."""
    return os.path.join(slots_dir, f"{slot}.{job}.launched")


def clear_markers(slots_dir, slot):
    """Remove every launch marker of slot `slot`. Returns the paths removed."""
    removed = []
    for path in glob.glob(os.path.join(glob.escape(slots_dir), f"{int(slot)}.*.launched")):
        try:
            os.remove(path)
        except FileNotFoundError:
            continue
        removed.append(path)
    return removed


def claim_launch(slots_dir, slot, job):
    """Create the job's launch marker, or refuse when it exists."""
    if not isinstance(job, str) or not JOB_RE.match(job):
        refuse(f"slot {slot} names job {job!r}, which is not [A-Za-z0-9_-]+")
    path = marker_path(slots_dir, slot, job)
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        refuse(
            f"slot {slot} (job {job}) was already launched ({path} exists): one agent per job, "
            "so this one gets no tools"
        )
    except OSError as err:
        refuse(f"cannot create {path}: {err}")
    with os.fdopen(fd, "w") as f:
        json.dump({"pid": os.getpid(), "launched": datetime.now(timezone.utc).isoformat()}, f)


def refuse(message):
    print(f"mcp_launch: {message}", file=sys.stderr, flush=True)
    sys.exit(1)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--arm", required=True, choices=ARMS)
    parser.add_argument("--slot", required=True, type=int)
    parser.add_argument("--slots-dir", default=DEFAULT_SLOTS_DIR)
    args = parser.parse_args(argv)

    path = os.path.join(args.slots_dir, f"{args.slot}.json")
    try:
        with open(path) as f:
            slot = json.load(f)
    except FileNotFoundError:
        refuse(f"slot {args.slot} holds no job ({path} does not exist); `run.py next` prepares it")
    except (OSError, ValueError) as err:
        refuse(f"cannot read {path}: {err}")
    if not isinstance(slot, dict):
        refuse(f"{path} is not a slot file")
    if slot.get("arm") != args.arm:
        refuse(
            f"slot {args.slot} holds job {slot.get('job')} for the {slot.get('arm')} arm, "
            f"not {args.arm}: this agent is the wrong one for the slot"
        )
    if slot.get("state") != "ready":
        refuse(f"slot {args.slot} (job {slot.get('job')}) is {slot.get('state')!r}, not ready")
    try:
        entry = server_entry(args.arm, DemoRun.from_slot(slot.get("demo") or {}))
    except HarnessError as err:
        refuse(f"slot {args.slot}: {err}")

    claim_launch(args.slots_dir, args.slot, slot.get("job"))
    env = dict(os.environ)
    env.update(entry.get("env", {}))
    workdir = slot["demo"]["workdir"]
    if os.path.isdir(workdir):
        os.chdir(workdir)
    command = entry["command"]
    try:
        os.execvpe(command, [command, *entry["args"]], env)
    except OSError as err:
        refuse(f"cannot start {command}: {err}")


if __name__ == "__main__":
    main()
