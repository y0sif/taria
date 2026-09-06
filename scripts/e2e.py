#!/usr/bin/env python3
"""End-to-end verification of the taria v0 vertical slice.

Launches the taria-demo TUI headless under a pty, connects the taria-mcp
bridge to its socket, speaks MCP (newline-delimited JSON-RPC 2.0) over the
bridge's stdio, and drives a full scenario through the read_tree / act / key
tools. Prints one PASS/FAIL line per scenario step, a summary table, and
exits non-zero on any FAIL.

Python 3 stdlib only. Usage:  python3 scripts/e2e.py [--no-build]
"""

import fcntl
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEMO_BIN = os.path.join(REPO, "target", "debug", "taria-demo")
MCP_BIN = os.path.join(REPO, "target", "debug", "taria-mcp")
READ_TIMEOUT = 5.0
SNAPSHOT_LIMIT = 8 * 1024  # scenario (i): compact tree must stay under 8KB


class ToolError(Exception):
    """A tool call failed (JSON-RPC error or isError result)."""

    def __init__(self, message, code=None):
        super().__init__(message)
        self.message = message
        self.code = code


class StepFailure(Exception):
    """An assertion inside a scenario step failed."""


def require(cond, msg):
    if not cond:
        raise StepFailure(msg)


# --- Process management -----------------------------------------------------


