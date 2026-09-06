#!/usr/bin/env python3
"""Adversarial probes against the taria v0.1 vertical slice.

Reuses the harness from e2e.py (pty app + MCP client) and pokes at edge
cases: acting while a modal dialog is open, rapid-fire acts, out-of-range and
malformed arguments, killing the app mid-session, and app restart/reconnect.
Each probe prints PASS/FAIL (FAIL = hang, panic, or wrong-state result) plus
an INFO line for observed-but-debatable behavior.

Usage:  python3 scripts/adversarial.py   (expects target/debug binaries built)
"""

import json
import os
import shutil
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from e2e import (  # noqa: E402
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
    one_focused,
    parse_result,
    require,
    response_text,
    task_items,
)

READ_TIMEOUT = 5.0
MODAL_DRAFT = "typed while modal"


def probe_act_while_dialog_open(client, ctx):
    """Modal enforcement, at both layers.

    Advertisement: while the delete dialog is open, non-dialog nodes
    advertise no actions, so the bridge REJECTS acts on them with a
    'does not advertise' ToolError.

    Behaviour: an act the bridge validated before the dialog existed still
    reaches the app, and the app answers it with an explicit Ignored ack --
    the 'deliberately did nothing' note plus a tree -- instead of swallowing
    it. Getting that ack is the point: an agent waiting on the effect of a
    blocked act stops waiting only because the app says so.
    """
    info = []
    try:
        task = deletable_task(client.read_tree())
        tree = act(client, task, "delete")
        require(find(tree, "dialog") is not None, "dialog did not open")

        for action, value in (("set_value", MODAL_DRAFT), ("activate", None)):
            try:
                act(client, "input", action, value=value, refresh=False)
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
    """Fire 10 selects back-to-back without waiting for responses."""
    tree = client.read_tree()
    tasks = [
        c["id"]
        for c in find(tree, "tasks").get("children", [])
        if "select" in c.get("actions", [])
    ]
    require(len(tasks) >= 2, f"need two selectable tasks, have {tasks}")
    a, b = tasks[0], tasks[1]

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
    tree = client.read_tree()
    focused = one_focused(tree, "after rapid acts")
    require(errors == [], f"rapid acts returned errors: {errors}")
    return f"10/10 responses, no errors, final focus {focused}"


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
    tree = act(client, rows[0], "select")
    require(
        one_focused(tree, "before repeat=64") == rows[0],
        f"select did not park the cursor on {rows[0]}",
    )
    expected = rows[64 % len(rows)]
    kind, tree = client.call_outcome("key", {"key": "down", "repeat": 64})
    if kind == "no_change":
        # Only legitimate when 64 presses wrap exactly back to the start.
        require(
            expected == rows[0],
            f"key repeat=64 reported no change, but 64 presses over "
            f"{len(rows)} rows should have landed on {expected}",
        )
        tree = client.read_tree()
    else:
        require(kind == "tree", f"key repeat=64 answered {kind!r}, expected a tree")
    landed = one_focused(tree, "after repeat=64")
    require(
        landed == expected,
        f"repeat=64 over {len(rows)} rows landed on {landed}, expected "
        f"{expected}: presses were lost or coalesced",
    )
    return f"0 and 65 rejected; 64 accepted, all 64 presses landed ({landed})"


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
        act(client, "dialog-cancel", "activate", refresh=False)
        raise StepFailure("act on a stale (absent) node id succeeded")
    except ToolError as err:
        require(
            "valid node ids" in err.message,
            f"stale-node error does not list valid ids: {err.message[:120]}",
        )
        return "stale dialog-cancel rejected with valid-id list"


def probe_ctrl_c_key(client, ctx):
    text = client.call_raw("key", {"key": "ctrl+c"})
    tree = client.read_tree()
    require(tree is not None, "app died or bridge lost it after ctrl+c key")
    return "ctrl+c forwarded, app alive"


def probe_kill_and_restart(client, ctx, app, sock):
    """SIGKILL the app mid-session; read_tree must error (not hang); then a
    restarted app must be picked up by the bridge's reconnect loop."""
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
    ), new_app


def main():
    tmpdir = tempfile.mkdtemp(prefix="taria-adv-")
    sock = os.path.join(tmpdir, "adv.sock")
    app = None
    client = None
    results = []

    probes = [
        ("acts while dialog open", probe_act_while_dialog_open),
        ("rapid consecutive acts", probe_rapid_acts),
        ("empty key string", probe_empty_key),
        ("unparseable key string", probe_unparseable_key),
        ("key repeat bounds", probe_key_repeat_bounds),
        ("type_text bounds", probe_type_text_bounds),
        ("empty action string", probe_empty_action),
        ("stale node id", probe_stale_node),
        ("ctrl+c raw key", probe_ctrl_c_key),
    ]

    try:
        app = PtyApp(sock)
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
            evidence, app = probe_kill_and_restart(client, {}, app, sock)
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
        if app is not None:
            app.kill()
        shutil.rmtree(tmpdir, ignore_errors=True)

    print("\n== adversarial summary ==")
    for name, ok, evidence in results:
        print(f"{'PASS' if ok else 'FAIL'}  {name}: {evidence}")
    passed = sum(1 for _, ok, _ in results if ok)
    print(f"{passed}/{len(results)} probes passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
