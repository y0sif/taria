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
import re
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

# Where the binaries under test come from. Both scripts build before they run,
# and `--no-build` swaps the build for [`require_fresh_binaries`] rather than
# for trust: a run against a `target/debug` older than these trees tests a
# build nobody asked for, which has already happened twice -- once scoring a
# leftover mutant as the product, and the more dangerous inverse, an edit that
# was never compiled passing green.
SOURCE_ROOTS = ("crates", "examples", "Cargo.toml", "Cargo.lock")
SOURCE_SUFFIXES = (".rs", ".toml", ".lock")

# The five shapes an input-sending tool (act / key / type_text) can answer
# with, as of the v0.1 ack protocol. Matched by prefix, verbatim: an agent
# reads these strings, so a reworded one is a behaviour change this script has
# to notice rather than absorb.
IGNORED_PREFIX = (
    "The app received this input and deliberately did nothing with it "
    "(for example an act a modal dialog blocks or naming a node it no "
    "longer knows, a set_value carrying no value, or text sent while "
    "nothing is accepting typing). Re-plan from the current tree below."
)
NO_CHANGE_PREFIX = "The app received this input, and its tree did not change within"
NO_ACK_PREFIX = "The app neither acknowledged this input nor changed its tree within"
# An input the app took delivery of and did not survive. A result rather than
# an error, because the input's fate is known and an advertised `quit` reaches
# this state on purpose; the shape exists so a working quit is not reported as
# a failed call.
GONE_PREFIX = "The app acknowledged this input and then disconnected:"

# The bridge's report for a burst the app dropped part of, and the demo's own
# tally of the same event once the terminal is restored. The scenario's clean
# run asserts the demo line is *absent*, which proves nothing on its own -- the
# demo prints it only above zero, so a deleted counter would pass too. The
# positive control below drives a second demo into really dropping input and
# holds the two numbers against each other.
BRIDGE_DROP_RE = re.compile(
    r"^the app dropped (?P<dropped>\d+) of the (?P<sent>\d+) inputs this call sent "
    r"because its input queue was full,"
)
DEMO_DROPPED_RE = re.compile(r"^taria-demo: dropped (\d+) agent input\(s\)", re.M)


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
      "gone"      the app acked the input and then disconnected; no tree,
                  because the app it would describe is gone

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
    if text.startswith(GONE_PREFIX):
        return "gone", None
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
        # Both fds are owned from here on, so every exit from this block has
        # to account for them: `finally` for the slave (handed to the child
        # or not, this side is done with it), and the `except` for the master
        # (nothing will ever call `kill` on a half-built object, so this is
        # its only chance to be closed). Two leaked fds per failed launch is
        # enough to exhaust the limit in a script that restarts the app.
        try:
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
        except BaseException:
            os.close(self.master)
            raise
        finally:
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

    def read_tree(self, retries=25, delay=0.2):
        """read_tree with retries while the bridge is still connecting.

        Retryable errors are the bridge's two "no app right now" states:
        never connected ("no snapshot ...") and app went away
        ("... disconnected ..."); both clear once the app (re)connects.

        read_tree sends no input, so it has exactly one successful shape: a
        tree. Any of the input-tool shapes coming back here means read_tree
        grew a behaviour it is not supposed to have.
        """
        last = None
        for _ in range(retries):
            try:
                kind, tree = self.call_outcome("read_tree")
                require(
                    kind == "tree",
                    f"read_tree answered {kind!r}; it sends no input, so a tree "
                    "is its only successful answer",
                )
                return tree
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


# What a step says it expects back from an input-sending tool.
#
# Which of these a call site picks is the whole assertion: the three answers
# an app can give -- a new tree, "acknowledged, nothing changed", "nothing
# came back at all" -- are exactly what v0.1's per-input acknowledgement made
# distinguishable, and a helper that silently re-read the tree on the last two
# erased the difference. So the expectation is named at every call site, and
# anything weaker than TREE has to be asked for.
#
#   TREE  the app must have reacted: a fresh tree, or the step fails. The
#         strongest and the default, because a step that sends an input
#         usually wants its effect.
#   ACK   the app must have acknowledged the input; whether the tree moved is
#         then the step's own business, and a re-read fills in the tree. This
#         is the visible opt-in for a step that tolerates a delayed or absent
#         effect -- selecting the tab that may already be selected, parking a
#         cursor that may already be parked -- and such a step asserts the
#         state it wanted separately. `no_ack` still fails: an app that says
#         nothing at all is not one that had nothing to do.
#   NONE  the app must have acknowledged the input and published nothing: the
#         `no_change` note exactly, never a tree and never silence. This is
#         the only expectation that can see an acknowledgement at all, and it
#         is why the steps using it exist (see below).
#
# There is deliberately no mode that absorbs `no_ack` too. Under the v0.1
# contract an app answers every input it dequeues, so a step written against
# `taria-demo` that shrugged at silence would be tolerating a broken contract
# rather than a slow app.
#
# # Where the acknowledgement is actually observable
#
# On a call whose tree changes, the bridge answers with that tree whether the
# app acked `Delivered` or said nothing at all: the no-ack path is the
# compatibility path for adapters that do not implement acks, and those two
# results are byte-identical by design. So `EXPECT_TREE`, the default and the
# expectation of most calls here, cannot see an ack and is not meant to. The
# contract is testable exactly where no tree change is expected, because
# `no_change` (acked, nothing to do) and `no_ack` (nothing came back) are
# distinct messages there. Three steps assert it outright -- p (re-selecting
# the current tab), q (a key the app has no binding for) and r (a `set_value`
# that sets the value it already holds) -- each reaching it a different way,
# and none of them borrowing its setup from a helper that would go red first.
# scripts/adversarial.py carries the other half: `probe_ack_vs_silence` puts
# the same input to a peer that acks and to one that does not, which is the
# only place the two paths can be compared side by side.
EXPECT_TREE = "tree"
EXPECT_ACK = "ack"
EXPECT_NO_CHANGE = "no_change"