class PtyApp:
    """taria-demo running headless under a pty."""

    def __init__(self, sock_path):
        self.sock_path = sock_path
        self.master, slave = pty.openpty()
        # Give the pty a sane size so ratatui has an area to draw into.
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        env = dict(os.environ)
        env["TARIA_SOCK"] = sock_path
        env.setdefault("TERM", "xterm-256color")
        self.proc = subprocess.Popen(
            [DEMO_BIN],
            stdin=slave,
            stdout=slave,
            stderr=subprocess.PIPE,
            env=env,
            close_fds=True,
        )
        os.close(slave)
        self._stderr = bytearray()
        self._screen = bytearray()
        # Drain the pty master forever: if nobody reads, the kernel pty
        # buffer fills and the app blocks inside terminal.draw().
        self._drain = threading.Thread(target=self._drain_master, daemon=True)
        self._drain.start()
        self._errdrain = threading.Thread(target=self._drain_stderr, daemon=True)
        self._errdrain.start()

    def _drain_master(self):
        while True:
            try:
                chunk = os.read(self.master, 4096)
            except OSError:
                return  # app exited, slave side closed
            if not chunk:
                return
            self._screen += chunk
            del self._screen[:-8192]

    def _drain_stderr(self):
        for line in self.proc.stderr:
            self._stderr += line
            del self._stderr[:-8192]

    def wait_for_socket(self, timeout=READ_TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if os.path.exists(self.sock_path):
                return True
            time.sleep(0.02)
        return False

    def stderr_tail(self):
        return self._stderr.decode(errors="replace").strip()

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            try:
                self.proc.wait(timeout=READ_TIMEOUT)
            except subprocess.TimeoutExpired:
                pass
        try:
            os.close(self.master)
        except OSError:
            pass


class McpClient:
    """taria-mcp over stdio: newline-delimited JSON-RPC 2.0."""

    def __init__(self, sock_path):
        self.proc = subprocess.Popen(
            [MCP_BIN, "--socket", sock_path],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self._next_id = 0
        self._buf = b""
        self._stderr = bytearray()
        self._errdrain = threading.Thread(target=self._drain_stderr, daemon=True)
        self._errdrain.start()

    def _drain_stderr(self):
        for line in self.proc.stderr:
            self._stderr += line
            del self._stderr[:-8192]

    def stderr_tail(self):
        return self._stderr.decode(errors="replace").strip()

    def _send(self, obj):
        line = json.dumps(obj) + "\n"
        self.proc.stdin.write(line.encode())
        self.proc.stdin.flush()

    def _read_line(self, timeout=READ_TIMEOUT):
        deadline = time.monotonic() + timeout
        fd = self.proc.stdout.fileno()
        while True:
            nl = self._buf.find(b"\n")
            if nl >= 0:
                line, self._buf = self._buf[:nl], self._buf[nl + 1 :]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"no line from taria-mcp within {timeout}s "
                    f"(buffered: {self._buf[:200]!r})"
                )
            ready, _, _ = select.select([fd], [], [], min(remaining, 0.25))
            if ready:
                chunk = os.read(fd, 65536)
                if not chunk:
                    raise TimeoutError("taria-mcp closed stdout (EOF)")
                self._buf += chunk

    def request(self, method, params=None, timeout=READ_TIMEOUT):
        """Send one request and wait for its response (skipping notifications)."""
        self._next_id += 1
        req_id = self._next_id
        msg = {"jsonrpc": "2.0", "id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"no response to {method} within {timeout}s")
            line = self._read_line(timeout=remaining)
            if not line.strip():
                continue
            resp = json.loads(line)
            if resp.get("id") == req_id:
                return resp
            # server-initiated notification/request: ignore

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)

    def initialize(self):
        resp = self.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "taria-e2e", "version": "0.0.1"},
            },
        )
        if "error" in resp:
            raise ToolError(str(resp["error"]))
        self.notify("notifications/initialized")
        return resp["result"]

    def tools_list(self):
        resp = self.request("tools/list", {})
        if "error" in resp:
            raise ToolError(str(resp["error"]))
        return [t["name"] for t in resp["result"]["tools"]]

    def call_raw(self, tool, arguments=None):
        """tools/call; returns the text content. Raises ToolError on failure."""
        resp = self.request(
            "tools/call", {"name": tool, "arguments": arguments or {}}
        )
        if "error" in resp:
            err = resp["error"]
            raise ToolError(err.get("message", str(err)), err.get("code"))
        result = resp["result"]
        texts = [
            c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"
        ]
        text = "\n".join(texts)
        if result.get("isError"):
            raise ToolError(text or "tool reported isError with no content")
        return text

    def call_tree(self, tool, arguments=None):
        """tools/call and parse the returned tree; None if the tree did not
        change (the bridge's 'Input sent, but ...' message)."""
        text = self.call_raw(tool, arguments)
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            if "did not change" in text:
                return None
            raise StepFailure(f"unparseable tool result: {text[:200]}")

    def read_tree(self, retries=25, delay=0.2):
        """read_tree with retries while the bridge is still connecting.

        Retryable errors are the bridge's two "no app right now" states:
        never connected ("no snapshot ...") and app went away
        ("... disconnected ..."); both clear once the app (re)connects.
        """
        last = None
        for _ in range(retries):
            try:
                return self.call_tree("read_tree")
            except ToolError as err:
                last = err
                if (
                    "no snapshot" not in err.message
                    and "disconnected" not in err.message
                ):
                    raise
                time.sleep(delay)
        raise StepFailure(f"read_tree never connected: {last.message}")

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        if self.proc.poll() is None:
            try:
                self.proc.wait(timeout=READ_TIMEOUT)
            except subprocess.TimeoutExpired:
                self.proc.kill()


# --- Tree helpers -----------------------------------------------------------


def flatten(node):
    yield node
    for child in node.get("children", []):
        yield from flatten(child)


def find(snapshot, node_id):
    for node in flatten(snapshot["root"]):
        if node["id"] == node_id:
            return node
    return None


def focused_ids(snapshot):
    return [n["id"] for n in flatten(snapshot["root"]) if n.get("focused")]


def one_focused(snapshot, context):
    ids = focused_ids(snapshot)
    require(len(ids) == 1, f"{context}: expected exactly one focused node, got {ids}")
    return ids[0]


def act(client, node, action, value=None, refresh=True):
    """act, falling back to read_tree when the tool says nothing changed."""
    args = {"node": node, "action": action}
    if value is not None:
        args["value"] = value
    tree = client.call_tree("act", args)
    if tree is None and refresh:
        tree = client.read_tree(retries=5)
    return tree


