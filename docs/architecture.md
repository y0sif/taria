# Architecture

How the v0 vertical slice fits together. For why taria exists, see
`landscape.md`.

## Components

| Component | Location | Role |
|---|---|---|
| Protocol types | `crates/taria` | `Snapshot`, `Node`, `Role`, `Action`, `AgentInput`, and the `wire` messages. Serde only, no I/O. |
| Ratatui adapter | `crates/taria-ratatui` | `TariaLayer` serves the app's Unix socket and feeds agent input back into the event loop. `FrameRecorder` and `sem` record nodes per rendered frame. |
| MCP bridge | `crates/taria-mcp` | Stdio MCP server (rmcp) exposing `read_tree`, `act`, and `key`. A socket-manager task owns the connection to the app. |
| Demo | `examples/demo-app` | `taria-demo`, a task-manager TUI an agent drives end to end. |

## Data flow

```text
up:   render frame -> record nodes -> Snapshot -> socket -> bridge watch channel -> read_tree
down: act/key tool -> bridge validation -> Input -> socket -> app event loop -> state change -> next Snapshot
```

The app records semantic nodes alongside drawing each frame and publishes
them as a snapshot. The bridge keeps only the latest snapshot, so agents see
current state, never a backlog. Agent input travels the same socket in the
other direction and is applied by the app exactly like keyboard input. The
`act` tool checks the node id and the advertised action against the latest
tree before forwarding; after sending, it waits up to 500 ms for a changed
snapshot and returns it.

## Wire protocol

- Transport: Unix domain socket, newline-delimited JSON (ndjson). One JSON
  object per line.
- Up (app to bridge): one `hello` per connection, carrying `app_label` and
  `protocol_version`, then a stream of `snapshot` messages.
- Down (bridge to app): `input` messages wrapping an `AgentInput` (a
  semantic act or a raw key).
- Line cap: both sides refuse incoming lines over 1 MiB and treat the
  connection as broken rather than buffering without bound.
- `seq`: increments per published snapshot within one app run. It resets
  when the app restarts, so the bridge detects change by comparing whole
  snapshots, not by `seq` ordering. `seq` signals staleness only within one
  connection.
- Dedup: a frame whose tree is identical to the previous publish is skipped
  entirely; `seq` does not move.
- A `protocol_version` mismatch in `hello` logs a warning on the bridge; the
  connection continues. `PROTOCOL_VERSION` is 0; breaking changes bump it.

## Socket lifecycle

App side (`TariaLayer::bind`):

1. Resolve the path: `$TARIA_SOCK` verbatim, else
   `$XDG_RUNTIME_DIR/taria/<label>.sock`, else
   `<temp dir>/taria-<uid>/<label>.sock`.
2. Create the parent directory with mode `0700` and vet it: a real directory
   (not a symlink), owned by the current user, no group/other permission
   bits. Binding is refused otherwise, so another local user cannot swap the
   socket.
3. Remove a stale socket file, bind, and serve one client at a time from a
   listener thread. Each connection gets the `hello` plus the latest
   snapshot, then streams every new publish. Dropping the layer shuts the
   threads down and removes the socket file.

Bridge side (`taria-mcp`):

1. Connect, retrying forever: backoff starts at 250 ms, doubles, and caps at
   2 s. A connection that dies young without delivering a snapshot keeps the
   backoff growing instead of resetting it.
2. On disconnect, the latest-snapshot watch clears to `None`, so tool calls
   fail fast instead of acting on a stale tree.
3. On reconnect, inputs queued while disconnected are discarded; they were
   aimed at an app instance that no longer exists.

Guard rails on the app side: a 5 s write timeout drops a peer that stops
reading, and a bounded input queue (256 entries) drops the newest input with
a rate-limited warning instead of blocking the socket thread.

## Focus contract

Every snapshot carries exactly one focused node.

- The adapter guarantees at least one: the auto-generated `app` root is
  focused only when no recorded node (or descendant) is.
- The app is responsible for recording at most one. The demo unit-tests the
  invariant in every state: list, input, modal dialog, and empty list (focus
  parks on the list node itself).

Focus tells the agent where raw keys would land, which is what makes the
`key` fallback usable.

## Deferred (post-v0)

- Rect geometry on nodes (screen coordinates, for correlating the tree with
  rendered output).
- Nesting inference: `sem`-wrapped widgets record as a flat list today;
  hierarchy comes only from explicitly built children.
- Multiple simultaneous bridge clients per app.
- Adapters for other frameworks (Bubble Tea, Textual, Ink).
