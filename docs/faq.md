# taria FAQ

The questions that come up before adopting taria, in the words people search
with.

**Is taria "ARIA for terminals"?**

Yes: ARIA for terminals is the analogy taria is built on. A web page exposes
an accessibility tree so a screen reader does not have to guess at the
pixels, and taria does the same for a TUI: roles, labels, values, focus,
typed state such as selected, checked or disabled, and the actions available
right now. The similarity is the shape of the idea rather than the standard,
and taria is not affiliated with the W3C or with ARIA. The vocabulary is
taria's own, 29 roles and 7 actions drawn from what ratatui apps put on screen
rather than ported from the ARIA role list.

**How is taria different from ht or tmux `send-keys`?**

taria works inside the app, where ht and tmux work outside it. The app itself
publishes its widget tree, so an agent acts on a node id and an advertised
action rather than a guessed keymap, and the app acknowledges each input. ht,
tmux and the PTY and MCP drivers around them run the app under a
pseudo-terminal, read the screen and send keystrokes, which works on any
program, while taria reaches only apps whose authors adopted it.
[docs/comparison.md](comparison.md) covers it, including when to use the
other thing.

**Can AI agents drive a TUI I did not write?**

An app you cannot patch needs a screen-level tool such as tmux or ht, since
taria reaches only apps whose authors adopted it
([When to use something else](comparison.md#when-to-use-something-else)).

**Do I have to use MCP, or can I speak the taria protocol directly?**

You can speak the protocol directly; MCP is a convenience. `taria-mcp` is one
client of a plain protocol: ndjson over a Unix domain socket, or a named pipe
on Windows, one JSON object per line. Anything that can open one can read
snapshots and send input. [docs/protocol.md](protocol.md) specifies every
message, so a bridge of your own, in another language, is a matter of writing
one.

**Does my app have to be ratatui?**

Today, yes: the adapter that exists is `taria-ratatui`. The protocol itself
knows nothing about ratatui or Rust, and [docs/protocol.md](protocol.md) is
written for someone building an adapter for another framework. Adapters for
Bubble Tea, Textual and Ink are wanted and not written.

**What does adopting taria cost an app author?**

Adopting taria takes five edits and one dependency.
[docs/integration-guide.md](integration-guide.md) is the walkthrough.

**Does taria work on Windows?**

taria works on Windows: the app and the bridge talk over a named pipe instead
of a Unix socket, carry the same JSON lines, and the pipe is owned by your
account and open to nobody else. The commands are the same as on Linux and
macOS ([install](../README.md#install)), and what differs per platform is in
[docs/compatibility.md](compatibility.md#what-works-where).

**Can two agents connect to the same app at once?**

One agent drives an app at a time: the adapter serves one bridge, and a
second bridge is told the app is held by another bridge. Agents take turns
instead: an idle `taria-mcp` lets the app go after 60 s by default, and the
waiting one retries ([docs/cli.md](cli.md#connecting-on-demand)). Multiple
simultaneous clients are on the deferred list in
[docs/architecture.md](architecture.md#deferred).

**Is the protocol stable?**

The wire format is frozen and only grows, while the Rust API is not frozen
yet. [docs/compatibility.md](compatibility.md#compatibility) states the
promise and what each version pairing gets.

**What happens to the parts of my UI I have not annotated?**

Unannotated parts keep working for a person and stay out of the tree, and an
agent reaching one falls back to the raw `key` tool
([Partial adoption is the normal case](comparison.md#partial-adoption-is-the-normal-case)).

**Why does backgrounding the demo with `&` fail?**

Crossterm needs a real terminal for raw mode, so a detached process exits
immediately. The README's
[running the demo headless](../README.md#running-the-demo-headless) gives it a
terminal with tmux instead.
