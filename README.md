# taria

**ARIA for terminals.** taria is an agent accessibility layer for terminal
user interfaces: a small protocol plus framework integrations that let a TUI
app expose its live widget tree, focus state, and available actions directly
to AI agents, with an MCP bridge so any harness (Claude Code, OpenCode,
Cursor) can drive the app without modification.

Agents today reach TUIs by scraping rendered screens through tmux or headless
terminals and guessing at structure. taria works on the other side of the
terminal: the app publishes what is on screen semantically, the way
accessibility trees transformed GUI automation.

> Status: pre-alpha. The v0 vertical slice (ratatui adapter + MCP bridge +
> demo) is under construction. See `docs/landscape.md` for why this project
> exists and how it compares to ht, tmux, and PTY-based drivers.

## Workspace layout

```
crates/taria          Core protocol types (widget tree, actions, snapshots)
crates/taria-ratatui  Ratatui adapter: publish semantics alongside rendering
crates/taria-mcp      MCP bridge binary for agent harnesses
examples/demo-app     Demo ratatui app driven by an agent through taria
docs/                 Landscape research, architecture, comparisons
```

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

Dual-licensed under MIT or Apache-2.0, at your option.
