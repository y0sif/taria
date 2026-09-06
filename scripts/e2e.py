#!/usr/bin/env python3
"""End-to-end verification of the taria v0.1 vertical slice.

Launches the taria-demo TUI headless under a pty, connects the taria-mcp
bridge to its socket, speaks MCP (newline-delimited JSON-RPC 2.0) over the
bridge's stdio, and drives a full scenario through the read_tree / act / key /
type_text tools. Prints one PASS/FAIL line per scenario step, a summary table,
and exits non-zero on any FAIL.

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
SNAPSHOT_LIMIT = 8 * 1024  # scenario (n): compact tree must stay under 8KB
INVALID_PARAMS = -32602  # JSON-RPC code the bridge rejects bad arguments with

# The four shapes an input-sending tool (act / key / type_text) can answer
# with, as of the v0.1 ack protocol. Matched by prefix, verbatim: an agent
# reads these strings, so a reworded one is a behaviour change this script has
# to notice rather than absorb.
IGNORED_PREFIX = (
    "The app received this input and deliberately did nothing with it "
    "(for example an action a modal dialog blocks, or a node it no longer "
    "knows). Re-plan from the current tree below."
)
NO_CHANGE_PREFIX = "The app received this input, and its tree did not change within"
NO_ACK_PREFIX = "The app neither acknowledged this input nor changed its tree within"


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


def parse_result(text):
    """Classify one act / key / type_text result as (kind, tree).

    kind is one of:
      "tree"      the app applied the input and published a new tree
      "ignored"   the app acked Ignored; the tree to re-plan from follows
                  the note on the second line
      "no_change" the app acked Delivered but published nothing new
      "no_ack"    no ack and no new tree inside the bridge's window

    Anything else is a failure, not a shape to absorb: an unrecognised
    result means the tool surface moved and this script is asserting on a
    contract that no longer exists.
    """
    if text.startswith(IGNORED_PREFIX):
        parts = text.split("\n", 1)
        require(
            len(parts) == 2 and parts[1].strip(),
            f"ignored result carried no tree on its second line: {text[:200]}",
        )
        try:
            return "ignored", json.loads(parts[1])
        except json.JSONDecodeError:
            raise StepFailure(
                f"ignored result's second line is not a tree: {parts[1][:200]}"
            )
    if text.startswith(NO_CHANGE_PREFIX):
        return "no_change", None
    if text.startswith(NO_ACK_PREFIX):
        return "no_ack", None
    try:
        return "tree", json.loads(text)
    except json.JSONDecodeError:
        raise StepFailure(f"unparseable tool result: {text[:200]}")


def response_text(resp):
    """Text content of one tools/call response; raises ToolError on failure."""
    if "error" in resp:
        err = resp["error"]
        raise ToolError(err.get("message", str(err)), err.get("code"))
    result = resp["result"]
    text = "\n".join(
        c.get("text", "") for c in result.get("content", []) if c.get("type") == "text"
    )
    if result.get("isError"):
        raise ToolError(text or "tool reported isError with no content")
    return text


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

    def wait_stderr(self, timeout=READ_TIMEOUT):
        """Wait for the drain thread to reach EOF on the app's stderr.

        The demo prints its dropped- and discarded-input counts *after*
        restoring the terminal, so they are the last thing it writes. Reading
        stderr the instant the process exits can miss them, which would turn
        an assertion about those lines into one that quietly always passes.
        """
        self._errdrain.join(timeout)
        return not self._errdrain.is_alive()

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
        return response_text(
            self.request("tools/call", {"name": tool, "arguments": arguments or {}})
        )

    def send_call(self, tool, arguments=None):
        """Write one tools/call without waiting for it; returns its id.

        Pipelining two calls is the only way to reach the app's own gates
        from here: the bridge validates a call against the newest tree it
        has, so a second call written before the app has published the
        effect of the first is the one the app itself gets to refuse.
        """
        self._next_id += 1
        req_id = self._next_id
        self._send(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {"name": tool, "arguments": arguments or {}},
            }
        )
        return req_id

    def collect(self, ids, timeout=15.0):
        """Wait for the responses to `ids`; returns {id: response}."""
        out = {}
        deadline = time.monotonic() + timeout
        while len(out) < len(ids):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"only {len(out)}/{len(ids)} pipelined responses within {timeout}s"
                )
            line = self._read_line(timeout=remaining)
            if not line.strip():
                continue
            resp = json.loads(line)
            if resp.get("id") in ids:
                out[resp["id"]] = resp
        return out

    def call_outcome(self, tool, arguments=None):
        """tools/call, classified by [`parse_result`]: (kind, tree)."""
        return parse_result(self.call_raw(tool, arguments))

    def call_tree(self, tool, arguments=None):
        """tools/call for a tool expected to have an effect: the new tree, or
        None when the app acked but published nothing new.

        An `ignored` ack is a failure here, not a None: the app said it
        deliberately did nothing, and a step that asked for an effect wants
        to hear that rather than quietly retry a read_tree.
        """
        kind, tree = self.call_outcome(tool, arguments)
        if kind == "ignored":
            raise StepFailure(
                f"{tool} {arguments!r} was IGNORED by the app; expected it to apply"
            )
        return tree

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


def key(client, key_name, repeat=None, refresh=True):
    args = {"key": key_name}
    if repeat is not None:
        args["repeat"] = repeat
    tree = client.call_tree("key", args)
    if tree is None and refresh:
        tree = client.read_tree(retries=5)
    return tree


def task_items(snapshot):
    """The visible task list items, in list order."""
    tasks = find(snapshot, "tasks")
    require(tasks is not None, "tree has no 'tasks' node")
    return tasks.get("children", [])


def task_labels(snapshot):
    """{node id: label} for every visible task."""
    return {item["id"]: item.get("label") for item in task_items(snapshot)}


def has_action(node, name):
    """True if the node advertises `name` (built-in string or custom object)."""
    for action in node.get("actions", []):
        if action == name or (isinstance(action, dict) and action.get("custom") == name):
            return True
    return False


def deletable_task(snapshot):
    """A visible task advertising the app's custom delete action."""
    for node in flatten(snapshot["root"]):
        if has_action(node, "delete"):
            return node["id"]
    raise StepFailure("no node advertises the custom delete action")