def expect_input(client, tool, args, expect):
    """Send one input and hold the tool to `expect`; returns a tree.

    An `ignored` ack fails every expectation here: the app said it
    deliberately did nothing, and a caller that asked for an effect wants to
    hear that rather than quietly retry a read_tree. Steps that mean to
    provoke an ignore classify the result themselves with `call_outcome`.
    """
    kind, tree = client.call_outcome(tool, args)
    if kind == "ignored":
        raise StepFailure(
            f"{tool} {args!r} was IGNORED by the app; expected it to apply"
        )
    if expect == EXPECT_TREE:
        require(
            kind == "tree",
            f"{tool} {args!r} answered {kind!r}; expected the app to react and "
            "publish a new tree",
        )
        return tree
    if expect == EXPECT_ACK:
        require(
            kind in ("tree", "no_change"),
            f"{tool} {args!r} answered {kind!r}; expected the app to acknowledge "
            "it, whether or not the tree moved",
        )
        return tree if tree is not None else client.read_tree(retries=5)
    if expect == EXPECT_NO_CHANGE:
        require(
            kind == "no_change",
            f"{tool} {args!r} answered {kind!r}; expected the app to acknowledge "
            "an input it had nothing to do about, and to publish nothing",
        )
        return None
    raise StepFailure(f"unknown expectation {expect!r}")


def act(client, node, action, value=None, expect=EXPECT_TREE):
    args = {"node": node, "action": action}
    if value is not None:
        args["value"] = value
    return expect_input(client, "act", args, expect)


def key(client, key_name, repeat=None, expect=EXPECT_TREE):
    args = {"key": key_name}
    if repeat is not None:
        args["repeat"] = repeat
    return expect_input(client, "key", args, expect)


def type_text(client, text, expect=EXPECT_TREE):
    return expect_input(client, "type_text", {"text": text}, expect)


TAB_LABELS = {"tab-active": "Active", "tab-done": "Done"}


def show_tab(client, tab_id):
    """Put one tab on screen and return the tree showing it.

    Selecting the tab that is already selected is a no-op the app still
    dequeues and acknowledges, and which tab a step inherits depends on the
    step before it. So the expectation here is the acknowledgement, and the
    thing the caller actually needs -- this tab on screen -- is asserted
    outright rather than inferred from the tree having changed.
    """
    tree = act(client, tab_id, "select", expect=EXPECT_ACK)
    require(
        find(tree, "tabs")["value"] == TAB_LABELS[tab_id],
        f"selected {tab_id}, but the tabs node reads "
        f"{find(tree, 'tabs').get('value')!r}, expected {TAB_LABELS[tab_id]!r}",
    )
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
    require(
        find(tree, target) is None,
        f"{target} still visible on the Active tab after toggling to done",
    )
    tree = show_tab(client, "tab-done")
    node = find(tree, target)
    require(node is not None, f"{target} not found on the Done tab after toggle")
    require(node["value"] == "done", f"{target} value {node['value']!r}, expected done")

    # Toggle back: value returns to todo (visible again on the Active tab).
    tree = act(client, target, "toggle")
    require(
        find(tree, target) is None,
        f"{target} still on the Done tab after toggling back",
    )
    tree = show_tab(client, "tab-active")
    node = find(tree, target)
    require(node is not None, f"{target} not back on the Active tab")
    require(node["value"] == "todo", f"{target} value {node['value']!r}, expected todo")
    one_focused(tree, "after toggle round-trip")
    ctx["snapshot"] = tree
    return f"{target}: todo -> done -> todo"


