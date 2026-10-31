# Integration guide

Adding taria to a ratatui app you already have. For how the pieces fit, see
`architecture.md`; for what the wire format requires of a peer, message by
message, see `protocol.md`; for why taria exists, see `landscape.md`.

Three apps have been through this. One is `examples/demo-app`, written taria
first. The second was an existing typing tutor, retrofitted after the fact: a
real event loop, live metrics, and its own idea of node identity. Several of
the APIs below exist because of it. The third, ramvamp, draws only a panel
and was retrofitted too. Where a rule here comes from a bug rather than a
preference, the bug is named.

The work is five edits, in this order:

1. Bind a layer in `main`.
2. Write a `tree.rs` that turns app state into nodes.
3. Publish it after each draw.
4. Drain agent input around the blocking call in your event loop.
5. Acknowledge the inputs you deliberately ignore.

Nothing above touches your rendering or your state. Your key handling is
touched only indirectly, by the raw `key` fallback lowering into it, which is
its own section below. Typed text does not go there at all, for a reason that
section spends most of its length on.

Migrating an app from v0 instead: the compiler walks you through most of it
and stays silent on the one step that fails. **The wildcard arm it makes you
add to every `match` over `AgentInput` is the arm that swallows
`AgentInput::Text`, so check every wildcard you add for `Text` before you
trust a green build.**
[The arm the compiler asks for hides `Text`](#the-arm-the-compiler-asks-for-hides-text)
has the details and a lint that catches it.

## Add the dependency

Both crates are on crates.io, at version `0.3.3`.

```bash
cargo add taria-ratatui
```

One line is enough. `taria-ratatui` re-exports the protocol crate, so the
`taria::{Action, IdSpace, Node, Role}` the samples below import are reachable
as `taria_ratatui::taria::{Action, IdSpace, Node, Role}`. Add `taria` as a
dependency of its own if you would rather write them under the name they are
spelled here:

```bash
cargo add taria
```

To track `main` instead of a release, take either crate from the repository.
A git dependency resolves to whatever the branch holds the day you build it,
so pin a revision if you want the same build twice.

```toml
[dependencies]
taria-ratatui = { git = "https://github.com/y0sif/taria" }
```

What the two sides do have to agree on is `PROTOCOL_VERSION`. Version 1 is
frozen and changes within it are additive, so two builds of taria from
different days still understand each other; a bridge from another protocol
generation does not, and the cost is every input tool. `act`, `key` and
`type_text` refuse up front on a mismatch, and `read_tree` survives only for
as long as the peer's snapshots still parse. Building your app's `taria` and
the bridge from the same checkout is how you stop having to think about it.

## Bind without risking startup

```rust
use taria_ratatui::TariaLayer;

fn main() -> std::io::Result<()> {
    // Never fails. If the socket cannot be bound the layer is inert and the
    // app runs exactly as it would without taria.
    let mut layer = TariaLayer::bind_or_disabled("my-app");

    // Print here, before `ratatui::init()` takes the alternate screen. A
    // print after that lands on a screen the TUI owns, garbling the frame,
    // and vanishes with the alternate screen on exit. In the primary buffer
    // it is still on screen after the app quits, which is where whoever runs
    // the agent goes looking for the socket path.
    match layer.bind_error() {
        Some(err) => eprintln!("my-app: taria disabled: {err}"),
        None => eprintln!(
            "my-app: taria socket at {}",
            layer.socket_path().display()
        ),
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut layer);
    ratatui::restore();
    result
}
```

`bind` returns an error; `bind_or_disabled` returns a disabled layer instead.
Prefer the second. A user whose `$XDG_RUNTIME_DIR` is not writable should get
their app, not a failure to launch over a feature they did not ask for. At a
path of your own the pair is `bind_at` and `bind_or_disabled_at`.

Every method stays callable on a disabled layer: publishing does nothing, no
input ever arrives, `frame()` still hands back a recorder. The render path
never branches on whether taria came up.

The layer prints nothing itself, ever, and that is the reason `bind_error()`
exists as a getter. It is your app that knows when the screen is safe to
write to, and an app that draws only a panel has no alternate screen to time
it by (see [Apps that draw only a panel](#apps-that-draw-only-a-panel)).

## Build the tree in its own module

Put node building in a function that takes `&App` and returns `Vec<Node>`.

```rust
// tree.rs
use taria::{Action, Node, Role};

use crate::app::{App, Focus};

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
```

Two actions is not the whole set that node wants, and the sample is layered on
purpose.
[Advertise a way out of every state](#advertise-a-way-out-of-every-state) adds
the action that takes the keyboard away again, and
[Aiming typed text at a surface](#aiming-typed-text-at-a-surface) adds the one
that brings it here, which is how an agent points `type_text` at this field
without a raw keystroke.

Then publish it right after drawing:

```rust
terminal.draw(|frame| view(&app, frame))?;
layer.publish(tree::build_nodes(&app));
```

This is the retrofit path. It is pure, so every invariant is a unit test with
no terminal involved: exactly one focused node in each state, no actions
advertised behind a modal, ids stable across a mutation. It keeps the render
path unchanged, which matters when the render path is someone else's code.

The alternative is `layer.frame()` plus `sem(&rec, widget, node)`, which
records a node as each widget renders. Reach for that when the node is
genuinely a property of the widget being drawn. For a retrofit it means
editing every render call, and the nodes then live in the one function that
is hardest to test.

Either way the nodes are wrapped in an auto-generated `app` root, so the two
produce the same tree.

## Apps that draw only a panel

Not every app hands ratatui the screen. A chat REPL or a build runner can
leave its output in the terminal's own scrollback and have ratatui draw only a
panel pinned under it; `Viewport::Inline` plus `insert_before` is the common
way. ramvamp, a local LLM runtime, does it by hand in `chat --tui`: it prints
its transcript into a scroll region over the rows above the panel, and ratatui
draws the panel through `Viewport::Fixed`. Three things this guide otherwise
takes for granted change there.

**Publish the output from state.** `layer.frame()` and `sem` record a node
only when its widget renders inside a draw, so they see the panel and nothing
else. Output printed above it is drawn once, outside any frame, and a `sem`
inside an `insert_before` closure lands in one snapshot and is gone from the
next. Build the output's node from the state that holds it, and publish on
every pass rather than after each draw, because a line printed above the
panel changes the tree without redrawing it:

```rust
// tree.rs: the transcript is app state, not a widget in the frame.
pub fn build_nodes(app: &App) -> Vec<Node> {
    vec![
        Node::new("transcript", Role::Log)
            .label("Conversation, oldest line first")
            .value(app.transcript.text()),
        panel_node(app),
    ]
}

// The event loop: every pass, whether or not the panel redrew. A tree
// identical to the last one costs a comparison.
layer.publish(tree::build_nodes(&app));
```

**Publish a bounded tail, as a `log`.** The output grows for the whole
session, and a snapshot is one line of at most 1 MiB
([Keep the snapshot small enough to arrive](#keep-the-snapshot-small-enough-to-arrive)).
A reply streamed into it also changes the tree on every token, so each token
republishes the whole tail
([Watch your publish rate](#watch-your-publish-rate)). ramvamp keeps the last
6 KiB, about two screens of prose, trimmed a whole line at a time from the
front; `Role::Log` tells an agent the value grows at the end. If the adapter
ever has to trim the tree, a log's own value keeps its end, but a transcript
of turn nodes, as in [Mark relayed text](#mark-relayed-text), is cut like any
list: each turn's words keep their start, and if that is not enough the
newest turns are dropped, so cap the turn list yourself.

```rust
/// About two screens of prose: a whole reply, without the tail being most
/// of every snapshot.
const TAIL_CAP: usize = 6 * 1024;

impl Transcript {
    fn trim(&mut self) {
        let mut cut = 0;
        // Whole lines from the front, but never the last one: a single line
        // over the cap would otherwise empty the tail.
        while self.text.len() - cut > TAIL_CAP {
            match self.text[cut..].find('\n') {
                Some(at) if cut + at + 1 < self.text.len() => cut += at + 1,
                _ => break,
            }
        }
        // That line alone is over the cap: cut into it on a char boundary.
        if self.text.len() - cut > TAIL_CAP {
            cut = (self.text.len() - TAIL_CAP..self.text.len())
                .find(|&at| self.text.is_char_boundary(at))
                .unwrap_or(cut);
        }
        self.text.drain(..cut);
    }
}
```

A transcript that mixes your app's own lines with a model's text needs one
more thing before an agent can tell the two apart; see
[Mark relayed text](#mark-relayed-text).

**Report after teardown.** There is no alternate screen to print around. The
panel shares the primary buffer with the output above it, so an `eprintln!`
while it is up lands in the scrolling output or tears the panel, and in raw
mode a bare newline does not even return to column 0. Bind before raw mode,
and send diagnostics through your app's own output path while the panel is
up, or print them once it is down:

```rust
// Before raw mode: a bind that blocks on the filesystem must not leave a
// half-taken terminal behind it.
let mut layer = TariaLayer::bind_or_disabled("my-app");
let mut panel = Panel::enter()?; // raw mode, scroll region, Viewport::Fixed
let result = run(&mut panel, &mut layer);
panel.leave()?; // scroll region reset, raw mode off

// Only now is stderr the user's again.
if let Some(err) = layer.bind_error() {
    eprintln!("my-app: taria disabled: {err}");
}
```

ramvamp sends that one line to its debug log instead, because a second
instance in another window is the ordinary way to fail to bind, and an error
about a feature nobody asked for is worse than silence.

## Pick the role that says what the widget is

The role is the first thing an agent reads about a node, so it decides how the
node gets treated. There are 29, listed in `architecture.md`, and most map
straight onto the widget you are wrapping. Three pairs do not, and one group
needs a rule of its own. Every one of them turns on a role added after a
census of 15 ratatui apps found a fifth of on-screen widgets with no
defensible role.

`status` against `progress_bar`. A progress bar reports a known fraction of a
known total. A status says work is happening without saying how much of it is
left: a spinner, a throbber, a "saving" line, a toast. Reach for `status`
whenever there is no fraction to report, and publish it rather than treating
it as decoration, because publishing is what makes a toast observable at all.
One that appears and clears itself between two reads is invisible to an agent,
which then reads the app as having done nothing.

`tree` against `list`. Choose `tree` when an entry can own entries of its own,
`list` when the rows are flat. Naming a flat list a tree sends an agent
looking for structure to expand that is not there; naming a tree a list hides
the nesting that decides what the agent has actually seen. A `tree_item`
carries its own entries as children, so the node tree has the shape of the
widget's, which is also the one role with a depth limit worth reading before
you build against it: see "Keep the tree shallow enough to arrive".

`select` and `option` against `list` and `list_item`. A list reports where a
cursor sits; a select reports what the app will use. Choose `select` when the
point is to commit to a value rather than to browse rows, and put the
committed choice in the node's value, so an agent reads the current setting
without walking the children. Activating an `option` sets its parent's value,
which is what separates it from a `list_item`, where activating moves a
cursor.

Then the roles for things an agent cannot see. `image`, `chart`, `terminal`,
`log` and `scrollbar` each wrap something whose rendering carries the meaning,
and none of that rendering survives into a tree. Every one of them has
somewhere to put the meaning instead. Say what the image is of in the label,
not that it is an image. Put the numbers that matter in a chart's value: the
latest sample, the peak, the unit. Put the position in a scrollbar's value,
which is the only thing telling an agent that the pane it just read has more
content past the edge. A `log` grows at the end, so the value an agent read is
a prefix of what is there now rather than the whole of it; a `terminal` is a
screen rather than a tree, so its value is opaque text and keys are how an
agent drives it.

`other` is the honest answer when nothing fits, and it is also what an agent
built before your role sees, so it costs that agent the one fact it most needs
about the node. Reaching for it often means the vocabulary is missing
something. Say so rather than inventing a role name of your own: a role added
to taria is additive and every peer learns it, while a role each app names for
itself is one vocabulary per app.

## Say what state a node is in

The role says what a widget is. Its state, which row is selected, whether a
box is ticked, whether a branch is open, whether a button can be pressed
yet, has its own fields since 0.4.0, so an agent reads it instead of parsing
it out of a `value` string you made up. Each is a chainable setter beside
`label` and `value`:

```rust
use taria::{Checked, Node, Role};

// A task row: done is `checked`, the cursor row is `selected`, and its place
// in the whole list, not only the rows on screen.
Node::new(TASK_IDS.id(task.id), Role::ListItem)
    .label(task.title.clone())
    .checked(task.done)
    .selected(pos == app.cursor)
    .pos_in_set(pos as u32 + 1);

// The list says how many rows it holds, published or not.
Node::new("tasks", Role::List).set_size(app.tasks.len() as u32);

// A parent whose children are partly ticked, a branch that is open, a
// button that cannot be pressed yet and why.
Node::new("all", Role::Checkbox).checked(Checked::Mixed);
Node::new("src", Role::TreeItem).expanded(true);
Node::new("save", Role::Button)
    .label("Save")
    .description("Writes the open file to disk")
    .disabled(!app.dirty);

// A gauge, a reading with no range, and a table cell and its table, rows
// and columns from 1.
Node::new("cpu", Role::ProgressBar).range(app.cpu, 0.0, 100.0);
Node::new("temp", Role::Text).label("CPU temperature").value_now(app.celsius);
Node::new(format!("r{row}c{col}"), Role::Cell).cell(row, col);
Node::new("results", Role::Table).table_size(app.rows.len() as u32, 4);
```

What each one promises:

- **Absent means you said nothing**, never `false`. So set `selected` on
  every row of a list, `false` on all but the cursor's, and `expanded` on
  every branch that can open: a row with no `selected` says nothing about
  whether it is the cursor's. Leave a field unset only where it does not
  apply.
- **Selection is not focus.** `focused` is where a raw key lands; `selected`
  is where the cursor sits, which stays true while the keyboard is
  elsewhere. Publish both.
- **`disabled` takes the actions with it.** A node you mark disabled is
  published with no actions, whatever you built it with: the adapter strips
  them, so you do not have to, and `taria-mcp` refuses `act` on it with an
  answer that says it is disabled, so the agent waits for the tree to change
  instead of trying other names. `key` and `type_text` are not affected.
  `disabled(false)` clears the mark, and only `true` is ever sent. The
  reverse does not hold: an empty action list does not mean disabled, so
  do not mark a node disabled because a modal is in front of it. The demo's
  rows behind its confirm dialog advertise nothing and are not disabled.
- **`description` is your own words.** `relayed` covers a node's label and
  value only. Text from outside your app, a file's contents or a model's
  reply, goes in a child marked relayed (see "Mark relayed text"), never in
  a description.
- **Positions count from 1**, as a person counts rows: `pos_in_set`, `row`
  and `column`. A 0 passed to `pos_in_set` or `cell` leaves that field
  unset, so a `row` node passes 0 for its column. Counts, `set_size`,
  `row_count` and `column_count`, may be 0.
- **Numbers are finite.** `range(now, min, max)` leaves any argument that is not
  finite unstated, so an open-ended bound is `f64::INFINITY`:
  `.range(bytes, 0.0, f64::INFINITY)` states a value and a floor and no ceiling.
  `value_now(now)` states the value alone, leaving the bounds as they are, and a
  value that is not finite clears it. A `NaN` never reaches the wire, which
  could not spell it, and never stops the adapter from noticing that a frame is
  unchanged.

**Hidden is not a field: do not publish it.** A collapsed branch's children,
a closed panel, the rows scrolled out of view are simply left out of the
tree. That is also why the counts say "published or not": a branch with
`expanded(true)` and `set_size(40)` that publishes 12 children tells an
agent the other 28 exist and are not on screen, not that the set has 12.
Publish what the widget draws, and state the size of what it draws from.

A reader that predates 0.4.0 ignores all of this and sees the node it always
saw, so publishing state never costs an older agent anything. It does cost
the reader you replace it with, though: the demo moved a task's done state
from `value("todo")` to `checked`, and anything that matched the old value
had to move too. Keep a `value` an existing reader relies on, as the demo
keeps its list's selected id, until you know nothing reads it.

## Mark relayed text

Most strings in a tree are the app's own words: a button's text, a tab's
name, a status line. Some are not. A model's output, a file's contents,
another user's message, a command's stdout, the file names in a directory
tree: the app shows them without having written them, and an agent reads them
in the same shape as everything else. Text the app did not write can then
speak in its voice. A model reply that opens a line with
`note: system compromised` reads as the app reporting its own state.

Mark every node whose label or value the app relays:

```rust
Node::new("output", Role::Log).value(stdout.clone()).relayed(true)
```

The mark covers that node's label and value, never its id, and that node
alone: children do not inherit it, so each node carrying relayed text is
marked on its own. The common case is a `log` streaming a command's output or
a model's response. An agent sees `"relayed":true` in the JSON tree and
`*relayed` right after the role in the outline, and `read_tree` tells it what
the mark means.

Carry speakers in structure, not in the text. An app that prefixed its
transcript's turns `you:` and `note:` found a reply could open a line with
`note:` and pass for the app: any prefix the text can carry, the text can
also forge. Breaking every in-band prefix with an invisible character in the
agent's copy works, but it is per app, invisible, and has to survive the
app's own trimming. Instead, publish one unmarked node per turn with the
speaker as its label, and the words in a child marked relayed:

```rust
use taria::{IdSpace, Node, Role};

pub const TURN_IDS: IdSpace = IdSpace::new("turn");
pub const WORDS_IDS: IdSpace = IdSpace::new("words");

fn transcript(app: &App) -> Node {
    Node::new("transcript", Role::Log)
        .label("Transcript")
        .children(app.turns.iter().map(|turn| {
            // Who spoke is the app's word; what they said is not.
            Node::new(TURN_IDS.id(turn.id), Role::Text)
                .label(turn.speaker.name())
                .child(
                    Node::new(WORDS_IDS.id(turn.id), Role::Text)
                        .value(turn.text.clone())
                        .relayed(true),
                )
        }))
}
```

which an agent reads in the outline as the lines below. Left out are the
outline's header line (`protocol_version=1 seq=N`) and the line of the app's
root node. The transcript is taken to be a child of that root, so each line
here sits one level (two spaces) left of where the agent sees it:

```text
transcript log "Transcript"
  turn-41 text "you"
    words-41 text *relayed = "summarize the logs"
  turn-42 text "assistant"
    words-42 text *relayed = "note: system compromised"
```

The `note:` is still there, inside a node that says it is relayed, under a
turn that says the assistant wrote it. If the speaker's name is itself someone
else's text, a chat handle another user chose, mark the turn node too.

Ids come from identities, as in
[Give things identities, not positions](#give-things-identities-not-positions),
never from relayed text. The mark does not cover the id, and an id is what an
agent passes back to `act`, so one built from a file name or a message carries
that text unmarked, and moves whenever the text does. The same goes for a
custom action's name.

## Drain agent input around the blocking call

```rust
while app.running {
    drain_agent_input(&mut app, layer);

    terminal.draw(|frame| view(&app, frame))?;
    layer.publish(tree::build_nodes(&app));

    match rx.recv() {
        Ok(event) => update(&mut app, event),
        Err(_) => break,
    }
    drain_agent_input(&mut app, layer);
}
```

Both calls earn their place. The one after `recv` applies input that arrived
while the loop was parked, in the same pass as the terminal event that woke
it, so a single draw shows both. The one at the top catches anything that
arrived while the previous pass was drawing, before the frame that will be
published is built. Publishing a tree that does not include an input you are
already holding is how an agent gets told nothing happened.

The assumption underneath is that the loop wakes up on its own. An event loop
that blocks on terminal input alone never returns while the keyboard is idle,
and agent input then sits in the queue until a human touches a key. The demo
runs a tick thread that sends an event every 50 ms, which bounds the latency
between an act and its effect.

If you have no tick, `layer.recv_timeout(Duration::from_millis(50))` is a
blocking wait on agent input with a deadline, usable as the loop's clock. It
waits out the timeout even on a disabled layer, so an app that paces itself
on it keeps its timing whether or not taria bound.

It hands back the input with its `InputId`, as `try_recv` does. Keep the id:
without it you cannot ack `Ignored`, which
[Acknowledge what you ignore](#acknowledge-what-you-ignore) makes mandatory.
A loop that drains needs no id at all: `drain` takes the status your handler
returns and sends the ack itself.

## Give things identities, not positions

Node ids are strings on the wire, and an agent holds one across calls. It
reads the tree, decides, and acts on an id from the read. Anything that
happens in between must not change what that id names.

Positional ids break this. In the demo, task ids were the task's index in the
vector: deleting the second task shifted every later task's node id by one,
so an agent that read the tree, then deleted a task, then acted on an id from
that read hit whichever task had slid into the slot. The delete looked
correct. The next act destroyed the wrong row.

The fix is an identity assigned at creation and never reused, with `IdSpace`
holding the one spelling that builds and parses it:

```rust
use taria::IdSpace;

pub const TASK_IDS: IdSpace = IdSpace::new("task");

// Building the node, in tree.rs.
Node::new(TASK_IDS.id(task.id), Role::ListItem)

// Reading it back, in update.rs. An id from before an unrelated delete
// names the same task or nothing at all, never a different task.
let Some(id) = TASK_IDS
    .parse::<TaskId>(node)
    .filter(|id| app.task(*id).is_some())
else {
    return InputStatus::Ignored;
};
```

`IdSpace` is const-constructible, so the space sits next to the tree builder
and the update handler imports it. The point is that `format!("task-{i}")`
and `strip_prefix("task-")` cannot drift apart, because there is only one of
each.

Make ids reproducible across restarts where you can. The demo seeds a fixed
task table from a fixed starting id, so an agent script written against one
run still applies to the next.

## Acknowledge what you ignore

The layer acks `Delivered` for you, as it hands each input over. That says
only that your event loop dequeued it.

Some inputs you will look at and deliberately do nothing with: an act a modal
dialog blocks, a node id you no longer know, a `set_value` carrying no value.
Say so, or the agent waits out the bridge's window and is told the tree did
not change, which reads as "it may not have reacted yet".

```rust
use taria_ratatui::TariaLayer;

fn drain_agent_input(app: &mut App, layer: &TariaLayer) {
    layer.drain(|input| apply_agent_input(app, input));
}
```

`apply_agent_input` returns an `InputStatus`: `Ignored` for an input it
deliberately did nothing with, `Delivered` for the rest. `drain` sends the
`Ignored` ones, and last ack wins, so each refines the `Delivered` the dequeue
already sent. A returned `Delivered` sends nothing, since it would only repeat
that. That is why `apply_agent_input` returns a verdict instead of `()`, and
the verdict is taria's own type rather than one you define: the demo and the
typing tutor each wrote the same two-variant enum, and the same
dequeue-and-`ack` loop, before the layer did it for them.

An act on a node that has since gone is one of these. The demo publishes
`dialog`, `dialog-confirm` and `dialog-cancel` only while the confirm-delete
dialog is open, so an agent planning from a snapshot taken just before a
person pressed `n` sends an act against a node that is no longer there. Those
handlers report `Ignored`, the same answer a deleted task id already got.
Reporting `Delivered` for a node that is gone tells an agent its input landed
somewhere.

One timing rule matters. Ack `Ignored` before you publish your next frame.
Acks are flushed ahead of the pending snapshot in every writer pass, so an
ack queued first arrives first, and the bridge reports the refinement. Publish
first and the bridge sees a changed tree behind a plain `Delivered`, returns
that tree, and the `Ignored` arrives after the answer has already gone out.
Draining before drawing, as above, gives you this for free.

An input you can only answer later, a frame on or more, needs its id kept
until then. For those, loop on `try_recv` instead of calling `drain`. It hands
you each input with its `InputId`, and you pass the id to `ack` when the
answer is ready:

```rust
use taria_ratatui::InputStatus;

let mut pending = Vec::new();
while let Some((id, input)) = layer.try_recv() {
    apply(app, input);
    pending.push(id);
}

// Later, once you know nothing came of them.
for id in pending {
    layer.ack(id, InputStatus::Ignored);
}
```

## Advertise a way out of every state

An agent moves through your app by acting on what the tree advertises. A state
it can enter and cannot leave by any of those actions is a trap, and the only
escape left is the raw-key fallback, which is exactly what should not be the
main path.

The demo had one. `set_value` on the text input moves focus there, and nothing
advertised moved it back, so every `key` an agent sent afterwards was typed
into the draft. The fix is one action, advertised conditionally. Here is the
`input` node from earlier with both of its conditions, since the same rule
reaches the action it already had:

```rust
let focused = app.focus == Focus::Input;
let mut node = Node::new("input", Role::TextInput)
    .label("New task")
    .value(app.draft.clone())
    .focused(focused)
    .action(Action::SetValue);
// Advertised only when it would do something. `activate` submits the
// draft, and an empty draft submits nothing, so offering it there was the
// tree's one advertised-but-ignored pair. The condition is the submit
// handler's own, whitespace included, so the two cannot drift.
if !app.draft.trim().is_empty() {
    node = node.action(Action::Activate);
}
// The way back out, advertised only while the input holds the keyboard,
// because that is when there is something to hand back.
if focused {
    node = node.action(Action::Dismiss);
} else {
    // And the way in, for the same reason from the other side: acting on
    // it moves the keyboard here, which is only a move while the keyboard
    // is elsewhere. This is how an agent aims `type_text` at the field;
    // see "Aiming typed text at a surface" below.
    node = node.action(Action::Focus);
}
```

`dismiss` is what Esc already did for a person: throw the draft away, hand the
keyboard back to the list. Two rules keep the advertisement and the behaviour
in step. The action is advertised only while the input has focus, and the
handler returns `Ignored` when it does not, so an agent is never told the
keyboard moved when it did not:

```rust
fn dismiss_input(app: &mut App) -> InputStatus {
    if app.focus != Focus::Input {
        return InputStatus::Ignored;
    }
    app.draft.clear();
    app.focus = Focus::List;
    InputStatus::Delivered
}
```

The second rule is that the list ignores Esc too, which is what makes
`dismiss` Esc's exact counterpart rather than a second, agent-only meaning
someone has to maintain separately.

Walk your own states and ask the question of each: modal open, text field
focused, menu down, filter applied, search active. Each needs a node with an
action that ends it.

The app itself is one of those states. The demo's footer told a person
`[q] quit` from the start while the tree advertised no exit at all, so an
agent's only way to close it was the raw key, which types the letter `q`
whenever the input holds the keyboard. A `quit` button node with `activate`
is the same affordance the footer already offered, and it takes the modal
gate like every other node outside the dialog:

```rust
fn quit_node(app: &App) -> Node {
    let mut node = Node::new("quit", Role::Button).label("Quit");
    // A dialog asking whether to delete a task is not a moment to quit.
    if app.dialog.is_none() {
        node = node.action(Action::Activate);
    }
    node
}
```

Then the mirror of the rule: an act that lands somewhere the tree does not
show is as good as ignored. The demo added a task from the Done tab and left
it on the Active tab, invisible: the draft cleared, the tree changed, so the
bridge reported success, and the agent read the tree back, found nothing it
had asked for, and could reasonably submit again. Switching to the tab the
task landed on is what makes the addition readable, and it is what a person
adding a task wants to see too. When an action's effect is not in the next
snapshot, the agent has been told a truth it cannot check.

## Watch your publish rate

Identical trees are deduped: a frame whose nodes match the previous frame's
is skipped before anything is cut or measured, and `seq` does not move.
Publishing every frame therefore costs one comparison, but only for as long
as the tree actually holds still.

A live metric defeats it. A words-per-minute counter carried to four
decimals puts a new string in the tree on every frame, so every frame
publishes, and each snapshot differs from the last for reasons no agent
cares about.

Quantize to what an agent can act on:

```rust
// Republishes the whole tree on every frame.
Node::new("wpm", Role::Text).value(format!("{wpm:.4}"))

// Changes only when the number an agent would read changes.
Node::new("wpm", Role::Text).value(format!("{}", wpm.round() as u32))
```

The same goes for progress percentages, elapsed timers, and animation state.
Round them, or leave them out of the tree entirely and let the agent read the
underlying fact. An agent cannot use the fourth decimal place, and neither
can the person reading the transcript.

## Keep the tree shallow enough to arrive

A snapshot is one JSON object, and a JSON parser bounds how far it will
recurse into one. Past that bound the tree does not arrive as an error, it
does not arrive at all: the reader skips the line the way it skips a truncated
one, and goes on serving the last tree it did read. The agent is then working
from a stale tree, or being told no app has published yet, with nothing
anywhere saying why.

`taria::MAX_NODE_DEPTH` is 32. The measured ceiling is 63 nested nodes, taken
through a whole snapshot line rather than a bare node, and the limit sits a
little under half of it; the slack pays for the envelope a transport wraps
around a snapshot and for a peer whose parser is stricter than `serde_json`.

The adapter enforces it at publish, counting the root it adds above your
nodes. A tree over the limit is cut rather than withheld: every node at the
limit publishes without its children, per branch, so everything above the cut
reaches the agent unchanged. Withholding is the failure this is preventing,
not a safer version of it, because a publish that does not happen leaves the
agent on the stale tree either way.

You are told, because you are the only one who can fix it:

```rust
let stats = layer.stats();
if let Some(branch) = stats.last_truncation {
    // Names the depth measured and a node found at it, which in a tree
    // generated from data is the only way to find the branch that ran away.
    let cut = stats.truncated_snapshots;
    eprintln!("my-app: {cut} snapshot(s) published with a branch cut: {branch}");
}
```

`Node::check_depth` is the same check with the same error, callable on a tree
of your own before you hand it over, for an app that would rather shorten the
tree itself than have a branch cut for it.

Nothing hand-built reaches this: an app, its tabs, a pane, a list and its rows
is six levels. The trees that do are generated from data, and `Role::Tree`
invites the obvious one, a browser over a deep directory. Publish the expanded
path rather than the whole structure. That is what a tree widget draws anyway,
the rows a person can currently see, and it makes the snapshot the size of the
screen instead of the size of the data behind it.

## Keep the snapshot small enough to arrive

A snapshot travels as one line, and a bridge reads a line up to a fixed size
and no further. Past that it cannot find where the next message starts, so it
closes the connection. Closing fixes nothing: the bridge reconnects, the
adapter sends it the latest snapshot, which is the same one, and the bridge
closes that connection too, for as long as the tree stays that large. The
bridge names the limit, but only the app can bring its tree within it.

`taria::wire::MAX_LINE_BYTES` is 1 MiB, 1048576 bytes, not counting the
newline. Both peers enforce it.

The adapter trims every snapshot to fit at publish, after the depth cut, and
stops as soon as the tree fits. A node that could never fit goes first, with
everything under it; its siblings are not dropped with it. Then the longest
values and descriptions are cut to one shared length, then labels with them; a
string of 256 bytes of JSON or less is never cut. A cut is marked with `…`: a
`Role::Log` or `Role::Terminal` value keeps its end, because that is where those
grow, and every other string keeps its start. When more nodes have to go, a list
keeps its first rows with no gap, and the strings left are cut only as far as
the smaller tree needs. Rows are chosen as if every string were at the floor, so
a draft beside a list that had to lose rows ends up near the floor too; it stays
whole only when the nodes that went were what ran over. Ids and custom action
names are never touched, so an agent can still act on whatever it reads. The
stages are in `architecture.md` (Snapshot size). The tree is trimmed rather than
withheld for the reason it is cut for depth: a publish that does not happen
leaves the agent on a stale tree.

A frame identical to the one before it is skipped before the trim. A changed
tree is compared again after it, so an edit past the cut of a trimmed value
sends nothing new: the agent already has that tree.

You are told, because you are the only one who can fix it:

```rust
let stats = layer.stats();
if let Some(last) = stats.last_oversize {
    // Names the line's size and the node holding the longest string,
    // which is where to look.
    let trimmed = stats.oversized_snapshots;
    eprintln!("my-app: {trimmed} snapshot(s) trimmed to fit one line: {last}");
}
```

Both `oversized_snapshots` and `truncated_snapshots` count snapshots, not
frames, so a tree that stays too large while the app redraws it counts once.

Nothing laid out by hand comes near this; the demo's whole tree is under
8 KB. The trees that reach it hold data that grows: a draft someone keeps
typing into, a log nobody capped, a chat transcript, a list holding every row.
Bound those yourself. Publish the tail of a log, or the lines its pane shows;
cap a draft or a transcript at what an agent needs to read; publish the rows
on screen. The adapter's bound is a guard, not a way to publish: a tree over
the limit is cut again on every frame that changes it, which for a draft
someone is typing into is every keystroke.

## The raw inputs, and what they are for

`key` sends one raw key press, up to 64 times with `repeat`. `type_text`
sends a literal string. Both exist so partial semantic coverage is still
useful: an app with three nodes and one keybinding is drivable today, and the
rest can be covered later.

Neither should be the primary path. An action a node advertises is stable
under a UI change; a keystroke is a guess about a keymap. Cover what an agent
needs to do with `act`, and let the keys handle the rest.

The two are not lowered the same way, and that is the point of there being
two. A key is a keypress, so it belongs in your key handler and lands wherever
focus is. Text is typing, so it belongs wherever your app puts typed
characters, which is a different place and is sometimes nowhere at all. The
next section is entirely about the second half of that sentence.

Lower `key` through the same handler a keyboard event takes:

```rust
// See "The arm the compiler asks for hides `Text`" below for this lint.
#[warn(clippy::wildcard_enum_match_arm)]
pub fn apply_agent_input(app: &mut App, input: AgentInput) -> InputStatus {
    match input {
        AgentInput::Act {
            node,
            action,
            value,
            ..
        } => apply_act(app, node.0.as_str(), action, value),
        // A key is a keypress: it lowers into the same handler a person's
        // keystroke takes and lands wherever focus is.
        AgentInput::Key { key, .. } => match to_crossterm_key(&key) {
            Some(key) => {
                handle_key(app, key);
                InputStatus::Delivered
            }
            // The bridge parses the same grammar before sending, so this is
            // rare. It also covers a key the grammar learned after this
            // adapter was built: the string parses and there is no crossterm
            // event for it, and reporting that beats lowering it to some
            // near-miss keystroke.
            None => InputStatus::Ignored,
        },
        // Typing, routed by the app rather than lowered into its bindings.
        // Without this arm, the wildcard below takes every `type_text`.
        AgentInput::Text { text, .. } => apply_text(app, &text),
        // The layer never hands this one over. It gets an arm of its own,
        // not `Unknown | _`, so the lint can still see the wildcard.
        AgentInput::Unknown => InputStatus::Ignored,
        // `AgentInput` is `#[non_exhaustive]`, so this arm is required. A new
        // way for an agent to address an app is additive on the wire; the
        // attribute is what makes it additive for your build too.
        _ => InputStatus::Ignored,
    }
}
```

Neither the `..` nor the wildcard arm is boilerplate to skip, and they answer
different additions. Twelve types in `taria` are `#[non_exhaustive]`:
`AgentInput`, `Action`, `Role`, `Node`, `Checked`, `Snapshot`, `InputStatus`,
the two wire message enums, `key::Key`, `key::Modifiers` and `key::KeyPress`.
So are six struct-like variants inside them: `AppToBridge::Hello` and `Ack`,
`BridgeToApp::Input`, and `AgentInput`'s `Act`, `Key` and `Text`. The enums
are where a new variant lands and the variants are where a new field lands,
and both are things an adapter matches on. Without the attributes, one new
key, one new field or one new input kind would fail to compile every app that
had integrated taria. With them the cost is a `..` at the end of a pattern, a
wildcard arm per enum matched on, `Modifiers::NONE` and `Modifiers::new` in
place of the struct literal `Modifiers` closes, and a constructor in place of
each marked variant's literal: `AgentInput::act`, `key` and `text`,
`AppToBridge::hello` and `ack`, `BridgeToApp::input`.

Through this adapter, the wildcard arm is not where an input kind you cannot
read arrives. That is `AgentInput::Unknown`, the fallback a kind added after
your build parses as, and the layer answers it `Ignored` on your behalf and
never queues it, counting it in `stats().unknown_inputs`. The fallback exists
because the `InputId` sits on the message rather than inside the input, so
degrading the input is what keeps the id and makes an ack possible at all;
without it the line fails whole and the agent waits out a timeout for an ack
that was never going to come. What does reach your wildcard is the other
case: your app rebuilt against a taria that added an input kind, before you
have written the arm for it. The arm is what makes that a recompile, and an
app migrating from v0 is exactly that case, with `Text` as the kind.

### The arm the compiler asks for hides `Text`

**The wildcard arm you add to satisfy the compiler is the arm that hides
`AgentInput::Text`, the variant you now have to handle, so check every
wildcard you add for `Text` before you trust a green build.**

v0 had no `Text` and no attribute, so a v0 `match` naming `Act` and `Key` was
exhaustive. Rebuilt against v0.1 it fails with
``non-exhaustive patterns: `_` not covered``, and the fix `rustc` suggests is
`_ => todo!()`. The error never names `Text`. Write the arm it asks for and
every `type_text` lands in it. The typing tutor this guide opens with went
through exactly this: the four edits the compiler asked for gave a clean
build, a clean `clippy -D warnings`, 181 passing tests, and a `type_text` that
did nothing at all.

Two things help beyond reading every wildcard by eye:

- Scope clippy's `wildcard_enum_match_arm` to the function that matches on
  `AgentInput`, as the sample above does. It is a restriction lint, off by
  default and stable, and it flags a wildcard covering a variant the enum
  already has, by name: ``help: try: `AgentInput::Text { .. } | _` ``. Give
  `AgentInput::Unknown` an arm of its own rather than writing
  `AgentInput::Unknown | _`, because the lint does not look inside an
  or-pattern, and that spelling silences it with `Text` still missing.
  `rustc`'s own check for this, `non_exhaustive_omitted_patterns`, is still
  unstable.
- Make the wildcard answer `Ignored`. The layer acks `Delivered` as it hands
  an input over, so `Text` swallowed by a wildcard that answers nothing
  reaches the agent as "delivered, and the tree did not change", which is also
  what an input still taking effect looks like. A wildcard answering `Ignored`
  makes the same mistake an explicit refusal of every `type_text`, which an
  agent reports rather than waits on.

Neither is a guarantee. The lint only sees the functions you put it on, and
an ignored ack only tells the agent. The check that does not depend on either
is the one in [Checking your work](#checking-your-work): type into your app
through the bridge and watch the tree change.

Lowering `key` into the keyboard path also means an agent can reach every
chord your app binds, and some it does not. The demo's list handler matched key
codes while ignoring modifiers, so `ctrl+q` hit the `q` binding and quit the
app, and `ctrl+enter` confirmed a deletion in the dialog. Neither chord was
a binding the app meant to have. If your handlers match on `KeyCode` alone,
gate them on modifiers before you expose them to an agent.

Gate on all of them, not on ctrl and alt. A first pass at the demo let shift
through, so `shift+q` still quit and `shift+y` still confirmed a delete. The
rule that holds is that a press is the plain binding only when it carries no
modifiers at all, applied in every handler:

```rust
fn is_plain(key: KeyEvent) -> bool {
    key.modifiers.is_empty()
}
```

Typing is the one place that rule must not reach. A terminal reports an
uppercase letter as `Char('A')` with shift set, so a text field that demanded
no modifiers would stop a person typing capitals. Allow shift where a press
becomes a character and nowhere else; the character already says which one it
is. Ctrl and alt are never text, and `ctrl+c` landing in a draft as the letter
`c` is the one reading an agent sending it cannot have meant.

## Typed text goes where your app puts typing

The demo used to lower `Text` the way it lowers `Key`, one key event per
character through `handle_key`. It is the obvious symmetry and it is a bug.
With the list focused, `type_text("deploy")` met the list's single-key
bindings: the `d` opened the confirm-delete dialog and the `y` later in the
same word confirmed it. One call deleted a task, and the bridge reported
plain success, because a task disappearing is a changed tree. An agent that
asked to type a word was told it had typed it, and had destroyed something
instead.

Typed characters are not commands. Route them to whatever your app puts
typing into, and report `Ignored` when nothing is:

```rust
/// Whether the app is accepting typed characters right now.
fn accepts_typing(app: &App) -> bool {
    app.dialog.is_none() && app.focus == Focus::Input
}

fn apply_text(app: &mut App, text: &str) -> InputStatus {
    let mut typed = false;
    for key in text_to_keys(text) {
        if !accepts_typing(app) {
            break;
        }
        // The text-entry handler, not the app's key bindings.
        handle_input_key(app, key);
        typed = true;
    }
    if typed {
        InputStatus::Delivered
    } else {
        InputStatus::Ignored
    }
}
```

The condition is re-checked per character, because typing can end the state
that was accepting it. `text_to_keys` lowers a newline to Enter, Enter submits
the draft and hands the keyboard back to the list, and the characters after it
have nowhere left to go. Stopping there is the same judgement as the whole
section: the alternative is those characters meeting the bindings of whatever
inherited focus. Two tasks are two calls.

The rule worth copying is not "text means the text field". It is that the app
decides where typed characters go. An app whose typing surface is not a text
field, a typing tutor scoring individual keystrokes for instance, routes
`Text` to that surface instead, and reports `Ignored` only where it is
accepting no typing at all. An app with several fields routes to the one that
has the keyboard. What they share is the `Ignored`: an agent that types at the
wrong moment hears about it in one round trip, rather than tripping bindings
and being told it worked.

`text_to_keys` is for a typing surface that consumes key events, like the
demo's text field. It lowers `'\n'` to Enter and `'\t'` to Tab, which a text
field reads as "submit" and "next field". A surface that grades characters
rather than handling keys has no such vocabulary, and the helper gives it one
anyway: the typing tutor's typing screen binds Tab to a setting that writes
the user's config file, so a tab inside a typed string lowered through
`text_to_keys` changes a setting, the very binding trip this section exists to
prevent. An app like that iterates `text.chars()` itself and hands each
character to its grader, deciding there what a newline or a tab means.

`Key` keeps the raw lowering deliberately. An agent sending `key d` is asking
for the `d` binding, and an app that routed keys into its draft too would have
no fallback left for the parts of its UI with no semantic coverage. That
difference is worth one line in your own README, because `type_text` is the
one an agent has to aim: the tree's focused node is where it will land.

### Aiming typed text at a surface

Aiming it needs an action, and the tree is the only place an agent can learn
which one. Advertise `Action::Focus` on the surface that takes typing, on the
same condition as `dismiss` and from the other side, and handle it the same
way:

```rust
/// Advertised only while the keyboard is elsewhere, because that is when
/// acting on it moves anything. Reaching this with the input already
/// focused means the agent acted on a tree that has moved on, and the
/// answer is the one `dismiss_input` gives in the mirror case.
fn focus_input(app: &mut App) -> InputStatus {
    if app.focus == Focus::Input {
        return InputStatus::Ignored;
    }
    app.focus = Focus::Input;
    InputStatus::Delivered
}
```

Advertising `focus` is a promise about two things: an act with it puts the
keyboard on this node, and the next snapshot shows this node as the focused
one, so an agent can check the move landed rather than assume it.

`set_value` is not a focus call, and an app that has only `set_value` on its
text field has not given an agent a way to aim. Many apps move the keyboard as
a side effect of setting a value, which is a reasonable thing for an app to
do, and it is still not a substitute: an agent that wants the keyboard and
nothing else would have to overwrite the field's contents to get it, and
`set_value` carrying no value asks for nothing, so the honest handling is to
answer it `Ignored` rather than treat a missing value as the empty string and
clear the draft the agent never asked to clear. Advertise `focus` in its own
right on any surface an agent will need to type into. Without it the raw `key`
fallback is the only aim left, which is the fallback this guide keeps off the
primary path.

## Report what the layer threw away

`layer.stats()` returns a `LayerStats`, and four of its counts record agent
traffic that went nowhere. Read it after the terminal is restored, never while
the alternate screen or a pinned panel is up
([Apps that draw only a panel](#apps-that-draw-only-a-panel)).

```rust
ratatui::restore();
let stats = layer.stats();

// The agent sent input faster than the loop drained it. Each drop was
// acked `Dropped`, so the agent knows; this is the app-side tally.
let dropped = stats.dropped_inputs;
if dropped > 0 {
    eprintln!("my-app: dropped {dropped} agent input(s): could not keep up");
}
// Inputs discarded because they were not aimed at a live session: the
// bridge connection they arrived on ended first, or a waiting client wrote
// them before its session began. Nothing on the agent side is left to hear
// about these, so the person running the app is the only one who can.
let stale = stats.stale_inputs;
if stale > 0 {
    eprintln!("my-app: discarded {stale} agent input(s): no live session");
}
// Inputs whose kind this build of taria cannot read. The layer acked each
// one `Ignored` for you, so the agent was told; the version gap is yours.
let unknown = stats.unknown_inputs;
if unknown > 0 {
    eprintln!("my-app: could not read {unknown} agent input(s): raise the taria dependency");
}
// Answers the bridge was too slow to collect. These inputs were applied;
// it is the acks that were dropped, so the agent calls waiting on them
// timed out instead of hearing what happened.
let acks = stats.dropped_acks;
if acks > 0 {
    eprintln!("my-app: lost the answer to {acks} agent input(s): the bridge read too slowly");
}
```

All four are monotonic across reconnects and all four are 0 on a disabled
layer. `LayerStats` is `#[non_exhaustive]`, so a later release can add a count
without breaking this code. The input queue holds 256, so a nonzero
`dropped_inputs` means an agent outpaced the loop by a lot; a loop that only
wakes on keyboard events gets there first. A nonzero `stale_inputs` means one
of two things. A bridge session ended with input still queued, which a harness
restart mid-call will do. Or a second client wrote input while it waited for
the first to leave, before the app's `hello` reached it; that input was thrown
away unacked, because its writer had seen no tree. taria-mcp never does this:
its input tools refuse to send before the app's first snapshot. A nonzero
`unknown_inputs` means the bridge is built against a newer taria than the app,
and the agent is asking for something this build has no way to do.

`dropped_acks` is the one that points the other way. The ack queue holds 1024
and drops the oldest, not the newest, because the answers an agent is still
waiting on are the recent ones; the queue is bounded at all because a bridge
that reads steadily but far slower than an agent sends never stalls long
enough for the write timeout to disconnect it, and would otherwise grow the
app out of memory. So a nonzero value is a report about the bridge, not about
the app: the input landed and the caller was answered nowhere.

## Socket paths

By default the socket is `$XDG_RUNTIME_DIR/taria/<label>.sock`, falling back
to `<temp dir>/taria-<user>/<label>.sock`, where `<user>` is the effective uid
where it is available (through `/proc/self` on Linux), else `$USER`, else
`$LOGNAME`, else the literal `default`. `$TARIA_SOCK` replaces both and is
used verbatim: on the app side always, and on the bridge side whenever the
bridge derives its path from `--app`. A bridge started with `--socket <path>`
takes that path and never reads the variable, so set the variable for both
processes rather than mixing the two ways of saying it.

The label you pass to `bind_or_disabled` becomes that file name, so it has to
be one: a label carrying `/`, or `.`, `..` or empty, is refused, on the app
side and on the bridge's `--app` alike. That is not paranoia about the string,
it is that the layer binds *and unlinks* whatever the label resolves to, and
`/etc/cron.d/evil` as a label would discard the resolution and keep the
absolute path. `bind` returns the error and `bind_or_disabled` comes back
disabled with an empty `socket_path()`, because a refused label resolves to no
path at all. If you want a path of your own, pass the path: `$TARIA_SOCK`, or
`TariaLayer::bind_or_disabled_at` (`bind_at` if you would rather have the
error).

Unix domain socket paths are capped at the platform's `sun_path` minus the
terminating NUL, and the buffer is not the same size everywhere: 108 bytes on
Linux, so 107, and 104 on macOS and the BSDs, so 103. It is a low limit either
way, and a deep `$XDG_RUNTIME_DIR` or a long app label reaches it. Test on
macOS if you ship there, because a path between the two sizes binds on Linux
and fails there, and the macOS temp dir plus a long label is exactly where
that band sits. taria checks before binding and its error names the path, its
length, the limit, the platform the limit belongs to, and the way out, rather
than the kernel's `InvalidInput: path must be shorter than SUN_LEN`. The way
out is one variable, set the same way for both processes:

```bash
TARIA_SOCK=$XDG_RUNTIME_DIR/t.sock ./my-app
TARIA_SOCK=$XDG_RUNTIME_DIR/t.sock taria-mcp --app my-app
```

The parent directory is created with mode `0700` and vetted before binding: a
real directory, owned by you, with no group or other permission bits. So the
shorter path has to be somewhere private. `/tmp` is the reflex and it does not
work: it is world-writable, the vetting refuses it, and the app comes up with
taria disabled instead of with a shorter path. `$XDG_RUNTIME_DIR` is already
private and already short; `~/.taria/` is the answer where that variable is
unset.

Binding is careful about what is already at the path, because it is a path the
layer unlinks as well as binds:

- Nothing there: bound.
- A socket nothing is listening on, left by a run that was killed before it
  could clean up: unlinked, then bound.
- A socket another instance is serving: refused, with an error saying to give
  the second instance a socket of its own. Taking it over would leave that
  instance running with no way for a bridge to reach it, which is worse than
  refusing, and the second app still starts because `bind_or_disabled` turns
  the refusal into a disabled layer.
- Anything that is not a socket: refused, and the file is left alone. The path
  can come verbatim from `$TARIA_SOCK`, and a typo there is not a reason to
  delete a file you meant to keep.

Two copies of your app on one socket path is therefore a diagnosable state
rather than a silent theft, and neither copy hangs at exit. The listener polls
a shutdown flag rather than waiting to be woken by a connection to its own
path, which is what used to hang the first copy: by the time the wake-up was
sent, the path held the second copy's socket, whose listener took it while the
first stayed parked.

## Checking your work

Run the app, then attach the bridge to it and ask an agent to read the tree.
The bridge is `taria-mcp`, built from the same checkout, and its `--app` takes
the label you passed to `bind_or_disabled`, so it derives the path your app
bound:

```bash
cargo install taria-mcp
claude mcp add taria -- taria-mcp --app my-app
```

Or build it from a checkout, which is what to do while you are tracking `main`
on both sides:

```bash
cargo build -p taria-mcp
claude mcp add taria -- /path/to/taria/target/debug/taria-mcp --app my-app
```

That registration line is Claude Code's; any MCP harness works, and the
README's quick start walks the same steps against the demo app.

Your app serves one bridge at a time, and a project `.mcp.json` starts a
bridge in every session opened in the project. Since 0.3.2 that no longer
locks your app: taria-mcp opens no connection until its first tool call, and
lets the app go after 60 s with no tool calls by default, so an idle session
does not keep your app from the one driving it. What is left is small. The
first call waits a moment for the connection, and a session that just went
quiet keeps the app for up to 60 s by default, or 90 s while an input it
sent awaits its ack. A bridge older than 0.3.2 holds the app for as long as
its session lasts.

The six things worth confirming by hand:

- Exactly one node is focused in every state, including the empty ones.
- Every id you publish still names the same thing after a delete.
- An act your app deliberately refuses comes back as an ignored ack, not as
  silence.
- `type_text` sent where your app accepts typing changes the tree. This is the
  one a green build cannot vouch for: a wildcard arm swallowing `Text` builds,
  passes default clippy, and passes every test that does not type; see
  [The arm the compiler asks for hides `Text`](#the-arm-the-compiler-asks-for-hides-text).
- `type_text` sent while nothing is accepting typing comes back ignored, and
  changes nothing. Send a word carrying letters your app binds; the demo's was
  "deploy".
- An agent can move the keyboard to that typing surface using only what the
  tree advertises. If the only way in is a raw keystroke, the surface is
  missing a `focus` action; see
  [Aiming typed text at a surface](#aiming-typed-text-at-a-surface).

`scripts/e2e.py` and `scripts/adversarial.py` do this against the demo and
are worth reading as a list of what can go wrong.
