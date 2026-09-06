# taria

**ARIA for terminals.** taria is an agent accessibility layer for terminal
user interfaces. A TUI app declares its live widget tree, focus state, and
available actions through a small protocol. An MCP bridge exposes that tree
to any agent harness (Claude Code, OpenCode, Cursor): the harness drives the
app through ordinary MCP tools, with no taria-specific code of its own.

Agents today reach TUIs by scraping rendered screens through tmux or headless
terminals and guessing at structure. taria works on the other side of the
terminal: the app publishes what is on screen semantically, the way
accessibility trees transformed GUI automation.

> Status: pre-alpha. The vertical slice works end to end: a ratatui adapter,
> an MCP bridge, and a demo app an agent can drive today. The wire format is
> frozen at `PROTOCOL_VERSION` 1. See `docs/landscape.md` for why this
> project exists, `docs/architecture.md` for how the pieces fit, and
> `docs/integration-guide.md` for adding taria to an app you already have.

## Quick start

Prerequisites: a Rust toolchain (1.88 or newer) and, for step 2, the
`claude` CLI. Three steps, two terminals.

1. Run the demo app in one terminal. It prints its taria socket path on
   startup and then behaves like a normal task-manager TUI:

   ```bash
   cargo run -p taria-demo
   ```

2. Register the bridge with your agent harness. Build it first, then for
   Claude Code:

   ```bash
   cargo build -p taria-mcp
   claude mcp add taria -- $(pwd)/target/debug/taria-mcp --app taria-demo
   ```

   Working inside this repo, you can skip `claude mcp add`: the committed
   `.mcp.json` registers the same server, and Claude Code picks it up here
   once you approve it.

3. Ask the agent to drive the app: "read the tree, add a task called ship
   v0, mark it done, then delete it". The agent works through the
   `read_tree`, `act`, `type_text`, and `key` tools while the TUI reacts in
   the first terminal.

### Running the demo headless

Backgrounding the demo with plain `&` fails. Crossterm needs a real terminal
for raw mode, so a detached process exits immediately. Give it a terminal
with tmux instead:

```bash
cargo build -p taria-demo
tmux new-session -d -s taria-demo -x 120 -y 34 './target/debug/taria-demo'
```

Peek at the screen without attaching:

```bash
tmux capture-pane -p -t taria-demo
```

Stop it when you are done:

```bash
tmux kill-session -t taria-demo
```

## How it works

The app publishes a semantic snapshot of its widget tree over a Unix domain
socket, one JSON object per line (ndjson), on every meaningful change. The
bridge holds the latest snapshot and exposes it, plus input back into the
app, as MCP tools. Agent input reaches the app's event loop like any other
input source, and the app acknowledges each input by id, so the bridge can
tell an input the app acted on from one it never saw.

```text
+---------------+   Unix socket  +-----------+  MCP over stdio  +---------------+
|    TUI app    | <------------> | taria-mcp | <--------------> | agent harness |
| taria-ratatui |    (ndjson)    |  bridge   |                  | (Claude Code) |
+---------------+                +-----------+                  +---------------+
```

`docs/architecture.md` covers the wire protocol, acknowledgement, socket
lifecycle, and focus contract in detail. `docs/integration-guide.md` is the
guide to retrofitting taria into a ratatui app you already have.

## MCP tools

| Tool | What it does |
|---|---|
| `read_tree` | Returns the app's current semantic tree as JSON: node ids, roles, labels, values, focus, and the actions each node advertises. |
| `act` | Invokes an advertised action on a node by id, with an optional value (e.g. for `set_value`). The node id and the action are checked against the latest tree before anything is sent. |
| `key` | Sends a raw key press (`"q"`, `"enter"`, `"ctrl+c"`), up to 64 times with `repeat`. A key that does not match the grammar is rejected here rather than swallowed by the app. A fallback for parts of the UI without semantic coverage. |
| `type_text` | Types a literal string in one call instead of one `key` call per character, up to 4096 characters. The text lands wherever the app currently sends typing, so focus the target first. |

The three input tools wait up to 500 ms for the app's answer and report what
actually happened: the updated tree, an input the app deliberately ignored
(with the current tree to re-plan from), or an error for an input that was
dropped, never applied, or aimed at an app that has gone away.

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
| `TARIA_SOCK` | Socket path, used verbatim. Replaces the default on the app side always, and on the bridge side only when the path is derived from `--app`; an explicit `--socket` wins over it. |
| `TARIA_LOG` | Bridge log filter (tracing env-filter syntax), default `info`. Logs go to stderr; stdout carries MCP. |

With `--app <label>`, the bridge resolves the socket path the same way the
app-side adapter does when binding:

1. `$TARIA_SOCK`, if set and non-empty (used verbatim);
2. `$XDG_RUNTIME_DIR/taria/<label>.sock`;
3. `<temp dir>/taria-<uid>/<label>.sock`.

## Workspace layout

```text
crates/taria          Core protocol types (widget tree, actions, snapshots)
crates/taria-ratatui  Ratatui adapter: publish semantics alongside rendering
crates/taria-mcp      MCP bridge binary for agent harnesses
examples/demo-app     Demo ratatui app driven by an agent through taria
scripts/              Python verification harnesses (e2e, adversarial)
docs/                 Landscape research, architecture, integration guide
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

## Compatibility

`PROTOCOL_VERSION` is 1 and the wire format is frozen. Within version 1,
changes are additive: new optional fields, new message variants, new roles,
new action names. An older peer ignores fields it does not know, skips a
message it cannot parse, and degrades an unknown role to `other` and an
unknown action to a custom action keeping its name. So an app built against
a later taria stays readable by an agent built against this one, at the cost
of one degraded field rather than the whole tree.

Removing a field, renaming one, making an optional field required, or
changing what an existing field means bumps the version. `wire.rs` in
`crates/taria` is the normative statement of the rule, and
`docs/architecture.md` explains it.

## Limitations

- The adapter serves one bridge client per app at a time.
- An app on a different `protocol_version` can still be read for as long as
  its snapshots parse, which is the common case rather than a guarantee: a
  version bump is defined by the changes that break parsing, so a peer whose
  snapshot shape moved leaves the bridge with no tree at all and `read_tree`
  reports that none has arrived. Every input tool refuses either way,
  because the app cannot parse the input messages this bridge writes.
- Unix only for now: the transport is a Unix domain socket. Linux is the
  tested platform.
- Socket paths are capped at 107 bytes by AF_UNIX. Set `$TARIA_SOCK` to a
  shorter path when the default is too long.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