def key(client, key_name, refresh=True):
    tree = client.call_tree("key", {"key": key_name})
    if tree is None and refresh:
        tree = client.read_tree(retries=5)
    return tree


# --- Scenario steps ---------------------------------------------------------


def step_a_tools_list(client, ctx):
    names = client.tools_list()
    require(
        sorted(names) == ["act", "key", "read_tree"],
        f"tools/list returned {names}, expected exactly read_tree, act, key",
    )
    return f"tools: {', '.join(sorted(names))}"


def step_b_read_tree(client, ctx):
    snapshot = client.read_tree()
    root = snapshot["root"]
    require(root["role"] == "app", f"root role is {root['role']!r}, expected 'app'")
    for wanted in ("tabs", "tasks", "input"):
        require(find(snapshot, wanted) is not None, f"tree is missing id {wanted!r}")
    focused = one_focused(snapshot, "initial tree")
    ctx["snapshot"] = snapshot
    return f"root=app, ids ok, focused={focused}"


def step_c_act_select(client, ctx):
    before = one_focused(ctx["snapshot"], "before select")
    # Pick a non-focused task list item so select must move focus.
    tasks = find(ctx["snapshot"], "tasks")
    candidates = [
        c["id"]
        for c in tasks.get("children", [])
        if c["id"] != before and "select" in c.get("actions", [])
    ]
    require(candidates, "no unfocused task with a select action to target")
    target = candidates[-1]
    tree = act(client, target, "select")
    require(tree is not None, "act select produced no tree")
    after = one_focused(tree, "after select")
    require(
        after == target and after != before,
        f"focus went {before} -> {after}, expected {target}",
    )
    ctx["snapshot"] = tree
    ctx["selected"] = target
    return f"focus {before} -> {after}"


def step_d_act_toggle(client, ctx):
    target = ctx["selected"]
    before_val = find(ctx["snapshot"], target)["value"]
    require(before_val == "todo", f"{target} starts as {before_val!r}, expected todo")

    # Toggle: the task flips to done and therefore moves to the Done tab.
    tree = act(client, target, "toggle")
    require(tree is not None, "act toggle produced no tree")
    require(
        find(tree, target) is None,
        f"{target} still visible on the Active tab after toggling to done",
    )
    tree = act(client, "tab-done", "select")
    node = find(tree, target)
    require(node is not None, f"{target} not found on the Done tab after toggle")
    require(node["value"] == "done", f"{target} value {node['value']!r}, expected done")

    # Toggle back: value returns to todo (visible again on the Active tab).
    tree = act(client, target, "toggle")
    require(
        find(tree, target) is None,
        f"{target} still on the Done tab after toggling back",
    )
    tree = act(client, "tab-active", "select")
    node = find(tree, target)
    require(node is not None, f"{target} not back on the Active tab")
    require(node["value"] == "todo", f"{target} value {node['value']!r}, expected todo")
    one_focused(tree, "after toggle round-trip")
    ctx["snapshot"] = tree
    return f"{target}: todo -> done -> todo"


def step_e_add_task(client, ctx):
    title = "Bought via e2e"
    tree = act(client, "input", "set_value", value=title)
    require(tree is not None, "act set_value produced no tree")
    node = find(tree, "input")
    require(
        node["value"] == title,
        f"input value {node.get('value')!r} after set_value, expected {title!r}",
    )
    one_focused(tree, "after set_value")

    tree = act(client, "input", "activate")
    require(tree is not None, "act activate produced no tree")
    added = [n for n in flatten(tree["root"]) if n.get("label") == title]
    require(added, f"no node labeled {title!r} after activate")
    require(added[0]["value"] == "todo", "new task should start as todo")
    require(
        find(tree, "input")["value"] in (None, ""),
        "input should clear after submitting",
    )
    one_focused(tree, "after activate")
    ctx["snapshot"] = tree
    ctx["new_task"] = added[0]["id"]
    return f"task {added[0]['id']} ({title!r}) added"


