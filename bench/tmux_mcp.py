#!/usr/bin/env python3
"""A screen-scraping MCP server over tmux: the benchmark's baseline arm.

Speaks MCP (newline-delimited JSON-RPC 2.0) on stdio, the way taria-mcp does,
and exposes the two tools a tmux-driving agent gets: `screen`, the pane's text
from `tmux capture-pane -p`, and `send_keys`, `tmux send-keys` followed by a
wait for the screen to settle. The settle budget is the same 500 ms taria-mcp
waits for an app to react (UPDATE_WAIT in crates/taria-mcp/src/server.rs), so
neither arm buys accuracy with a longer wait.

Nothing here knows which application runs in the pane. Python 3 stdlib only.

Usage:  python3 bench/tmux_mcp.py --session NAME [--socket-name NAME]

`--socket-name` is tmux's `-L`: the benchmark runs each demo on a private
tmux server so a user's own tmux configuration and sessions stay out of it.
"""

import argparse
import json
import subprocess
import sys
import time

SERVER_NAME = "tmux-mcp"
SERVER_VERSION = "0.1.0"

# Newest first. A client asking for one of these gets it back; anything else
# gets the newest, which is how rmcp (and so taria-mcp) answers.
PROTOCOL_VERSIONS = ("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")

POLL_INTERVAL = 0.05  # seconds between captures while waiting for a settle
SETTLE_CAP = 0.5  # taria-mcp's UPDATE_WAIT
MAX_KEYS_CHARS = 4096  # taria-mcp's bound on type_text and set_value

PARSE_ERROR = -32700
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

INSTRUCTIONS = (
    "Bridge to a terminal application running in a tmux pane. Call screen first: it "
    "returns the pane's current text exactly as displayed, one line per row, with no "
    "colours or text styles, so emphasis such as a highlighted row or a focused box "
    "shows only through the characters drawn. send_keys sends keystrokes to the "
    "application as if typed on its keyboard; they land wherever the application has "
    "focus. keys is a space-separated list of tmux key names, each sent as one press: "
    "Enter, Escape, Tab, BTab (shift+tab), Space, BSpace (backspace), Up, Down, Left, "
    "Right, Home, End, PageUp, PageDown, F1 to F12; C-x is ctrl+x and M-x is alt+x, so "
    "C-c is ctrl+c. Any other word is sent as its characters, so \"d\" presses d and "
    "\"Down Down Enter\" is three presses. Set literal to true to send keys verbatim "
    "as characters, spaces included, with no key names interpreted; use it to type "
    "text, and send Enter as its own call. Characters typed this way still meet "
    "whatever the application binds those keys to. send_keys waits up to 0.5 seconds "
    "for the screen to stop changing and returns it; call screen again whenever you "
    "need a fresh view."
)

TOOLS = [
    {
        "name": "screen",
        "description": (
            "Read the terminal pane's current contents as plain text, one line per "
            "row, exactly as the application drew them, without colours or styles. "
            "Call this first; it is the only view of the application there is."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "send_keys",
        "description": (
            "Send keystrokes to the application in the pane, then wait up to 0.5 "
            "seconds for the screen to stop changing and return it. `keys` is a "
            "space-separated list of tmux key names, one press each: Enter, Escape, "
            "Tab, BTab, Space, BSpace, Up, Down, Left, Right, Home, End, PageUp, "
            "PageDown, F1-F12, C-x for ctrl+x, M-x for alt+x; any other word is sent "
            "as its characters. With `literal` true, `keys` is sent verbatim as "
            "characters, spaces included, which is how to type text; send Enter "
            "separately. Up to 4096 characters. The keys reach whatever has focus, "
            "exactly as typed on a keyboard."
        ),
        "inputSchema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "keys": {
                    "type": "string",
                    "description": (
                        "Space-separated tmux key names, e.g. \"Down\", \"Down Down "
                        "Enter\", \"C-c\"; or, with literal, the text to type."
                    ),
                    "minLength": 1,
                    "maxLength": MAX_KEYS_CHARS,
                },
                "literal": {
                    "type": ["boolean", "null"],
                    "description": (
                        "Send keys verbatim as characters instead of as key names. "
                        "Defaults to false."
                    ),
                },
            },
            "required": ["keys"],
        },
    },
]


