# How taria compares

There are two places to let an AI agent use a terminal application. Outside
the app: run it under a pseudo-terminal, read the rendered screen, send
keystrokes. Inside the app: have the app itself say what it shows and what can
be done with it right now. Most tools below work outside, and taria works
inside, which decides everything else, including what taria cannot touch.

## The tools

| Tool | What it does | Side | Works with |
|---|---|---|---|
| [ht](https://github.com/andyk/ht) | Runs a command under a headless VT100 emulator, takes JSON commands on stdin, serves the screen over a WebSocket. Built to make terminals easy for LLMs. | Outside | Any program, on macOS and Linux |
| tmux, with `send-keys` and `capture-pane` | Detached sessions, virtual keystrokes, a text dump of the pane. The incumbent, widely installed and stable for years. | Outside | Any program |
| tmux and PTY MCP servers ([tmux-mcp](https://github.com/bnomei/tmux-mcp), [tmux-mcp-server](https://github.com/lox/tmux-mcp-server), [terminal-control-mcp](https://github.com/wehnsdaefflae/terminal-control-mcp), and others) | Wrap tmux or a raw PTY as MCP tools, so a harness reaches the screen through the same channel it reaches everything else. | Outside | Any program |
| [agent-tui](https://github.com/pproenca/agent-tui) | PTY daemon serving screenshots and input over a CLI and JSON-RPC. | Outside | Any program, on Unix only |
| [tui-use](https://github.com/onesuper/tui-use) | Screen capture plus keystrokes, packaged for agents. | Outside | Any program |
| [agent-terminal](https://github.com/jasonkneen/agent-terminal) | Headless terminal automation built on node-pty. | Outside | Any program |
| [Termwright](https://github.com/gorce-ai/termwright) | End-to-end tests for CLIs and TUIs in the style of Playwright (a browser test framework): runs the program under a PTY, sends keyboard and mouse input, asserts on the screen. Optional integrations for Ink, OpenTUI, Textual, tview, Bubble Tea and Ratatui report roles, labels and state; input still goes through the terminal. | Outside, with optional integrations inside | Any program; roles and state where an integration is installed |
| [molerat](https://github.com/orhun/molerat) | The app builds an AccessKit tree (AccessKit is an accessibility toolkit for Rust UIs). Optional features serve it to agents over MCP and to Linux screen readers over AT-SPI, the Linux desktop's accessibility bus, which taria does not reach. On crates.io. | Inside | Rust apps whose authors adopted it |
| **taria** | The app publishes its widget tree, focus, and the actions available right now. An MCP bridge hands that tree to a harness and carries input back, with a per-input acknowledgement saying what the app did with it. | Inside | Apps whose authors adopted it |

## What the line costs on each side

A screen-level tool sees what a person sees: a grid of characters. That is
everything for a program you did not write, but it does not carry why any of
it is there. A highlighted row and a selected row look the same, and whether
`d` deletes something depends on which widget has the keyboard, which the
screen does not say. So an agent infers structure from layout, aims
keystrokes at a keymap it guessed, and judges an input by comparing two
screens, which reports any unrelated repaint as success.

taria removes the inference by asking the app. A node has an id that is an
identity rather than a screen position, a role, a label, a value, whether it
holds the keyboard, and the actions it accepts at this moment. An agent acts
by naming a node and one of its advertised actions, and the app answers that
input `delivered`, `dropped` or `ignored`. The price is that none of it exists
until the app's author puts it there.

## When to use something else

**The app is not yours.** taria cannot read `htop`, `vim`, `psql`, or a
vendor binary you have no source for. If you cannot patch the app and get
your patch shipped, screen-level is the only thing that works: use tmux, ht,
or one of the PTY MCP servers.

**You need the rendered output itself.** Colours, box drawing, an ASCII
chart, the exact layout: taria publishes a tree, not cells, and node geometry
is not in the protocol.

**You are driving a shell rather than an app.** Running commands, reading
their output and pasting into a REPL is a terminal session, not a widget
tree. tmux is good at it.

**You need several agents on one app at once.** The adapter serves one
bridge at a time ([Limitations](compatibility.md#limitations)).

**The app already has a real API.** If what you want is available over HTTP,
a CLI flag or a library call, use that. A TUI is a human interface, worth
going through when it is the only interface there is.

## When taria is the right answer

**You write the app, or you can send it a patch.** That is the whole gate.

**You want an agent to act on identities, not positions.** A node id is
stable across a delete, a sort and a restart. An agent that read row 3 and
then acts on row 3 in a screen tool acts on whatever moved into that slot.

**You want to know whether an input landed.** Every input carries an id and
the app answers it. Ignored is a first-class answer, so an agent that acted at
the wrong moment hears about it in one round trip instead of reading a changed
screen as confirmation.

**You want the agent to know what is possible right now.** Actions are
advertised per node, per frame. A dialog that blocks everything behind it
advertises nothing behind it, which also keeps an agent out of a state it
cannot leave.

**Your app is one an agent has real reason to use**, such as a deployment
tool, a database client, a dashboard or a test runner.

## They compose

taria does not replace tmux. The demo in this repository runs under tmux
whenever it has to run without a terminal of its own, and an agent can hold
both: the tree through taria for structure and verdicts, the pane when it
needs to see what was drawn. A harness's own interactive terminal tool is
screen-level too, and it can consume a taria tree from an app that publishes
one.

## Partial adoption is the normal case

Annotating an app is incremental. Publish the three nodes an agent needs and
stop. Anything you have not annotated still renders and still works for a
person, and an agent reaching it falls back to the raw `key` tool, which
lowers a keystroke into your key handler as a person's would. The fallback is
a floor, not the path: a keystroke is a guess about a keymap, while an
advertised action is the app's own statement of what it accepts.

## What taria is not

It does not spawn your app, own its PTY, read its screen, or sit between it
and the terminal. It binds a Unix socket, or a named pipe on Windows,
publishes a tree after each frame, and hands agent input to your event loop
like any other input source. Today the adapter is for
[ratatui](https://ratatui.rs); the protocol is framework-agnostic and
specified in [docs/protocol.md](protocol.md).

Adding taria to an app is in [docs/integration-guide.md](integration-guide.md),
and how the pieces fit in [docs/architecture.md](architecture.md).
