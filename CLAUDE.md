# taria

Agent accessibility layer for TUIs (ARIA for terminals): protocol + ratatui
adapter + MCP bridge. Pre-alpha; the v0 vertical slice (adapter + bridge +
demo) works end to end.

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

- `crates/taria`: wire types only (`Snapshot`, `Node`, `Role`, `Action`,
  `AgentInput`) plus the `wire` module (`AppToBridge`/`BridgeToApp` ndjson
  messages). Serde-serializable, no I/O, no framework deps. Breaking wire
  changes bump `PROTOCOL_VERSION`.
- `crates/taria-ratatui`: `TariaLayer` binds the app's Unix socket, vets the
  socket directory, and serves one bridge client at a time from background
  threads (listener plus per-connection reader/writer). `FrameRecorder`/`sem`
  record nodes per frame; identical trees are deduped; `AgentInput` reaches
  the app via `try_recv`/`recv_timeout` like any other event.
- `crates/taria-mcp`: rmcp stdio server exposing `read_tree`/`act`/`key`. A
  socket-manager task reconnects with capped backoff and holds the latest
  snapshot in a watch channel; `act` validates node ids and advertised
  actions against that snapshot before forwarding.
- `examples/demo-app` (`taria-demo`): ratatui task manager (tabs, list, text
  input, confirm-delete dialog) an agent drives end to end. `tree.rs` and
  `update.rs` are pure and unit-tested, including the one-focused-node and
  modal-dialog invariants.

Details: docs/architecture.md.

## Conventions

- Dual MIT/Apache-2.0. Adoption is the project's oxygen: keep the protocol
  minimal, keep `taria` (core) dependency-light.
- Add dependencies with `cargo add` (picks current versions); justify heavy
  ones in docs/tech-stack.md.
- Never panic in library crates; return errors.
- Conventional commits: feat:, fix:, docs:, chore:, refactor:.
- The `Key` raw-input fallback exists so partial semantic coverage is still
  useful; never let it become the primary path in demos.

## Context

- Phase 0 landscape and differentiation: docs/landscape.md. The agent-side
  driver space (ht, tmux, agent-tui) is crowded; taria is framework-side.
  Do not drift into building another screen-scraper.