def step_e_add_task(client, ctx):
    title = "Bought via e2e"
    tree = act(client, "input", "set_value", value=title)
    node = find(tree, "input")
    require(
        node["value"] == title,
        f"input value {node.get('value')!r} after set_value, expected {title!r}",
    )
    one_focused(tree, "after set_value")

    tree = act(client, "input", "activate")
    added = [n for n in flatten(tree["root"]) if n.get("label") == title]
    # Exactly one, not merely at least one: an `activate` applied twice -- the
    # layer redelivering an input, the demo submitting on both the act and a
    # lowered Enter -- adds the task twice, and a truthiness check would call
    # that a pass. Step i counts the same way for the same reason.
    require(
        len(added) == 1,
        f"expected exactly one node labelled {title!r} after activate, got "
        f"{len(added)}: {[n['id'] for n in added]}",
    )
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
    dialog = find(tree, "dialog")
    require(dialog is not None, "no node id 'dialog' after custom delete action")
    require(find(tree, "dialog-cancel") is not None, "dialog has no dialog-cancel")
    require(one_focused(tree, "dialog open") == "dialog", "dialog should hold focus")

    tree = act(client, "dialog-cancel", "activate")
    require(find(tree, "dialog") is None, "dialog still present after cancel")
    require(
        find(tree, target) is not None,
        "cancel must not delete the task",
    )
    ctx["snapshot"] = tree
    return "dialog opened via custom delete, closed via dialog-cancel"


def step_g_errors(client, ctx):
    # Called through `call_raw` rather than `act`: nothing here is expected to
    # reach the app at all, so there is no result shape to hold the call to.
    try:
        client.call_raw("act", {"node": "no-such-node", "action": "select"})
        raise StepFailure("act on bogus node id succeeded, expected an error")
    except ToolError as err:
        require(
            "valid node ids" in err.message and "tabs" in err.message,
            f"bogus-node error does not list valid ids: {err.message[:200]}",
        )
        bogus_msg = err.message

    # Which actions the input advertises depends on its state -- `activate`
    # only while the draft would submit something, `dismiss` only while it
    # holds the keyboard -- so the error is held against the tree's own list
    # rather than a hard-coded one, and stays a real check whatever state the
    # step before this one left behind.
    advertised = [
        action
        for action in find(client.read_tree(), "input").get("actions", [])
        if isinstance(action, str)
    ]
    require(advertised, "the input advertises nothing to be listed")
    try:
        client.call_raw("act", {"node": "input", "action": "toggle"})
        raise StepFailure("act with unadvertised action succeeded, expected an error")
    except ToolError as err:
        missing = [name for name in advertised if name not in err.message]
        require(
            "advertise" in err.message and not missing,
            f"unadvertised-action error does not list advertised ones "
            f"({missing} missing): {err.message[:200]}",
        )
    return (
        f"bogus id error lists ids ({bogus_msg[:40]}...), "
        "bad action error lists advertised actions"
    )


def step_h_keys(client, ctx):
    before_tab = find(ctx["snapshot"], "tabs")["value"]
    tree = key(client, "tab")
    after_tab = find(tree, "tabs")["value"]
    require(
        after_tab != before_tab,
        f"tabs value still {after_tab!r} after key tab",
    )
    before_focus = one_focused(tree, "before key down")
    tree = key(client, "down")
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
    show_tab(client, "tab-active")  # step h left the Done tab on screen
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
    tree = type_text(client, title + "\n")

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
    # The cursor may already be parked on the first row, so this select is
    # held to its acknowledgement and the parking is asserted below.
    tree = act(client, items[0], "select", expect=EXPECT_ACK)
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
    active_before = task_labels(show_tab(client, "tab-active"))
    done_before = task_labels(show_tab(client, "tab-done"))
    show_tab(client, "tab-active")
    victim = ctx.get("typed_task") or next(iter(active_before))
    require(victim in active_before, f"{victim} is not on the Active tab")
    require(len(active_before) >= 2, f"need a survivor to check, have {active_before}")

    tree = act(client, victim, "delete")
    require(find(tree, "dialog") is not None, "delete did not open the dialog")
    tree = act(client, "dialog-confirm", "activate")

    active_after = task_labels(tree)
    done_after = task_labels(show_tab(client, "tab-done"))
    show_tab(client, "tab-active")

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


