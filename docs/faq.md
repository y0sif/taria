# taria FAQ

The questions that come up before adopting taria, in the words people search
with.

**Is taria "ARIA for terminals"?**

That is the analogy it is built on. A web page exposes an accessibility tree so
a screen reader does not have to guess at the pixels, and taria does the same
thing for a TUI: roles, labels, values, focus, typed state such as selected,
checked or disabled, and the actions available right now. The similarity is the
shape of the idea rather than the standard, and taria is not affiliated with the
W3C or with ARIA. The vocabulary is taria's own: 29 roles and 7 actions, shaped
by a census of 15 real ratatui apps rather than ported from the ARIA role list.

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
over a Unix domain socket, or a named pipe on Windows, one JSON object per
line. Anything that can open one can read snapshots and send input without
MCP in the picture.
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
`serde`, plus `serde_json`, the ratatui you already had, and on Windows only
`windows-sys`, the raw Win32 bindings. The same dependency builds on Linux,
macOS and Windows, so nothing needs a `cfg` around it. If taria cannot bind
its socket, or its pipe on Windows, the layer is inert and the app runs
exactly as it did before, so taria cannot keep your app from starting.
`docs/integration-guide.md` is the walkthrough, with the mistakes two real
retrofits made.

**Does taria work on Windows?**

Yes, from 0.4.0. On Windows the app and the bridge talk over a named pipe (a
kernel object named under `\\.\pipe\`, which two processes open like a file)
instead of a Unix domain socket, and carry the same JSON lines over it, byte
for byte. The adapter creates `\\.\pipe\taria-<your SID>-<label>`, where the
SID is the string Windows names your account by, and makes it yours alone:
owned by you, open to nobody else, refusing clients on other machines.
`taria-mcp --app <label>` derives the same name, and checks that the pipe is
owned by you before it writes anything to it or parses anything it sent.
Install the bridge with `cargo install taria-mcp`, or take the Windows `.zip`
from the releases page.

CI runs the Rust suites on Linux, macOS and Windows, including a test that
drives a real adapter from the real `taria-mcp` binary on each. The two
Python scripts, `scripts/e2e.py` and `scripts/adversarial.py`, drive the demo
through a pseudo-terminal and run on Linux and macOS only. Locally the
Windows build was run under Wine (`scripts/wine-test.sh`, see
`CONTRIBUTING.md`), which is a stand-in: the Windows CI job is the word that
counts.

What Windows lacks is the stopped-app diagnosis. On Linux the bridge reads
`/proc` to tell an app stopped by Ctrl-Z, a signal or a debugger from one
held by another bridge, and names it with its pid. Windows has no `/proc`:
there an app suspended while it listens shows as a connection that stays
silent, which the bridge reports as the app suspended or stuck, without a
pid, and one suspended after a session cannot listen again, so its pipe
reads as busy, and the held answer allows that it may be suspended or
stuck. macOS has no `/proc` either, and reports a stopped app as held.

**Can two agents connect to the same app at once?**

No. The adapter serves one bridge client at a time. A second bridge's
connection is completed by the kernel and not served until the first client
leaves, so until then it sits there receiving no handshake and no snapshot;
`read_tree` on that bridge says the app is held by another bridge rather than
sending you back to check the path. (On Linux, an app that is stopped, by
Ctrl-Z, a signal or a debugger, is named as stopped instead, with its pid,
since it leaves the same silence. A TUI suspended as a job in its terminal is
resumed only by `fg` there, so the agent is told to ask you to run it.)
Anything it wrote while it waited is thrown away when its turn comes, because
it was written before it had seen a tree. taria-mcp sends nothing until it has
a tree, so a second agent loses nothing. On Windows a pipe has no such queue:
the second bridge's open is refused as busy, it has nothing to write into,
and `read_tree` says the app is held, with the same retry advice. Multiple
simultaneous clients are on the deferred list in `docs/architecture.md`.

They can take turns, though. Since 0.3.2, taria-mcp opens no connection until
its first tool call, and lets the app go after 60 s with no tool calls. A
harness starts every registered server in every session, so a session that
never calls a taria tool, or has stopped calling them, no longer keeps the app
from the one that wants it. The waiting bridge's answer says when the holder
lets go, so the agent retries rather than asking you to kill a process: after
60 s idle on the default, and after the waiting bridge's own window when it
was started with another, since a holder started from the same `.mcp.json` has
the same one. `--idle-release` changes the 60 s (`docs/cli.md`). A bridge that
let go and came back says, in every answer it gives once reconnected (the app
taken, stopped, not publishing yet, or sending a line it cannot read), that
its own disconnect was that release, not a fault, after its window or up to
30 s more if an input was awaiting its ack. A bridge older than 0.3.2 still
holds the app until its session ends; until every session runs the new one,
disable taria with `/mcp` in the Claude Code sessions that do not drive the
app.

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
