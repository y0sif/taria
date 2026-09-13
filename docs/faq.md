# taria FAQ

The questions that come up before adopting taria, in the words people search
with.

**Is taria "ARIA for terminals"?**

That is the analogy it is built on. A web page exposes an accessibility tree
so a screen reader does not have to guess at the pixels, and taria does the
same thing for a TUI: roles, labels, values, focus, and the actions available
right now. The similarity is the shape of the idea rather than the standard,
and taria is not affiliated with the W3C or with ARIA. The vocabulary is
taria's own: 29 roles and 7 actions, shaped by a census of 15 real ratatui
apps rather than ported from the ARIA role list.

**How is taria different from ht or tmux `send-keys`?**

ht, tmux, and the PTY and MCP drivers around them work outside the app: they
run it under a pseudo-terminal, read the rendered screen, and send keystrokes.
That works on any program, including ones you did not write. taria works
inside the app: the app itself publishes its widget tree, so an agent acts on
a node id and an advertised action rather than a guessed keymap, and the app
acknowledges each input instead of the agent diffing two screens. The tradeoff
is total: taria reaches only apps whose authors adopted it.
`docs/comparison.md` does this properly, including when to use the other
thing.

**Can AI agents drive a TUI I did not write?**

Not with taria. If you cannot patch the app and ship the patch, screen-level
is the only thing that works, and tmux or ht is the right tool. taria is for
the app you control, and it composes with the rest: nothing stops an agent
reading a taria tree for one app and capturing a tmux pane for another.

**Do I have to use MCP, or can I speak the taria protocol directly?**

MCP is a convenience. `taria-mcp` is one client of a plain protocol: ndjson
over a Unix domain socket, one JSON object per line. Anything that can open a
socket can read snapshots and send input without MCP in the picture.
`docs/protocol.md` specifies every message, so an MCP TUI bridge of your own,
in another language, is a matter of writing one.

**Does my app have to be ratatui?**

The adapter that exists today is `taria-ratatui`, so ratatui is the path with
no work in front of it. The protocol itself knows nothing about ratatui or
Rust, and `docs/protocol.md` is written for someone building an adapter for
another framework. Adapters for Bubble Tea, Textual and Ink are wanted and
not written.

**What does adopting taria cost an app author?**

Five edits: bind a layer in `main`, write a function that turns your state
into nodes, publish it after each draw, drain agent input around the blocking
call in your event loop, and acknowledge the inputs you deliberately ignore.
It touches neither your rendering nor your state, and it is one direct
dependency: `taria-ratatui` pulls in the core crate and its one dependency
`serde`, plus `serde_json` and the ratatui you already had. If the socket cannot be bound the layer is inert and
the app runs exactly as it did before, so taria cannot keep your app from
starting. `docs/integration-guide.md` is the walkthrough, with the mistakes
two real retrofits made.

**Does taria work on Windows?**

No. The adapter is built on unix-only APIs, and the transport is a Unix
domain socket bound through them. Linux is the tested platform and
the only one CI runs the suites on; the release workflow builds `taria-mcp`
for macOS, and the AF_UNIX path limit is handled per platform, but nothing
exercises macOS end to end. Windows is not supported.

**Can two agents connect to the same app at once?**

No. The adapter serves one bridge client at a time. A second bridge's
connection is completed by the kernel and then never served, so it sits there
receiving no handshake and no snapshot; `read_tree` on that bridge says the
socket is held by another client rather than sending you back to check the
path. Multiple simultaneous clients are on the deferred list in
`docs/architecture.md`.

**Is the protocol stable?**

The wire format is frozen at `PROTOCOL_VERSION` 1, and changes within it are
additive: new optional fields, new message variants, new roles, new action
names. A peer that meets a role or action it does not know degrades that one
field instead of failing the tree. The crates are pre-alpha and their Rust
APIs can still move under semantic versioning; the format is the part that
made a promise.

**What happens to the parts of my UI I have not annotated?**

Nothing, which is the point. They render as they always did, a person uses
them as they always did, and they are simply absent from the tree. An agent
reaching one falls back to the raw `key` tool, which the integration guide
wires into the same handler a person's keystroke takes, so it lands wherever
focus is. That is why an app with three annotated nodes is already useful,
and why annotating is something you do a widget at a time.

**Why does backgrounding the demo with `&` fail?**

Crossterm needs a real terminal for raw mode, so a detached process exits
immediately. The README's
[running the demo headless](../README.md#running-the-demo-headless) gives it a
terminal with tmux instead.