def step_o_select_while_typing(client, ctx):
    """`select` moves the cursor even when the keyboard belongs to the input.

    Focus and selection are separate facts. With the input focused, a select
    on a task must not steal the keyboard, so `focused` cannot report it and
    the list's own value is the only thing that can. A tree publishing focus
    alone answers this call with "acknowledged, nothing changed", and the
    agent that just moved the cursor has no way to see that it moved -- which
    is why every other select in this scenario is sent from list focus, where
    focus moves anyway and would cover for a missing value.
    """
    tree = show_tab(client, "tab-active")
    rows = [item["id"] for item in task_items(tree)]
    require(len(rows) >= 2, f"need two visible tasks to move between, have {rows}")
    target = rows[-1]

    # Park the cursor away from the target, then hand the keyboard to the
    # input. `set_value` focuses the input as a side effect, which is how an
    # agent gets there without a raw key.
    tree = act(client, rows[0], "select", expect=EXPECT_ACK)
    require(
        find(tree, "tasks").get("value") == rows[0],
        f"tasks value {find(tree, 'tasks').get('value')!r} after parking the "
        f"cursor on {rows[0]}",
    )
    draft = "typing while the cursor moves"
    tree = act(client, "input", "set_value", value=draft)
    require(
        one_focused(tree, "before the select") == "input",
        "set_value must leave the input focused",
    )

    tree = act(client, target, "select")
    require(
        find(tree, "tasks").get("value") == target,
        f"select moved the cursor to {target}, but the tasks node names "
        f"{find(tree, 'tasks').get('value')!r}",
    )
    require(
        one_focused(tree, "after the select") == "input",
        "select stole the keyboard from the input",
    )
    require(
        find(tree, "input")["value"] == draft,
        f"the draft became {find(tree, 'input').get('value')!r} during a select",
    )

    # `esc` is the demo's way out of the input: it clears the draft and hands
    # the keyboard back to the list, which the shutdown step needs (`q` typed
    # into a focused input is a character, not a quit).
    tree = key(client, "esc")
    require(
        find(tree, "input")["value"] in (None, ""),
        "esc did not clear the draft",
    )
    require(
        one_focused(tree, "after esc") != "input",
        "esc did not hand the keyboard back to the list",
    )
    ctx["snapshot"] = tree
    return f"cursor moved {rows[0]} -> {target} with the keyboard in the input"


def step_p_ack_without_change(client, ctx):
    """The app acknowledges an input it deliberately does nothing about.

    This is the shape v0.1 added, and the one most of this scenario cannot
    distinguish: `no_change` says the app dequeued the input, looked at it and
    had nothing to do, while `no_ack` says nothing came back at all. An agent
    tells "done, nothing to see" from "still busy, ask again" by exactly that
    difference, so a step that accepted either would assert nothing. The other
    half of the contract -- the `Ignored` refinement -- is step j.

    The re-selected tab is whichever one is on screen right now, read out of
    the tree rather than put there by `show_tab`. The helper takes either
    answer and re-reads, so setting the scene with it made this step's own
    assertion reachable only when the tab happened to be selected already:
    deleting the app's acks failed the helper on a different call, and
    reordering an earlier step would have left this one asserting nothing at
    all. Nothing here depends on a helper or on the step before it.
    """
    before = client.read_tree()
    require(find(before, "dialog") is None, "a modal is open; the tab acts are gated")
    label = find(before, "tabs").get("value")
    current = [tab_id for tab_id, name in TAB_LABELS.items() if name == label]
    require(
        len(current) == 1,
        f"the tabs node reads {label!r}, which names none of {sorted(TAB_LABELS)}",
    )
    tab_id = current[0]

    expect_input(
        client, "act", {"node": tab_id, "action": "select"}, EXPECT_NO_CHANGE
    )
    # And it really did nothing: an app that redrew something would have
    # published a new tree rather than the no-change note.
    after = client.read_tree()
    require(
        after == before,
        f"the tree moved under a no-change answer: seq {before['seq']} -> "
        f"{after['seq']}",
    )
    ctx["snapshot"] = after
    return f"re-selecting {tab_id} (already current): acknowledged, no tree change"


def step_q_unbound_key_acked(client, ctx):
    """A key the app has no binding for is acknowledged, not swallowed.

    The second way into the observable half of the ack contract, and the one
    that needs no state at all: `f1` parses in the shared grammar, lowers to a
    real crossterm event, reaches the demo's key handler and falls off the end
    of its match. The app has looked at the press and has nothing to do, which
    is `no_change`; silence would mean the press vanished somewhere between
    the bridge and the handler, and a tree would mean it did something.

    Deliberately not a key the demo binds and not a key the grammar rejects:
    the first would change the tree, the second never leaves the bridge.
    """
    before = client.read_tree()
    require(find(before, "dialog") is None, "a modal is open; it eats every key")
    expect_input(client, "key", {"key": "f1"}, EXPECT_NO_CHANGE)
    after = client.read_tree()
    require(
        after == before,
        f"an unbound key moved the app: seq {before['seq']} -> {after['seq']}",
    )
    ctx["snapshot"] = after
    return "key f1 (bound to nothing): acknowledged, no tree change"