def close_any_dialog(client):
    """Best-effort dismiss of an open modal, for cleanup paths.

    A step that fails with the dialog up would otherwise hand every later
    step an app that refuses acts and will not even quit on `q`.
    """
    try:
        if find(client.read_tree(retries=5), "dialog") is not None:
            client.call_raw("act", {"node": "dialog", "action": "dismiss"})
    except (ToolError, StepFailure, TimeoutError):
        pass


# --- Scenario steps ---------------------------------------------------------


def step_a_tools_list(client, ctx):
    names = client.tools_list()
    require(
        sorted(names) == ["act", "key", "read_tree", "type_text"],
        f"tools/list returned {names}, expected exactly read_tree, act, key, "
        "type_text",
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


def step_i_type_text(client, ctx):
    """type_text is the v0.1 headline: one call types a whole title."""
    title = "Typed end to end"
    act(client, "tab-active", "select")  # step h left the Done tab on screen
    before = client.read_tree()
    require(
        title not in task_labels(before).values(),
        f"a task labelled {title!r} exists before typing it",
    )
    before_ids = set(task_labels(before))

    # Focus the input the way an agent would, and from a known empty draft, so
    # what ends up in the task is exactly what type_text typed.
    tree = act(client, "input", "set_value", value="")
    require(
        one_focused(tree, "before typing") == "input",
        "set_value must leave the input focused before typing",
    )
    require(
        find(tree, "input")["value"] in (None, ""),
        f"draft is {find(tree, 'input').get('value')!r}, expected empty",
    )

    # One call types the title and the trailing newline that submits it.
    tree = client.call_tree("type_text", {"text": title + "\n"})
    if tree is None:
        tree = client.read_tree(retries=5)

    added = [n for n in flatten(tree["root"]) if n.get("label") == title]
    require(
        len(added) == 1,
        f"expected exactly one node labelled {title!r}, got {len(added)}",
    )
    node = added[0]
    require(
        node["id"] not in before_ids,
        f"{node['id']} existed before type_text; no new task was created",
    )
    require(node["value"] == "todo", f"typed task value {node['value']!r}, expected todo")
    require(
        find(tree, "input")["value"] in (None, ""),
        "input should clear after the typed newline submits it",
    )
    require(
        node["id"] == one_focused(tree, "after typing"),
        "the typed task should be the selected one after submitting",
    )
    ctx["snapshot"] = tree
    ctx["typed_task"] = node["id"]
    return f"one type_text call created {node['id']} labelled {title!r}"


def step_j_ignored_ack(client, ctx):
    """An act the modal blocks answers with the ignored shape, not silence.

    The bridge refuses an act on a node whose actions the modal stripped, so
    the app only gets to refuse one handed to it before the dialog existed:
    both calls go out back to back, and the second is validated against the
    pre-dialog tree. Losing that race is the bridge doing its job, so it is
    retried rather than failed.
    """
    draft = "typed while modal"
    target = deletable_task(client.read_tree())
    rejected = 0
    try:
        for _ in range(5):
            ids = [
                client.send_call("act", {"node": target, "action": "delete"}),
                client.send_call(
                    "act", {"node": "input", "action": "set_value", "value": draft}
                ),
            ]
            responses = client.collect(ids)
            opened = parse_result(response_text(responses[ids[0]]))[1]
            require(opened is not None, "the delete act published no tree")
            require(find(opened, "dialog") is not None, "delete did not open the dialog")

            try:
                text = response_text(responses[ids[1]])
            except ToolError as err:
                # The bridge saw the dialog first and refused on advertisement.
                require(
                    "does not advertise" in err.message,
                    f"unexpected rejection while modal: {err.message[:160]}",
                )
                rejected += 1
                act(client, "dialog", "dismiss")
                continue

            kind, tree = parse_result(text)
            require(
                kind == "ignored",
                f"act blocked by the modal came back as {kind!r}, expected the "
                f"ignored shape: {text[:160]}",
            )
            require(tree is not None, "ignored result carried no tree")
            require(
                find(tree, "input")["value"] != draft,
                f"the modal-blocked set_value took effect anyway (input={draft!r})",
            )
            # The tree the note offers is the state to re-plan from, which
            # means the frame that caused the ignore: the dialog the app
            # refused this act for has to be in it. The app acks while
            # draining and publishes after, so the ack lands first; the
            # bridge waits out its budget for that frame rather than handing
            # back the pre-input tree, in which nothing explains the refusal.
            require(
                find(tree, "dialog") is not None,
                "the ignored ack's tree has no dialog node: it predates the "
                "input, so it cannot explain why the act was refused",
            )
            replan = "with dialog"

            # The app really did nothing: the modal is still up, untouched.
            live = client.read_tree()
            require(find(live, "dialog") is not None, "the modal closed by itself")
            require(
                find(live, "input")["value"] != draft,
                "the modal-blocked draft reached the app after all",
            )

            # Leave the app as the other dialog steps do: no modal, task intact.
            tree = act(client, "dialog-cancel", "activate")
            require(find(tree, "dialog") is None, "dialog still open after cancel")
            require(find(tree, target) is not None, "the modal probe deleted its task")
            require(
                find(tree, "input")["value"] in (None, ""),
                "the modal-blocked draft leaked into the input after cancel",
            )
            ctx["snapshot"] = tree
            races = f", {rejected} race(s) refused by the bridge first" if rejected else ""
            return (
                f"set_value while the dialog was open -> ignored ack + tree "
                f"({replan}){races}"
            )
        raise StepFailure(
            f"never reached the app's modal gate: the bridge refused all "
            f"{rejected} attempts on advertisement"
        )
    finally:
        close_any_dialog(client)


def step_k_key_repeat(client, ctx):
    """key repeat sends exactly that many presses."""
    items = [item["id"] for item in task_items(client.read_tree())]
    require(len(items) >= 3, f"need three visible tasks to count moves, have {items}")
    tree = act(client, items[0], "select")
    require(
        one_focused(tree, "before key repeat") == items[0],
        f"select did not park the cursor on {items[0]}",
    )

    # One short of the list length: every press lands on a distinct row, so a
    # press too few or too many cannot alias back onto the expected one.
    steps = len(items) - 1
    tree = key(client, "down", repeat=steps)
    after = one_focused(tree, "after key repeat")
    require(
        after == items[steps],
        f"key down repeat={steps} moved {items[0]} -> {after}, expected "
        f"{items[steps]} (rows: {items})",
    )
    ctx["snapshot"] = tree
    return f"repeat={steps} moved exactly {steps} rows ({items[0]} -> {after})"


def step_l_bad_key(client, ctx):
    """An unparseable key is refused with the grammar, and changes nothing."""
    before = client.read_tree()
    try:
        client.call_raw("key", {"key": "inx"})
        raise StepFailure("key 'inx' was accepted, expected an invalid_params error")
    except ToolError as err:
        require(
            err.code == INVALID_PARAMS,
            f"bad key error code {err.code}, expected {INVALID_PARAMS}",
        )
        require(
            "unrecognized key `inx`" in err.message and "expected" in err.message,
            f"bad key error does not name the key and the grammar: {err.message[:200]}",
        )
        message = err.message
    after = client.read_tree()
    require(
        after == before,
        f"tree changed after a rejected key: seq {before['seq']} -> {after['seq']}",
    )
    ctx["snapshot"] = after
    return f"rejected at the bridge ({message[:60]}...), tree unchanged"


def step_m_id_stability(client, ctx):
    """Deleting a task leaves every surviving task id exactly as it was."""
    active_before = task_labels(client.read_tree())
    done_before = task_labels(act(client, "tab-done", "select"))
    tree = act(client, "tab-active", "select")
    victim = ctx.get("typed_task") or next(iter(active_before))
    require(victim in active_before, f"{victim} is not on the Active tab")
    require(len(active_before) >= 2, f"need a survivor to check, have {active_before}")

    tree = act(client, victim, "delete")
    require(find(tree, "dialog") is not None, "delete did not open the dialog")
    tree = act(client, "dialog-confirm", "activate")

    active_after = task_labels(tree)
    done_after = task_labels(act(client, "tab-done", "select"))
    act(client, "tab-active", "select")

    require(victim not in active_after, f"{victim} survived its own delete")
    require(
        set(active_after) == set(active_before) - {victim},
        f"Active ids {sorted(active_after)}, expected "
        f"{sorted(set(active_before) - {victim})}",
    )
    require(
        set(done_after) == set(done_before),
        f"Done ids moved: {sorted(done_before)} -> {sorted(done_after)}",
    )
    for node_id, label in list(active_after.items()) + list(done_after.items()):
        was = {**active_before, **done_before}[node_id]
        require(
            label == was,
            f"{node_id} now labels {label!r}, was {was!r}: ids are positional",
        )
    ctx["snapshot"] = client.read_tree()
    return (
        f"deleted {victim}; {len(active_after) + len(done_after)} surviving ids "
        "kept id and label"
    )


def step_n_snapshot_size(client, ctx):
    text = client.call_raw("read_tree")
    size = len(text.encode())
    require(
        size < SNAPSHOT_LIMIT,
        f"compact read_tree JSON is {size} bytes, expected < {SNAPSHOT_LIMIT}",
    )
    json.loads(text)  # and it must still be valid JSON
    return f"{size} bytes < {SNAPSHOT_LIMIT}"


def step_o_shutdown(client, ctx, app):
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

    # Both counters the demo reports once the terminal is restored must be
    # zero for a whole scenario. Either line means the run above silently lost
    # agent input -- inputs the app's queue overflowed on, or inputs discarded
    # because the bridge connection they arrived on ended first -- and every
    # step that passed did so over a hole. Checked here because this is the
    # only point where the demo has printed them and is done writing.
    app.wait_stderr()
    stderr = app.stderr_tail()
    silent_loss = [
        what
        for marker, what in (
            ("taria-demo: dropped ", "dropped inputs (the app's queue overflowed)"),
            (
                "taria-demo: discarded ",
                "discarded inputs (their bridge connection ended first)",
            ),
        )
        if marker in stderr
    ]
    require(
        not silent_loss,
        f"a clean run reported {' and '.join(silent_loss)}; demo stderr:\n{stderr}",
    )

    client.close()
    require(client.proc.poll() is not None, "taria-mcp did not exit on stdin EOF")
    return (
        f"app rc=0, socket removed, no dropped/discarded inputs, bridge exited "
        f"({not_connected[:40]}...)"
    )


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
        ("i type_text adds a task", step_i_type_text),
        ("j modal-blocked act is ignored", step_j_ignored_ack),
        ("k key repeat moves N rows", step_k_key_repeat),
        ("l unparseable key rejected", step_l_bad_key),
        ("m node ids stable across delete", step_m_id_stability),
        ("n snapshot < 8KB", step_n_snapshot_size),
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
                # steps share state; keep going only if the tree still reads,
                # and never leave a modal up for the steps that follow.
                try:
                    close_any_dialog(client)
                    ctx["snapshot"] = client.read_tree(retries=3)
                except Exception:
                    failed_hard = True

        # Shutdown is special: it consumes both processes.
        name = "o clean shutdown"
        if failed_hard:
            results.append((name, False, "skipped: earlier step failed"))
            print(f"FAIL {name}: skipped after earlier failure", flush=True)
        else:
            try:
                evidence = step_o_shutdown(client, ctx, app)
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
