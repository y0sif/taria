# How taria compares

You are probably here because you want an AI agent to use a terminal
application, and you have already found several ways to do it. This page says
what each one does, where taria sits, and when the honest answer is to use
something else.

There are two places to solve this problem. Outside the app: run it under a
pseudo-terminal, read the rendered screen, send keystrokes. Inside the app:
have the app itself say what it is showing and what can be done with it right
now. Every tool in the table below except taria works outside. taria works
inside. That single difference decides everything else, including the problems
taria cannot touch at all.

## The tools

| Tool | What it does | Side | Works with |
|---|---|---|---|
| [ht](https://github.com/andyk/ht) | Runs a command under a headless VT100 emulator, takes JSON commands on stdin, serves the screen over a WebSocket. Built explicitly to make terminals easy for LLMs. | Outside | Any program |
| tmux, with `send-keys` and `capture-pane` | Detached sessions, virtual keystrokes, a text dump of the pane. The incumbent, widely installed, and stable for years. | Outside | Any program |
| tmux and PTY MCP servers ([tmux-mcp](https://github.com/bnomei/tmux-mcp), [tmux-mcp-server](https://github.com/lox/tmux-mcp-server), [terminal-control-mcp](https://github.com/wehnsdaefflae/terminal-control-mcp), pty-mcp) | Wrap tmux or a raw PTY as MCP tools, so a harness reaches the screen through the same channel it reaches everything else. | Outside | Any program |
| [agent-tui](https://github.com/pproenca/agent-tui) | PTY daemon serving screenshots and input over a CLI and JSON-RPC. | Outside | Any program |
| [tui-use](https://github.com/onesuper/tui-use) | Screen capture plus keystrokes, packaged for agents. | Outside | Any program |
| [agent-terminal](https://github.com/jasonkneen/agent-terminal) | Headless terminal automation built on node-pty. | Outside | Any program |
| **taria** | The app publishes its widget tree, focus, and the actions available right now. An MCP bridge hands that tree to a harness and carries input back, with a per-input acknowledgement saying what the app did with it. | Inside | Apps whose authors adopted it |

## What the line costs on each side

A screen-level tool sees what a person sees: a grid of characters. That is a
lot, and it is everything for a program you did not write. What it does not
carry is why any of it is there. A highlighted row and a selected row look the
same. A dialog and a bordered panel look the same. Whether `d` deletes
something depends on which widget has the keyboard, and the screen does not
say which one does. So an agent infers structure from layout, aims keystrokes
at a keymap it guessed, and decides whether an input worked by comparing two
screens, which reports any unrelated repaint as success.

taria removes the inference by asking the app. A node has an id that is an
identity rather than a screen position, a role, a label, a value, whether it
holds the keyboard, and the list of actions it will accept at this moment. An
agent acts by naming a node and one of its advertised actions, and the app
answers that specific input `delivered`, `dropped`, or `ignored`. The price is
that none of it exists until the app's author puts it there.

## When to use something else

**The app is not yours.** This is the big one. taria cannot read `htop`,
`vim`, `psql`, or a vendor binary you have no source for. If you cannot patch
the app and get your patch shipped, screen-level is not a compromise, it is
the only thing that works. Use tmux, ht, or one of the PTY MCP servers.

**You need the rendered output itself.** Colours, box drawing, a progress bar,
an ASCII chart, the exact layout: taria publishes a tree, not pixels or cells.
Node geometry is not in the protocol. A screen tool is the right instrument
for anything about appearance.

**You are driving a shell rather than an app.** Running commands, reading
their output, pasting into a REPL, moving through a pipeline: that is a
terminal session, not a widget tree. tmux is good at it.

**You are on Windows.** taria's transport is a Unix domain socket. Linux is
the tested platform, and Windows is not supported.

**You need several agents on one app.** The adapter serves one bridge client
at a time.

**The app already has a real API.** If the thing you want is available over
HTTP, a CLI flag, or a library call, use that. A TUI is a human interface, and
going through one is worth doing when it is the only interface there is.

## When taria is the right answer

**You write the app, or you can send it a patch.** That is the whole gate. On
the other side of it, the agent stops guessing.

**You want an agent to act on identities, not positions.** A node id is stable
across a delete, a sort, and a restart. An agent that read row 3 and then acts
on row 3 in a screen tool acts on whatever moved into that slot.

**You want to know whether an input landed.** Every input carries an id and
the app answers it. Ignored is a first-class answer, so an agent that acted at
the wrong moment hears about it in one round trip and re-plans from the tree
it was given, instead of reading a changed screen as confirmation.

**You want the agent to know what is possible right now.** Actions are
advertised per node, per frame. A dialog that blocks everything behind it
advertises nothing behind it. That is also what keeps an agent out of a state
it cannot leave.

**Your app is one an agent has real reason to use.** Deployment tools,
database clients, dashboards, issue trackers, and test runners are where the
difference shows up. A thing an agent reads once is not worth annotating.

## They compose

taria is not a replacement for tmux, and the two are not competing for the
same slot. The demo in this repository is normally run under tmux, because a
TUI needs a real terminal and tmux is how you give a background process one.
An agent can hold both: read the tree through taria for structure and
verdicts, capture the pane when it needs to see what was drawn.

The same holds for the harness. If an agent harness ships its own interactive
terminal tool, that tool is screen-level too, and it can consume a taria tree
from an app that publishes one.

## Partial adoption is the normal case

Annotating an app is incremental and is meant to be. Publish the three nodes
an agent actually needs and stop. Anything you have not annotated is simply
absent from the tree: it still renders, a person still uses it, and an agent
reaching it falls back to the raw `key` tool, which lowers a keystroke into
your key handler exactly as a person's would. That fallback is why an app with
one annotated widget is already more useful to an agent than an app with none.

The fallbacks are a floor, not the path. A keystroke is a guess about a
keymap, and an advertised action survives a UI change that a keymap does not.

## What taria is not

It does not spawn your app, own its PTY, read its screen, or sit between it
and the terminal. It binds a Unix socket, publishes a tree after each frame,
and hands agent input to your event loop like any other input source. If taria
cannot bind, the layer is inert and the app runs exactly as it did before.

Today the adapter is for [ratatui](https://ratatui.rs). The protocol itself is
framework-agnostic and specified independently of Rust in
[docs/protocol.md](protocol.md), so an adapter for another framework in
another language is a matter of someone writing one.

## Further reading

- [docs/protocol.md](protocol.md): the wire format, for writing a peer.
- [docs/integration-guide.md](integration-guide.md): adding taria to a ratatui
  app you already have.
- [docs/architecture.md](architecture.md): why the protocol is shaped this way.
- [docs/landscape.md](landscape.md): the research this page is drawn from,
  including the published findings it rests on and the census behind the role
  vocabulary.