def step_r_idempotent_set_value(client, ctx):
    """Setting the value the input already holds is acked, and publishes
    nothing.

    The third way in, and the only one that goes through `act` with a payload:
    the app applies the value, the frame it draws is identical to the last
    one, and the layer dedupes it. So the input was handled -- `Handled`, not
    `Ignored` -- and still nothing is published, which is the case an agent
    most easily mistakes for a stalled app.
    """
    draft = "already what the input holds"
    before = client.read_tree()
    require(
        find(before, "input").get("value") != draft,
        f"the input already reads {draft!r}, so the first set_value below "
        "would be the no-op instead of the second",
    )

    restored = False
    try:
        tree = act(client, "input", "set_value", value=draft)
        require(
            find(tree, "input")["value"] == draft,
            f"could not seed the draft: input is {find(tree, 'input').get('value')!r}",
        )
        require(
            one_focused(tree, "before the repeat set_value") == "input",
            "set_value must leave the input focused",
        )

        expect_input(
            client,
            "act",
            {"node": "input", "action": "set_value", "value": draft},
            EXPECT_NO_CHANGE,
        )
        live = client.read_tree()
        require(
            live == tree,
            f"the tree moved under a no-change answer: seq {tree['seq']} -> "
            f"{live['seq']}",
        )

        # `esc` is the demo's way out of the input: it clears the draft and
        # hands the keyboard back to the list, which the shutdown step needs.
        tree = key(client, "esc")
        require(
            find(tree, "input")["value"] in (None, ""), "esc did not clear the draft"
        )
        require(
            one_focused(tree, "after esc") != "input",
            "esc did not hand the keyboard back to the list",
        )
        ctx["snapshot"] = tree
        restored = True
    finally:
        # Whatever happened above, do not hand the shutdown step an app whose
        # keyboard belongs to the input: `q` typed there is the letter q, not
        # a quit, and the step would report the app as still running -- a
        # second failure that says nothing about the app and buries the first.
        # Sent raw and unchecked, because this only runs on the failure path.
        if not restored:
            try:
                client.call_raw("key", {"key": "esc"})
            except (ToolError, StepFailure, TimeoutError):
                pass
    return f"set_value repeating {draft!r}: acknowledged, no tree change"


