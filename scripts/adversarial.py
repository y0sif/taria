#!/usr/bin/env python3
"""Adversarial probes against the taria v0.1 vertical slice.

Reuses the harness from e2e.py (pty app + MCP client) and pokes at edge
cases: acting while a modal dialog is open, rapid-fire acts, out-of-range and
malformed arguments, killing the app mid-session, and app restart/reconnect.
Each probe prints PASS/FAIL (FAIL = hang, panic, or wrong-state result) plus
an INFO line for observed-but-debatable behavior.

Some states the demo cannot be driven into honestly -- a peer on another
protocol version, a snapshot using a role this build has never heard of, an
app that drops half a burst, an app that stops reading its socket. Those are
probed against [`FakeApp`], which speaks the ndjson wire directly, so the
bridge under test is the real one and only the app is a stand-in.

Usage:  python3 scripts/adversarial.py   (expects target/debug binaries built)
"""

import contextlib
import json
import os
import re
import shutil
import socket
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from e2e import (  # noqa: E402
    EXPECT_ACK,
    INVALID_PARAMS,
    McpClient,
    PtyApp,
    StepFailure,
    ToolError,
    act,
    close_any_dialog,
    deletable_task,
    find,
    flatten,
    key,
    one_focused,
    parse_result,
    require,
    response_text,
    task_items,
)

READ_TIMEOUT = 5.0
MODAL_DRAFT = "typed while modal"

# --- A stand-in app, for states the demo cannot be driven into ---------------

# The bridge's own protocol version, and one the bridge cannot speak. Hard
# coded rather than derived: the mismatch error names both numbers verbatim,
# so a PROTOCOL_VERSION bump has to be a deliberate edit here, not something
# the script quietly follows.
BRIDGE_PROTOCOL = 1
OTHER_PROTOCOL = 2

# Verbatim strings the bridge answers with. An agent reads these, so a
# reworded one is a behaviour change to notice rather than absorb.
VERSION_MISMATCH_ERROR = (
    f"the app speaks taria protocol version {OTHER_PROTOCOL}, this bridge speaks "
    f"{BRIDGE_PROTOCOL}: the app cannot parse input from this bridge, so nothing was "
    "sent and no input tool will work against it. Match the app's taria dependency "
    "to the bridge's version. read_tree keeps working for as long as the app's "
    "snapshots still parse, which a mismatch does not guarantee."
)
QUEUE_FULL_ERROR = (
    "the app is not accepting input: the bridge's queue to it stayed full for 500ms, "
    "so this input was not sent. The app is stopped or not reading its socket; retry "
    "once it is responsive."
)


def partial_drop_error(dropped, sent):
    landed = sent - dropped
    return (
        f"the app dropped {dropped} of the {sent} inputs this call sent because its "
        f"input queue was full, so at most {landed} landed and the effect is partial. "
        "Send fewer inputs, or wait for each call to return before sending the next; "
        "slower input gets through."
    )


# A burst the bridge could not finish handing over. The counts are whatever
# the kernel's socket buffer happened to hold, so they are captured rather
# than spelled out; every other word is verbatim, and the bracketed drop
# clause must be absent when the app acked nothing. The verbs agree with the
# counts they follow, so a burst of one is not reported as "1 ... were sent".
PARTIAL_SEND_RE = re.compile(
    r"^the app stopped accepting input partway through this call: (?P<sent>\d+) of the "
    r"(?P<wanted>\d+) inputs (?:was|were) sent and may already have taken effect, and "
    r"the remaining (?P<unsent>\d+) (?:was|were) not sent, so the effect is partial\."
    r"(?P<dropped> Of the (?P=sent) sent, the app dropped (?P<n>\d+) because its input "
    r"queue was full\.)? Call read_tree to see what landed, then retry the rest once "
    r"the app is responsive\.$"
)

# The bridge's ack channel fell behind while a call was in flight. `lost` is a
# function of how fast the reader outran the observer, so it is captured; the
# `{known}` clause has exactly two shapes and both are spelled out. The noun
# and the verb agree with the counts they follow, so a one-input call reads
# "the 1 input it sent" rather than "the 1 inputs it sent".
LOST_ACKS_RE = re.compile(
    r"^the bridge lost (?P<lost>\d+) of the app's acknowledgements while this call was "
    r"in flight, so what became of the (?P<sent>\d+) inputs? it sent cannot be reported "
    r"in full: (?P<known>they may or may not have been applied|at least (?P<dropped>\d+)"
    r" of them (?:was|were) dropped because the app's input queue was full, and the "
    r"rest may or may not have been applied)\. Call read_tree to see what actually "
    r"landed, and send fewer inputs per call so the answers can be read as fast as "
    r"they arrive\.$"
)


