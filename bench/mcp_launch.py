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

Usage:  python3 bench/mcp_launch.py --arm taria|tmux --slot K [--slots-dir D]

Python 3 stdlib only.
"""

import argparse
import json
import os
import sys

BENCH = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BENCH)
from harness import ARMS, DemoRun, HarnessError, server_entry  # noqa: E402

DEFAULT_SLOTS_DIR = os.path.join(BENCH, "results", "slots")


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