def step_s_focus_then_type(client, ctx):
    """`focus` takes the keyboard without editing the draft, then one
    type_text fills it.

    The aiming path in the order an agent plans it: read the tree, see the
    input advertise a way in, take the keyboard, type the title, submit.
    Until `focus` was advertised the only semantic way in was `set_value`,
    which moves the keyboard as a side effect of replacing the draft. A live
    agent driving this demo looked for a way to aim at the input, found none,
    tried `set_value` with an empty string to clear itself a way in, and in
    the end let `set_value` carry the whole title -- which also means the
    headline `type_text` call never ran in that session. This step gates the
    two claims that workaround hid: that aiming is not an edit, and that the
    title can ride one `type_text`.

    The demo cannot hold a draft while the keyboard is on the list (`dismiss`
    and a submit both clear it, and `set_value` takes the keyboard with it),
    so the byte-identity assertion below is on the draft this step really
    finds, an empty one. The half-typed case is
    `focus_takes_the_keyboard_without_touching_the_draft` in
    examples/demo-app/src/update.rs, in process.

    The second `focus` is pipelined behind the first the way step j pipelines
    behind the dialog: once the app has published the tree the first one
    produced, the input no longer advertises `focus` and the bridge refuses
    the call on advertisement rather than handing it over. Losing that race
    is the bridge doing its job, so it is retried rather than failed; winning
    it is the only way to hear the app's own verdict on an action that would
    now do nothing.
    """
    title = "Typed after aiming: d j k q y i"

    # Whatever state this step inherits, the keyboard has to be on the list
    # for there to be anything to aim, and the Active tab on screen for the
    # task map below to mean anything. `dismiss` is the advertised way back,
    # so the precondition costs no raw key.
    tree = client.read_tree()
    require(find(tree, "dialog") is None, "a modal is open; it gates every act here")
    if one_focused(tree, "the state this step inherits") == "input":
        tree = act(client, "input", "dismiss")
    tree = show_tab(client, "tab-active")
    holder = one_focused(tree, "before aiming")
    require(
        holder != "input",
        "could not get the keyboard off the input, so there is nothing to aim",
    )
    before = task_labels(tree)
    require(
        title not in before.values(),
        f"a task labelled {title!r} exists before typing it",
    )

    # What an agent reads before it aims: a way in, and no way back out of a
    # keyboard the input does not hold.
    node = find(tree, "input")
    require(
        has_action(node, "focus"),
        f"the keyboard is on {holder} and the input advertises "
        f"{node.get('actions')}: nothing there aims at it",
    )
    require(
        not has_action(node, "dismiss"),
        f"the input advertises `dismiss` with the keyboard on {holder}: "
        f"{node.get('actions')}",
    )
    draft = node.get("value")

    refused = 0
    restored = False
    try:
        for _ in range(5):
            ids = [
                client.send_call("act", {"node": "input", "action": "focus"}),
                client.send_call("act", {"node": "input", "action": "focus"}),
            ]
            responses = client.collect(ids)

            kind, tree = parse_result(response_text(responses[ids[0]]))
            require(
                kind == "tree",
                f"the focus act answered {kind!r}; expected the app to take the "
                "keyboard and publish it",
            )
            aimed = one_focused(tree, "after the focus act")
            require(aimed == "input", f"focus left the keyboard on {aimed}")
            node = find(tree, "input")
            require(
                node.get("value") == draft,
                f"focus changed the draft from {draft!r} to "
                f"{node.get('value')!r}: aiming is not an edit",
            )
            require(
                has_action(node, "dismiss"),
                f"focus took the keyboard and the input advertises "
                f"{node.get('actions')}: no advertised way back out",
            )

            try:
                text = response_text(responses[ids[1]])
            except ToolError as err:
                # The bridge saw the tree the first act produced and refused on
                # advertisement, which is the same claim from the other side.
                require(
                    "does not advertise" in err.message,
                    f"unexpected rejection of the second focus: "
                    f"{err.message[:160]}",
                )
                refused += 1
                act(client, "input", "dismiss")
                continue

            kind, tree = parse_result(text)
            require(
                kind == "ignored",
                f"a second focus on the input that already holds the keyboard "
                f"came back {kind!r}, expected the ignored shape: {text[:160]}",
            )
            node = find(tree, "input")
            require(
                one_focused(tree, "the ignored ack's tree") == "input",
                "the ignored ack's tree does not show the input focused, so it "
                "cannot explain why the second focus did nothing",
            )
            require(
                not has_action(node, "focus"),
                f"the ignored ack's tree still advertises `focus` on an input "
                f"that holds the keyboard: {node.get('actions')}",
            )
            break
        else:
            raise StepFailure(
                f"never reached the app's verdict on a second focus: the bridge "
                f"refused all {refused} attempts on advertisement"
            )

        # One call types the whole title, and the draft is read before anything
        # submits it: every character has to be in the draft, because one that
        # went through the list's bindings instead would be missing from it.
        tree = type_text(client, title)
        node = find(tree, "input")
        require(
            node.get("value") == title,
            f"one type_text left the draft reading {node.get('value')!r}, "
            f"expected {title!r}",
        )
        require(
            find(tree, "dialog") is None,
            "type_text opened the delete dialog: the `d` in the title went "
            "through the list's bindings instead of into the draft",
        )

        # `activate` rather than the trailing newline step i submits with: the
        # newline is already gated there, and splitting the submit off is what
        # makes the draft above readable at all. The act also has to pass the
        # bridge's advertisement check, so it doubles as the assertion that
        # `activate` appeared once the draft would submit something.
        tree = act(client, "input", "activate")
        added = [item for item in task_items(tree) if item["id"] not in before]
        require(
            len(added) == 1,
            f"expected exactly one new task after submitting, got "
            f"{[item['id'] for item in added]}",
        )
        require(
            added[0].get("label") == title,
            f"the new task reads {added[0].get('label')!r}, expected {title!r}",
        )
        # Nothing else moved: a character that reached the list's bindings would
        # show here rather than in the draft -- ` ` toggles the selected task
        # onto the other tab, `d` opens the delete dialog and `y` confirms it,
        # and either way a task leaves this map.
        require(
            task_labels(tree) == {**before, added[0]["id"]: title},
            f"the Active tab reads {task_labels(tree)}, expected {before} plus "
            f"the one new task",
        )
        ctx["snapshot"] = tree
        restored = True
    finally:
        # This step takes the keyboard on purpose and only the submit hands it
        # back, so a failure in between would leave it in the input -- where
        # the shutdown step's `q` is the letter q rather than a quit, and that
        # step would report an app that is still running, burying this
        # failure under one that says nothing. Same guard, same reason, as
        # step r; sent raw and unchecked, because it only runs on that path.
        if not restored:
            try:
                client.call_raw("key", {"key": "esc"})
            except (ToolError, StepFailure, TimeoutError):
                pass
    races = f", {refused} race(s) refused by the bridge first" if refused else ""
    return (
        f"focus took the keyboard with the draft untouched, a second focus was "
        f"ignored{races}; one type_text created {added[0]['id']} labelled "
        f"{title!r}"
    )