def step_f_dialog(client, ctx):
    target = ctx["new_task"]
    tree = act(client, target, "delete")
    require(tree is not None, "act custom delete produced no tree")
    dialog = find(tree, "dialog")
    require(dialog is not None, "no node id 'dialog' after custom delete action")
    require(find(tree, "dialog-cancel") is not None, "dialog has no dialog-cancel")
    require(one_focused(tree, "dialog open") == "dialog", "dialog should hold focus")

    tree = act(client, "dialog-cancel", "activate")
    require(tree is not None, "act activate on dialog-cancel produced no tree")
    require(find(tree, "dialog") is None, "dialog still present after cancel")
    require(
        find(tree, target) is not None,
        "cancel must not delete the task",
    )
    ctx["snapshot"] = tree
    return "dialog opened via custom delete, closed via dialog-cancel"


def step_g_errors(client, ctx):
    try:
        act(client, "no-such-node", "select", refresh=False)
        raise StepFailure("act on bogus node id succeeded, expected an error")
    except ToolError as err:
        require(
            "valid node ids" in err.message and "tabs" in err.message,
            f"bogus-node error does not list valid ids: {err.message[:200]}",
        )
        bogus_msg = err.message

    try:
        act(client, "input", "toggle", refresh=False)
        raise StepFailure("act with unadvertised action succeeded, expected an error")
    except ToolError as err:
        require(
            "advertise" in err.message
            and "set_value" in err.message
            and "activate" in err.message,
            f"unadvertised-action error does not list advertised ones: "
            f"{err.message[:200]}",
        )
    return (
        f"bogus id error lists ids ({bogus_msg[:40]}...), "
        "bad action error lists advertised actions"
    )


def step_h_keys(client, ctx):
    before_tab = find(ctx["snapshot"], "tabs")["value"]
    tree = key(client, "tab")
    require(tree is not None, "key tab produced no tree")
    after_tab = find(tree, "tabs")["value"]
    require(
        after_tab != before_tab,
        f"tabs value still {after_tab!r} after key tab",
    )
    before_focus = one_focused(tree, "before key down")
    tree = key(client, "down")
    require(tree is not None, "key down produced no tree")
    after_focus = one_focused(tree, "after key down")
    require(
        after_focus != before_focus,
        f"focus still {after_focus!r} after key down",
    )
    ctx["snapshot"] = tree
    return f"tab {before_tab}->{after_tab}, focus {before_focus}->{after_focus}"


def step_i_snapshot_size(client, ctx):
    text = client.call_raw("read_tree")
    size = len(text.encode())
    require(
        size < SNAPSHOT_LIMIT,
        f"compact read_tree JSON is {size} bytes, expected < {SNAPSHOT_LIMIT}",
    )
    json.loads(text)  # and it must still be valid JSON
    return f"{size} bytes < {SNAPSHOT_LIMIT}"


def step_j_shutdown(client, ctx, app):
    client.call_raw("key", {"key": "q"})  # quit; tree may or may not update

    # The bridge must notice the disconnect and fail read_tree cleanly.
    deadline = time.monotonic() + READ_TIMEOUT
    not_connected = None
    while time.monotonic() < deadline:
        try:
            client.call_raw("read_tree")
            time.sleep(0.1)
        except ToolError as err:
            not_connected = err.message
            break
    require(
        not_connected is not None,
        "read_tree kept succeeding after the app quit",
    )
    # The app was connected and then quit, so the bridge must report the
    # disconnect (which app, last seq), not the never-connected message.
    require(
        "disconnected" in not_connected and "last snapshot seq" in not_connected,
        f"unexpected disconnect error: {not_connected[:200]}",
    )

    try:
        app.proc.wait(timeout=READ_TIMEOUT)
    except subprocess.TimeoutExpired:
        raise StepFailure("taria-demo did not exit after key q")
    require(
        app.proc.returncode == 0,
        f"taria-demo exited with {app.proc.returncode}",
    )

    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline and os.path.exists(app.sock_path):
        time.sleep(0.05)
    require(not os.path.exists(app.sock_path), "socket file still exists after exit")

    client.close()
    require(client.proc.poll() is not None, "taria-mcp did not exit on stdin EOF")
    return f"app rc=0, socket removed, bridge exited ({not_connected[:40]}...)"


