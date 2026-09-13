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

> Status: pre-alpha, at version 0.2.0. The vertical slice works end to end: a
> ratatui adapter, an MCP bridge, and a demo app an agent can drive today. The
> wire format is frozen at `PROTOCOL_VERSION` 1. See `CHANGELOG.md` for what
> this release changed and what the freeze promises, `docs/comparison.md` for
> how this differs from tmux, ht and the other ways agents reach a TUI,
> `docs/architecture.md` for how the pieces fit, `docs/protocol.md` for the
> wire format itself, and `docs/integration-guide.md` for adding taria to an
> app you already have.

## Install

Two sides, two different things to install.

**Writing a TUI app that agents should be able to use.** One dependency:

```bash
cargo add taria-ratatui
```

It re-exports the protocol crate, so `taria::{Action, IdSpace, Node, Role}`
is reachable as `taria_ratatui::taria::{...}` without a second dependency.
`docs/integration-guide.md` walks through the retrofit.

**Driving an app that already speaks taria.** Install the bridge binary:

```bash
cargo install taria-mcp
```

Or take a prebuilt tarball from the
[releases page](https://github.com/y0sif/taria/releases): a tag builds
`taria-mcp` for x86_64 and aarch64 Linux and for both macOS architectures,
each with its sha256. Then point your harness at it, with the app's label:

```bash
claude mcp add taria -- taria-mcp --app <label>
```

The quick start below uses the demo app in this repository instead, which
needs nothing installed.

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

Backgrounding the demo with plain `&` fails. Give it a terminal with tmux
instead:

```bash
cargo build -p taria-demo
tmux new-session -d -s taria-demo -x 120 -y 34 './target/debug/taria-demo'
tmux capture-pane -p -t taria-demo   # peek at the screen without attaching
tmux kill-session -t taria-demo      # stop it when you are done
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

## MCP tools

| Tool | What it does |
|---|---|
| `read_tree` | Returns the app's current semantic tree as JSON: node ids, roles, labels, values, focus, and the actions each node advertises. The app's own snapshot, relayed, so a role or field this bridge has never heard of arrives under its real name. |
| `act` | Invokes an advertised action on a node by id, with an optional value (e.g. for `set_value`), up to 4096 characters. The node id and the action are checked against the latest tree before anything is sent. |
| `key` | Sends a raw key press (`"q"`, `"enter"`, `"ctrl+c"`), up to 64 times with `repeat`. A key that does not match the grammar is rejected here rather than swallowed by the app. A fallback for parts of the UI without semantic coverage. |
| `type_text` | Types a literal string in one call instead of one `key` call per character, up to 4096 characters. It goes where the app puts typing, never through the app's key bindings, so move the keyboard to the target first: act on its advertised `focus` action, or on `set_value`, which many apps focus as a side effect. An app accepting no typing reports it ignored rather than acting on the characters. |

The three input tools wait up to 500 ms for the app's answer and report what
actually happened: the updated tree, an input the app deliberately ignored
(with the current tree to re-plan from), an input the app acknowledged and did
not survive (an advertised `quit`, working), or an error for an input that was
dropped, never applied, or left unaccounted for by an app that went away
before acknowledging it.

## taria-mcp CLI

The bridge takes exactly one of `--socket <path>` or `--app <label>`. With
`--app`, it resolves the socket path the same way the app-side adapter does
when binding. The flags, the environment variables, the resolution order and
the rule on labels: [docs/cli.md](docs/cli.md).

## Workspace layout

```text
crates/taria          Core protocol types (widget tree, actions, snapshots)
crates/taria-ratatui  Ratatui adapter: publish semantics alongside rendering
crates/taria-mcp      MCP bridge binary for agent harnesses
examples/demo-app     Demo ratatui app driven by an agent through taria
scripts/              Python verification harnesses (e2e, adversarial)
docs/                 Protocol spec, architecture, integration guide,
                      comparison, landscape research
```

In `docs/`: `protocol.md` specifies the wire format message by message,
`architecture.md` explains why it is shaped that way, and `landscape.md` is
the research behind both.

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --check
python3 scripts/e2e.py           # end-to-end: 21 steps, demo app + bridge + MCP
python3 scripts/adversarial.py   # 20 edge-case probes
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers the rest: the setup, the CI gate,
the commit style, and what a change has to clear while version 1 of the wire
format is frozen.

## Compatibility

See [docs/compatibility.md](docs/compatibility.md) for the full list. The
three that decide whether taria fits at all:

- **Unix only for now**: the adapter is built on unix-only APIs, and Linux is
  the tested platform. [Limitations](docs/compatibility.md#limitations).
- **One bridge client per app at a time**: the adapter serves one, so two
  agents cannot drive the same app at once.
  [Limitations](docs/compatibility.md#limitations).
- **The wire format is frozen at `PROTOCOL_VERSION` 1**: changes within
  version 1 are additive, so a version difference costs a degraded field
  rather than the whole tree.
  [What the freeze promises](docs/compatibility.md#compatibility).

---

## [How taria compares](docs/comparison.md)

## [FAQ](docs/faq.md)

---

## License

Dual-licensed under MIT or Apache-2.0, at your option.
