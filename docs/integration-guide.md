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
touched only indirectly, by the raw-input fallbacks lowering into it, which is
its own section below.

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

One timing rule matters. Ack `Ignored` before you publish your next frame.
Acks are flushed ahead of the pending snapshot in every writer pass, so an
ack queued first arrives first, and the bridge reports the refinement. Publish
first and the bridge sees a changed tree behind a plain `Delivered`, returns
that tree, and the `Ignored` arrives after the answer has already gone out.
Draining before drawing, as above, gives you this for free.

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

## The fallbacks, and what they are for

`key` sends one raw key press, up to 64 times with `repeat`. `type_text`
sends a literal string. Both exist so partial semantic coverage is still
useful: an app with three nodes and one keybinding is drivable today, and the
rest can be covered later.

Neither should be the primary path. An action a node advertises is stable
under a UI change; a keystroke is a guess about a keymap. Cover what an agent
needs to do with `act`, and let the keys handle the rest.

Lower both through the same handler a keyboard event takes:

```rust
AgentInput::Key { key } => match to_crossterm_key(&key) {
    Some(key) => {
        handle_key(app, key);
        Applied::Handled
    }
    // The bridge parses the same grammar before sending, so this is rare.
    None => Applied::Ignored,
},
AgentInput::Text { text } => {
    let keys = text_to_keys(&text);
    if keys.is_empty() {
        return Applied::Ignored;
    }
    for key in keys {
        handle_key(app, key);
    }
    Applied::Handled
}
```

Be honest about what that means: text is typing, not appending to a field. It
lands wherever focus is. Sent while a list has focus it meets the list's
single-key bindings, where `q` may quit and `d` may delete. An agent that
wants text in an input focuses the input first. Say so in the app's own docs
if the distinction can bite.

Lowering into the keyboard path also means an agent can reach every chord
your app binds, and some it does not. The demo's list handler matched key
codes while ignoring modifiers, so `ctrl+q` hit the `q` binding and quit the
app, and `ctrl+enter` confirmed a deletion in the dialog. Neither chord was
a binding the app meant to have. If your handlers match on `KeyCode` alone,
gate them on modifiers before you expose them to an agent.

## Report what the layer threw away

Two counters record input that never reached your state. Read them after the
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
```

Both are monotonic across reconnects and both are 0 on a disabled layer. The
queue holds 256 inputs, so a nonzero `dropped_inputs` means an agent
outpaced the loop by a lot; a loop that only wakes on keyboard events gets
there first. A nonzero `stale_inputs` means a bridge session ended with
input still queued, which a harness restart mid-call will do.

## Socket paths

By default the socket is `$XDG_RUNTIME_DIR/taria/<label>.sock`, falling back
to `<temp dir>/taria-<uid>/<label>.sock`. `$TARIA_SOCK` replaces both and is
used verbatim: on the app side always, and on the bridge side whenever the
bridge derives its path from `--app`. A bridge started with `--socket <path>`
takes that path and never reads the variable, so set the variable for both
processes rather than mixing the two ways of saying it.

Unix domain socket paths are capped at 107 bytes, because `sun_path` holds
108 including the NUL. It is a low limit and a deep `$XDG_RUNTIME_DIR` or a
long app label reaches it. taria checks before binding and its error names
the path, its length, the limit, and the way out, rather than the kernel's
`InvalidInput: path must be shorter than SUN_LEN`. The way out is one
variable, set the same way for both processes:

```bash
TARIA_SOCK=/tmp/my-app.sock ./my-app
TARIA_SOCK=/tmp/my-app.sock taria-mcp --app my-app
```

The parent directory is created with mode `0700` and vetted before binding: a
real directory, owned by you, with no group or other permission bits. If you
point `$TARIA_SOCK` somewhere shared, expect binding to be refused.

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

The three things worth confirming by hand:

- Exactly one node is focused in every state, including the empty ones.
- Every id you publish still names the same thing after a delete.
- An act your app deliberately refuses comes back as an ignored ack, not as
  silence.

`scripts/e2e.py` and `scripts/adversarial.py` do this against the demo and
are worth reading as a list of what can go wrong.
