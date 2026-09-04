# taria

Agent accessibility layer for TUIs (ARIA for terminals): protocol + ratatui
adapter + MCP bridge. Pre-alpha; v0 is a demo-first vertical slice.

## Commands

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings   # zero warnings policy
cargo fmt --check
```

Run all four before pushing.

## Architecture

- `crates/taria`: wire types only (`Snapshot`, `Node`, `Role`, `Action`,
  `AgentInput`). Serde-serializable, no I/O, no framework deps. Breaking wire
  changes bump `PROTOCOL_VERSION`.
- `crates/taria-ratatui`: adapter that publishes a `Snapshot` per meaningful
  frame and feeds `AgentInput` back to the app as events.
- `crates/taria-mcp`: bridge binary exposing `read_tree` / `act` / `key` MCP
  tools to harnesses, talking to the app over the taria transport.
- `examples/demo-app`: ratatui task-manager demo an agent drives end to end.

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
