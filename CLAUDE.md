# taria

Agent accessibility layer for TUIs (ARIA for terminals): protocol + ratatui
adapter + MCP bridge. Pre-alpha; the vertical slice (adapter + bridge +
demo) works end to end. Wire format frozen at `PROTOCOL_VERSION` 1.

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings   # zero warnings policy
cargo fmt --check
python3 scripts/e2e.py                      # end-to-end gate, 21 steps
python3 scripts/adversarial.py              # edge-case probes, 20 of them
```

Run all six before pushing. CI runs the same gate with `cargo build
--workspace` in place of `cargo check`. Both scripts build the debug binaries
they test; `--no-build` skips the build and keeps the freshness check, which
refuses a `target/debug` older than the sources.

## Architecture

- `crates/taria`: wire types (`Snapshot`, `Node`, `Role`, `Action`,
  `AgentInput`), the `wire` module (`AppToBridge`/`BridgeToApp` ndjson
  messages, `InputId`, `InputStatus`), and three modules both peers share:
  `key` (the key-string grammar), `IdSpace` (prefixed node ids, re-exported
  at the root),
  `socket` (path resolution, plus the per-platform AF_UNIX path limit: 107
  bytes on Linux, 103 on macOS and the BSDs, and a label that is not a plain
  file name is refused). No I/O, no framework deps, exactly one dependency
  (serde); `serde_json` is dev-only. 29 roles, 7 actions plus `Custom`, whose
  envelope folds back onto the built-ins on the way in. Version 1 is frozen:
  additive changes only (new optional fields, new message variants, new roles
  and actions, which peers degrade to `other` / `Custom` instead of failing
  the snapshot). The eleven types such an addition can reach are
  `#[non_exhaustive]`, and so are the six struct-like variants inside them
  (`AppToBridge::Hello`/`Ack`, `BridgeToApp::Input`, `AgentInput::Act`/`Key`/
  `Text`), which is why peers build through constructors and destructure with
  `..`; `AgentInput::Unknown` is the `#[serde(other)]` fallback that keeps an
  unreadable input's id, so it can still be acked. So additive on the wire is
  additive in Rust too. `MAX_NODE_DEPTH` is 32, against a measured ceiling of
  63 nested nodes before a JSON parser refuses; `Node::check_depth` reports a
  violation and names a node at the offending depth. Anything else bumps
  `PROTOCOL_VERSION`.
- `crates/taria-ratatui`: `TariaLayer` binds the app's Unix socket, refuses a
  label that is not a plain file name (it binds and unlinks what the label
  resolves to), vets the socket directory, and serves one bridge client at a
  time from background threads (listener plus per-connection reader/writer).
  `bind_or_disabled` never fails, so taria cannot block startup; the layer
  never prints (the app reports `bind_error()` outside the alternate screen).
  Nodes come from `publish(nodes)` or `FrameRecorder`/`sem`; identical trees
  are deduped. Input arrives via
  `drain_acking`/`drain`/`drain_with_ids`/`try_recv`/`recv_timeout`, acked
  `Delivered` on dequeue, `Dropped` when the queue is full, and `Ignored`
  when the app says so, by returning it from `drain_acking` or calling `ack`.
  The wildcard arm `#[non_exhaustive]` forces on a `match` over `AgentInput`
  is the arm that swallows `Text`; the demo carries
  `clippy::wildcard_enum_match_arm` to catch exactly that. Acks are flushed before the
  snapshot published after them, from a bounded queue that drops its oldest.
  Inputs are tagged with their connection's generation and discarded (not
  applied) once that connection ends, and `ack` refuses an id whose connection
  is gone. Four counters: `dropped_inputs()`, `stale_inputs()`,
  `unknown_inputs()`, `dropped_acks()`. A tree over `MAX_NODE_DEPTH` is cut
  branch-wise at publish rather than withheld (a withheld tree leaves the
  agent on a stale one), reported via
  `truncated_snapshots()`/`last_truncation()`. Binding unlinks only a socket
  nothing is listening on, refuses a live one so a second instance cannot
  steal it, and the listener polls a shutdown flag so exit never waits on a
  connection.
- `crates/taria-mcp`: rmcp stdio server exposing
  `read_tree`/`act`/`key`/`type_text`. A socket-manager task reconnects with
  capped backoff, holds the latest snapshot in a watch channel, and
  rebroadcasts acks. A snapshot is kept as the app's own line (minus the
  `type` framing key) beside the parse of it: the line is what the agent
  reads, so an unknown role, action or field survives by name; the parse is
  what the bridge validates and diffs against. `act` validates node ids and
  advertised actions and bounds `value` (4096 chars), `key` parses the shared
  grammar and bounds `repeat` (1-64), `type_text` bounds the payload (4096
  chars). Results are anchored on the app's ack, not on whatever the tree
  did: dropped, ignored, delivered-and-changed,
  delivered-and-unchanged, no ack, partial burst, lost acks, and the app
  disconnecting. A protocol mismatch keeps `read_tree` for as long as the
  peer's snapshots parse, and refuses every input tool. When lines from the
  app were rejected, `read_tree` says the socket path is right and quotes the
  reason rather than sending an adopter back to check the path.
- `examples/demo-app` (`taria-demo`): ratatui task manager (tabs, list, text
  input, confirm-delete dialog) an agent drives end to end. Task ids are
  identities (`IdSpace`), stable across deletes and restarts; the list
  publishes its selection as its own value; chords are never treated as the
  plain key they contain. Typed text is routed to the new-task input and
  `Ignored` elsewhere. The input advertises `Focus` while the keyboard is
  elsewhere, `Dismiss` while it holds it, and `Activate` only while the draft
  would submit, and a `quit` node advertises the way out of the app, so every
  state an agent can enter has an advertised way out; acts on nodes that are
  gone (the dialog's, a deleted task) report `Ignored`. Submitting switches
  to the tab the new task landed on, so the effect is in the next snapshot.
  `tree.rs` and `update.rs` are pure and unit-tested, including the
  one-focused-node and modal-dialog invariants.

Details: docs/architecture.md. Normative per-message wire spec:
docs/protocol.md. Retrofit guidance: docs/integration-guide.md. How taria
differs from the screen-level tools: docs/comparison.md.

## Conventions

- Dual MIT/Apache-2.0. Adoption is the project's oxygen: keep the protocol
  minimal, keep `taria` (core) dependency-light.
- Add dependencies with `cargo add` (picks current versions); justify heavy
  ones in docs/architecture.md.
- Never panic in library crates; return errors.
- Conventional commits: feat:, fix:, docs:, chore:, refactor:.
- The `Key` and `Text` raw inputs exist so partial semantic coverage is still
  useful; never let them become the primary path in demos. They are lowered
  differently on purpose: `Key` goes into the app's key handler and lands
  wherever focus is, `Text` goes to whatever the app puts typing into and is
  reported `Ignored` when nothing is. Text through the bindings is a bug, not
  a shortcut: one `type_text` containing `d` and `y` deleted a task.
- Every state an agent can enter needs an advertised way out, or the raw `key`
  fallback becomes the only escape. Advertise the action only where it does
  something, and report `Ignored` where it does not, so the tree and the
  verdict agree.

## Context

- Phase 0 landscape and differentiation: docs/landscape.md. The agent-side
  driver space (ht, tmux, agent-tui) is crowded; taria is framework-side.
  Do not drift into building another screen-scraper.
