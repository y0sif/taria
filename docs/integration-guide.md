# Integration guide

This guide adds taria to a ratatui app you already have, in five edits. What
to put in the tree is in [tree-guide.md](tree-guide.md), and the wire format
in [protocol.md](protocol.md).

1. Bind a layer in `main`.
2. Write a `tree.rs` that turns app state into nodes.
3. Publish it after each draw.
4. Drain agent input around the blocking call in your event loop.
5. Acknowledge the inputs you deliberately ignore.

## Add the dependency

Add `taria-ratatui` as in the [README](../README.md#install), and `taria` too
to write `taria::Node` rather than `taria_ratatui::taria::Node`. The adapter
brings `serde`, `serde_json` and, on Windows only, `windows-sys`. Your app has
to be on the adapter's ratatui, and the bridge on the app's `PROTOCOL_VERSION`
([compatibility.md](compatibility.md#compatibility)).

The same dependency builds on Linux, macOS and Windows, so nothing needs a
`cfg` around it: the layer serves a Unix domain socket on the first two and a
named pipe on Windows. To track `main`, depend on
`taria-ratatui = { git = "https://github.com/y0sif/taria" }` and pin a
revision.

## Bind without risking startup

```rust
let mut layer = TariaLayer::bind_or_disabled("my-app"); // never fails
// Print before `ratatui::init()` takes the alternate screen.
match layer.bind_error() {
    Some(err) => eprintln!("my-app: taria disabled: {err}"),
    None => eprintln!("my-app: taria at {}", layer.socket_path().display()),
}
let mut terminal = ratatui::init();
```

If the socket (a pipe on Windows) cannot be bound, `bind_or_disabled` returns
an inert layer whose every method stays callable, so taria cannot keep your
app from starting; `bind` returns the error instead, and `bind_at` and
`bind_or_disabled_at` take a path of your own. `is_enabled()` says whether
the layer is serving, and `app_label()` returns the label, which also labels
the tree's root. The layer never prints; your app decides when to.

## Build the tree in its own module

Put node building in a function of `&App`, and publish after each draw:

```rust
// tree.rs
pub fn build_nodes(app: &App) -> Vec<Node> {
    vec![
        Node::new("input", Role::TextInput)
            .label("New task")
            .value(app.draft.clone())
            .focused(app.focus == Focus::Input)
            .actions([Action::SetValue, Action::Activate]),
        list_node(app),
    ]
}
// In the event loop:
terminal.draw(|frame| view(&app, frame))?;
layer.publish(tree::build_nodes(&app));
```

A pure builder makes every invariant a unit test with no terminal. The
alternative records nodes as widgets render: `let rec = layer.frame()`,
`sem(&rec, widget, node)` around each widget inside the draw, then
`rec.publish()`, without which the recorded frame publishes nothing. Either
way the nodes sit under an auto-generated `app` root.

## Apps that draw only a panel

Some apps leave their output in the terminal's scrollback and have ratatui
draw only a panel under it. `frame()` and `sem` then see only the panel, so
build the output's node from the state that holds it, and publish on every
pass of the loop, since a printed line changes the tree without a redraw.
Publish a bounded tail of the output as a `Role::Log`
([Keep the snapshot small enough to arrive](tree-guide.md#keep-the-snapshot-small-enough-to-arrive)),
bind before raw mode, and print `bind_error()` only once the panel is down.

## Drain agent input around the blocking call

```rust
while app.running {
    drain_agent_input(&mut app, &layer);
    terminal.draw(|frame| view(&app, frame))?;
    layer.publish(tree::build_nodes(&app));
    match rx.recv() {
        Ok(event) => update(&mut app, event),
        Err(_) => break,
    }
    drain_agent_input(&mut app, &layer);
}
```

`fn drain_agent_input(app: &mut App, layer: &TariaLayer)` is one call,
`layer.drain(|input| apply_agent_input(app, input))`. The drain after `recv`
applies input that arrived while the loop was parked; the one at the top
catches input that arrived during the last draw. Publish every frame, not
only when the tree changed: an unchanged tree costs a comparison, and after a
dequeue the layer resends it stamped at the input, which answers an input
that changed nothing at once.

**The first tree you publish after a drain is the answer.** The layer stamps
each snapshot with the newest input it has handed you, and the bridge answers
an input with the first tree stamped at it. So apply what you drained before
the next publish. If the work takes long enough to show, publish a status
first, a `Role::Status` node or a `disabled` control, then the result.

The loop has to wake on its own, or agent input waits until a person touches
a key. Run a tick, as the demo does every 50 ms, or use
`layer.recv_timeout(Duration::from_millis(50))` as the loop's clock; it waits
out the timeout even on a disabled layer. It and `try_recv` hand back each
input with its `InputId`, for acking it yourself.

## Give things identities, not positions

An agent reads the tree, decides, and acts on an id from that read, so
nothing in between may change what the id names. With a task's index as its
id, deleting one task shifts every later id, and the next act lands on the
wrong row. Assign an identity at creation, never reuse it, make it the same
across restarts where you can, and let `IdSpace` hold its one spelling:

```rust
pub const TASK_IDS: IdSpace = IdSpace::new("task");
Node::new(TASK_IDS.id(task.id), Role::ListItem) // in tree.rs
// In update.rs: an old id names the same task or nothing.
let Some(id) = TASK_IDS.parse::<TaskId>(node).filter(|id| app.task(*id).is_some())
else {
    return InputStatus::Ignored;
};
```

## Acknowledge what you ignore

The layer acks `Delivered` as it hands each input over, which says only that
your loop dequeued it. When you deliberately do nothing with an input, an act
a modal blocks, an id you no longer know, say so, or the agent reads the
unchanged tree as "it may not have reacted yet".

`drain` sends each `Ignored` your handler returns, and the last ack wins. An
act on a node that has gone, such as a dialog button after the dialog
closed, is `Ignored` too.

**Ack `Ignored` before you publish your next frame.** Acks go out ahead of
the next snapshot, and that snapshot answers the input, so an `Ignored` sent
a frame later arrives after the agent has its answer. Draining before drawing
gives you this. To hold the `InputId`, loop on `try_recv` and ack in the same
pass:

```rust
while let Some((id, input)) = layer.try_recv() {
    if apply_agent_input(app, input) == InputStatus::Ignored {
        layer.ack(id, InputStatus::Ignored);
    }
}
layer.publish(tree::build_nodes(app)); // every Ignored is queued ahead
```

A verdict that comes only after some work has run cannot be acked in time:
publish a status node saying the input is under way, then the outcome.

## The raw inputs, and what they are for

`key` sends one raw key press, up to 64 times with `repeat`, and `type_text`
sends a literal string. They keep partial coverage useful, but neither should
be the primary path: an advertised action survives a UI change, and a
keystroke is a guess about a keymap. A key goes into your key handler, through
`to_crossterm_key`, and lands wherever focus is. Text goes wherever your app
puts typing, which is sometimes nowhere:

```rust
#[warn(clippy::wildcard_enum_match_arm)]
pub fn apply_agent_input(app: &mut App, input: AgentInput) -> InputStatus {
    match input {
        AgentInput::Act { node, action, value, .. } => apply_act(app, &node.0, action, value),
        AgentInput::Key { key, .. } => apply_key(app, &key), // Ignored where to_crossterm_key is None
        AgentInput::Text { text, .. } => apply_text(app, &text),
        AgentInput::Unknown => InputStatus::Ignored, // own arm, for the lint
        _ => InputStatus::Ignored, // required: `#[non_exhaustive]`
    }
}
```

Every type a version-1 addition can reach is `#[non_exhaustive]`, so a new
key, field or input kind costs a recompile, paid for with a `..` per pattern,
a wildcard arm per enum, and constructors such as `AgentInput::text`. An
input kind your build cannot read is acked `Ignored` by the layer itself.

### The arm the compiler asks for hides `Text`

**The wildcard arm you add to satisfy the compiler is the arm that hides
`AgentInput::Text`, so check every wildcard you add for `Text` before you
trust a green build.** A `match` naming only `Act` and `Key` builds, passes
default clippy, and answers every `type_text` from its wildcard. Scope
clippy's `wildcard_enum_match_arm` to the function, as above, and give
`Unknown` its own arm, since the lint does not look inside `Unknown | _`.
Make the wildcard answer `Ignored`, so a swallowed `Text` reads as a refusal.

Lowering `key` into your handler also lets an agent reach chords you never
bound: a handler that matches `KeyCode` alone reads `ctrl+q` as `q`. Treat a
press as the plain binding only when `key.modifiers.is_empty()`. Typing is
the exception: a terminal reports a capital as `Char('A')` with shift, so
allow shift where a press becomes a character, and nowhere else.

## Typed text goes where your app puts typing

Typed characters are not commands. Lowered through the key handler,
`type_text("deploy")` on the demo's list would open the delete dialog with
`d` and confirm it with `y`. Route text to whatever your app puts typing
into, and report `Ignored` when nothing is:

```rust
fn apply_text(app: &mut App, text: &str) -> InputStatus {
    let mut typed = false;
    for key in text_to_keys(text) {
        // Checked per character: Enter submits and hands the keyboard back.
        if app.dialog.is_some() || app.focus != Focus::Input { break; }
        handle_input_key(app, key); // text entry, not the key bindings
        typed = true;
    }
    if typed { InputStatus::Delivered } else { InputStatus::Ignored }
}
```

The app decides where typing goes: a text field, the field with the
keyboard, or a surface that grades keystrokes. `text_to_keys` lowers `\n` to
Enter and `\t` to Tab, which suits a text field; a surface that grades
characters should walk `text.chars()` itself.

### Aiming typed text at a surface

An agent aims `type_text` with an action, and the tree is the only place it
can learn which one. Advertise `Action::Focus` on the surface that takes
typing while the keyboard is elsewhere (the sample is in
[Advertise a way out of every state](tree-guide.md#advertise-a-way-out-of-every-state)),
and answer it `Ignored` when the keyboard is already there. Advertising it
promises that the act puts the keyboard on this node and that the next
snapshot shows it focused.

`set_value` is not a focus call: an agent that wants only the keyboard would
have to overwrite the field to get it. Answer a `set_value` carrying no value
`Ignored` rather than clearing the draft.

## Socket paths

On Linux and macOS the layer binds a Unix socket. The default path,
`$TARIA_SOCK`, and the rule that a label is a plain file name are in
[cli.md](cli.md#socket-path). A refused label makes `bind` return an error
and `bind_or_disabled` come back disabled, with an empty `socket_path()`.

A socket path has a platform limit
([compatibility.md](compatibility.md#what-works-where)) that a deep
`$XDG_RUNTIME_DIR` or a long label reaches, on macOS first, and the error
names the path, its length and the limit. The way out is a shorter path, such
as `TARIA_SOCK=$XDG_RUNTIME_DIR/t.sock`, set for both the app and `taria-mcp`.

The parent directory is created with mode `0700` and vetted before binding:
a real directory, owned by you, with no group or other permission bits. So
`/tmp` is refused; `$XDG_RUNTIME_DIR` is private and short, and `~/.taria/`
works where it is unset. Binding unlinks a socket nothing listens on, refuses
one another instance serves, and leaves anything that is not a socket alone.

### On Windows

There is no socket file. The layer creates the named pipe
`\\.\pipe\taria-<SID>-<label>`, where `<SID>` is your account's security
identifier, and `socket_path()` returns that name. The pipe is owned by you,
open to nobody else and refuses remote clients. A label may not contain `\`,
and `$TARIA_SOCK` or a name passed to `bind_at` must be a local pipe,
`\\.\pipe\<name>`.

A bind error is `AddrInUse` when another copy serves the name or a bridge
still holds a just-exited app's end; `InvalidInput` for a name that is not a
local pipe or a label that cannot end one; and otherwise names the pipe and
the system's error.

## Checking your work

Run the app, register the bridge with its label, and ask an agent to read
the tree. In Claude Code that is
`claude mcp add taria -- taria-mcp --app my-app`, with `target/debug/taria-mcp`
from the same checkout while you track `main`. Then confirm by hand:

- Exactly one node is focused in every state, including the empty ones.
- Every id you publish still names the same thing after a delete.
- An act your app refuses comes back as an ignored ack, not as silence.
- `type_text` sent where your app accepts typing changes the tree; see
  [The arm the compiler asks for hides `Text`](#the-arm-the-compiler-asks-for-hides-text).
- `type_text` sent where nothing accepts typing comes back ignored and
  changes nothing. Send a word carrying letters your app binds.
- An agent can move the keyboard to the typing surface with only what the
  tree advertises; see [Aiming typed text at a surface](#aiming-typed-text-at-a-surface).
