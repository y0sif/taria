#!/usr/bin/env python3
"""Adversarial probes against the taria v0 vertical slice.

Reuses the harness from e2e.py (pty app + MCP client) and pokes at edge
cases: acting while a modal dialog is open, rapid-fire acts, empty inputs,
killing the app mid-session, and app restart/reconnect. Each probe prints
PASS/FAIL (FAIL = hang, panic, or wrong-state result) plus an INFO line for
observed-but-debatable behavior.

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
    McpClient,
    PtyApp,
    StepFailure,
    ToolError,
    act,
    find,
    flatten,
    one_focused,
    require,
)

READ_TIMEOUT = 5.0


def has_action(node, name):
    """True if the node advertises `name` (built-in string or custom object)."""
    for action in node.get("actions", []):
        if action == name or (isinstance(action, dict) and action.get("custom") == name):
            return True
    return False


def probe_act_while_dialog_open(client, ctx):
    """Modal enforcement: while the delete dialog is open, non-dialog nodes
    advertise no actions, so the bridge must REJECT acts on them with a
    'does not advertise' ToolError. Silent acceptance is a failure."""
    tree = client.read_tree()
    task = next(
        n["id"] for n in flatten(tree["root"]) if has_action(n, "delete")
    )
    tree = act(client, task, "delete")
    require(find(tree, "dialog") is not None, "dialog did not open")

    info = []
    try:
        for action, value in (("set_value", "typed while modal"), ("activate", None)):
            try:
                act(client, "input", action, value=value, refresh=False)
            except ToolError as err:
                require(
                    "does not advertise" in err.message,
                    f"unexpected rejection for {action} while modal: "
                    f"{err.message[:120]}",
                )
                info.append(f"{action} rejected while modal")
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
    finally:
        # Always dismiss the dialog so a failing probe cannot leak modal
        # state into later probes; also delete any task a leaked activate
        # may have added.
        try:
            tree = act(client, "dialog", "dismiss")
            for node in list(flatten(tree["root"])):
                if node.get("label") == "typed while modal":
                    act(client, node["id"], "delete")
                    tree = act(client, "dialog-confirm", "activate")
        except Exception:
            pass  # best-effort cleanup; the probe's own failure wins

    tree = client.read_tree()
    require(find(tree, "dialog") is None, "dialog did not dismiss in cleanup")
    one_focused(tree, "after dialog-open probe cleanup")
    return "; ".join(info) + "; dialog kept focus, input advertised no actions"


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