class RpcError(Exception):
    """Answered as a JSON-RPC error object rather than a tool result."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


class Pane:
    """The one tmux pane this server drives."""

    def __init__(self, session, socket_name=None):
        self.target = session
        self.base = ["tmux"] + (["-L", socket_name] if socket_name else [])

    def _tmux(self, *args):
        try:
            result = subprocess.run(
                self.base + list(args), capture_output=True, text=True, timeout=5
            )
        except (OSError, subprocess.TimeoutExpired) as err:
            raise RpcError(INTERNAL_ERROR, f"could not run tmux: {err}")
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip() or f"exit {result.returncode}"
            raise RpcError(
                INTERNAL_ERROR,
                f"tmux {args[0]} failed: {detail}. Is the application's tmux session "
                "still running?",
            )
        return result.stdout

    def capture(self):
        return self._tmux("capture-pane", "-p", "-t", self.target).rstrip("\n")

    def send(self, keys, literal):
        if literal:
            self._tmux("send-keys", "-t", self.target, "-l", "--", keys)
        else:
            self._tmux("send-keys", "-t", self.target, "--", *keys.split())

    def settle(self):
        """Poll until two consecutive captures match, or SETTLE_CAP runs out."""
        deadline = time.monotonic() + SETTLE_CAP
        time.sleep(POLL_INTERVAL)
        last = self.capture()
        while time.monotonic() < deadline:
            time.sleep(POLL_INTERVAL)
            current = self.capture()
            if current == last:
                return current
            last = current
        return last


def tool_result(text, is_error=False):
    return {"content": [{"type": "text", "text": text}], "isError": is_error}


def call_tool(pane, name, arguments):
    if name == "screen":
        return tool_result(pane.capture())
    if name == "send_keys":
        # Mirrors rmcp: a missing or mistyped field is a tool result carrying
        # the deserialization error; an out-of-bounds value is invalid params.
        keys = arguments.get("keys")
        if not isinstance(keys, str):
            return tool_result(
                "failed to deserialize parameters: missing field `keys`"
                if keys is None
                else "failed to deserialize parameters: `keys` must be a string",
                is_error=True,
            )
        literal = arguments.get("literal")
        if literal is None:
            literal = False
        if not isinstance(literal, bool):
            return tool_result(
                "failed to deserialize parameters: `literal` must be a boolean",
                is_error=True,
            )
        # Literal text may be all spaces; a list of key names may not be.
        if not (keys if literal else keys.split()):
            raise RpcError(INVALID_PARAMS, "keys must be non-empty")
        if len(keys) > MAX_KEYS_CHARS:
            raise RpcError(
                INVALID_PARAMS,
                f"keys is {len(keys)} characters, over the {MAX_KEYS_CHARS} character "
                "limit for one send_keys call; send it in smaller pieces",
            )
        pane.send(keys, literal)
        return tool_result(pane.settle())
    raise RpcError(INVALID_PARAMS, f"tool not found: {name}")


def handle(pane, msg):
    """Answer one JSON-RPC message; returns the response dict, or None."""
    method = msg.get("method")
    if "id" not in msg:
        return None  # a notification (notifications/initialized and friends)
    req_id = msg["id"]
    params = msg.get("params") or {}
    try:
        if method == "initialize":
            asked = params.get("protocolVersion")
            version = asked if asked in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
            result = {
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": INSTRUCTIONS,
            }
        elif method == "ping":
            result = {}
        elif method == "tools/list":
            result = {"tools": TOOLS}
        elif method == "tools/call":
            arguments = params.get("arguments") or {}
            if not isinstance(arguments, dict):
                raise RpcError(INVALID_PARAMS, "arguments must be an object")
            result = call_tool(pane, params.get("name"), arguments)
        else:
            raise RpcError(METHOD_NOT_FOUND, method or "missing method")
    except RpcError as err:
        return {"jsonrpc": "2.0", "id": req_id, "error": {"code": err.code, "message": err.message}}
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def serve(pane, stdin=sys.stdin, stdout=sys.stdout):
    for line in stdin:
        if not line.strip():
            continue
        try:
            msg = json.loads(line)
            if not isinstance(msg, dict):
                raise ValueError("not an object")
        except ValueError as err:
            response = {
                "jsonrpc": "2.0",
                "id": None,
                "error": {"code": PARSE_ERROR, "message": f"parse error: {err}"},
            }
        else:
            response = handle(pane, msg)
        if response is not None:
            stdout.write(json.dumps(response) + "\n")
            stdout.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--session", required=True, help="tmux target session")
    parser.add_argument("--socket-name", help="tmux -L socket name of the server")
    args = parser.parse_args()
    try:
        serve(Pane(args.session, args.socket_name))
    except (BrokenPipeError, KeyboardInterrupt):
        pass


if __name__ == "__main__":
    main()
