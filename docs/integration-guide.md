# Integration guide

Adding taria to a ratatui app you already have. For how the pieces fit, see
`architecture.md`; for why taria exists, see `landscape.md`.

Two apps have been through this. One is `examples/demo-app`, written taria
first. The other was an existing typing tutor, retrofitted after the fact: a
real event loop, live metrics, and its own idea of node identity. Several of
the APIs below exist because of the second one. Where a rule here comes from
a bug rather than a preference, the bug is named.

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

## Add the dependency

Neither crate is published yet, so both come from the repository. They carry
version `0.0.1` there, which is a placeholder rather than something to pin
against; pin a git revision if you want the same build twice.

```toml
[dependencies]
taria-ratatui = { git = "https://github.com/y0sif/taria" }
```

One line is enough. `taria-ratatui` re-exports the protocol crate, so the
`taria::{Action, Node, Role}` and `taria::id::IdSpace` the samples below
import are reachable as `taria_ratatui::taria::{Action, Node, Role}` and
`taria_ratatui::taria::id::IdSpace`. Add `taria` as a dependency of its own
if you would rather write them under the name they are spelled here:

```toml
taria = { git = "https://github.com/y0sif/taria" }
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
their app, not a failure to launch over a feature they did not ask for.

Every method stays callable on a disabled layer: publishing does nothing, no
input ever arrives, `frame()` still hands back a recorder. The render path
never branches on whether taria came up.

The layer prints nothing itself, ever, and that is the reason `bind_error()`
exists as a getter. It is your app that knows when the screen is safe to
write to.

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

If you have no tick, `layer.recv_timeout_with_id(Duration::from_millis(50))`
is a blocking wait on agent input with a deadline, usable as the loop's clock.
It waits out the timeout even on a disabled layer, so an app that paces itself
on it keeps its timing whether or not taria bound.

Pace on that variant rather than `recv_timeout`. The plain one drops the
`InputId`, and without the id you cannot ack `Ignored`, which the next section
makes mandatory. The same pairing runs through the rest of the API:
`try_recv_with_id` to `try_recv`, and `drain_with_ids` to `drain`, which is
that loop already written for you.

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
use taria::id::IdSpace;

pub const TASK_IDS: IdSpace = IdSpace::new("task");

// Building the node, in tree.rs.
Node::new(TASK_IDS.id(task.id), Role::ListItem)

// Reading it back, in update.rs. An id from before an unrelated delete
// names the same task or nothing at all, never a different task.
let Some(id) = TASK_IDS
    .parse::<TaskId>(node)
    .filter(|id| app.task(*id).is_some())
else {
    return Applied::Ignored;
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
use taria_ratatui::{InputStatus, TariaLayer};

fn drain_agent_input(app: &mut App, layer: &TariaLayer) {
    layer.drain_with_ids(|id, input| {
        if apply_agent_input(app, input) == Applied::Ignored {
            layer.ack(id, InputStatus::Ignored);
        }
    });
}
```

Last ack wins, so the `Ignored` refines the `Delivered` the dequeue already
sent. That is why `apply_agent_input` returns a verdict instead of `()`.

An act on a node that has since gone is one of these. The demo publishes
`dialog`, `dialog-confirm` and `dialog-cancel` only while the confirm-delete
dialog is open, so an agent planning from a snapshot taken just before a
person pressed `n` sends an act against a node that is no longer there. Those
handlers report `Ignored`, the same answer a deleted task id already got.
Reporting `Handled` for a node that is gone tells an agent its input landed
somewhere.

One timing rule matters. Ack `Ignored` before you publish your next frame.
Acks are flushed ahead of the pending snapshot in every writer pass, so an
ack queued first arrives first, and the bridge reports the refinement. Publish
first and the bridge sees a changed tree behind a plain `Delivered`, returns
that tree, and the `Ignored` arrives after the answer has already gone out.
Draining before drawing, as above, gives you this for free.

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
}
```

`dismiss` is what Esc already did for a person: throw the draft away, hand the
keyboard back to the list. Two rules keep the advertisement and the behaviour
in step. The action is advertised only while the input has focus, and the
handler returns `Ignored` when it does not, so an agent is never told the
keyboard moved when it did not:

```rust
fn dismiss_input(app: &mut App) -> Applied {
    if app.focus != Focus::Input {
        return Applied::Ignored;
    }
    app.draft.clear();
    app.focus = Focus::List;
    Applied::Handled
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

Identical trees are deduped: a frame whose nodes match the previous publish
is skipped entirely and `seq` does not move. Publishing every frame is
therefore free, but only for as long as the tree actually holds still.

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
let cut = layer.truncated_snapshots();
if let Some(branch) = layer.last_truncation() {
    // Names the depth measured and a node found at it, which in a tree
    // generated from data is the only way to find the branch that ran away.
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
        Applied::Handled
    }
    // The bridge parses the same grammar before sending, so this is rare.
    // It also covers a key the grammar learned after this adapter was
    // built: the string parses and there is no crossterm event for it, and
    // reporting that beats lowering it to some near-miss keystroke.
    None => Applied::Ignored,
},
// Typing, routed by the app rather than lowered into its bindings.
AgentInput::Text { text, .. } => apply_text(app, &text),
// `AgentInput` is `#[non_exhaustive]`, so this arm is required. A new way
// for an agent to address an app is additive on the wire; the attribute is
// what makes it additive for your build too.
_ => Applied::Ignored,
```

Neither the `..` nor the wildcard arm is boilerplate to skip, and they answer
different additions. Ten types in `taria` are `#[non_exhaustive]`:
`AgentInput`, `Action`, `Role`, `Node`, `Snapshot`, `InputStatus`, the two
wire message enums, `key::Key` and `key::Modifiers`. So are six struct-like
variants inside them: `AppToBridge::Hello` and `Ack`, `BridgeToApp::Input`,
and `AgentInput`'s `Act`, `Key` and `Text`. The enums are where a new variant
lands and the variants are where a new field lands, and both are things an
adapter matches on. Without the attributes, one new key, one new field or one
new input kind would fail to compile every app that had integrated taria.
With them the cost is a `..` at the end of a pattern, a wildcard arm per enum
matched on, `Modifiers::NONE` and `Modifiers::new` in place of the struct
literal `Modifiers` closes, and a constructor in place of each marked
variant's literal: `AgentInput::act`, `key` and `text`, `AppToBridge::hello`
and `ack`, `BridgeToApp::input`.

