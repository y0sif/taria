#!/usr/bin/env python3
"""Scripted MCP client for the README demo GIF (demo/demo.tape).

It starts target/debug/taria-mcp and speaks newline-delimited JSON-RPC 2.0
to it over stdio, the way scripts/e2e.py does, then drives taria-demo through
six fixed tool calls: semantic acts on node ids, plus one type_text. No model
is involved and no raw key is sent. Every line it prints about the tree is
taken from the bridge's answer, in the outline format.

The bridge finds the app with `--app taria-demo`, so set TARIA_SOCK for both
sides to keep the demo off any taria-demo you already run (the tape does).

Standard library only. Run from anywhere:

    python3 demo/drive.py [--hold]

`--hold` keeps the process (and so its tmux pane) alive after the last step,
for the recording; without it the script exits when it is done.
"""

import json
import os
import re
import select
import shutil
import subprocess
import sys
import textwrap
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MCP_BIN = ROOT / "target" / "debug" / "taria-mcp"
LABEL = "taria-demo"
TITLE = "scripted MCP client driving taria-demo through taria-mcp"

# Pacing, in seconds: long enough to follow in a GIF, short enough to keep
# the whole run near 20 s.
START_PAUSE = 1.5
CALL_LEAD = 0.7
STEP_PAUSE = 1.6
READ_PAUSE = 1.0
READ_TIMEOUT = 10.0

# The six steps: (tool, arguments, ids of the nodes worth printing from the
# tree that comes back). A node that left the tree is printed from the tree
# before, marked "-".
STEPS = [
    ("read_tree", {}, ["tasks", "task-1", "task-3", "task-4", "input"]),
    ("act", {"node": "task-4", "action": "toggle"}, ["task-4"]),
    ("act", {"node": "tab-done", "action": "select"}, ["tabs", "task-4"]),
    ("act", {"node": "input", "action": "focus"}, ["input"]),
    ("type_text", {"text": "ship 0.4.0"}, ["input"]),
    ("act", {"node": "input", "action": "activate"}, ["tabs", "task-6"]),
]

# ANSI styling.
RESET = "\x1b[0m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
RED = "\x1b[31m"
GREEN = "\x1b[32m"
YELLOW = "\x1b[33m"
BLUE = "\x1b[34m"
MAGENTA = "\x1b[35m"
CYAN = "\x1b[36m"
MARK_COLOR = {"+": GREEN, "~": YELLOW, "-": RED, " ": DIM}

MARK_RE = re.compile(r"(\*[a-z_]+(?:=[a-z]+)?)")
ACT_RE = re.compile(r"(act: \S+)")


def out(text=""):
    sys.stdout.write(text + "\n")
    sys.stdout.flush()


def colorize(row):
    """Highlight state marks and the action list in one printed row."""
    row = MARK_RE.sub(CYAN + r"\1" + RESET, row)
    return ACT_RE.sub(BLUE + r"\1" + RESET, row)


def print_node(mark, line, width):
    """One outline line, wrapped under its id, with a diff mark in front."""
    prefix = f"  {mark} "
    # A no-break space keeps "act:" on the row with its actions.
    rows = textwrap.wrap(
        line.replace("act: ", "act:\u00a0"),
        width=max(width - 1, 20),
        initial_indent=prefix,
        subsequent_indent=" " * (len(prefix) + 2),
        break_on_hyphens=False,
    )
    color = MARK_COLOR.get(mark, "")
    for i, row in enumerate(rows):
        row = row.replace("\u00a0", " ")
        if i == 0:
            out(f"  {color}{BOLD}{mark}{RESET} " + colorize(row[len(prefix) :]))
        else:
            out(colorize(row))


def parse_outline(text):
    """(header, {id: line}) from an outline tree; ids are the first token."""
    lines = text.splitlines()
    nodes = {}
    for line in lines[1:]:
        stripped = line.strip()
        if stripped:
            nodes[stripped.split(" ", 1)[0]] = stripped
    return lines[0] if lines else "", nodes


def socket_path():
    sock = os.environ.get("TARIA_SOCK")
    if sock:
        return Path(sock)
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime:
        return Path(runtime) / "taria" / f"{LABEL}.sock"
    return None