# --- Runner -----------------------------------------------------------------


def build():
    print("== cargo build --workspace ==", flush=True)
    result = subprocess.run(
        ["cargo", "build", "--workspace"], cwd=REPO, timeout=600
    )
    if result.returncode != 0:
        print("FAIL build: cargo build --workspace failed", flush=True)
        sys.exit(1)
    for binary in (DEMO_BIN, MCP_BIN):
        if not os.path.exists(binary):
            print(f"FAIL build: missing binary {binary}", flush=True)
            sys.exit(1)


def main():
    if "--no-build" not in sys.argv:
        build()

    tmpdir = tempfile.mkdtemp(prefix="taria-e2e-")
    sock = os.path.join(tmpdir, "e2e.sock")
    app = None
    client = None
    results = []

    steps = [
        ("a tools/list exact", step_a_tools_list),
        ("b read_tree shape", step_b_read_tree),
        ("c act select moves focus", step_c_act_select),
        ("d act toggle flips value", step_d_act_toggle),
        ("e set_value+activate adds task", step_e_add_task),
        ("f custom delete dialog + cancel", step_f_dialog),
        ("g error messages", step_g_errors),
        ("h key tab / key down", step_h_keys),
        ("i snapshot < 8KB", step_i_snapshot_size),
    ]

    try:
        app = PtyApp(sock)
        if not app.wait_for_socket():
            print(f"FAIL setup: socket {sock} never appeared", flush=True)
            print(f"demo stderr: {app.stderr_tail()}", flush=True)
            sys.exit(1)

        client = McpClient(sock)
        init = client.initialize()
        server_name = init.get("serverInfo", {}).get("name", "?")
        print(f"initialized: server={server_name} protocol="
              f"{init.get('protocolVersion', '?')}", flush=True)

        ctx = {}
        failed_hard = False
        for name, fn in steps:
            if failed_hard:
                results.append((name, False, "skipped: earlier step failed"))
                print(f"FAIL {name}: skipped after earlier failure", flush=True)
                continue
            try:
                evidence = fn(client, ctx)
                results.append((name, True, evidence))
                print(f"PASS {name}: {evidence}", flush=True)
            except (StepFailure, ToolError, TimeoutError) as err:
                results.append((name, False, str(err)))
                print(f"FAIL {name}: {err}", flush=True)
                # steps share state; keep going only if the tree still reads
                try:
                    ctx["snapshot"] = client.read_tree(retries=3)
                except Exception:
                    failed_hard = True

        # Shutdown is special: it consumes both processes.
        name = "j clean shutdown"
        if failed_hard:
            results.append((name, False, "skipped: earlier step failed"))
            print(f"FAIL {name}: skipped after earlier failure", flush=True)
        else:
            try:
                evidence = step_j_shutdown(client, ctx, app)
                results.append((name, True, evidence))
                print(f"PASS {name}: {evidence}", flush=True)
            except (StepFailure, ToolError, TimeoutError) as err:
                results.append((name, False, str(err)))
                print(f"FAIL {name}: {err}", flush=True)
    finally:
        failures = [r for r in results if not r[1]]
        if failures:
            if app is not None:
                print(f"-- taria-demo stderr --\n{app.stderr_tail()}", flush=True)
            if client is not None:
                print(f"-- taria-mcp stderr --\n{client.stderr_tail()}", flush=True)
        if client is not None:
            client.close()
            if client.proc.poll() is None:
                client.proc.kill()
        if app is not None:
            app.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)

    print("\n== summary ==")
    width = max(len(name) for name, _, _ in results) if results else 0
    for name, ok, evidence in results:
        print(f"{'PASS' if ok else 'FAIL'}  {name:<{width}}  {evidence}")
    passed = sum(1 for _, ok, _ in results if ok)
    print(f"{passed}/{len(results)} steps passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