Through this adapter, the wildcard arm is not where an input kind you cannot
read arrives. That is `AgentInput::Unknown`, the fallback a kind added after
your build parses as, and the layer answers it `Ignored` on your behalf and
never queues it, counting it in `unknown_inputs()`. The fallback exists
because the `InputId` sits on the message rather than inside the input, so
degrading the input is what keeps the id and makes an ack possible at all;
without it the line fails whole and the agent waits out a timeout for an ack
that was never going to come. What does reach your wildcard is the other
case: your app rebuilt against a taria that added an input kind, before you
have written the arm for it. The arm is what makes that a recompile.

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

fn apply_text(app: &mut App, text: &str) -> Applied {
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
        Applied::Handled
    } else {
        Applied::Ignored
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

`Key` keeps the raw lowering deliberately. An agent sending `key d` is asking
for the `d` binding, and an app that routed keys into its draft too would have
no fallback left for the parts of its UI with no semantic coverage. That
difference is worth one line in your own README, because `type_text` is the
one an agent has to aim: the tree's focused node is where it will land.

## Report what the layer threw away

Four counters record agent traffic that went nowhere. Read them after the
terminal is restored, never while the alternate screen is up.

```rust
ratatui::restore();

// The agent sent input faster than the loop drained it. Each drop was
// acked `Dropped`, so the agent knows; this is the app-side tally.
let dropped = layer.dropped_inputs();
if dropped > 0 {
    eprintln!("my-app: dropped {dropped} agent input(s): could not keep up");
}
// Inputs discarded because the bridge connection they arrived on ended
// first. Nothing on the agent side is left to hear about these, so the
// person running the app is the only one who can.
let stale = layer.stale_inputs();
if stale > 0 {
    eprintln!("my-app: discarded {stale} agent input(s): bridge went away");
}
// Inputs whose kind this build of taria cannot read. The layer acked each
// one `Ignored` for you, so the agent was told; the version gap is yours.
let unknown = layer.unknown_inputs();
if unknown > 0 {
    eprintln!("my-app: could not read {unknown} agent input(s): raise the taria dependency");
}
// Answers the bridge was too slow to collect. These inputs were applied;
// it is the acks that were dropped, so the agent calls waiting on them
// timed out instead of hearing what happened.
let acks = layer.dropped_acks();
if acks > 0 {
    eprintln!("my-app: lost the answer to {acks} agent input(s): the bridge read too slowly");
}
```

All four are monotonic across reconnects and all four are 0 on a disabled
layer. The input queue holds 256, so a nonzero `dropped_inputs` means an agent
outpaced the loop by a lot; a loop that only wakes on keyboard events gets
there first. A nonzero `stale_inputs` means a bridge session ended with input
still queued, which a harness restart mid-call will do. A nonzero
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
to `<temp dir>/taria-<uid>/<label>.sock`. `$TARIA_SOCK` replaces both and is
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
`TariaLayer::bind_at`.

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
cargo build -p taria-mcp
claude mcp add taria -- /path/to/taria/target/debug/taria-mcp --app my-app
```

That registration line is Claude Code's; any MCP harness works, and the
README's quick start walks the same steps against the demo app.

The four things worth confirming by hand:

- Exactly one node is focused in every state, including the empty ones.
- Every id you publish still names the same thing after a delete.
- An act your app deliberately refuses comes back as an ignored ack, not as
  silence.
- `type_text` sent while nothing is accepting typing comes back ignored, and
  changes nothing. Send a word carrying letters your app binds; the demo's was
  "deploy".

`scripts/e2e.py` and `scripts/adversarial.py` do this against the demo and
are worth reading as a list of what can go wrong.
