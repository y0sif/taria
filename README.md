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

> Status: pre-alpha, at version 0.1.0. The vertical slice works end to end: a
> ratatui adapter, an MCP bridge, and a demo app an agent can drive today. The
> wire format is frozen at `PROTOCOL_VERSION` 1. See `CHANGELOG.md` for what
> this release changed and what the freeze promises, `docs/landscape.md` for
> why this project exists, `docs/architecture.md` for how the pieces fit, and
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

What an app can say about a widget is a fixed vocabulary: 29 roles (`list`,
`tree`, `table`, `text_input`, `select`, `dialog`, `log`, `chart`,
`scrollbar`, `terminal` and the rest) and 7 actions (`activate`, `focus`,
`select`, `toggle`, `scroll`, `set_value`, `dismiss`), plus a custom action
for anything an app names itself. Both vocabularies are open: a peer that
meets a role or an action it does not know degrades that one field instead of
failing the tree, which is what lets either grow inside a frozen format.

`docs/architecture.md` covers the wire protocol, the role and action
vocabularies, acknowledgement, socket lifecycle, and focus contract in detail.
`docs/integration-guide.md` is the guide to retrofitting taria into a ratatui
app you already have, including which role to reach for.

## MCP tools

| Tool | What it does |
|---|---|
| `read_tree` | Returns the app's current semantic tree as JSON: node ids, roles, labels, values, focus, and the actions each node advertises. The app's own snapshot, relayed, so a role or field this bridge has never heard of arrives under its real name. |
| `act` | Invokes an advertised action on a node by id, with an optional value (e.g. for `set_value`), up to 4096 characters. The node id and the action are checked against the latest tree before anything is sent. |
| `key` | Sends a raw key press (`"q"`, `"enter"`, `"ctrl+c"`), up to 64 times with `repeat`. A key that does not match the grammar is rejected here rather than swallowed by the app. A fallback for parts of the UI without semantic coverage. |
| `type_text` | Types a literal string in one call instead of one `key` call per character, up to 4096 characters. It goes where the app puts typing, never through the app's key bindings, so focus the target first; an app accepting no typing reports it ignored rather than acting on the characters. |

The three input tools wait up to 500 ms for the app's answer and report what
actually happened: the updated tree, an input the app deliberately ignored
(with the current tree to re-plan from), an input the app acknowledged and did
not survive (an advertised `quit`, working), or an error for an input that was
dropped, never applied, or left unaccounted for by an app that went away
before acknowledging it.

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

The label is interpolated into a file name, so it has to be one. Both sides
refuse a label carrying a path separator, or `.`, `..` or empty, because the
app-side adapter binds and unlinks whatever the label resolves to. `--socket`
is how to name a path.

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
python3 scripts/e2e.py           # end-to-end: 20 steps, demo app + bridge + MCP
python3 scripts/adversarial.py   # 18 edge-case probes
```

CI runs this gate, with `cargo build --workspace` in place of `cargo check`.
Both scripts use only the Python standard library and both build the debug
binaries they test. `--no-build` skips the build and keeps the freshness
check: it refuses a `target/debug` older than the sources, because a stale
binary makes every result a report about a build nobody asked for.

## Compatibility

`PROTOCOL_VERSION` is 1 and the wire format is frozen. Within version 1,
changes are additive: new optional fields, new message variants, new roles,
new action names. An older peer ignores fields it does not know, skips a
message it cannot parse, and degrades an unknown role to `other` and an
unknown action to a custom action keeping its name. So an app built against
a later taria stays readable by an agent built against this one, at the cost
of one degraded field rather than the whole tree.

Reading through the bridge keeps more than that. It relays the app's own
snapshot instead of re-serializing its parse of it, so an unknown role, action
or field reaches the agent under its real name; the degraded parse is what the
bridge validates and compares against, not what the agent reads.

In Rust the same promise is `#[non_exhaustive]` on the eleven types a version-1
addition can reach, from `Role` and `Action` to the two wire message enums,
and on the six struct-like variants inside them, where a new optional field
would land. So a new role, key, field or message variant costs an app that
integrated taria a recompile rather than a repair, in exchange for building
messages through their constructors and ending a destructuring pattern with
`..`. An input kind this build cannot read parses as `AgentInput::Unknown`,
which keeps the input's id, so the app can still acknowledge it instead of
leaving the agent waiting.

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
- Socket paths are capped by AF_UNIX at the platform's `sun_path` minus the
  terminating NUL: 107 bytes on Linux, 103 on macOS and the BSDs. Set
  `$TARIA_SOCK` to a shorter path when the default is too long, inside a
  directory only you can reach: the adapter binds only under a directory you
  own that grants no group or other access, so `/tmp` is refused.
- Trees are capped at `MAX_NODE_DEPTH`, 32 levels, because a snapshot past
  that exceeds what a JSON parser will recurse into and would arrive as
  nothing. The adapter cuts deeper branches at publish and tells the app.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
