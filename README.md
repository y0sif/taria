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

> Status: pre-alpha. The v0 vertical slice works end to end: a ratatui
> adapter, an MCP bridge, and a demo app an agent can drive today. The wire
> format is not stable yet. See `docs/landscape.md` for why this project
> exists and `docs/architecture.md` for how the pieces fit.

## Quick start

Three steps, two terminals.

1. Run the demo app in one terminal. It prints its taria socket path on
   startup and then behaves like a normal task-manager TUI:

   ```bash
   cargo run -p taria-demo
   ```

2. Register the bridge with your agent harness. Build it first, then for
   Claude Code:

   ```bash
   cargo build -p taria-mcp
   claude mcp add taria -- <repo>/target/debug/taria-mcp --app taria-demo
   ```

   Working inside this repo, you can skip `claude mcp add`: the committed
   `.mcp.json` registers the same server, and Claude Code picks it up here
   once you approve it.

3. Ask the agent to drive the app: "read the tree, add a task called ship
   v0, mark it done, then delete it". The agent works through the
   `read_tree`, `act`, and `key` tools while the TUI reacts in the first
   terminal.

## How it works

The app publishes a semantic snapshot of its widget tree over a Unix domain
socket, one JSON object per line (ndjson), on every meaningful change. The
bridge holds the latest snapshot and exposes it, plus input back into the
app, as MCP tools. Agent input reaches the app's event loop like any other
input source.

```text
+---------------+   Unix socket  +-----------+  MCP over stdio  +---------------+
|    TUI app    | <------------> | taria-mcp | <--------------> | agent harness |
| taria-ratatui |    (ndjson)    |  bridge   |                  | (Claude Code) |
+---------------+                +-----------+                  +---------------+
```

`docs/architecture.md` covers the wire protocol, socket lifecycle, and focus
contract in detail.

## MCP tools

| Tool | What it does |
|---|---|
| `read_tree` | Returns the app's current semantic tree as JSON: node ids, roles, labels, values, focus, and the actions each node advertises. |
| `act` | Invokes an advertised action on a node by id, with an optional value (e.g. for `set_value`). Validated against the tree; returns the updated tree once the app reacts. |
| `key` | Sends a raw key press (`"q"`, `"enter"`, `"ctrl+c"`). A fallback for parts of the UI without semantic coverage. |

## taria-mcp CLI

```text
taria-mcp --socket <path>   Connect to an explicit Unix socket path
taria-mcp --app <label>     Derive the socket path for <label>
taria-mcp --help            Show help
```

Exactly one of `--socket` or `--app` is required.

Environment variables:

| Variable | Effect |
|---|---|
| `TARIA_SOCK` | Overrides the socket path, on both the app side and the bridge side. |
| `TARIA_LOG` | Bridge log filter (tracing env-filter syntax), default `info`. Logs go to stderr; stdout carries MCP. |

With `--app <label>`, the bridge resolves the socket path the same way the
app-side adapter does when binding:

1. `$TARIA_SOCK`, if set and non-empty (used verbatim);
2. `$XDG_RUNTIME_DIR/taria/<label>.sock`;
3. `<temp dir>/taria-<uid>/<label>.sock`.

## Workspace layout

```
crates/taria          Core protocol types (widget tree, actions, snapshots)
crates/taria-ratatui  Ratatui adapter: publish semantics alongside rendering
crates/taria-mcp      MCP bridge binary for agent harnesses
examples/demo-app     Demo ratatui app driven by an agent through taria
scripts/              Python verification harnesses (e2e, adversarial)
docs/                 Landscape research and architecture notes
```

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check
python3 scripts/e2e.py           # end-to-end: demo app + bridge + MCP scenario
python3 scripts/adversarial.py   # edge-case probes (expects debug binaries built)
```

Both scripts use only the Python standard library. `e2e.py` builds the debug
binaries first; pass `--no-build` to skip that.

## Limitations

- The adapter serves one bridge client per app at a time.
- A `protocol_version` mismatch in the handshake logs a warning on the
  bridge; it does not disconnect.
- Unix only for now: the transport is a Unix domain socket. Linux is the
  tested platform.
- Pre-alpha wire format. `PROTOCOL_VERSION` is 0, and breaking changes will
  bump it without a compatibility path.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