def step_t_shutdown(client, ctx, app):
    # `q` quits, so the app is gone before this call can answer. The answer
    # has to say that: an input that ended the app reported as "the tree did
    # not change" tells the agent the app is idle while it is in fact gone.
    #
    # Two honest answers, decided by whether the app's ack for this key got
    # out before it exited. An acknowledged input is a *result*: its fate is
    # known, and quitting on purpose is the ordinary way to reach this state,
    # so flagging it as an error made a working quit read as a failed call.
    # An unacknowledged one stays an error, because nobody can say whether it
    # landed. Neither may read as "the tree did not change".
    try:
        kind, _ = client.call_outcome("key", {"key": "q"})
        require(
            kind == "gone",
            f"the key that quit the app answered {kind!r}, as if it were still there",
        )
        quit_answer = "result: acked, then gone"
    except ToolError as err:
        require(
            "disconnected before acknowledging this input" in err.message
            and "may have exited" in err.message,
            f"unexpected report from the quitting key: {err.message[:200]}",
        )
        quit_answer = "error: gone before acking"

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

    # Every counter the demo reports once the terminal is restored must be
    # zero for a whole scenario. Any of these lines means the run above
    # silently lost agent input -- inputs the app's queue overflowed on,
    # inputs discarded because the bridge connection they arrived on ended
    # first, or inputs whose kind this build of taria could not read -- and
    # every step that passed did so over a hole. Checked here because this is
    # the only point where the demo has printed them and is done writing.
    #
    # An absence proves nothing by itself: the demo prints each line only when
    # its count is above zero, so a deleted counter would satisfy this too.
    # [`step_u_dropped_counter`] is the positive control -- it drives a demo
    # of its own into really dropping input and holds the printed count
    # against the bridge's tally of the same drops.
    # A join that timed out leaves the drain mid-stream, so the absence
    # assertion below would be run against whatever happened to have arrived
    # -- possibly nothing at all -- and pass for the wrong reason. The point
    # of the wait is that the lines cannot still be in flight.
    require(
        app.wait_stderr(),
        "taria-demo's stderr never reached EOF, so its dropped/discarded "
        "counts may not have been written yet and their absence proves "
        "nothing",
    )
    stderr = app.stderr_tail()
    silent_loss = [
        what
        for marker, what in (
            ("taria-demo: dropped ", "dropped inputs (the app's queue overflowed)"),
            (
                "taria-demo: discarded ",
                "discarded inputs (their bridge connection ended first)",
            ),
            (
                "taria-demo: could not read ",
                "unreadable inputs (a taria version gap the demo cannot act across)",
            ),
            (
                "taria-demo: lost the answer to ",
                "unanswered inputs (the bridge read the app's acks too slowly)",
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
        f"({not_connected[:40]}...); quitting key answered {quit_answer}"
    )


def step_u_dropped_counter(ctx):
    """Positive control for the counter the shutdown step asserts is silent.

    "No dropped-input line in the demo's stderr" is an assertion about a line
    the demo prints only when its count is above zero, so on its own it holds
    just as well for a demo that lost the counter entirely, or for a layer
    that stopped counting. This step makes the line appear on purpose and
    reads the number in it.

    Its own demo and its own bridge: the scenario's app has to finish clean,
    and dropping input into it would turn the shutdown step's assertion into
    the failure it is meant to catch.

    Overflow is provoked rather than timed. The layer's input queue holds 256
    and the demo drains it around a 50ms tick, so a round of six pipelined
    64-press bursts puts 384 inputs in front of it inside one tick; rounds are
    repeated until the bridge reports a drop, which also gives the exact tally
    to hold the demo's number against. One round stays under the bridge's ack
    channel too (384 acks against 512 slots), so the tally cannot quietly be
    a floor.
    """
    rounds, per_round, repeat = 0, 6, 64
    tmpdir = tempfile.mkdtemp(prefix="taria-e2e-drops-")
    sock = os.path.join(tmpdir, "drops.sock")
    app = None
    client = None
    try:
        app = PtyApp(sock)
        require(app.wait_for_socket(), f"the flood demo never bound {sock}")
        client = McpClient(sock)
        client.initialize()
        client.read_tree()

        # What the bridge itself saw dropped, summed over every call. Only
        # comparable to the demo's tally when every call gave a definite
        # answer: a call that lost acks to the bridge's channel, or one that
        # timed out waiting for them, knows less than it sent.
        reported = 0
        definite = True
        while rounds < 8 and reported == 0:
            rounds += 1
            ids = [
                client.send_call("key", {"key": "down", "repeat": repeat})
                for _ in range(per_round)
            ]
            for resp in client.collect(ids, timeout=30.0).values():
                try:
                    kind, _ = parse_result(response_text(resp))
                except ToolError as err:
                    match = BRIDGE_DROP_RE.match(err.message)
                    if match:
                        reported += int(match["dropped"])
                    else:
                        definite = False
                    continue
                if kind == "no_ack":
                    definite = False
        sent = rounds * per_round * repeat
        require(
            reported > 0,
            f"{sent} inputs across {rounds} rounds never overflowed the app's "
            "256-slot queue, so this control proved nothing about the counter",
        )

        # Quit the way the scenario does, so the demo restores the terminal
        # and gets to print. The call itself cannot answer: the app is gone.
        try:
            client.call_raw("key", {"key": "q"})
        except ToolError:
            pass
        try:
            app.proc.wait(timeout=READ_TIMEOUT)
        except subprocess.TimeoutExpired:
            raise StepFailure("the flood demo did not exit after key q")
        require(
            app.proc.returncode == 0,
            f"the flood demo exited with {app.proc.returncode}",
        )
        require(
            app.wait_stderr(),
            "the flood demo's stderr never reached EOF, so its dropped count "
            "may not have been written yet",
        )
        stderr = app.stderr_tail()
        match = DEMO_DROPPED_RE.search(stderr)
        require(
            match,
            f"the app dropped {reported} input(s) and printed no dropped-input "
            f"line; demo stderr:\n{stderr}",
        )
        counted = int(match.group(1))
        require(
            0 < counted <= sent,
            f"the demo reports {counted} dropped of {sent} sent",
        )
        require(
            counted >= reported,
            f"the demo counted {counted} drops but the bridge was told about "
            f"{reported}: the app's tally cannot be the smaller of the two",
        )
        if definite:
            require(
                counted == reported,
                f"the demo counted {counted} drops, the bridge was told about "
                f"{reported}, and every call answered definitively: the two "
                "tallies are the same event and have to agree",
            )
        return (
            f"{sent} inputs in {rounds} round(s) -> demo reports {counted} "
            f"dropped, bridge reported {reported}"
            + ("" if definite else " (a floor: some call lost acks)")
        )
    finally:
        if client is not None:
            client.close()
            if client.proc.poll() is None:
                client.proc.kill()
        if app is not None:
            app.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)


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


def newest_source():
    """The most recently modified file the binaries under test are built from.

    Returns `(path, mtime)`, or `(None, 0.0)` if nothing matched, which is
    treated as "cannot tell" rather than "up to date".
    """
    newest, newest_at = None, 0.0
    for root in SOURCE_ROOTS:
        path = os.path.join(REPO, root)
        if os.path.isfile(path):
            candidates = [path]
        else:
            candidates = [
                os.path.join(dirpath, name)
                for dirpath, _, names in os.walk(path)
                for name in names
                if name.endswith(SOURCE_SUFFIXES)
            ]
        for candidate in candidates:
            try:
                mtime = os.path.getmtime(candidate)
            except OSError:
                continue
            if mtime > newest_at:
                newest, newest_at = candidate, mtime
    return newest, newest_at


def require_fresh_binaries():
    """Refuse to run against a `target/debug` older than the sources.

    The `--no-build` escape hatch is for a caller that has just built, not for
    trusting whatever is lying around: a stale binary makes every result in
    this script a report about a build nobody asked for. Both directions have
    bitten -- a leftover mutant scored as the product, and the worse inverse,
    an edit that was never compiled passing green.
    """
    missing = [binary for binary in (DEMO_BIN, MCP_BIN) if not os.path.exists(binary)]
    if missing:
        print(
            f"FAIL build: --no-build was passed but {', '.join(missing)} "
            "do(es) not exist; run cargo build --workspace",
            flush=True,
        )
        sys.exit(1)
    source, source_at = newest_source()
    if source is None:
        print("FAIL build: found no sources to date the binaries against", flush=True)
        sys.exit(1)
    # The *newest* binary, not each one: a build relinks only what changed, so
    # editing the adapter leaves taria-mcp's timestamp where it was and
    # comparing binaries one by one would refuse a perfectly fresh
    # target/debug. What the newest one dates is the last build, and a last
    # build older than the newest source is the state this guard exists for.
    built_at = max(os.path.getmtime(binary) for binary in (DEMO_BIN, MCP_BIN))
    if built_at < source_at:
        print(
            f"FAIL build: the last build in target/debug predates "
            f"{os.path.relpath(source, REPO)} by "
            f"{source_at - built_at:.0f}s; the binaries under test are not "
            "this source tree. Run cargo build --workspace, or drop "
            "--no-build.",
            flush=True,
        )
        sys.exit(1)


def main():
    if "--no-build" in sys.argv:
        require_fresh_binaries()
    else:
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
        ("o select moves the cursor while typing", step_o_select_while_typing),
        ("p re-select of the current tab acked, no change", step_p_ack_without_change),
        ("q unbound key acked, no change", step_q_unbound_key_acked),
        ("r repeated set_value acked, no change", step_r_idempotent_set_value),
        ("s focus aims at the input, then one type_text", step_s_focus_then_type),
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
        name = "t clean shutdown"
        if failed_hard:
            results.append((name, False, "skipped: earlier step failed"))
            print(f"FAIL {name}: skipped after earlier failure", flush=True)
        else:
            try:
                evidence = step_t_shutdown(client, ctx, app)
                results.append((name, True, evidence))
                print(f"PASS {name}: {evidence}", flush=True)
            except (StepFailure, ToolError, TimeoutError) as err:
                results.append((name, False, str(err)))
                print(f"FAIL {name}: {err}", flush=True)

        # The positive control for the counters step t asserts are silent.
        # Its own demo and its own bridge, so it neither needs the scenario's
        # app nor cares that the shutdown step just consumed it -- which is
        # also why an earlier failure does not skip it.
        name = "u dropped-input counter reports drops"
        try:
            evidence = step_u_dropped_counter(ctx)
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