class FakeApp:
    """An app that speaks the taria ndjson wire directly.

    Binds the socket, serves one bridge connection, sends the handshake, and
    then does exactly what the probe tells it to: publish a snapshot, ack an
    input, or -- with `read_inputs=False` -- never read its socket at all.

    It exists for the four states `taria-demo` cannot be put into on demand:
    another protocol version, a snapshot using a role and an action this
    build has never heard of, an app that drops part of one burst, and an app
    that stops reading. In every one of them the bridge under test is the
    real binary; only the peer is a stand-in.
    """

    def __init__(self, path, protocol_version=BRIDGE_PROTOCOL, read_inputs=True):
        self.path = path
        self.protocol_version = protocol_version
        self.read_inputs = read_inputs
        self.conn = None
        self._lines = []
        self._malformed = []
        self._lock = threading.Lock()
        self._connected = threading.Event()
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(path)
        self.srv.listen(1)
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self):
        try:
            conn, _ = self.srv.accept()
        except OSError:
            return  # closed during teardown
        self.conn = conn
        self.send(
            {
                "type": "hello",
                "app_label": "fake-app",
                "protocol_version": self.protocol_version,
            }
        )
        self._connected.set()
        if not self.read_inputs:
            # Deliberately leave the socket unread: this is the "app stopped
            # or not reading its socket" state the bridge's queue-full error
            # describes, and the only way to reach it without a real app to
            # stop.
            return
        buf = b""
        while True:
            try:
                chunk = conn.recv(65536)
            except OSError:
                return
            if not chunk:
                return
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                if not line.strip():
                    continue
                # This runs on a daemon thread, where an exception kills the
                # thread and nothing else: the probe waiting on `wait_inputs`
                # would then time out with a message about a missing input
                # rather than about the line the bridge actually wrote. Keep
                # the line instead and let [`malformed`] fail the probe with
                # it in hand.
                try:
                    parsed = json.loads(line)
                except json.JSONDecodeError:
                    with self._lock:
                        self._malformed.append(line[:400])
                    continue
                with self._lock:
                    self._lines.append(parsed)

    def send(self, msg):
        self.conn.sendall((json.dumps(msg) + "\n").encode())

    def wait_connected(self, timeout=READ_TIMEOUT):
        return self._connected.wait(timeout)

    def snapshot(self, seq, root):
        """Publish one tree, stamped with this app's protocol version."""
        self.send(
            {
                "type": "snapshot",
                "protocol_version": self.protocol_version,
                "seq": seq,
                "root": root,
            }
        )

    def ack(self, input_id, status):
        self.send({"type": "ack", "id": input_id, "status": status})

    def lines(self):
        """Every message received from the bridge, in arrival order."""
        with self._lock:
            return list(self._lines)

    def malformed(self):
        """Lines from the bridge this app could not parse as JSON.

        Non-empty means the wire carried something the protocol does not
        describe, which is a finding rather than a reason for the reader
        thread to disappear.
        """
        with self._lock:
            return list(self._malformed)

    def inputs(self):
        return [msg for msg in self.lines() if msg.get("type") == "input"]

    def wait_inputs(self, count, timeout=READ_TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if len(self.inputs()) >= count:
                return True
            time.sleep(0.02)
        return False

    def close(self):
        for sock in (self.srv, self.conn):
            if sock is not None:
                try:
                    sock.close()
                except OSError:
                    pass


# A minimal tree: one advertised action is all `act` needs to get past the
# bridge's validation and reach whatever the probe is actually testing.
FAKE_ROOT = {
    "id": "root",
    "role": "app",
    "focused": False,
    "children": [
        {
            "id": "btn",
            "role": "button",
            "label": "Save",
            "focused": True,
            "actions": ["activate"],
        }
    ],
}

# The same tree with one node using a role and two actions no build of this
# bridge has ever heard of. Both are nested inside the snapshot, so before
# the vocabularies grew their fallbacks a single such leaf cost the whole
# tree: the bridge skipped every line as malformed and went on serving its
# last good snapshot, and the app looked frozen with no error anywhere.
# Kept as literal JSON-shaped data because the probe now asserts the agent
# reads these values back exactly, not a round trip through the bridge's own
# types.
UNKNOWN_VOCAB_ROOT = {
    "id": "root",
    "role": "app",
    "focused": False,
    "children": [
        {
            "id": "chart",
            "role": "sparkline",
            "label": "cpu",
            "focused": False,
            "actions": ["zoom", {"set_range": {"from": 1, "to": 9}}],
        },
        {
            "id": "btn",
            "role": "button",
            "label": "Save",
            "focused": True,
            "actions": ["activate"],
        },
    ],
}


@contextlib.contextmanager
def fake_session(**kwargs):
    """A real bridge wired to a [`FakeApp`], torn down together.

    Its own socket and its own taria-mcp process, so a probe using one cannot
    disturb the demo the other probes share.
    """
    tmpdir = tempfile.mkdtemp(prefix="taria-fake-")
    app = None
    client = None
    try:
        app = FakeApp(os.path.join(tmpdir, "fake.sock"), **kwargs)
        client = McpClient(app.path)
        client.initialize()
        require(app.wait_connected(), "the bridge never connected to the fake app")
        yield client, app
        # Only on the success path: a probe's own failure is the better
        # report, and this one would otherwise hide it.
        require(
            not app.malformed(),
            f"the bridge wrote lines the wire format does not describe: "
            f"{app.malformed()[:3]}",
        )
    finally:
        if client is not None:
            client.close()
            if client.proc.poll() is None:
                client.proc.kill()
        if app is not None:
            app.close()
        shutil.rmtree(tmpdir, ignore_errors=True)


# --- Probes ------------------------------------------------------------------


def probe_act_while_dialog_open(client, ctx):
    """Modal enforcement, at both layers.

    Advertisement: while the delete dialog is open, non-dialog nodes
    advertise no actions, so the bridge REJECTS acts on them with a
    'does not advertise' ToolError.

    Behaviour: an act the bridge validated before the dialog existed still
    reaches the app, and the app answers it with an explicit Ignored ack --
    the 'deliberately did nothing' note plus a tree -- instead of swallowing
    it. Getting that ack is the point: an agent waiting on the effect of a
    blocked act stops waiting only because the app says so, and the tree it
    is told to re-plan from has to show the dialog that blocked the act.
    """
    info = []
    try:
        task = deletable_task(client.read_tree())
        tree = act(client, task, "delete")
        require(find(tree, "dialog") is not None, "dialog did not open")

        for action, value in (("set_value", MODAL_DRAFT), ("activate", None)):
            args = {"node": "input", "action": action}
            if value is not None:
                args["value"] = value
            try:
                # `call_raw`: nothing is expected to reach the app, so there
                # is no result shape to hold the call to.
                client.call_raw("act", args)
            except ToolError as err:
                require(
                    "does not advertise" in err.message,
                    f"unexpected rejection for {action} while modal: "
                    f"{err.message[:120]}",
                )
                info.append(f"{action} rejected at the bridge")
                continue
            # No exception: the bridge forwarded (or silently accepted) the
            # act despite the modal dialog -> modality is not enforced.
            raise StepFailure(
                f"act input {action} while dialog open was ACCEPTED, expected "
                "a 'does not advertise' rejection"
            )

        tree = client.read_tree()
        node = find(tree, "input")
        require(
            node.get("actions", []) == [],
            f"input still advertises {node.get('actions')} while dialog open",
        )
        require(
            find(tree, "dialog") is not None,
            "dialog closed during rejected acts",
        )
        require(
            one_focused(tree, "while modal") == "dialog",
            "dialog should hold focus while open",
        )
        act(client, "dialog", "dismiss")

        # Now past the bridge's guard: both calls are written back to back, so
        # the second is validated against the tree from before the dialog
        # existed and only the app can refuse it.
        ignored = None
        for _ in range(5):
            ids = [
                client.send_call("act", {"node": task, "action": "delete"}),
                client.send_call(
                    "act",
                    {"node": "input", "action": "set_value", "value": MODAL_DRAFT},
                ),
            ]
            responses = client.collect(ids)
            opened = parse_result(response_text(responses[ids[0]]))[1]
            require(
                opened is not None and find(opened, "dialog") is not None,
                "delete did not open the dialog",
            )
            try:
                text = response_text(responses[ids[1]])
            except ToolError as err:
                require(
                    "does not advertise" in err.message,
                    f"unexpected rejection while modal: {err.message[:120]}",
                )
                act(client, "dialog", "dismiss")  # lost the race; try again
                continue
            kind, ignored = parse_result(text)
            require(
                kind == "ignored",
                f"act the modal blocks came back as {kind!r}, expected an "
                f"explicit ignored ack: {text[:160]}",
            )
            require(ignored is not None, "ignored result carried no tree")
            # And the tree is the one to re-plan from, meaning the frame that
            # caused the ignore: the dialog blocking this act has to be in it.
            # The app acks while draining and publishes after, so the bridge
            # has to wait out its budget for that frame instead of answering
            # with the pre-input tree, in which nothing explains the refusal.
            require(
                find(ignored, "dialog") is not None,
                "the ignored ack's tree has no dialog node: it predates the "
                "input, so it cannot explain why the act was refused",
            )
            require(
                find(ignored, "input")["value"] != MODAL_DRAFT,
                "the ignored ack's tree shows the blocked draft as applied",
            )
            break
        require(ignored is not None, "the bridge refused all 5 attempts to reach the app")

        live = client.read_tree()
        require(find(live, "dialog") is not None, "the modal closed by itself")
        require(
            find(live, "input")["value"] != MODAL_DRAFT,
            "an act the modal blocks changed the app anyway",
        )
        require(
            one_focused(live, "while modal") == "dialog",
            "dialog should still hold focus after an ignored act",
        )
        info.append("set_value that reached the app acked Ignored, state untouched")
    finally:
        # Always dismiss the dialog so a failing probe cannot leak modal
        # state into later probes; also delete any task a leaked activate
        # may have added.
        try:
            close_any_dialog(client)
            for node in list(flatten(client.read_tree()["root"])):
                if node.get("label") == MODAL_DRAFT:
                    act(client, node["id"], "delete")
                    act(client, "dialog-confirm", "activate")
        except Exception:
            pass  # best-effort cleanup; the probe's own failure wins

    tree = client.read_tree()
    require(find(tree, "dialog") is None, "dialog did not dismiss in cleanup")
    require(
        find(tree, "input")["value"] in (None, ""),
        "the modal-blocked draft leaked into the input",
    )
    one_focused(tree, "after dialog-open probe cleanup")
    return "; ".join(info)


def probe_rapid_acts(client, ctx):
    """Fire 10 selects back-to-back without waiting for responses.

    Answering all ten without an error is only half of it. "The tree did not
    change within 500ms" is a success result, so an app that acknowledged ten
    selects and applied none would pass an errors-only probe -- and so would
    a bridge that answered without forwarding anything. The burst therefore
    ends on a row it did not start on, and the list has to name that row when
    the dust settles.
    """
    tree = client.read_tree()
    tasks = [
        c["id"]
        for c in find(tree, "tasks").get("children", [])
        if "select" in c.get("actions", [])
    ]
    require(len(tasks) >= 2, f"need two selectable tasks, have {tasks}")
    a, b = tasks[0], tasks[1]
    # Park the cursor on `a` first, so the row the burst ends on is not the
    # row it started on and "nothing moved" cannot look like success.
    start = act(client, a, "select", expect=EXPECT_ACK)
    require(
        find(start, "tasks").get("value") == a,
        f"could not park the cursor on {a}: tasks names "
        f"{find(start, 'tasks').get('value')!r}",
    )

    ids = []
    for i in range(10):
        client._next_id += 1
        ids.append(client._next_id)
        client._send(
            {
                "jsonrpc": "2.0",
                "id": client._next_id,
                "method": "tools/call",
                "params": {
                    "name": "act",
                    "arguments": {"node": a if i % 2 == 0 else b, "action": "select"},
                },
            }
        )
    got, errors = set(), []
    deadline = time.monotonic() + 15.0
    while len(got) < len(ids):
        remaining = deadline - time.monotonic()
        require(remaining > 0, f"only {len(got)}/10 rapid responses within 15s")
        line = client._read_line(timeout=remaining)
        if not line.strip():
            continue
        resp = json.loads(line)
        if resp.get("id") in ids:
            got.add(resp["id"])
            if "error" in resp:
                errors.append(resp["error"].get("message", "?")[:60])
            elif resp.get("result", {}).get("isError"):
                errors.append("isError result")
    require(errors == [], f"rapid acts returned errors: {errors}")
    tree = client.read_tree()
    selected = find(tree, "tasks").get("value")
    require(
        selected == b,
        f"the last of 10 rapid selects asked for {b}, but the list names "
        f"{selected!r} (the burst started parked on {a}): acts were "
        "acknowledged and not applied",
    )
    focused = one_focused(tree, "after rapid acts")
    return f"10/10 responses, no errors, cursor {a} -> {selected} (focus {focused})"


def probe_empty_key(client, ctx):
    try:
        client.call_raw("key", {"key": ""})
        raise StepFailure("empty key string was accepted")
    except ToolError as err:
        require(
            "non-empty" in err.message,
            f"unexpected empty-key error: {err.message[:120]}",
        )
        return f"rejected: {err.message[:60]}"


def probe_unparseable_key(client, ctx):
    """`inx` is the string from the real incident: a typo'd key must come
    back as invalid_params with the grammar, not be forwarded to be dropped
    somewhere the agent cannot see it."""
    before = client.read_tree()
    message = None
    try:
        client.call_raw("key", {"key": "inx"})
        raise StepFailure("unparseable key `inx` was accepted")
    except ToolError as err:
        message = err.message
        require(
            err.code == INVALID_PARAMS,
            f"unparseable key error code {err.code}, expected {INVALID_PARAMS}",
        )
        require(
            "unrecognized key `inx`" in message,
            f"error does not name the offending key: {message[:120]}",
        )
        require(
            "expected" in message and "ctrl" in message,
            f"error does not carry the grammar: {message[:200]}",
        )
    after = client.read_tree()
    require(
        after == before,
        f"tree changed after a rejected key: seq {before['seq']} -> {after['seq']}",
    )
    return f"rejected with the grammar, tree unchanged: {message[:60]}..."


def probe_type_text_bounds(client, ctx):
    """type_text takes 1 to 4096 characters; both ends are refused before
    anything reaches the app."""
    before = client.read_tree()
    info = []
    for text, expected in (("", "non-empty"), ("a" * 4097, "4096")):
        try:
            client.call_raw("type_text", {"text": text})
            raise StepFailure(f"type_text accepted {len(text)} characters")
        except ToolError as err:
            require(
                err.code == INVALID_PARAMS,
                f"type_text({len(text)} chars) error code {err.code}, "
                f"expected {INVALID_PARAMS}",
            )
            require(
                expected in err.message,
                f"type_text({len(text)} chars) error does not say why: "
                f"{err.message[:120]}",
            )
            info.append(f"{len(text)} chars rejected")
    after = client.read_tree()
    require(
        after == before,
        f"tree changed after rejected type_text calls: seq {before['seq']} -> "
        f"{after['seq']}",
    )
    return "; ".join(info) + "; nothing typed"


def probe_key_repeat_bounds(client, ctx):
    """repeat is 1 to 64: both out-of-range ends are refused, and the top of
    the range is accepted with every press landing."""
    before = client.read_tree()
    for repeat in (0, 65):
        try:
            client.call_raw("key", {"key": "down", "repeat": repeat})
            raise StepFailure(f"key repeat={repeat} was accepted")
        except ToolError as err:
            require(
                err.code == INVALID_PARAMS,
                f"repeat={repeat} error code {err.code}, expected {INVALID_PARAMS}",
            )
            require(
                "between 1 and 64" in err.message,
                f"repeat={repeat} error does not state the range: "
                f"{err.message[:120]}",
            )
    after = client.read_tree()
    require(
        after == before,
        f"a rejected repeat still moved the app: seq {before['seq']} -> "
        f"{after['seq']}",
    )

    rows = [item["id"] for item in task_items(before)]
    require(len(rows) >= 2, f"need two rows to count moves, have {rows}")
    # 64 presses over a list whose length divides 64 wrap exactly back to the
    # start -- which is also where a burst that lost every press sits. The
    # landing assertion below would then hold with nothing moved at all, so
    # refuse to run rather than report a pass that proves nothing. Three
    # active rows is what the demo seeds; if that ever changes to 2, 4, 8 or
    # 16, this is the loud failure that says so.
    require(
        64 % len(rows) != 0,
        f"cannot count 64 presses over {len(rows)} rows ({rows}): they wrap "
        "exactly back to the starting row, so landing there would prove "
        "nothing about whether any press arrived",
    )
    tree = act(client, rows[0], "select", expect=EXPECT_ACK)
    require(
        one_focused(tree, "before repeat=64") == rows[0],
        f"select did not park the cursor on {rows[0]}",
    )
    expected = rows[64 % len(rows)]
    # `no_change` is not a legitimate answer any more: with the wrap ruled out
    # above, 64 presses must move the cursor and the app must publish it.
    tree = key(client, "down", repeat=64)
    landed = one_focused(tree, "after repeat=64")
    require(
        landed == expected,
        f"repeat=64 over {len(rows)} rows landed on {landed}, expected "
        f"{expected}: presses were lost or coalesced",
    )
    return (
        f"0 and 65 rejected; 64 accepted, all 64 presses landed over "
        f"{len(rows)} rows ({rows[0]} -> {landed})"
    )


def probe_empty_action(client, ctx):
    try:
        client.call_raw("act", {"node": "input", "action": ""})
        raise StepFailure("empty action string was accepted")
    except ToolError as err:
        require(
            "non-empty" in err.message,
            f"unexpected empty-action error: {err.message[:120]}",
        )
        return f"rejected: {err.message[:60]}"


def probe_stale_node(client, ctx):
    """A node id that existed earlier (dialog-cancel) but is gone now."""
    tree = client.read_tree()
    require(find(tree, "dialog") is None, "dialog unexpectedly open")
    try:
        client.call_raw("act", {"node": "dialog-cancel", "action": "activate"})
        raise StepFailure("act on a stale (absent) node id succeeded")
    except ToolError as err:
        require(
            "valid node ids" in err.message,
            f"stale-node error does not list valid ids: {err.message[:120]}",
        )
        return "stale dialog-cancel rejected with valid-id list"


def probe_ctrl_c_key(client, ctx):
    """ctrl+c reaches the app, is acknowledged, and types nothing.

    Sent with the *input* focused, which is the only place the modifier
    matters: the demo's input handler appends a bare `c` to the draft and its
    guard is what stops `ctrl+c` from doing the same
    (examples/demo-app/src/update.rs, the `KeyCode::Char(c)` arm). Sent with
    the list focused instead, `c` is unbound either way, so the probe would
    pass whether the guard existed or not.

    The answer has to be the acknowledged-no-change shape: the app dequeued
    the press and chose to do nothing. A tree back would mean the guard let
    the character through, and no ack at all would mean the press vanished.
    """
    draft = "ctrl guard"
    tree = act(client, "input", "set_value", value=draft)
    require(
        one_focused(tree, "before ctrl+c") == "input",
        "set_value must leave the input focused for the guard to be reachable",
    )
    require(
        find(tree, "input")["value"] == draft,
        f"could not seed the draft: input is {find(tree, 'input').get('value')!r}",
    )

    kind, _ = client.call_outcome("key", {"key": "ctrl+c"})
    require(
        kind == "no_change",
        f"ctrl+c into the focused input answered {kind!r}; expected the app to "
        "acknowledge a press it deliberately does nothing with",
    )
    live = client.read_tree()
    require(
        find(live, "input")["value"] == draft,
        f"ctrl+c typed into the draft: it now reads "
        f"{find(live, 'input').get('value')!r}, expected {draft!r}",
    )
    require(
        one_focused(live, "after ctrl+c") == "input",
        "ctrl+c moved the keyboard out of the input",
    )

    # Leave the app where this probe found it: draft cleared, keyboard back
    # on the list. `esc` is the demo's own way out of the input.
    tree = key(client, "esc")
    require(
        find(tree, "input")["value"] in (None, ""),
        "could not clear the draft after the ctrl+c probe",
    )
    back_on = one_focused(tree, "after the ctrl+c probe")
    require(back_on != "input", "esc did not hand the keyboard back to the list")
    return (
        f"ctrl+c acked with no change; draft {draft!r} untouched, focus "
        f"restored to {back_on}"
    )


def probe_set_value_without_value(client, ctx):
    """`set_value` carrying no value asks for nothing, and gets nothing.

    Treating a missing value as the empty string would clear the draft -- an
    edit the agent never asked for -- and report it as applied. The app
    ignores it instead, so the agent hears "nothing happened" and the half
    typed draft it forgot to pass is still there to act on.
    """
    draft = "half typed draft"
    tree = act(client, "input", "set_value", value=draft)
    require(
        find(tree, "input")["value"] == draft,
        f"could not seed the draft: input is {find(tree, 'input').get('value')!r}",
    )

    kind, tree = client.call_outcome("act", {"node": "input", "action": "set_value"})
    require(
        kind == "ignored",
        f"set_value with no value came back as {kind!r}, expected the ignored ack",
    )
    require(tree is not None, "ignored result carried no tree")
    require(
        find(tree, "input")["value"] == draft,
        f"the ignored ack's tree shows the draft as "
        f"{find(tree, 'input').get('value')!r}, expected it untouched",
    )
    live = client.read_tree()
    require(
        find(live, "input")["value"] == draft,
        f"a valueless set_value edited the draft to "
        f"{find(live, 'input').get('value')!r}",
    )

    # Leave the app as this probe found it. Seeding the draft moved focus to
    # the input, and `select` on a list item does not move it back (it only
    # moves the list cursor), so the later probes would start with the
    # keyboard somewhere they did not put it. `esc` is the demo's own way
    # out: it clears the draft and hands focus back to the list.
    tree = key(client, "esc")
    require(
        find(tree, "input")["value"] in (None, ""),
        "could not clear the draft again after the probe",
    )
    back_on = one_focused(tree, "after the valueless set_value probe")
    require(
        back_on != "input",
        "focus stayed on the input after esc, so later probes start somewhere "
        "this probe put them",
    )
    return f"ignored ack, draft {draft!r} untouched; focus restored to {back_on}"


def probe_version_mismatch(client, ctx):
    """A peer on another protocol version: reads work, input does not.

    Snapshots parse across the mismatch, so the bridge keeps answering
    read_tree. Inputs do not: the app would skip every `Input` line this
    bridge writes, so an input tool that sent one anyway would report
    "it may not have reacted yet" for a session that can never react. All
    three input tools must refuse *before* sending, and say so naming both
    versions.
    """
    with fake_session(protocol_version=OTHER_PROTOCOL) as (mcp, app):
        app.snapshot(1, FAKE_ROOT)

        tree = mcp.read_tree()
        require(
            find(tree, "btn") is not None and tree["root"]["id"] == "root",
            f"read_tree lost the tree across the version mismatch: {tree}",
        )
        require(
            tree["protocol_version"] == OTHER_PROTOCOL,
            f"read_tree reports protocol_version {tree['protocol_version']}, "
            f"expected the app's {OTHER_PROTOCOL}",
        )

        refused = []
        for tool, args in (
            ("act", {"node": "btn", "action": "activate"}),
            ("key", {"key": "down"}),
            ("type_text", {"text": "hello"}),
        ):
            try:
                mcp.call_raw(tool, args)
            except ToolError as err:
                require(
                    err.message == VERSION_MISMATCH_ERROR,
                    f"{tool} refused with an unexpected message:\n{err.message}",
                )
                refused.append(tool)
                continue
            raise StepFailure(
                f"{tool} was accepted against an app on protocol "
                f"{OTHER_PROTOCOL}; the app cannot parse what it sent"
            )

        # "nothing was sent" is half the claim, and the only half the error
        # message cannot prove on its own.
        time.sleep(0.3)  # let a late write land before calling it absent
        require(
            app.lines() == [],
            f"the bridge wrote to a peer it refused to send to: {app.lines()}",
        )
    return f"read_tree ok; {', '.join(refused)} refused, not one line written"


def probe_unknown_vocabulary(client, ctx):
    """A role and an action this build has never heard of reach the agent by
    name.

    Two failures live here, one behind the other. The first is the whole
    snapshot: a role or an action sits nested inside it, where serde's usual
    escape hatches do not reach, so rejecting one used to reject the tree it
    was in, and a reader that skips malformed lines then goes on serving its
    last good snapshot with the app looking frozen and no error anywhere. The
    vocabularies grew fallbacks for that.

    The second is what the bridge then did with the tree it had kept. It
    deserialized the app's line into its own `Snapshot` and serialized that
    back out, so an app publishing `"role":"sparkline"` had the agent read
    `"role":"other"`: the name was on the wire and the relay threw it away.
    The bridge now forwards the app's line as sent, so the name survives, and
    so does every field a newer app adds. The principle is the one the project
    already applies to `Action::Custom` below, which keeps an unknown action's
    name for exactly this reason; the relay extends it to everything else in
    the line.
    """
    with fake_session() as (mcp, app):
        app.snapshot(9, UNKNOWN_VOCAB_ROOT)

        tree = mcp.read_tree()
        require(tree["seq"] == 9, f"snapshot seq {tree['seq']}, expected 9")
        children = tree["root"]["children"]
        require(
            [c["id"] for c in children] == ["chart", "btn"],
            f"tree lost nodes to the unknown vocabulary: {[c['id'] for c in children]}",
        )

        chart, button = children
        require(
            chart["role"] == "sparkline",
            f"the role reached the agent as {chart['role']!r}, expected the "
            "'sparkline' the app published",
        )
        require(chart["label"] == "cpu", f"the relayed node lost its label: {chart}")
        require(
            chart["actions"] == ["zoom", {"set_range": {"from": 1, "to": 9}}],
            f"unknown actions came back as {chart['actions']}, expected them "
            "exactly as the app sent them",
        )
        # The sibling is the point: an unknown vocabulary must cost nothing.
        require(
            button
            == {
                "id": "btn",
                "role": "button",
                "label": "Save",
                "focused": True,
                "actions": ["activate"],
            },
            f"the sibling of the relayed node came back as {button}",
        )

        # The relayed line is untyped, so `act` validates against the bridge's
        # parse of it, where the unknown action is a custom one keeping its
        # name. That is what keeps it usable: an agent reads `zoom` out of the
        # tree and invokes it, all the way back to the app.
        mcp.call_raw("act", {"node": "chart", "action": "zoom"})
        require(app.wait_inputs(1), "the unknown custom action never reached the app")
        sent = app.inputs()[0]["input"]
        require(
            sent == {"kind": "act", "node": "chart", "action": {"custom": "zoom"}},
            f"the bridge forwarded {sent}, expected the custom action by name",
        )
    return (
        "unknown role and actions relayed verbatim, siblings intact, "
        "the unknown action still invocable"
    )


def probe_partial_burst_drop(client, ctx):
    """A burst whose middle presses were dropped is an error, not a tree.

    The last press landing (and the tree changing with it) is exactly what
    makes this dangerous: the last input's ack alone reads as plain success,
    and answering with the new tree would tell the agent a burst arrived
    intact when half of it never did.

    No single call against `taria-demo` can produce this: its input queue
    holds 256 and `repeat` caps at 64. A dozen concurrent calls do overflow
    it, but which of them sees drops among its own ids is a race, so the acks
    come from a stand-in app instead and the drop pattern is exact. The
    reporting under test is the bridge's either way.
    """
    sent, dropped_at = 4, (1, 2)
    with fake_session() as (mcp, app):
        app.snapshot(1, FAKE_ROOT)
        mcp.read_tree()

        req = mcp.send_call("key", {"key": "down", "repeat": sent})
        require(
            app.wait_inputs(sent),
            f"only {len(app.inputs())}/{sent} inputs of the burst reached the app",
        )
        ids = [msg["id"] for msg in app.inputs()]
        require(
            len(set(ids)) == sent,
            f"the burst reused input ids: {ids}",
        )
        for index, input_id in enumerate(ids):
            app.ack(input_id, "dropped" if index in dropped_at else "delivered")
        # The last press landed and the tree moved with it.
        app.snapshot(2, FAKE_ROOT)

        try:
            response_text(mcp.collect([req])[req])
        except ToolError as err:
            require(
                err.message == partial_drop_error(len(dropped_at), sent),
                f"partial-drop report reads:\n{err.message}",
            )
            return f"{len(dropped_at)}/{sent} dropped mid-burst reported as partial"
        raise StepFailure(
            f"a burst with {len(dropped_at)} of {sent} inputs dropped was "
            "reported as success"
        )


def probe_input_queue_full(client, ctx):
    """An app that stops reading its socket gets errors, not a hang.

    Without the bounded wait the tool call parks until the app comes back,
    with nothing said to the agent meanwhile. The stand-in app here accepts
    the connection and never reads it, which is the state the errors name.

    Backing the queue up passes through two distinct reports, and both are
    asserted here because they say different things to the agent:

    - the burst that runs out of room *partway* is neither a failure nor a
      success. Some of its presses are on the wire and may already have
      landed, so "this input was not sent" would hide them and a tree would
      claim a burst that arrived intact. Nothing lines the queue's capacity
      up with a multiple of 64, so one call has to straddle the boundary.
    - every call after it is refused outright, and only then does "this input
      was not sent" mean exactly that.
    """
    with fake_session(read_inputs=False) as (mcp, app):
        app.snapshot(1, FAKE_ROOT)
        mcp.read_tree()

        # Each call fills at most 64 slots and returns once its own wait for
        # an ack that cannot come times out, so a handful of calls is enough
        # to back the queue up; the cap is a guard against looping forever if
        # the bound ever goes away.
        partial = None
        for attempt in range(1, 17):
            try:
                mcp.call_raw("key", {"key": "down", "repeat": 64})
            except ToolError as err:
                match = PARTIAL_SEND_RE.match(err.message)
                if match:
                    require(
                        partial is None,
                        f"a second burst was cut short partway ({err.message}); "
                        f"the first was {partial}, so the queue drained in "
                        "between and the app is reading after all",
                    )
                    sent, wanted, unsent = (
                        int(match["sent"]),
                        int(match["wanted"]),
                        int(match["unsent"]),
                    )
                    require(
                        wanted == 64 and 0 < sent < 64 and sent + unsent == wanted,
                        f"partial-send counts do not add up: {err.message}",
                    )
                    # The app never read a line, so it acked nothing, so the
                    # drop clause has no business being here.
                    require(
                        match["dropped"] is None,
                        f"the partial-send report claims drops from an app that "
                        f"never read its socket: {err.message}",
                    )
                    partial = f"{sent} of {wanted} sent"
                    continue
                require(
                    err.message == QUEUE_FULL_ERROR,
                    f"the queue-full report reads:\n{err.message}",
                )
                require(
                    partial is not None,
                    f"the queue went from accepting a whole 64-press burst to "
                    f"refusing one outright at call {attempt}, with no call cut "
                    "short partway: the partial-send report is unreachable here "
                    "and untested",
                )
                return (
                    f"burst cut short partway ({partial}), then refused outright "
                    f"after {attempt} calls to an app that stopped reading, no hang"
                )
        raise StepFailure(
            "16 key calls (up to 1024 inputs) queued for an app that never "
            "read its socket, and none was refused"
        )


def probe_lost_acks(client, ctx):
    """Acks the bridge never got to read make the whole call unreportable.

    The bridge's ack channel drops the *oldest* entries when a reader falls
    behind, and the oldest are a burst's earliest presses -- exactly the ones
    whose `Dropped` answers nothing else would ever show. So a call that fell
    behind cannot stand behind any verdict: a clean tree would claim an
    intact burst nobody watched, and the drop tally it does have is a floor.

    No real app produces this on demand: it needs acks arriving faster than
    one tool call can read them, which is a property of the bridge's channel
    rather than of any app. The stand-in floods acks for ids nobody is
    waiting on, which is what a busy app looks like from the channel's side.
    Both halves of the report are covered: with no drops seen, and with one
    seen first so the tally is known to be incomplete rather than zero.
    """
    flood = 2000
    seen = []
    for label, drop_first in (("nothing known", False), ("a drop seen first", True)):
        sent = 2 if drop_first else 1
        with fake_session() as (mcp, app):
            app.snapshot(1, FAKE_ROOT)
            mcp.read_tree()

            req = mcp.send_call("key", {"key": "down", "repeat": sent})
            require(
                app.wait_inputs(sent),
                f"only {len(app.inputs())}/{sent} inputs reached the fake app",
            )
            ids = [msg["id"] for msg in app.inputs()]
            if drop_first:
                # Read before the flood, so the tally is a known 1 rather
                # than a casualty of the lag it is meant to qualify.
                app.ack(ids[0], "dropped")
                time.sleep(0.05)
            # One write, so the bridge's reader drains a full buffer of acks
            # per scheduling slot and outruns the call watching for its own.
            app.conn.sendall(
                b"".join(
                    b'{"type":"ack","id":%d,"status":"delivered"}\n' % (10_000_000 + i)
                    for i in range(flood)
                )
            )
            app.ack(ids[-1], "delivered")

            try:
                text = response_text(mcp.collect([req], timeout=20.0)[req])
            except ToolError as err:
                match = LOST_ACKS_RE.match(err.message)
                require(match, f"the lost-acks report reads:\n{err.message}")
                require(
                    int(match["lost"]) > 0 and int(match["sent"]) == sent,
                    f"lost-acks report counts {match['lost']} lost of "
                    f"{match['sent']} sent, expected some lost of {sent}",
                )
                if drop_first:
                    require(
                        match["dropped"] == "1",
                        f"the report should name the one drop it did read: "
                        f"{err.message}",
                    )
                else:
                    require(
                        match["dropped"] is None,
                        f"the report claims drops none of which were read: "
                        f"{err.message}",
                    )
                seen.append(f"{label} ({match['lost']} lost)")
                continue
            raise StepFailure(
                f"a call that lost acks to the channel answered as if it had "
                f"watched the whole burst: {text[:200]}"
            )
    return "; ".join(seen)


def probe_kill_and_restart(client, ctx, app, sock, launched):
    """SIGKILL the app mid-session; read_tree must error (not hang); then a
    restarted app must be picked up by the bridge's reconnect loop.

    `launched` is the runner's list of every demo process this script has
    started. The restarted app joins it the moment it exists, before anything
    that can fail: reached only through a return value, a failure between the
    launch and the return would leave the runner killing the corpse of the
    old one and deleting the socket directory out from under a live demo.
    """
    app.proc.kill()
    app.proc.wait(timeout=READ_TIMEOUT)

    start = time.monotonic()
    deadline = start + READ_TIMEOUT
    err_msg = None
    while time.monotonic() < deadline:
        try:
            client.call_raw("read_tree")
            time.sleep(0.1)
        except ToolError as err:
            err_msg = err.message
            break
        except TimeoutError:
            raise StepFailure("read_tree HUNG after SIGKILL of the app")
    require(err_msg is not None, "read_tree kept succeeding after SIGKILL")
    # The app died after delivering snapshots, so the bridge must report the
    # disconnect (which app, last seq), not the never-connected message.
    require(
        "disconnected" in err_msg and "last snapshot seq" in err_msg,
        f"unexpected error: {err_msg[:120]}",
    )
    errored_after = time.monotonic() - start

    # SIGKILL skips Drop, so the stale socket file is expected to linger.
    stale = os.path.exists(sock)
    new_app = PtyApp(sock)
    launched.append(new_app)
    require(new_app.wait_for_socket(), "restarted app never bound the socket")
    tree = client.read_tree()  # retries while the bridge reconnects
    focused = one_focused(tree, "after restart")
    require(
        find(tree, "tasks") is not None,
        "restarted app tree is missing the task list",
    )
    return (
        f"errored {errored_after:.1f}s after SIGKILL (stale socket file: {stale}); "
        f"bridge reconnected to restarted app, focus={focused}"
    )


def main():
    tmpdir = tempfile.mkdtemp(prefix="taria-adv-")
    sock = os.path.join(tmpdir, "adv.sock")
    # Every demo process this run starts, in start order. The restart probe
    # adds one, and teardown has to reach all of them: removing `tmpdir` with
    # a live demo still bound to the socket inside it leaves an orphan.
    launched = []
    app = None
    client = None
    results = []

    probes = [
        ("acts while dialog open", probe_act_while_dialog_open),
        ("set_value without a value", probe_set_value_without_value),
        ("rapid consecutive acts", probe_rapid_acts),
        ("empty key string", probe_empty_key),
        ("unparseable key string", probe_unparseable_key),
        ("key repeat bounds", probe_key_repeat_bounds),
        ("type_text bounds", probe_type_text_bounds),
        ("empty action string", probe_empty_action),
        ("stale node id", probe_stale_node),
        ("ctrl+c raw key", probe_ctrl_c_key),
        # Below here the demo is not the peer: each of these runs its own
        # bridge against a FakeApp, so they neither see nor disturb the app
        # the probes above share.
        ("protocol version mismatch", probe_version_mismatch),
        ("unknown role and action", probe_unknown_vocabulary),
        ("partial burst drop", probe_partial_burst_drop),
        ("app stops reading its socket", probe_input_queue_full),
        ("acks lost to the bridge's channel", probe_lost_acks),
    ]

    try:
        app = PtyApp(sock)
        launched.append(app)
        if not app.wait_for_socket():
            print("FAIL setup: socket never appeared")
            sys.exit(1)
        client = McpClient(sock)
        client.initialize()

        for name, fn in probes:
            try:
                evidence = fn(client, {})
                results.append((name, True, evidence))
                print(f"PASS {name}: {evidence}", flush=True)
            except (StepFailure, ToolError, TimeoutError) as err:
                results.append((name, False, str(err)))
                print(f"FAIL {name}: {err}", flush=True)
                # A probe that failed mid-modal must not hand the next one an
                # app that refuses every act.
                close_any_dialog(client)

        name = "kill app + restart/reconnect"
        try:
            evidence = probe_kill_and_restart(client, {}, app, sock, launched)
            results.append((name, True, evidence))
            print(f"PASS {name}: {evidence}", flush=True)
        except (StepFailure, ToolError, TimeoutError) as err:
            results.append((name, False, str(err)))
            print(f"FAIL {name}: {err}", flush=True)
    finally:
        if client is not None:
            client.close()
            if client.proc.poll() is None:
                client.proc.kill()
        # Every app, not just the last one named: the socket directory goes
        # with them, so a demo still running would lose its socket and linger.
        for started in launched:
            started.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)

    print("\n== adversarial summary ==")
    for name, ok, evidence in results:
        print(f"{'PASS' if ok else 'FAIL'}  {name}: {evidence}")
    passed = sum(1 for _, ok, _ in results if ok)
    print(f"{passed}/{len(results)} probes passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