class Bridge:
    """taria-mcp over stdio."""

    def __init__(self):
        env = dict(os.environ)
        env["TARIA_LOG"] = "warn"
        self.proc = subprocess.Popen(
            [str(MCP_BIN), "--app", LABEL, "--tree-format", "outline"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        self.next_id = 0
        self.buf = b""

    def send(self, msg):
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def read_line(self, deadline):
        fd = self.proc.stdout.fileno()
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                line, self.buf = self.buf[:nl], self.buf[nl + 1 :]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("no answer from taria-mcp")
            ready, _, _ = select.select([fd], [], [], min(remaining, 0.25))
            if ready:
                chunk = os.read(fd, 65536)
                if not chunk:
                    raise TimeoutError("taria-mcp closed its stdout")
                self.buf += chunk

    def request(self, method, params):
        self.next_id += 1
        req_id = self.next_id
        self.send({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params})
        deadline = time.monotonic() + READ_TIMEOUT
        while True:
            line = self.read_line(deadline)
            if line.strip():
                resp = json.loads(line)
                if resp.get("id") == req_id:
                    return resp

    def initialize(self):
        resp = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "taria-demo-gif", "version": "0.1.0"},
            },
        )
        if "error" in resp:
            raise RuntimeError(resp["error"].get("message", "initialize failed"))
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def call(self, tool, arguments):
        """(is_error, text) of one tools/call."""
        resp = self.request("tools/call", {"name": tool, "arguments": arguments})
        if "error" in resp:
            return True, resp["error"].get("message", str(resp["error"]))
        result = resp["result"]
        text = "\n".join(
            c.get("text", "")
            for c in result.get("content", [])
            if c.get("type") == "text"
        )
        return bool(result.get("isError")), text

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()
            self.proc.wait()


def command_line(tool, args):
    if tool == "act":
        return f"act {args['node']} {args['action']}"
    if tool == "type_text":
        return f"type_text {json.dumps(args['text'])}"
    return tool


def run(bridge, width):
    old = {}
    delivered = 0
    for n, (tool, args, show) in enumerate(STEPS):
        out(f"{BLUE}{BOLD}>{RESET} {BOLD}{command_line(tool, args)}{RESET}")
        time.sleep(CALL_LEAD)
        is_error, text = bridge.call(tool, args)
        if is_error:
            out(f"{RED}< error: {text.splitlines()[0] if text else 'no text'}{RESET}")
            return False
        if not text.startswith("protocol_version="):
            # A note instead of a tree: ignored, unchanged, or no ack. Say it
            # in the bridge's own words and stop, since the script assumed a
            # changed tree at every step.
            first = textwrap.shorten(
                text.splitlines()[0], width=width - 4, placeholder=" ..."
            )
            out(f"{YELLOW}< {first}{RESET}")
            return False
        header, nodes = parse_outline(text)
        seq = header.split("seq=", 1)[-1].split(" ", 1)[0]
        if tool == "read_tree":
            out(
                f"{GREEN}< tree seq {seq}, {len(nodes)} nodes ({len(show)} shown){RESET}"
            )
        else:
            delivered += 1
            out(f"{GREEN}< delivered, tree changed (seq {seq}){RESET}")
        for node_id in show:
            if node_id in nodes:
                if tool == "read_tree":
                    mark = " "
                elif node_id not in old:
                    mark = "+"
                elif old[node_id] != nodes[node_id]:
                    mark = "~"
                else:
                    mark = " "
                print_node(mark, nodes[node_id], width)
            elif node_id in old:
                # Gone from the tree: id, role and label of its last line.
                gone = old[node_id].split(" *", 1)[0].split(" act: ", 1)[0]
                print_node("-", gone, width)
        old = nodes
        out()
        time.sleep(STEP_PAUSE + (READ_PAUSE if n == 0 else 0.0))
    out(
        f"{MAGENTA}done: {len(STEPS)} calls, {delivered} inputs delivered, 0 raw keys{RESET}"
    )
    return True


def main():
    hold = "--hold" in sys.argv[1:]
    size = shutil.get_terminal_size((80, 24))
    width = size.columns
    sys.stdout.write("\x1b[?25l")  # hide the cursor
    out(f"{MAGENTA}{BOLD}{TITLE}{RESET}")
    out()
    if sys.stdout.isatty():
        # Pin the title: scroll only the rows below it, then start there.
        sys.stdout.write(f"\x1b[3;{size.lines}r\x1b[3;1H")

    ok = False
    sock = socket_path()
    deadline = time.monotonic() + 15
    while sock is not None and not sock.exists() and time.monotonic() < deadline:
        time.sleep(0.1)
    if sock is not None and not sock.exists():
        out(f"{RED}failed: no taria socket at {sock}{RESET}")
    elif not MCP_BIN.exists():
        out(f"{RED}failed: {MCP_BIN} is missing; cargo build -p taria-mcp{RESET}")
    else:
        time.sleep(START_PAUSE)
        bridge = Bridge()
        try:
            bridge.initialize()
            ok = run(bridge, width)
        except (OSError, RuntimeError, TimeoutError, ValueError) as err:
            out(f"{RED}< error: {err}{RESET}")
        finally:
            bridge.close()
        if not ok:
            out(f"{RED}failed: the script stopped early{RESET}")

    if hold:
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            pass
    if sys.stdout.isatty():
        sys.stdout.write("\x1b[r")  # the whole screen scrolls again
    sys.stdout.write("\x1b[?25h")
    sys.stdout.flush()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
