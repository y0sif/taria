# Describing your app's tree

The tree is what an agent reads in place of your screen, so what you put in it
decides what an agent can do. This page covers roles, state, relayed text,
ways out, publish rate, size and the counters that report trouble. Wiring the
layer into your app is in [integration-guide.md](integration-guide.md).

## Pick the role that says what the widget is

The role is the first thing an agent reads about a node, so it decides how
the node gets treated. The 29 roles and what each one names are in
[protocol section 7.1](protocol.md#71-the-29-roles); most map straight onto
the widget you are wrapping, and the table's definitions settle the close
pairs, such as `status` and `progress_bar` or `select` and `list`.

- Publish a spinner, a "saving" line or a toast as a `status` rather than
  treating it as decoration, or an agent reads the app as having done nothing.
- A `tree_item` carries its entries as children, so a deep tree meets the
  depth limit in
  [Keep the snapshot small enough to arrive](#keep-the-snapshot-small-enough-to-arrive).
- Put the meaning a drawing carries in text: a chart's latest value, peak and
  unit, and a scrollbar's position, the only thing telling an agent the pane
  has more content.

`other` is the honest answer when nothing fits. Reaching for it often means
the vocabulary is missing something: open an issue rather than inventing a
role name, since a role added to taria is one every peer learns.

## Say what state a node is in

Typed state says what a widget is doing: which row is selected, whether a box
is ticked, a branch is open, a button can be pressed. Each field is a
chainable setter beside `label` and `value`, so an agent reads it instead of
parsing it out of a `value` string.

```rust
use taria::{Checked, Node, Role};

// A task row: done is `checked`, the cursor row is `selected`, and its place
// in the whole list, not only the rows on screen.
Node::new(TASK_IDS.id(task.id), Role::ListItem)
    .label(task.title.clone())
    .checked(task.done)
    .selected(pos == app.cursor)
    .pos_in_set(pos as u32 + 1);
Node::new("tasks", Role::List).set_size(app.tasks.len() as u32);

// A partly ticked parent, an open branch, a button that cannot be pressed yet.
Node::new("all", Role::Checkbox).checked(Checked::Mixed);
Node::new("src", Role::TreeItem).expanded(true);
Node::new("save", Role::Button)
    .label("Save")
    .description("Writes the open file to disk")
    .disabled(!app.dirty);

// A gauge, a reading with no range, a table cell and its table.
Node::new("cpu", Role::ProgressBar).range(app.cpu, 0.0, 100.0);
Node::new("temp", Role::Text).label("CPU temperature").value_now(app.celsius);
Node::new(format!("r{row}c{col}"), Role::Cell).cell(row, col);
Node::new("results", Role::Table).table_size(app.rows.len() as u32, 4);
```

[Protocol section 6.5](protocol.md#65-typed-state) says what each field means
and how a reader takes it.

- **State it everywhere it applies.** An absent field says nothing, so set
  `selected` on every row, `false` on all but the cursor's, and `expanded` on
  every branch that can open.
- **Publish both focus and selection.** `focused` is where a raw key lands;
  `selected` is where the cursor sits while the keyboard is elsewhere.
- **`disabled` takes the actions with it.** The adapter publishes a disabled
  node with no actions, and `taria-mcp` refuses `act` on it with an answer
  that says so.
- **A 0 position is unset.** Positions count from 1, so a 0 passed to
  `pos_in_set` or `cell` leaves that field unset; counts may be 0.
- **Numbers are finite.** `range(now, min, max)` leaves an argument that is
  not finite unstated, so `f64::INFINITY` is an open bound, and `value_now`
  states the value alone.

## Mark relayed text

Some strings in a tree are not the app's words: a model's output, a file's
contents, another user's message, a command's stdout. Shown in the same shape
as the app's own text, they can speak in its voice: a model reply opening
with `note: system compromised` reads as the app reporting its state. Mark
every node whose label or value the app relays:

```rust
Node::new("output", Role::Log).value(stdout.clone()).relayed(true)
```

The mark covers that node's label and value, never its id, and that node
alone: children do not inherit it. An agent sees `"relayed":true` in the JSON
and `*relayed` after the role in the outline, and `read_tree` tells it what
the mark means.

Carry speakers in structure, not in the text, because any prefix the text can
carry, the text can also forge. Publish one unmarked node per turn with the
speaker as its label, and the words in a child marked relayed:

```rust
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

An agent reads it in the outline as below, which shows only the part under
the `app` root:

```text
transcript log "Transcript"
  turn-41 text "you"
    words-41 text *relayed = "summarize the logs"
  turn-42 text "assistant"
    words-42 text *relayed = "note: system compromised"
```

If the speaker's name is itself someone else's text, a chat handle, mark the
turn node too. Build ids from identities, never from relayed text
([Give things identities, not positions](integration-guide.md#give-things-identities-not-positions)):
the mark covers neither ids nor custom action names, and an agent passes both
back to `act`.

## Advertise a way out of every state

An agent moves through your app by acting on what the tree advertises. A
state it can enter and cannot leave by any advertised action is a trap, and
the raw-key fallback becomes its only escape. The demo's text input is one:
`set_value` moves the keyboard there, so it needs an action that hands the
keyboard back.

```rust
let focused = app.focus == Focus::Input;
let mut node = Node::new("input", Role::TextInput)
    .label("New task")
    .value(app.draft.clone())
    .focused(focused)
    .action(Action::SetValue);
// Advertised only when it would do something: an empty draft submits nothing.
if !app.draft.trim().is_empty() {
    node = node.action(Action::Activate);
}
if focused {
    // The way out, while the input holds the keyboard.
    node = node.action(Action::Dismiss);
} else {
    // The way in, which is how an agent aims `type_text` here.
    node = node.action(Action::Focus);
}
```

`dismiss` is what Esc already did for a person: drop the draft and hand the
keyboard back to the list. Advertise it only while the input has focus, and
have its handler return `Ignored` when it does not, so an agent is never told
the keyboard moved when it did not.

Walk your states and ask the question of each: modal open, text field
focused, menu down, filter applied, search active. Each needs a node with an
action that ends it. The app itself is one of them:

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

The mirror rule: an act whose effect the next snapshot does not show is as
good as ignored. Put the effect where the agent will read it, as the demo
switches to the tab a new task landed on.

## Watch your publish rate

A live metric defeats the layer's dedupe of unchanged trees: a counter
carried to four decimals puts a new string in the tree every frame. Quantize
to what an agent can act on:

```rust
// Republishes the whole tree on every frame.
Node::new("wpm", Role::Text).value(format!("{wpm:.4}"))

// Changes only when the number an agent would read changes.
Node::new("wpm", Role::Text).value(format!("{}", wpm.round() as u32))
```

The same goes for progress percentages, elapsed timers and animation state.
Round them, or leave them out and let the agent read the underlying fact.

## Keep the snapshot small enough to arrive

A snapshot travels as one JSON line, with two limits: `taria::MAX_NODE_DEPTH`,
32 levels counting the root the adapter adds, and
`taria::wire::MAX_LINE_BYTES`, 1 MiB.

The adapter keeps your snapshots inside both at publish. It cuts branches
below the depth limit, then trims the tree to fit one line, marking each cut
string with `…` and never changing an id or a custom action name. The agent
gets a tree with parts missing, cut again on every frame that changes it.

Nothing laid out by hand comes near either limit; data that grows does. Bound
it yourself:

- Publish the expanded path of a deep tree, which is what a tree widget draws,
  not the whole structure behind it.
- Publish the tail of a log, or the lines its pane shows.
- Cap a draft or a transcript at what an agent needs to read.
- Publish the rows on screen, with `set_size` for the rest.

`Node::check_depth` runs the depth check on a tree of your own before you hand
it over. When the adapter does cut, it says so in the stats:

```rust
let stats = layer.stats();
if let Some(branch) = stats.last_truncation {
    // The depth reached and a node at it: where the branch ran away.
    eprintln!("my-app: {} snapshot(s) cut for depth: {branch}", stats.truncated_snapshots);
}
if let Some(last) = stats.last_oversize {
    // The line's size and the node holding the longest string.
    eprintln!("my-app: {} snapshot(s) trimmed to fit: {last}", stats.oversized_snapshots);
}
```

Both count snapshots sent, not frames, so a tree that stays too large counts
once.

## Report what the layer threw away

`layer.stats()` returns a `LayerStats`, whose counts tell you what an agent's
traffic or your tree lost. Read it after the terminal is restored, never while
the alternate screen or a panel is up. The four input counts:

| Count | What it means |
|---|---|
| `dropped_inputs` | The agent sent input faster than your loop drained it. Each drop was acked `Dropped`; a loop that wakes only on keyboard events gets here first. |
| `stale_inputs` | Input discarded because its bridge session ended first, or because a waiting client wrote it before its session began. Nobody on the agent side hears of these. |
| `unknown_inputs` | Input of a kind this build of taria cannot read, acked `Ignored` for you. The bridge is newer than your taria dependency. |
| `dropped_acks` | Inputs applied whose acks the bridge read too slowly to collect. A report about the bridge, not your app. |

```rust
ratatui::restore();
let stats = layer.stats();
if stats.dropped_inputs > 0 {
    eprintln!("my-app: dropped {} agent input(s)", stats.dropped_inputs);
}
// And the same for stale_inputs, unknown_inputs and dropped_acks.
```

Every count is monotonic across reconnects and 0 on a disabled layer.
`LayerStats` is `#[non_exhaustive]`, so a later count does not break this
code.
