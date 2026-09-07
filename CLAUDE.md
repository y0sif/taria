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
python3 scripts/e2e.py                      # end-to-end gate, 20 steps
python3 scripts/adversarial.py              # edge-case probes, 17 of them
```

Run all six before pushing. CI runs the same gate with `cargo build
--workspace` in place of `cargo check`. Both scripts build the debug binaries
they test; `--no-build` skips the build and keeps the freshness check, which
refuses a `target/debug` older than the sources.

## Architecture

- `crates/taria`: wire types (`Snapshot`, `Node`, `Role`, `Action`,
  `AgentInput`), the `wire` module (`AppToBridge`/`BridgeToApp` ndjson
  messages, `InputId`, `InputStatus`), and three modules both peers share:
  `key` (the key-string grammar), `id::IdSpace` (prefixed node ids),
  `socket` (path resolution, plus the per-platform AF_UNIX path limit: 107
  bytes on Linux, 103 on macOS and the BSDs, and a label that is not a plain
  file name is refused). No I/O, no framework deps, exactly one dependency
  (serde); `serde_json` is dev-only. 29 roles, 7 actions plus `Custom`, whose
  envelope folds back onto the built-ins on the way in. Version 1 is frozen:
  additive changes only (new optional fields, new message variants, new roles
  and actions, which peers degrade to `other` / `Custom` instead of failing
  the snapshot). The ten types such an addition can reach are
  `#[non_exhaustive]`, so additive on the wire is additive in Rust too.
  Anything else bumps `PROTOCOL_VERSION`.
- `crates/taria-ratatui`: `TariaLayer` binds the app's Unix socket, refuses a
  label that is not a plain file name (it binds and unlinks what the label
  resolves to), vets the socket directory, and serves one bridge client at a
  time from background threads (listener plus per-connection reader/writer).
  `bind_or_disabled` never fails, so taria cannot block startup; the layer
  never prints (the app reports `bind_error()` outside the alternate screen).
  Nodes come from `publish(nodes)` or `FrameRecorder`/`sem`; identical trees
  are deduped. Input arrives via
  `drain`/`drain_with_ids`/`try_recv`/`recv_timeout`, acked `Delivered` on
  dequeue, `Dropped` when the queue is full, and
  `Ignored` when the app says so with `ack`. Acks are flushed before the
  snapshot published after them. Inputs are tagged with their connection's
  generation and discarded (not applied) once that connection ends; the two
  silent discards are counted in `dropped_inputs()`/`stale_inputs()`.
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
  disconnecting. A protocol mismatch keeps `read_tree` and refuses every
  input tool.
- `examples/demo-app` (`taria-demo`): ratatui task manager (tabs, list, text
  input, confirm-delete dialog) an agent drives end to end. Task ids are
  identities (`IdSpace`), stable across deletes and restarts; the list
  publishes its selection as its own value; chords are never treated as the
  plain key they contain. The input advertises `Dismiss` while it holds the
  keyboard, so every state an agent can enter has an advertised way out; acts
  on nodes that are gone (the dialog's, a deleted task) report `Ignored`.
  `tree.rs` and `update.rs` are pure and unit-tested, including the
  one-focused-node and modal-dialog invariants.

Details: docs/architecture.md. Retrofit guidance: docs/integration-guide.md.

## Conventions

- Dual MIT/Apache-2.0. Adoption is the project's oxygen: keep the protocol
  minimal, keep `taria` (core) dependency-light.
- Add dependencies with `cargo add` (picks current versions); justify heavy
  ones in docs/architecture.md.
- Never panic in library crates; return errors.
- Conventional commits: feat:, fix:, docs:, chore:, refactor:.
- The `Key` and `Text` raw-input fallbacks exist so partial semantic
  coverage is still useful; never let them become the primary path in demos.
  Both lower into the app's key handler, so they land wherever focus is.
- Every state an agent can enter needs an advertised way out, or the fallbacks
  become the only escape. Advertise the action only where it does something,
  and report `Ignored` where it does not, so the tree and the verdict agree.

## Context

- Phase 0 landscape and differentiation: docs/landscape.md. The agent-side
  driver space (ht, tmux, agent-tui) is crowded; taria is framework-side.
  Do not drift into building another screen-scraper.
