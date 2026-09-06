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
python3 scripts/e2e.py                      # end-to-end gate (builds debug binaries)
python3 scripts/adversarial.py              # edge-case probes (needs debug binaries)
```

Run all six before pushing.

## Architecture

- `crates/taria`: wire types (`Snapshot`, `Node`, `Role`, `Action`,
  `AgentInput`), the `wire` module (`AppToBridge`/`BridgeToApp` ndjson
  messages, `InputId`, `InputStatus`), and three modules both peers share:
  `key` (the key-string grammar), `id::IdSpace` (prefixed node ids),
  `socket` (path resolution, the 107-byte AF_UNIX limit). No I/O, no
  framework deps, exactly one dependency (serde); `serde_json` is dev-only.
  Version 1 is frozen: additive changes only (new optional fields, new
  message variants, new roles and actions, which peers degrade to `other` /
  `Custom` instead of failing the snapshot). Anything else bumps
  `PROTOCOL_VERSION`.
- `crates/taria-ratatui`: `TariaLayer` binds the app's Unix socket, vets the
  socket directory, and serves one bridge client at a time from background
  threads (listener plus per-connection reader/writer). `bind_or_disabled`
  never fails, so taria cannot block startup; the layer never prints (the
  app reports `bind_error()` outside the alternate screen). Nodes come from
  `publish(nodes)` or `FrameRecorder`/`sem`; identical trees are deduped.
  Input arrives via `drain`/`drain_with_ids`/`try_recv`/`recv_timeout`,
  acked `Delivered` on dequeue, `Dropped` when the queue is full, and
  `Ignored` when the app says so with `ack`. Acks are flushed before the
  snapshot published after them. Inputs are tagged with their connection's
  generation and discarded (not applied) once that connection ends; the two
  silent discards are counted in `dropped_inputs()`/`stale_inputs()`.
- `crates/taria-mcp`: rmcp stdio server exposing
  `read_tree`/`act`/`key`/`type_text`. A socket-manager task reconnects with
  capped backoff, holds the latest snapshot in a watch channel, and
  rebroadcasts acks. `act` validates node ids and advertised actions, `key`
  parses the shared grammar and bounds `repeat` (1-64), `type_text` bounds
  the payload (4096 chars). Results are anchored on the app's ack, not on
  whatever the tree did: dropped, ignored, delivered-and-changed,
  delivered-and-unchanged, no ack, partial burst, lost acks, and the app
  disconnecting. A protocol mismatch keeps `read_tree` and refuses every
  input tool.
- `examples/demo-app` (`taria-demo`): ratatui task manager (tabs, list, text
  input, confirm-delete dialog) an agent drives end to end. Task ids are
  identities (`IdSpace`), stable across deletes and restarts; the list
  publishes its selection as its own value; chords are never treated as the
  plain key they contain. `tree.rs` and `update.rs` are pure and
  unit-tested, including the one-focused-node and modal-dialog invariants.

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

## Context

- Phase 0 landscape and differentiation: docs/landscape.md. The agent-side
  driver space (ht, tmux, agent-tui) is crowded; taria is framework-side.
  Do not drift into building another screen-scraper.
