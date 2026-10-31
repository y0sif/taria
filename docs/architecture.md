# Architecture

How the pieces fit together, and why the protocol is shaped this way. For what
a conforming peer has to do, message by message, see `protocol.md`: that is
the normative specification of `PROTOCOL_VERSION` 1 and it wins wherever this
file and it disagree. For why taria exists, see `landscape.md`. For adding
taria to an existing ratatui app, see `integration-guide.md`. For how taria
compares with the screen-level tools, see `comparison.md`.

## Components

| Component | Location | Role |
|---|---|---|
| Protocol types | `crates/taria` | `Snapshot`, `Node`, `Role`, `Action`, `AgentInput`, the `wire` messages, and the three modules both peers share: `key`, `id`, `socket`. Pure, no I/O, one dependency (serde). |
| Ratatui adapter | `crates/taria-ratatui` | `TariaLayer` serves the app's Unix socket, or on Windows its named pipe, publishes snapshots, feeds agent input into the event loop, and acknowledges every input. `FrameRecorder` and `sem` record nodes per rendered frame. |
| MCP bridge | `crates/taria-mcp` | Stdio MCP server (rmcp) exposing `read_tree`, `act`, `key`, and `type_text`. A socket-manager task owns the connection to the app and republishes its acks. The binary opens that connection on its first tool call and lets it go after 60 s with no tool calls. |
| Demo | `examples/demo-app` | `taria-demo`, a task-manager TUI an agent drives end to end. |

## Data flow

```text
up:   build nodes -> Snapshot -> socket -> bridge watch channel -> read_tree
      dequeue input -> Ack -> socket -> bridge broadcast channel -> tool result
down: act/key/type_text -> validation -> Input{id} -> socket -> event loop
```

The app builds its semantic nodes for the state it just drew and publishes
them as a snapshot. The bridge keeps only the latest snapshot, so agents see
current state, never a backlog. Agent input travels the same socket in the
other direction and is applied by the app exactly like keyboard input.

Every input carries an id, and the app answers that id with an ack. The ack
is what separates "the app acted on this" from "the app never saw it", which
a snapshot diff cannot tell apart: an unrelated redraw looks like success,
and an input the app deliberately dropped looks like a slow one.

## Wire protocol

- Transport: a Unix domain socket, or on Windows a named pipe in byte mode
  (see Transports below), carrying newline-delimited JSON (ndjson), the same
  bytes on either. One JSON object per line.
- Up (app to bridge): one `hello` per connection, carrying `app_label` and
  `protocol_version`, then `snapshot` and `ack` messages.
- Down (bridge to app): `input` messages carrying an `id` and an
  `AgentInput`: a semantic act, a raw key, or literal text.
- Framing: both message enums are internally tagged, so every line carries a
  `type` naming its variant. That key frames the message on the transport and
  says nothing about the tree, so the bridge drops it from the snapshot it
  hands an agent, and the stamp (`after_input`, below) with it. `read_tree`
  returns the snapshot, not the line carrying it.
- Ids: the bridge tags each input with an `InputId` and must not reuse one
  for the lifetime of its process. An ack can outlive the connection it was
  sent on, so ids that never repeat are what stop a dead app's ack from
  answering a live input. A process-wide counter satisfies this.
- Line cap: both sides refuse incoming lines over 1 MiB
  (`taria::wire::MAX_LINE_BYTES`) and treat the connection as broken rather
  than buffering without bound. The adapter trims every snapshot to fit; see
  Snapshot size below.
- `seq`: counts the trees published within one app run; it never decreases,
  and repeats only on a resend whose sole change is `after_input` (Dedup,
  below). It resets when the app restarts, so the bridge detects change by
  comparing trees on their `root`, not by `seq` ordering. `seq` signals
  staleness only within one connection.
- Dedup: a frame whose tree is the one the previous frame built is skipped
  before anything is cut or measured, and a frame whose cut tree is the
  snapshot already sent is skipped after the cut. Either way `seq` does not
  move. The one exception is a stamp moving: once the app has dequeued an
  input since the last snapshot went out, that snapshot goes out again,
  neither cut nor measured again, with only `after_input` moved and under
  the same `seq`.
- Stamps (0.4.0, issue #17): every snapshot carries `after_input`, the newest
  input the app had dequeued on this connection when it published, and the
  hello says `"stamps":true`. Only an input `drain`, `try_recv` or
  `recv_timeout` handed over stamps: a dropped one, one of a kind the build
  cannot read, a stale one and one written before the hello never do. Stamps
  belong to a connection, because ids belong to a bridge process, so the
  latest snapshot a new connection is greeted with goes out unstamped. The
  bridge credits a tree to an input only once it is stamped at that input,
  which is what lets a slow handler's tree reach its own input's answer
  (What the bridge does with each outcome, below).
- `focused`: serialized on every node and accepted when absent, where absence
  reads as `false`. Nothing on the wire moves today; what the pairing buys is
  that omitting the field for the nodes that are not focused stays an additive
  change later, and a 500-node tree spends around 8 KB per publish on
  `"focused":false` against an agent's output budget. Relaxing what a peer
  accepts is only free while no peer yet relies on it, which is why it lands
  under the freeze rather than when it is needed.
- `relayed`: an optional boolean, omitted when `false`, saying a node's label
  and value come from outside the app (a model's output, a file's contents,
  another user's message), so an agent reads them as data rather than as the
  app's own words. Absent says only that the node is not marked, not that the
  app wrote it. Per node, never inherited, and never covering the id. An
  additive field that reaches an agent through any taria-mcp, since taria-mcp
  hands on the app's own line and one built before the field passes it on
  too. What it means is said once, in `read_tree`'s description in both
  formats, and not again in the instructions, which are resent with every
  model call as well. Protocol section 6.4 has the rule, and the integration
  guide shows a transcript that carries speakers in structure instead of in
  prefixes the content can spell.
- Typed state, added in 0.4.0: `selected`, `checked` (`true`, `false` or
  `"mixed"`), `expanded`, `disabled` (sent only as `true`), `description`,
  `value_now`/`value_min`/`value_max`, `pos_in_set`/`set_size`,
  `row`/`column` and `row_count`/`column_count`. Flat optional fields on the
  node rather than a nested object, each omitted when unset, so a node that
  states nothing is the line 0.3.3 wrote, byte for byte. Absent means not
  stated, never `false`, which is why `selected` and `expanded` send `false`
  when they mean it. Every one of them reads a wrong shape as not stated
  instead of failing the node, unlike `focused` and `relayed`: a field that
  fails the node fails the snapshot, and a reader that skips it leaves the
  agent on a stale tree over one field. Numbers are `taria::Number`, a
  finite-only `f64`, so `Node` keeps `Eq` and the frame dedupe keeps working:
  a `NaN` is unequal to itself and would have republished every frame.
  Positions count from 1. `disabled` is enforced at both ends: the adapter
  strips a disabled node's actions at publish, and the bridge refuses `act`
  on one before looking at the action, so the agent is told why. An empty
  action list never means disabled, because the demo's modal strips actions
  from nodes that are not. Hidden has no field: a hidden node is not
  published. Protocol section 6.5 has the rules.

### Tree depth

A snapshot is one JSON object, so how deep a tree can be is decided by how far
a JSON parser will recurse. `serde_json`, which both reference peers use,
stops at 128 nested values, and every node costs two of them: its own object
and its `children` array. Measured through a whole `{"type":"snapshot",...}`
line rather than a bare node, 63 nested nodes parse and 64 fail.

Crossing that reports nothing anywhere. The reader skips the line it cannot
parse exactly as it skips a truncated one, so a deep first snapshot leaves the
bridge saying it has no tree while the app is connected and healthy, and a
deep later snapshot leaves it serving the last shallow tree with nothing
marking it stale. Writing is worse than reading: serialization recurses per
level too, and a tree thousands of levels deep exhausts the stack and aborts
the process from inside a library thread. Freeing one recurses the same way,
so every tree the adapter lets go of, a branch the cut removed, a frame never
published or a tree handed to a disabled layer, is freed one node at a time.

`MAX_NODE_DEPTH` is 32, a little under half the measured ceiling, and the
slack pays for the transport envelope, for a root an adapter adds above the
nodes an app hands it, and for a peer whose parser is stricter than
`serde_json`. The core crate states the limit and offers `Node::check_depth`,
which reports the depth measured and names a node found at it; it enforces
nothing, because what to do about a deep tree belongs to the adapter.

The ratatui adapter cuts. Over the limit, every node at the limit publishes
without its children, per branch, and the app is told through `stats()`'s
`truncated_snapshots` and `last_truncation` fields. The count is of snapshots
sent, not frames drawn, so a deep tree that sits still while the app redraws
it sixty times a second counts once. Cutting, because the other two answers
are the same failure: publishing the tree as built puts a line on the wire the
bridge cannot parse, so it skips it and goes on serving the last tree it read,
and skipping the publish is that same stale tree chosen deliberately. Only the
cut still delivers the part of the tree that is fine. The app is the peer told
about it because the app is the only one that can fix it, and the fix is
nearly always to publish what the widget draws rather than the data behind it:
for a deep tree view, the expanded path and the rows on screen.

### Snapshot size

A snapshot is one line, and both peers read a line up to
`taria::wire::MAX_LINE_BYTES`, 1 MiB, and no further. Unlike a deep tree, a
tree too large does not fail quietly: the reader cannot find where the next
message starts, so it closes the connection. That loops. The bridge
reconnects, the adapter greets it and sends the latest snapshot, which is the
same one, and the bridge closes that connection too, for as long as the app
holds the tree. One unbounded value is enough: a draft, a log, a transcript.

The bridge keeps the overrun as a reason, across reconnects, until a snapshot
parses or a connect finds nothing listening at the socket, which means the
app has gone. So `read_tree` and the input tools say the line was over the
limit and that only a smaller tree fixes it, instead of that the app may have
exited, and an app that overran and then exited reads as exited.

The ratatui adapter trims. A frame whose tree is the one the previous frame
built stops before the bound, so a tree that sits still costs one comparison
whether or not it fits. A changed tree is bounded at publish, after the depth
cut, because measuring it serializes it, which recurses, and the cut is what
keeps that safe. It is measured exactly, by serializing it into a counter
that keeps nothing, and one over the limit is cut in stages, stopping as soon
as the tree fits:

1. every node that could never be published goes, with everything under it,
   and its siblings stay. That is a node whose id, role, actions and strings
   cut to 256 bytes of JSON cannot fit with only its ancestors beside it;
2. the longest values and descriptions, all down to one shared length, then
   labels with them, the same way. A description is content, like a value;
   a label is a name. No string of 256 bytes of JSON or less is cut;
3. every node keeps at most one shared number of children, the largest that
   fits. This stage and the next plan as stage 1 does, with every string at
   that 256-byte floor. Once they have dropped nodes, the kept tree's strings
   are cut only as far as it needs: not at all if it fits whole, otherwise as
   in stage 2;
4. shallowest first, the children that cap removed go back while they fit.
   Each parent keeps a run of its first children with no gap, apart from a
   node that could never fit, and a child too large for the room left closes
   its parent's list;
5. if all else fails, a bare root, which always fits.

A cut string is marked with `…` (U+2026). A `log` or `terminal` value keeps
its tail, since that is where those grow; every other string keeps its head.
Ids and custom action names are never changed, because they are what an agent
sends back: a node it can read but not address is worse than one it cannot
read at all. Focus is decided after the bound, since a dropped row can be the
focused one. The line is sized for the largest possible `seq` and the largest
stamp, so the same tree is always cut the same way and dedup still works on
it: an edit past the cut of a trimmed value produces the tree already sent,
and nothing goes out, or only that tree again with its stamp moved, which
still fits.

The app is told through `stats()`'s `oversized_snapshots` and
`last_oversize` fields, the second a `SnapshotTooLarge` that carries the size
the line would have had and the node holding the longest string, ids and
custom action names included. Like `truncated_snapshots`, the count is of
snapshots sent, not frames drawn.
Trimming rather than withholding, for the reason the depth cut gives: a
withheld tree leaves the agent on a stale one.

### Key strings

`AgentInput::Key` carries the press as a plain string, so every adapter and
every bridge has to read the same grammar or the two disagree about what a
key means. `KEY_GRAMMAR` in `crates/taria/src/key.rs` is the normative
statement, and this is it verbatim, phrased there to follow "expected":

> a single character (`a`, `Q`, `?`, `+`), or a named key (enter, esc, tab,
> backtab, backspace, delete, up, down, left, right, home, end, pageup,
> pagedown, space, f1 through f12, plus the aliases return, escape, del),
> optionally prefixed with modifiers joined by `+` (ctrl, alt, shift;
> `control` is an alias for ctrl). Names and modifiers are case-insensitive,
> a single character keeps its case. Examples: `q`, `Q`, `ctrl+c`,
> `alt+enter`, `ctrl+shift+p`, `space`

Two rules the sentence above leaves implicit, both pinned by tests in the
same module. A `+` that opens or closes the rest is the base key rather than
a separator, so `ctrl++` is ctrl plus the `+` character while `+a` and
`ctrl+` are errors. And `shift+tab` and `backtab` are the same press, because
a terminal delivers shift+tab as a distinct backwards tab.

A Rust peer parses this with `taria::key`, which is why the bridge's verdict
on a key and the app's are identical by construction. An adapter in another
language reimplements it, and `KeyPress`'s parse/render round-trip is the
contract to reimplement against: every press the parser can produce renders
to a string that parses back to the same press.

### Roles and actions

A node's `role` says what the widget is; its `actions` say what an agent may
do with it right now. Both are open vocabularies, and both degrade rather than
fail on a name the reader does not know, which is what lets either grow inside
version 1.

Twenty-nine roles, in the snake_case they take on the wire:

| Group | Roles |
|---|---|
| Structure | `app`, `pane`, `dialog`, `tabs`, `tab`, `menu`, `menu_item` |
| Collections | `list`, `list_item`, `tree`, `tree_item`, `table`, `row`, `cell` |
| Controls | `text_input`, `button`, `checkbox`, `select`, `option`, `link` |
| Readouts | `text`, `log`, `progress_bar`, `status`, `scrollbar`, `chart`, `image`, `terminal` |
| Fallback | `other` |

Eleven of those (`tree`, `tree_item`, `select`, `option`, `link`, `log`,
`terminal`, `image`, `chart`, `status`, `scrollbar`) were added before the
freeze, on the evidence of a census of 15 real ratatui apps: 21% of on-screen
widgets had no defensible role and fell to `other`, and 8 of ratatui's 15
publishable built-in widgets had no role at all. `scrollbar` appeared in 6 of
the 15 apps, more often than `tabs`, which did have a role, while `button`
fired zero times and `checkbox` twice. The first vocabulary was an HTML forms
model rather than a description of what terminals put on screen. Adding the
role is what the accessibility prior art tells an author with no matching role
to do; the alternative, letting each app name its own, gives every agent a
different vocabulary to learn. `docs/integration-guide.md` covers which role
to reach for.

Seven actions: `activate`, `focus`, `select`, `toggle`, `scroll`, `set_value`,
`dismiss`. Anything else travels as `{"custom":"<name>"}`. The envelope folds
back onto the built-ins on the way in, so `{"custom":"dismiss"}` reads as
`dismiss` rather than as a custom action shadowing it. That is what lets an
action name graduate to a built-in: an older peer reads the new name as
custom, correctly, keeps it, and echoes it back in the only form it has, and
the newer peer's own arm still fires instead of accepting the act and doing
nothing.

The envelope is a wire encoding, not an argument. `act` takes the name alone,
so a node advertising `{"custom":"delete"}` is invoked with `"delete"`. An
agent told to take action names from the tree meets that difference on its
first custom action, so in the default JSON mode `act`'s description states it
rather than leaving it to be inferred. The server's instructions do not repeat
it: they and the tool definitions are both resent with every model call, so
the rule is said once, in the tool it governs. With `--tree-format outline`
the outline prints the name `act` takes, `delete`, so an agent reading it
never meets the envelope at all.

### Version 1 is frozen

`PROTOCOL_VERSION` is 1 and the format is fixed. Within version 1, changes
must be additive. Two properties make that safe: a reader skips a line it
cannot parse instead of dropping the connection, and serde ignores unknown
fields. So a new optional field, or a whole new message variant, reaches an
older peer as something it quietly ignores.

Neither property covers a value nested inside a message the peer does want,
so the open vocabularies and the typed state carry their own fallbacks, four
in all. An unknown `Role` reads as `Role::Other`. An unknown `Action` name
reads as `Action::Custom` keeping that name, so an agent can still advertise
it, echo it back, and have the app recognize it. Without those, one leaf node
using a role added later makes the whole snapshot unparseable, the reader
skips every line, and the app looks frozen with no error anywhere.

An `AgentInput` whose `kind` this build does not know is the third, and it
degrades for a sharper reason than the role and the action. The `InputId`
sits on the message rather than inside the input, so a `kind` that failed the
line would take the id with it, and an agent would wait out the bridge's
window for an ack that was never possible. `AgentInput::Unknown` is a
`#[serde(other)]` fallback carrying nothing at all, which is enough: the
input arrives with its id intact, the adapter answers it `ignored` on the
app's behalf without queueing it, and the agent learns in one round trip that
this app can do nothing with what it asked for. An app never sees the
variant.

The typed state is the fourth: a state field in a shape this build cannot
read, such as a `checked` of `"partial"`, reads as not stated, and the node
keeps the rest. Read strictly, the field would fail its node, the node its
line, and the agent would be left on a stale tree over one field.

`InputStatus` has no fallback. A status this build does not know fails its
ack and the reader skips that line, which leaves the input reading as
unacknowledged. That is the safe reading, and the loss is one ack rather
than a tree.

Degrading keeps the connection; passing the line on keeps the information.
The bridge stores each snapshot as the app's own line beside the parse of it,
and it is the line an agent reads. So a role, an action or a field this build
has never heard of reaches the agent by name instead of flattened into
whatever the typed struct could hold: an app publishing `"role":"sparkline"`
had the agent read `"role":"other"`, with the real name on the wire the whole
time. The parse stays authoritative for what the bridge decides, because it
is typed: `act` validates node ids and advertised actions against it, and
change detection compares parses rather than text. A line is passed on only
after it parsed, so a malformed one is still skipped.
`taria-mcp --tree-format outline` hands the agent an indented outline instead
of the line, and renders it from the line for the same reason, so the outline
keeps every role, action and field name the JSON would have shown. It drops
only the extra keys inside an action object, since `act` takes the action's
name alone.

Additive on the wire is not automatically additive in Rust. A peer skips a
message variant it cannot parse, but a peer rebuilt against the version that
added one meets it in an exhaustive `match` and stops compiling, and the peers
this format is written for are exactly the ones that match on these types: an
adapter dispatching `BridgeToApp`, a bridge dispatching `AppToBridge`. So every
type a version-1 addition can reach is `#[non_exhaustive]`. Twelve of them:
`AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`, `Action`, `Role`,
`Node`, `Checked`, `Snapshot`, `key::Key`, `key::Modifiers` and
`key::KeyPress`. Marking a type is itself a breaking change, which is why it
was done before the first release rather than at the first addition. Each
costs an outside peer one wildcard arm, or one `..` in a pattern, and buys
back that a new variant, field, role, action or key is a recompile rather
than a repair. `Modifiers` gains `NONE` and a const `new` in exchange for the
struct literal it closes, and `KeyPress` already had the const `new` its own
marking needs.

The same reasoning reaches one level in, to the variants. A new optional
field on an existing message is the format's cheapest additive change and
also the one that breaks an outside peer hardest, because it breaks
everything that builds or destructures that message. So the six struct-like
variants are marked too: `AppToBridge::Hello` and `Ack`,
`BridgeToApp::Input`, and `AgentInput`'s `Act`, `Key` and `Text`. A marked
variant has no struct literal from outside the crate, so each has a
constructor beside it (`AppToBridge::hello` and `ack`, `BridgeToApp::input`,
`AgentInput::act`, `key` and `text`), and a peer destructuring one ends the
pattern with `..`. `AppToBridge::Snapshot` needs neither: it is a newtype
around `Snapshot`, which is marked already and built by `Snapshot::new`.

Anything else needs a version bump: removing a field, renaming one, making
an optional field required, or changing what an existing field means. The
last is the dangerous one, because an old peer parses it and acts on the old
meaning. `crates/taria/src/wire.rs` is the normative statement of this rule.

Both fallbacks are hand-written deserializers calling `deserialize_any`, so
`Role` and `Action` decode from self-describing formats only. ndjson is one;
bincode and its relatives are not.

### Version mismatch

Peers on different versions disagree about the shape of every message, so an
app on another version cannot parse a single input this bridge sends it. The
bridge splits its tool surface by direction: it keeps the connection and
logs a warning, `read_tree` keeps working for as long as the peer's snapshots
still parse, and `act`, `key`, and `type_text` refuse up front with an error
naming both versions, having sent nothing. Forwarding input across a mismatch
would leave the agent waiting on a session that can never react.

Reading across a mismatch is the common case, not a guarantee. A bump is
defined by the changes that break parsing, so a peer that moved a field of
`Snapshot` delivers lines the bridge skips one by one, holds no tree at all,
and answers `read_tree` with "no snapshot from the app yet" rather than with
a degraded one. What survives a bump is whatever the two versions still
happen to spell the same way.

## Input acknowledgement

`InputStatus` has three values. One input may be acked more than once and
the last ack wins.

| Status | Meaning | Who sends it |
|---|---|---|
| `delivered` | The app's event loop dequeued the input. | The adapter, automatically, as it hands the input over. |
| `dropped` | The input never reached the app: its queue was full. | The adapter's reader thread. |
| `ignored` | The app looked at the input and deliberately did nothing. | The app, by returning it from the `layer.drain` handler, or with `layer.ack(id, InputStatus::Ignored)` and the id `try_recv` or `recv_timeout` handed over. |

`delivered` says only that the input was dequeued, before the app knows what
it will do with it. An app that then does nothing (an act a modal dialog
blocks, an unknown node id, a `set_value` with no value) refines that to
`ignored`, and should do so before the first snapshot it publishes after
dequeuing the input, which is the tree a stamping bridge credits to it
(protocol section 5.3). Returning `Ignored` from the `drain` handler does
that; an app that can decide only later publishes a status node saying the
input is under way instead.

Acks share the connection's writer with snapshots and are flushed first in
every pass, so an ack always reaches the bridge before the snapshot
sent after it, the stamped resend of an unchanged tree included. An
agent that saw the snapshot first could not tell whether it reflects its own
input yet.

### What the bridge does with each outcome

An input tool sends, then watches acks and snapshots. Both signals are
needed and neither is sufficient: an ack says the app saw the input but not
what it did, a new tree says something happened but not that this input
caused it. The window is not cut short by a tree arriving without an ack,
because the ack still owed can be a `dropped`, for an input that never
reached the app while an unrelated redraw did, and answering that with a tree
would report a dropped input as applied. A `delivered` does not cut it short
either: the app can still refine it to `ignored`. What a `delivered` never
becomes is a `dropped`, which says the input never reached the app at all.

How long it watches, and which tree answers, depends on whether the peer's
hello said it stamps. Against a peer that does not, the window is 500 ms and
any tree that differs from the one the input was aimed at is the answer, as
it was before 0.4.0, so a handler slower than the window reads as "no change"
and its tree lands in the next input's window (issue #17). Against a peer
that stamps, the window is 3 s and the stamp decides. A tree stamped at or
after the input is the tree published after the app took it: compared with
the baseline on its `root`, not its `seq`, it is the answer when it differs,
and "did not change" at once when it does not, so a no-op still costs about
a frame. A tree stamped before the input, or not at all, was published before
the app had it, so it becomes the baseline rather than the answer, and a late
tree is no longer credited to the input after it, but for one race
(protocol section 5.3, and below). For a burst the stamp has to reach the
last press, and the baseline is the tree before the first. An `ignored` ends
the wait at the first credited tree, or 500 ms after the ack; a `dropped` at
once; a disconnect within 500 ms of it, inside the 3 s cap. A peer whose
hello says `stamps` but whose snapshots go out unstamped where protocol
section 5.3 does not allow it is read, from the call that shows it for the
rest of the connection, as a peer that does not stamp (a tree that differs
is credited), and the bridge logs a warning. For the comparison to be
sound, the bridge claims each id and queues its input under one lock, so
ids reach the app in the order they were claimed even when tool calls
overlap. The one gap is the snapshot watch, which keeps only the newest
tree: a tree stamped before the input and the credited one arriving back to
back are read as the credited one alone.

| Outcome | Tool answer |
|---|---|
| Nothing could be handed over: the bridge's queue to the app stayed full for 500 ms | Error: this input was not sent; the app is stopped or not reading its socket (its pipe, on Windows). |
| A burst the bridge could not finish sending | Error: how many went out and may already have taken effect, how many did not, and that the effect is partial. |
| Acks lost (the bridge fell behind its own ack channel) | Error: what became of the inputs cannot be reported in full; call `read_tree`, send fewer inputs per call. |
| Any input of a multi-input burst `dropped` | Error: how many landed, and that the effect is partial. |
| The app acknowledged the input and then went away | Text: the app acknowledged it and disconnected, possibly because of it; no tree, because the app is gone. Not an error: the input's fate is known, and an advertised `quit` reaches this on purpose. |
| The app went away without acknowledging the input | Error: it disconnected before acknowledging this input, so whether the input was applied is not known. |
| `dropped` | Error: nothing was applied; send fewer inputs. |
| `ignored` | Text saying the app deliberately did nothing, plus the current tree to re-plan from. |
| `delivered`, tree changed | The new tree. |
| `delivered`, tree unchanged | Text: received, no change within 500 ms, from a peer that does not stamp. From one that does: received, no change, once a tree stamped at the input arrives unchanged. |
| `delivered`, no stamped tree within 3 s (a peer that stamps) | Text: received, but no tree published after it within 3000 ms; it may still be working, call `read_tree`. |
| No ack, tree changed | The new tree. |
| No ack, no change | Text: neither acknowledged nor changed within 500 ms; it may not report acks, or may not have reacted yet. From a peer that stamps, which acks every input it dequeues: neither acknowledged nor published a tree after it within 3000 ms (nor changed its tree, once its claim to stamp broke in the call); it may be stuck, busy or stopped. A tree stamped at the input that arrived unchanged answers "did not change" instead, ack or not. If a line was rejected inside the window, that line is quoted instead, because it may have been this input's ack. Otherwise, on Linux, if the app is stopped, the text says so instead, with its pid. The input waits in the socket, and the app applies it only if it resumes while the bridge still holds the connection: the window plus 30 s from the last tool call, since the input awaits its ack, and for good with `--idle-release 0`. A bridge that lets go first leaves the adapter to discard it. So the agent is told that deadline and how to resume the app (as for a stopped app below), then to call `read_tree` and resend the input if its effect is missing. A burst is spoken of as one, "these 3 inputs", unless a peer that stamps acknowledged part of it: then each of these answers says how many ("acknowledged 2 of the 3 inputs this call sent"), and the stopped-app one names only the rest as unread ("the other one"). |

The order matters. The two send failures come first: an input that never left
the bridge has no ack to wait for, and a burst cut short still waits out the
window, because its earlier copies can be dropped too, but answers with the
partial-send error whatever else it sees. "Some of this landed and some was
never sent" is the only true report of it. Below those, a drop outranks a
departure, because "never applied" stays true whether or not the app is still
there. A departure outranks a tree, because a tree would describe a UI that
no longer exists. Lost acks outrank the rest, because any of them could have
been a `dropped` this call was watching for.

Which of the two departure rows applies is decided by the ack, not by the
departure. That is where the error flag is drawn throughout: a call that was
invalid, or an input whose fate nobody can state. An acknowledged input has a
known fate, so an app that takes it and exits is reported and not raised.
Raised, it made every caller that branches on the flag read a working `quit`
as a failed call, and the prose saying otherwise did not help them.

When the bridge itself closed the connection, over a line past the cap, both
departure answers keep their opening and change their reason: the app did not
exit, its next tree was too large, and it resends that tree on every
reconnect, so nothing the agent sends can fix it.

An adapter that sends no acks at all still works, which is what the last two
rows are for.

### Tool-side validation

Rejecting bad input at the bridge costs one error instead of one round trip
and a silence.

- `act` checks the node id against the latest tree and lists the valid ids
  if it misses. It checks the action against that node's advertised actions
  and lists them if it misses. A `value` is bounded at 4096 characters, the
  same bound `type_text` takes, because it answers the same question and an
  agent that learns one limit should not then meet a second. The bound is also
  what keeps a value from being a weapon: it travels as one ndjson line, and
  both peers treat a line over 1 MiB as a broken connection, so an unbounded
  value was a single legal call that cut an app off from its bridge.
- `key` parses the key string with `taria::key`, the same parser the adapter
  lowers with, so a key the app would refuse is refused here with the
  grammar in the error. `repeat` is 1 to 64, and the key string itself is 64
  characters, checked before the parse: the grammar peels modifier prefixes
  without a limit, so a megabyte of `ctrl+` parses to the key it ends in and
  serializes to the line neither peer will read, the same single-legal-call
  hole the `value` bound closes.
- `type_text` takes up to 4096 characters. Where they go is the app's, and it
  is the app's typing surface rather than its key bindings. A surface that
  consumes key events can lower the text with the adapter's `text_to_keys`,
  one key event per character with `\n` as Enter and `\t` as Tab; one that
  grades characters iterates them itself, since those two are keys it may
  bind. An app accepting no typing at that moment answers `ignored`.
- Every input tool refuses while no app is connected. A queued input would
  otherwise be delivered to the next app instance. An on-demand bridge that
  has not connected yet, or has let the app go, connects first and waits
  briefly for the tree, as any first call does, so a tree that arrives in
  that wait lets the input through (see On-demand connect and idle release).

## Socket lifecycle

This is the lifecycle on Linux and macOS, where the endpoint is a Unix socket
file. On Windows it is a named pipe, which has no file to resolve, vet or
unlink; Transports, below, says what takes their place.

App side (`TariaLayer::bind`, or `bind_or_disabled`):

1. Resolve the path with `taria::socket::resolve_path`: `$TARIA_SOCK`
   verbatim, else `$XDG_RUNTIME_DIR/taria/<label>.sock`, else
   `<temp dir>/taria-<user>/<label>.sock`, where `<user>` is the effective uid
   where it is available (through `/proc/self` on Linux), else `$USER`, else
   `$LOGNAME`, else the literal `default`. That derivation lives in each peer
   rather than in the shared crate, so a third-party adapter that wants to be
   found through the fallback branch has to match it.
   The label is formatted into a file
   name, so it has to be one: a label carrying `/`, or `.`, `..` or empty, is
   refused rather than resolved. The adapter binds *and unlinks* what comes
   out, and `/etc/cron.d/evil` as a label makes the join discard the whole
   resolution and keep the absolute path, while `../../../tmp/pwn` walks out
   of the runtime directory. The check runs ahead of the precedence above, so
   a label is acceptable on its own terms and cannot pass on a machine that
   happens to set `$TARIA_SOCK`. An app that wants a path this rejects passes
   the path itself, to `$TARIA_SOCK` or to `bind_at`. The bridge refuses the
   same labels while parsing `--app`, where `--socket` remains the way to
   name a path.
2. Check its length. `sockaddr_un.sun_path` has to hold the path and its
   terminating NUL, so the limit is that buffer minus one, and the buffer is
   not the same size everywhere: 108 bytes on Linux, so 107, and 104 on macOS
   and the BSDs, so 103. A path in the band between the two binds on Linux and
   is refused on macOS, which is not hypothetical, because the macOS temp dir
   is around 49 bytes and the fallback path plus a long label lands in it. The
   kernel's own refusal names neither the path, its length, the limit, nor a
   way out; taria checks first and its error names all four, plus the platform
   the limit belongs to, since a bare number sends someone comparing it against
   the wrong `sun_path`. The bridge checks the same limit while parsing its
   arguments, so an over-long path never surfaces from inside the reconnect
   loop, where it would read as "the app is not running".
3. Create the parent directory with mode `0700` and vet it: a real
   directory (not a symlink), owned by the current user, no group or other
   permission bits. Binding is refused otherwise, so another local user
   cannot swap the socket.
4. Free the path, but only where freeing it is safe. A socket nothing is
   listening on is unlinked and the bind proceeds. A socket another instance
   is serving is refused with `AddrInUse`, naming the path and saying to give
   the second instance one of its own, because taking it over would leave the
   first running with no way for a bridge to reach it. Anything that is not a
   socket is refused with `AlreadyExists` and left untouched, since the path
   can come verbatim from `$TARIA_SOCK` and a typo there is not a reason to
   delete a file. The liveness probe is a `connect`, which the standard
   library offers no timeout for, so it runs on a thread of its own and is
   waited on for 250 ms; no answer in that time refuses the bind rather than
   unlinking a socket that may still be serving. Nothing here can park an app
   during startup, which is the one thing taria promises never to do.
5. Bind, and serve one client at a time from a listener thread. Each
   connection gets the `hello` plus the latest snapshot, then streams every
   new publish. Whatever a client that waited behind the previous session
   wrote before its `hello` is discarded first (see One connection owns its
   inputs). The listener socket is non-blocking and the thread polls a
   shutdown flag every 50 ms rather than parking in `accept()`: waking a
   parked `accept()` by connecting to the socket path fails exactly when it
   matters, because by then the path may hold a socket another process bound,
   whose listener takes the wake-up while this one stays parked forever.
   Dropping the layer shuts the threads down and removes the socket file, but
   only while that file is still the one this layer bound, compared by device
   and inode: a second instance that took the path over is serving a socket of
   its own there, and unlinking it would leave that instance unreachable.
   Before a connected bridge's connection closes, the acks still queued are
   written, so an app that answers an input and exits in the same frame does
   not leave the agent told that input's fate is unknown; a snapshot still
   waiting is not sent, because it describes a UI about to be gone. The flush
   gets 250 ms in all, so a drop takes at most one 50 ms poll, the 5 s the
   line being written may take, and that 250 ms.

`bind_or_disabled` turns any of those failures into an inert layer instead
of an error, so taria can never stop an app from starting. Every method
stays callable on a disabled layer, and the app reads `bind_error()` when it
chooses. A refused label is the one failure with no path behind it, so that
layer reports an empty `socket_path()`: the error names the label, which is
the thing that has to change, and an invented path would only look like
somewhere the socket might be. The layer prints nothing itself: a print once
the app owns the alternate screen garbles the display.

Bridge side (`taria-mcp`):

1. Connect on demand: nothing until the first tool call, then retrying
   while tool calls want the app. Backoff starts at 250 ms, doubles, and caps
   at 2 s. A connection that dies young without delivering a snapshot keeps
   the backoff growing instead of resetting it. After 60 s with no tool calls
   the bridge lets the app go and stops retrying until the next call (see
   On-demand connect and idle release). A library caller that builds the
   server with `TariaMcpServer::builder(..).connected()` gets the old
   behaviour: connect at once, retry forever, never let go.
2. On disconnect, the watch flips to `Disconnected`, keeping the app label
   and the last `seq`, so tool calls fail fast with an error that says which
   app went away instead of acting on a stale tree. Every such error opens
   "app 'X' disconnected (last snapshot seq N)", which is what a caller
   matches on, and the rest says why there is no tree:
   - When the bridge closed the connection itself, over a line past the cap,
     the error says that instead of "it may have exited": the app is still
     running, and sends the same line on every reconnect. This outranks the
     answers below until a connect finds nothing listening.
   - With a connection open again and a line on it the bridge could not
     read, the bridge says it has reconnected and quotes the reason, with
     the same two causes the never-served answer names (a tree past
     `MAX_NODE_DEPTH`, a taria whose snapshot shape differs). This outranks
     the handshake and the link. It is how an adopter who restarts the app
     after a change that broke its tree hears about it; until 0.3.3 the
     answer was one of the three below, each of which says to retry, and no
     retry fixes a tree the bridge cannot read (issue #16). The bridge
     forgets every such reason but the oversized one at each connect, before
     the connection reads as open, so a reason seen with a connection open
     is that connection's and never a previous instance's.
   - With a connection open again and the app's `hello` on it, the bridge
     says it has reconnected, that the app is running and nothing needs
     restarting, and that a retry is served the first snapshot the app
     publishes.
   - With a connection open again, no `hello` yet, and the connection under
     the 500 ms grace, the bridge says how many milliseconds ago it
     reconnected, and that this is worth one retry.
   - With the same silence past the grace, the reconnect has landed in the
     listen backlog (the kernel's queue of connections the app has not
     accepted yet) behind another bridge. That is what happens to the bridge
     that loses the race when an app restarts with two bridges after it, and
     to one that let go while another took the app. The error says the app
     is held by another bridge. When the departure the error opens with was
     this bridge letting go after its idle window, it says so after that
     phrase: the disconnect was on purpose, so another session could use the
     app, and is not a fault. Agents read the bare "disconnected" as one,
     and went after it with `ss`, `ps` and `pgrep`. It names its window once,
     then the hold past it ("after 60 s with no tool calls, or up to 30 s
     later while an input awaited its ack"), and says when to retry. The
     bridge records that its last departure was a release beside the record
     itself, with the pid of the app it let go of (`SO_PEERCRED`, below),
     and clears it once a connection serves a tree, the app closes one, or a
     connect finds nothing listening, or a pid listening other than that one
     (the app has gone since), so it cannot outlive the departure it
     describes. A pid the kernel did not name, on either side, is no change.
   - On Linux, with the same silence, the bridge first reads the state of the
     process at the other end from `/proc/<pid>/stat`. The pid comes from
     `SO_PEERCRED`, the pid the kernel records for the other end of a Unix
     socket, which for a connection still in the backlog is the listener's. A
     stopped app (`SIGSTOP`, a shell's Ctrl-Z, a debugger) keeps its socket
     listening and accepts nothing, which is exactly a held app's silence, and
     waiting for a release does not help it. Such an app is named with its pid
     and how to resume it, which the same `stat` line decides. A process with
     a controlling terminal, stopped outside that terminal's foreground
     process group, is a job suspended in its shell, and only `fg` there
     resumes it: a ratatui app turns Ctrl-Z off with raw mode, so one that
     suspends raises `SIGTSTP` itself. `kill -CONT` resumes it only in the
     background, where it runs, drawing over the shell, until its next use of
     the terminal stops it again, and one that suspended itself makes that use
     at once, taking the terminal back into raw mode. The agent cannot run
     `fg`, so it is told to ask the user to. Any other stop gets `kill -CONT`
     with the pid, and a tracing stop (state `t` in `/proc/<pid>/stat`) is
     sent to the debugger. After a release, the opening's sentence says the
     disconnect was that release and not a fault before it says the bridge
     has reconnected ("when this bridge let go of it on purpose, not as a
     fault, after 60 s with no tool calls, or up to 30 s later while an
     input awaited its ack"), and so do the three answers above that say it:
     told only that the app was suspended, an agent took the suspension for
     the cause. Only the window and the hold past it are the held answer's
     words; the rest of the clause is its own. Any failure to read the state
     leaves the held answer, and other platforms always get it. Only a bridge
     started on demand records the pid; one built with
     `TariaMcpServer::builder(..).connected()` gets the held answer, as in
     0.3.2.
   - With no connection open, between two connects or because nothing is
     listening, the error says "it may have exited", in the words 0.3.1 used.
     This is the one state in which the bridge cannot tell an app that is
     gone from one it has not reached yet.

   In 0.3.1 all three answers about an open connection said "it may have
   exited". Each of those apps is running, since something at the path
   accepted the connect, so that answer was wrong about every one of them.
3. On reconnect, inputs queued while disconnected are discarded, and the
   peer's `protocol_version` is forgotten so the next connection is not
   judged by the previous app's handshake. A peer that publishes before it
   greets is taken at the version its snapshot states until a `hello`
   replaces it, so a hand-written peer that skips the handshake is still
   refused input it cannot parse.
4. Lines it could not read are kept, one at a time, with the reason, until
   the next connect. A line over the cap is kept as a fixed reason that
   survives reconnects, until a snapshot parses or a connect finds nothing
   listening.
5. Its own `connect` is published too: whether a connection is open, and
   since when.

Those last two exist for the same reason. "No snapshot yet" is five states
wearing one name, and the bridge holds the facts that separate them:

| What is true | What `read_tree` says |
|---|---|
| A line ran past the 1 MiB cap and the bridge closed the connection over it | The socket path is right, so the tree is what is wrong: it is over the line limit, the app resends it on every reconnect, and only a smaller tree fixes it. Outranks every row below, since each reconnect greets the bridge and then overflows again. |
| A line arrived and could not be read, on the open connection or, with none open, the last one | The app found the socket, so the line is what is wrong; the reason is quoted, and the two things that produce it named (a tree past `MAX_NODE_DEPTH`, an app built against a taria whose snapshot shape differs from this bridge's). |
| The peer greeted this bridge and published nothing | The app is connected; what is missing is the tree. An app that binds the layer and never publishes nodes looks exactly like this. |
| A connection is open and nobody has said a word on it (past a 500 ms grace) | The path is not what is wrong. An adapter greets a bridge the moment it accepts one and serves one at a time, so this is the second bridge's view of an app held by another bridge. The answer says when a holder lets go, so the agent retries rather than asking for a process to be killed. The holder's window cannot be known, so it goes by this bridge's own, the best guess for a holder started from the same `.mcp.json`: after 60 s and "retry in a minute" on the default, after N s and "retry in N s" for `--idle-release N`, and for `0` that a holder started the same way keeps the app until its session ends. A holder older than 0.3.2 keeps the app until its session ends. On Linux, a stopped app leaves the same silence and is named as stopped instead, with its pid and how to resume it: the user's `fg` for a job suspended in its terminal, `kill -CONT` for any other stop. |
| Nothing is connected | Is the app running, and is the socket path correct? |

All of them used to be answered by asking whether the socket path was right,
which is the first wrong turn a retrofit takes and the first wall a new agent
hits: in every row but the last, the bridge reached something listening at
that path. The input tools ask for a tree before they send anything, so they
carry whichever of these applies rather than a second vocabulary for it.

On Windows the table has three more rows and one row means something else:
a pipe another account owns, so not the app's, a pipe that denied this
bridge access, an open refused as busy, and a silent connection that is the
app rather than another bridge. What the bridge says to each is under
Transports, What the bridge says on Windows. The rows above get the same
answers on Windows, except that each names the pipe and the pipe name where
this table says socket and socket path.

An ack whose `status` this build cannot name fails its whole line, id
included, so the app answering an input is indistinguishable at the bridge
from the app saying nothing; a rejected line inside the window is the only
trace left of the difference, and the "neither acknowledged nor changed"
answer says so rather than letting the agent read silence as an app that does
not ack.

Diagnosing the held socket is bridge-side only. The adapter could accept a
second connection and close it at once, turning the silence into a fast,
legible disconnect, but the listener thread is the thread that serves a
client, so accepting anything while a bridge is connected is the
one-client-at-a-time change itself, which is deferred. A bridge-side answer
also costs adapters nothing, works against adapters already shipped, and
covers the other ways this silence happens: a peer wedged before its
handshake, or a process at that path that does not speak taria at all.

### On-demand connect and idle release

Up to 0.3.1, `taria-mcp` connected at startup and held the app until its
process ended. A harness starts every server it has registered when a session
opens, and a project `.mcp.json` registers the bridge for every session in
the project. So whichever session opened first held the app, whether or not
it ever called a taria tool, and the session that wanted the app had to find
a bridge and kill it.

The binary now does two things:

- **It connects on demand.** Starting it, and the MCP `initialize` and
  `tools/list` that follow, or the `server/discover` of the stateless MCP
  lifecycle (the 2026-07-28 MCP revision, where each request carries its
  version and capabilities in `_meta` and there is no `initialize` handshake),
  open nothing. The first tool call opens the connection and waits for the
  app's `hello` and its tree, so that call still answers with the live tree.
  The wait is normally milliseconds, because the adapter's listener thread
  writes the `hello` and the latest snapshot the moment it accepts a bridge,
  so an app sitting idle answers as fast as a busy one. It runs longer only on
  a connection that stays silent, until that connection is 500 ms old, the age
  at which silence means another bridge holds the app, so the first call gives
  that answer. The 500 ms is counted from when the connection opened, so a
  connect that ran late still gets all of it, and the whole wait never runs
  past 1.2 s from the start of the call.
- **It lets go when idle.** After 60 s with no tool calls, the bridge closes
  its connection, or leaves the listen backlog if it was waiting there, and
  stops reconnecting. A bridge waiting in the backlog is then accepted at
  once. The next tool call connects again, the way the first one did.
  `--idle-release <SECS>` sets the delay, and `0` keeps a connection for good
  once it is made.

It does not let go while an input it sent is still waiting for the app's ack.
Closing then would end the connection that input belongs to, and the adapter
discards an input whose connection has ended rather than apply it (see One
connection owns its inputs), so the agent would never learn what became of
it. That wait is capped at 30 s past the idle window, so an app that never
acks cannot keep the bridge from letting go.

Why 60 s: it is longer than the pause an agent takes between two calls while
it works, and far shorter than the gap between one session using the app and
another. A session driving the app keeps it, and one that has moved on gives
it up within a minute.

Two other designs were rejected:

- **The adapter hands the app to the newest bridge.** An evicted bridge
  reconnects at once, because reconnecting is what a bridge does, so two
  bridges would take the app from each other about every 50 ms, the
  listener's poll. It would also reach only apps rebuilt against a new
  adapter, where a bridge-side fix reaches every app already shipped.
- **The bridge connects only for the length of a call.** Two active sessions
  would then alternate on every call. And a call can end before the app has
  taken its input, when the app is slow to answer, so closing right after the
  call throws that input away.

### One connection owns its inputs

An input belongs to the connection it arrived on. Each connection is tagged
with a generation, every queued input carries the generation it arrived on,
and the reader retires that generation as it exits, before the disconnect is
observable anywhere. An input from a retired generation is discarded at
dequeue rather than applied, and counted in `stale_inputs`.

Without this, a `key q` sent just before the bridge went away could quit the
app after its sender was gone, with nobody left to hear about it. The
discard is deliberately not acked: the peer that would read the ack is the
one that left. The bridge drops its own queue on reconnect for the
mirror-image reason, so both sides agree.

A waiting client's inputs start after its `hello`. A client that connects
while another is served waits in the listen backlog (the kernel's queue of
connections not yet accepted), and whatever it writes there sits in the
kernel's buffer until its turn. That client has seen no tree, so nothing it
wrote can be a reply to one. So for a client that was already waiting when the
previous session ended, the adapter reads off what is waiting just before the
`hello` goes out and throws it away, unacked, counting each complete `input`
line in `stale_inputs`. If that read stops partway through a line, the rest
of the line is skipped when it arrives. More than 1 MiB waiting breaks the
connection, like a line that long.

A client that waited behind nobody keeps what it wrote: the first connection
of a process, and one that connects to an idle app, such as a script that
connects and writes at once. The listener tells the two apart by whether an
accept has found the backlog empty since the last session ended, so a client
that connects the instant a session ends counts as one that waited. taria-mcp
is never affected: its input tools refuse to send before the app's first
snapshot.

The same ownership decides who may still be answered. `ack` refuses an id
whose connection has ended, because ids are unique only within a bridge
process: a fresh bridge counts from the start again, so an id an app held
across a disconnect can already name a live input of the next bridge's, and
that bridge's waiter must not receive a verdict from a session it never saw.
An id also stops being answerable once its own connection has delivered a
queue's worth of newer inputs past it, which bounds what the layer remembers
and costs at most a late ack the peer already has to survive.

Guard rails on the app side: every line has 5 s in all to be written, so a
peer that stops reading, or reads too slowly to take a whole line in that
time, is dropped; the acks flushed at shutdown get 250 ms in all; and a
bounded input queue (256 entries) drops the newest input, acks it `dropped`,
and counts it in `dropped_inputs` instead of blocking the socket thread.
The ack queue is bounded too, at 1024, and drops from the opposite end: the
oldest, because an ack answers one specific input and the answers an agent is
still waiting on are the newest, while the oldest name inputs whose waiter has
long since timed out. It has to be bounded because a bridge that reads
steadily but far slower than an agent sends never stalls long enough for the
write timeout to fire, and would otherwise grow the app's memory until the
process is killed. Drops are counted in `dropped_acks`, which is a report
about the bridge rather than the app: the input was applied and the caller
was answered nowhere. On the bridge side, an input waits at most 500 ms for
room in the queue to the app, so a stopped app fails a tool call instead of
hanging it.

Four counters in all record agent traffic that went nowhere:
`dropped_inputs`, `stale_inputs`, `unknown_inputs` and `dropped_acks`. They
are fields of the `LayerStats` that `layer.stats()` returns, all monotonic
across reconnects, all 0 on a disabled layer, and all read after the terminal
is restored, because the layer never prints. Two more, `truncated_snapshots`
and `oversized_snapshots`, lose no input: they count trees the agent was
handed cut (see Tree depth and Snapshot size). `LayerStats` is
`#[non_exhaustive]`, so a new count is not a breaking change.

## Transports

Since 0.4.0 the adapter and the bridge each keep everything that is the same
on every platform in one place, and put what is not behind a small seam, in
`transport/` in each crate. Linux and macOS share the Unix socket transport,
and Windows has a named pipe one (a pipe is a kernel object with a name under
`\\.\pipe\`, which two processes open like a file). The layer's threads, its
input and ack queues, generations and stamps, the dedupe and the bounding of
trees have one implementation, and so do the bridge's manager and every
answer it builds, which is why the bytes on the connection are the same on
every platform. `docs/protocol.md` section 14.8 is the normative statement of
the Windows rules; this section is how the reference peers keep them.

### The seam

The adapter needs four types and one function from a transport:

- `Listener`, bound with `Listener::bind` at the path or pipe name the app
  asked for. It accepts one bridge at a time, gives up once the layer's
  shutdown flag is set, says whether the bridge it hands over waited behind
  the previous session (which decides the discard of input written before
  the `hello`), and cleans up after itself when dropped.
- `Connection`, one per bridge, with a write timeout, split into a `Reader`
  for the reader thread and a `Writer` for the writer loop.
- `Reader`, a `Read`, which can also read just what is already waiting
  (`with_nonblocking_reads`), for that discard.
- `Writer`, whose `write_all_before(deadline)` writes a whole buffer by a
  deadline or fails, which is what bounds every line at `WRITE_TIMEOUT` and
  the acks flushed at shutdown at `SHUTDOWN_FLUSH`, and whose `shutdown`
  wakes a `Reader` blocked on the same connection.
- `user_identity`, which namespaces the default endpoint between users: the
  uid on a unix, the SID (security identifier, the string that names an
  account, `S-1-5-21-...`) on Windows.

Every transport keeps three promises for `Writer::shutdown`. What was written
stays written: a byte a write finished putting on the connection is still
readable by the bridge after the shutdown, and after the listener closes,
until the bridge has read it, and nothing in the shutdown waits for that
read, which could wait forever. It wakes the reader: a `Reader` blocked on the
connection returns end of stream at once. It stays shut: every read started
after it returns end of stream at once, so a reader that was between two
reads cannot start one that never ends. A Unix socket has all three from the
kernel, since `shutdown(Both)` is sticky and leaves sent data readable.
Windows has them from a closed event per session and from never disconnecting
a pipe at shutdown, only closing it.

The bridge serves whatever its transport's `connect` returns as a plain
`AsyncRead + AsyncWrite` stream, split with `tokio::io::split`, and needs
only this much more:

| Item | Unix | Windows |
|---|---|---|
| `connect` | `UnixStream::connect` | tokio's named-pipe client, a busy open retried for 250 ms, then the owner check |
| `peer_pid` | `SO_PEERCRED` (`LOCAL_PEERPID` on macOS) | `GetNamedPipeServerProcessId` |
| `nothing_listens` | `ConnectionRefused` or `NotFound` | `NotFound` |
| `refusal` | never | `Busy` (`ERROR_PIPE_BUSY`) or `Foreign` (another account's pipe, or an open refused with `ERROR_ACCESS_DENIED`) |
| `BACKLOG` | true: a connect opens before the app accepts it | false: an open succeeds only into a free instance |
| `check_endpoint` | any path | a local pipe name, by `check_pipe_name` |
| `ENDPOINT`, `ENDPOINT_ADDRESS` | "socket", "socket path" | "pipe", "pipe name" |

`BACKLOG` is what decides what a connection that stays silent means: a
bridge waiting behind another where it is true, the app itself where it is
false. `ENDPOINT` and `ENDPOINT_ADDRESS` are the words the answers name the
app's endpoint and what the bridge was given to find it by, so one answer
reads right on each platform and the Unix bytes do not move.
`ensure_supported` is the last item, which the server builders call first
so that a platform with no transport says so; every platform taria builds
for has one now.

When a connection ends the bridge drops the whole stream, which closes both
directions. `tokio::io::split` keeps one stream under both halves, so dropping
only the write half closes nothing. A future change that drops only the
bridge's write half, to tell the app it is done writing while it goes on
reading, needs an explicit `shutdown().await` on that half. On Windows even
that is only a flush, since tokio's named pipe has no half-close; the app
sees end of stream once both halves are gone.

### Windows: one named pipe

The adapter creates `\\.\pipe\taria-<SID>-<label>`, or the pipe
`$TARIA_SOCK` names when that is a local pipe name and nothing else. It is
the first instance of its name (`FILE_FLAG_FIRST_PIPE_INSTANCE`), the only
one allowed, refuses clients on other machines
(`PIPE_REJECT_REMOTE_CLIENTS`), is a byte stream both ways, and is owned by
the user with a DACL (discretionary access control list, the part of a
security descriptor that says who may open the object) that grants that user
read and write and nobody anything, protected from inherited entries. The SID
in the name only keeps one user's pipes apart from another's in the
machine-wide pipe namespace; the safety comes from those flags, that DACL
and the bridge's owner check, together.

So the socket file's rules have Windows counterparts rather than copies.
There is no directory to vet: the descriptor decides who can open the pipe.
There is no stale socket to unlink and no liveness probe: a second app on a
live name fails to create the first instance, which the adapter reports as
`AddrInUse`, as on Unix, decided by the system and not by a probe. And there
is no file to remove at exit. A pipe, and its name, lives while any handle
to it is open, so an app restarted while a bridge still holds its old end
finds the name taken until that bridge reads end of stream and closes,
which taria-mcp does at once. Refused then, the new app would run with taria
off for the whole of its run, so the bind retries a create that Windows
refuses as access denied (the first-instance flag's answer while any
instance exists) or busy, every 20 ms for 500 ms (`BIND_RETRY`), and only
then reports `AddrInUse`. A second app on a name still served is refused
all the same, half a second later, and startup stays bounded.

The one instance is reused from session to session. The next accept retires
the last session: it sets the session's closed event, waits until nothing of
the session is left (the layer has joined its reader and writer by then), and
only then disconnects the client (`DisconnectNamedPipe`) and listens again
(`ConnectNamedPipe`). A disconnect throws away what the client has not read,
and between sessions is the only place one happens. At shutdown the handle is
closed instead, which leaves what the writer wrote, the acks flushed at
shutdown included, readable by the bridge until it has read it.

Names reach Windows only through the wide-character (`W`) calls, as UTF-16.
An ANSI call would pass the name through the code page first, whose best-fit
mapping can turn a character `check_pipe_name` passed into a `\`. Windows
trims trailing dots and spaces as it opens a name, so `\\.\pipe\taria.` and
`\\.\pipe\taria` are one pipe; both peers hand the name to Windows, which
normalises it the same way for each.

The bridge opens the pipe with tokio's named-pipe client, at tokio's default
`SECURITY_IDENTIFICATION` level, so what serves the pipe can learn who the
bridge is and cannot act as it. An open refused as busy is retried every
10 ms for 250 ms (`BUSY_RETRY`) before it stands: between two sessions the
one instance is busy for a moment, while the adapter retires the last
session (a 50 ms accept poll and the session's threads ending), disconnects
it and listens again, and a bridge refused at once in that moment was told
a free app was held. Before it writes anything or parses anything the pipe
sent, the bridge reads the pipe's owner (`GetSecurityInfo`) and compares it
with its own token's user SID. Registering the pipe with tokio starts a read
into tokio's own buffer at once, so bytes may have been read by then, but
nothing takes them out of that buffer before the check passes, and a pipe
that fails it is dropped with them. Pipe names are machine-wide and any
process can create one under the app's name first, so a pipe another
account owns is closed with nothing parsed or written, refused as
`Foreign`, and logged as a warning, when the refusal is new, that names the
owner. An open refused as access denied is never opened, and is refused as
`Denied`, not `Foreign`: the adapter's DACL lets its own user in, but a
token of that user is still refused in a sandbox (a restricted token or an
AppContainer) or at low integrity, and so is another account given the name
whole, so access denied does not say the pipe is not the app's. With
`--app` the binary derives the name from the same token SID, read by
`transport/windows_sid.rs`, which the library compiles as part of its
Windows transport and the binary compiles again through `#[path]`, so the
SID is not a public item of the library.

### Overlapped I/O

The reader thread and the writer loop use the one pipe handle at the same
time. A handle opened for synchronous I/O lets one call through at a time,
so a read waiting for the bridge would hold every write up behind it. The
pipe is opened for overlapped I/O instead, Windows' asynchronous I/O: a call
starts an operation and returns, and the kernel finishes it later, writing
the result into an `OVERLAPPED` structure the caller provided and signalling
an event. That memory belongs to the kernel until the operation finishes,
which is the whole of the design's risk, and these invariants contain it:

- **Every operation runs through an `Op`**: the `OVERLAPPED`, its event and
  its buffer, boxed, so the addresses the kernel holds stay put however the
  `Op` moves.
- **An operation never outlives the call that started it.** Each is started
  and finished inside one call on the `Op`'s owner, so none is in flight once
  that call returns.
- **Finished means the kernel said so.** A wait that gives up, on a deadline
  or a shutdown, cancels the operation (`CancelIoEx`) and then waits for it
  to finish, and only then is its memory reused or freed. An `Op` dropped
  with an operation still in flight, which only a panic can cause, does the
  same, and leaks its memory rather than free it if even that fails.
- **The handle is closed once, and never under a live operation.** The
  listener and the session share it through an `Arc`, and every operation
  runs inside a call on an owner that holds that `Arc`. The one operation
  that can still be in flight at the close is the leaked one above: an `Op`
  dropped with an operation it could not see finish keeps its memory and
  lets go of its `Arc`, so closing the handle cancels that operation, and
  what the kernel writes as it ends lands in memory never freed or reused.
- **A shutdown stays shut.** Each session has a closed event, set once by
  `Writer::shutdown` and never reset. Every read and write checks it before
  it starts and waits on it beside its own event. Cancelling the read in
  flight alone would not do: a cancel reaches only the operation in flight
  when it is made, and a reader between reads would start its next one after
  it and wait forever.
- **A write is bounded whole.** A line is one `WriteFile`, waited on for what
  is left of its deadline and on the closed event beside it. At the deadline
  it is cancelled and waited for, so its buffer is never freed under the
  kernel, and it fails `TimedOut` unless it finished in full first.
  `FlushFileBuffers`, which waits for the bridge to read and can wait
  forever, is never called.

The listener's `ConnectNamedPipe` is overlapped too, waited on in 50 ms
slices with the shutdown flag read between them, the counterpart of the Unix
listener's poll, so a layer dropped with no bridge connected waits at most
one slice.

### Why `windows-sys` and not `interprocess`

Both crates take `windows-sys` on Windows only: raw bindings to the Win32
API, with no code of their own to run, which costs nothing on Linux or macOS.
The bridge's stream is tokio's named-pipe client, from a dependency it
already had. The adapter's pipe is about 1,300 lines written against those
bindings, tests aside, where a crate could have done it in a few. The
obvious crate, `interprocess`, was turned down for three things it does that
break a promise taria makes:

- **Its pipe streams have no write timeout.** Every line the adapter writes
  has 5 s, and the acks flushed at shutdown 250 ms, so that a bridge that
  stops reading is dropped and an app's exit stays bounded. A write that can
  wait forever on a bridge that never reads breaks both.
- **Its listener keeps a hidden backlog.** Each accept creates the next
  instance ahead of time, so a second bridge connects into that spare and
  sits there silent: the Unix backlog back again, with the ambiguity a
  one-instance pipe removes. With one instance, a held app refuses the second
  bridge as busy, which is better evidence than silence, and a silent
  connection can only be the app.
- **It hands dropped streams to background "limbo" threads**, which flush
  the pipe, that is wait for the peer to read, before they close it. Those
  are threads the layer neither started nor can join, waiting on a bridge
  that may never read, and holding a handle that keeps the pipe's name taken,
  so a restarted app cannot create it.

The hand-rolled transport is tested where it can go wrong: a second bind
refused as in use once the retry runs out, a bind that waits for a name in
use to come free, a bridge still holding the last instance included, the
owner and the DACL read back, a second client told the pipe is busy, the
write deadline against a bridge that never reads, a shutdown waking a
blocked reader and every later read, the instance reused session after
session, and what was written before shutdown surviving the pipe's close.
On the bridge's side, a busy open between two sessions is waited out, a
pipe that refuses this user is read as denied, and a bridge that let go
after meeting a foreign pipe reads the app held by another bridge afresh.
`crates/taria-mcp/tests/adapter_e2e.rs` then drives a real `TariaLayer` in
a child process from the real `taria-mcp` binary on every platform, Windows
included.

### What the bridge says on Windows

Four answers exist only on Windows, because only the Windows transport
refuses a connect, and only there does a silent connection mean the app.
The answers Windows shares with Linux and macOS name the pipe and the pipe
name where those name the socket and the socket path, or the path in the
answer to a connection inside the grace (`ENDPOINT`, `ENDPOINT_ADDRESS`,
`GRACE_ADDRESS`): the answer to nothing connected, for one, asks whether
the pipe name is correct. Linux and macOS answers are unchanged, byte for
byte.

- **Busy.** The open was refused with `ERROR_PIPE_BUSY` for the whole of
  the 250 ms retry: another bridge has the one instance. The answer is the
  held answer, with the pipe's refusal as its evidence in place of a silent
  connection's age, and the same retry advice by this bridge's
  `--idle-release`. It ends by allowing that the app may instead be
  suspended or stuck: an app stopped after a session cannot retire it and
  listen again, so its one instance stays busy. A bridge that let go of the
  app itself gets the held-after-release answer, as on Unix. A busy refusal
  forgets the last connection's rejected lines, oversized tree aside, as a
  connect does.
- **Silent pipe.** A connection opened and has said nothing past the 500 ms
  grace. On Windows that is the app, suspended (a debugger, a process tool)
  or stuck before its `hello`, or something that is not a taria adapter, and
  the answer says so and that it is not another bridge. It replaces the held
  answer that the same silence gets on Unix.
- **Foreign.** The pipe at the name belongs to another account. The answer
  says it is not the app, that the bridge parsed nothing it sent and wrote
  nothing to it, and that the user should find what serves it; the log
  names the owner.
- **Denied.** The pipe refused this bridge access at the open. That is not
  proof of an impostor: the app's own pipe denies a bridge in a sandbox, at
  low integrity, or running as another account. The answer names both
  readings, says the bridge parsed nothing and wrote nothing, and asks the
  user to check that the bridge runs as the app's user, outside any sandbox
  and at the app's integrity level, and failing that to find what serves
  the pipe.

  Both outrank every other answer, an oversized tree included, since the
  pipe is not one whose tree or lines those would be about, and both clear
  the bridge's record of having let go, as finding nothing listening does.
  Each stands until a connect passes the owner check, finds nothing
  listening, or the bridge releases: a pipe that takes one open, fails the
  check and then stays busy is still not the app, not the app held. A
  release ends it because a released bridge stops trying, and meanwhile the
  impostor can exit and the app come back, held by another session; kept,
  the answer sent the agent after an impostor that had gone. So the manager
  puts the link down when it lets go (`forget_refusal`), and the next busy
  refusal gets the held answer.

Stopped-app detection reads `/proc`, so it stays Linux only. On Windows an
app suspended while its pipe listens gets the silent-pipe answer, and one
suspended after a session, which cannot listen again, gets the busy answer,
which allows for it; the no-ack answer never names a stopped app.
`docs/cli.md` quotes the four texts.

## Focus contract

Every snapshot should carry exactly one focused node, and one half of that is
the app's obligation rather than something the implementation enforces.
`docs/protocol.md` section 15 is the normative statement.

- The adapter guarantees at least one: the auto-generated `app` root is
  focused only when no recorded node (or descendant) is.
- At most one is the app's to keep. Nothing checks it. The bridge never reads
  `focused` beyond passing it to the agent, so an app that publishes two
  focused nodes produces a tree that parses and misleads. The demo unit-tests
  the invariant in every state: list, input, modal dialog, and empty list
  (focus parks on the list node itself), which is the pattern to copy.

Focus tells the agent where a raw key would land, which is what makes the
`key` fallback usable. It is also how an agent aims `type_text`, but only
because an app that is accepting typing normally focuses the surface taking
it. What decides where typed characters go is the app, not the protocol.

Focus is not selection. Focus says where a key would go; a cursor sits on a
row whether or not that list owns the keyboard. An app that publishes only
focus makes a moved cursor invisible, and the bridge then truthfully reports
"the tree did not change" for an input the app handled. Publish the
selection as its own readable fact, in `selected`: the demo marks the cursor
row `selected` and every other row `selected: false`, and the active tab the
same way. Before 0.4.0 that fact had nowhere to go but a `value`, and the
demo still also names the selected row's id in the list node's `value`, and
the active tab in the tab bar's, so a reader built earlier and the
benchmark's recorded runs read it where they always did.

## Mapping to AccessKit

taria's vocabulary is close to AccessKit's on purpose, so an exporter from a
taria tree to AccessKit 0.25, and through it to AT-SPI, UIA and
NSAccessibility, is mostly a table rather than a design. None ships yet. Most
fields, actions and roles have a direct counterpart. A few have only a best
fit, marked so below, a judgment rather than a certainty, and `select`,
`dismiss` and `scroll` have no single counterpart at all:

| taria | AccessKit 0.25 |
| --- | --- |
| `label`, `value`, `description` | `set_label`, `set_value`, `set_description` |
| `focused: true` | the `TreeUpdate`'s `focus`, not a node property |
| `selected` | `set_selected(bool)` |
| `checked` | `set_toggled(Toggled::True)`, `Toggled::False`, `Toggled::Mixed` |
| `expanded` | `set_expanded(bool)` |
| `disabled: true` | `set_disabled()` |
| `value_now`, `value_min`, `value_max` | `set_numeric_value`, `set_min_numeric_value`, `set_max_numeric_value` |
| `pos_in_set` | `set_position_in_set(pos_in_set - 1)` |
| `set_size` | `set_size_of_set` |
| `row`, `column` | `set_row_index(row - 1)`, `set_column_index(column - 1)` |
| `row_count`, `column_count` | `set_row_count`, `set_column_count` |
| `relayed` | nothing: AccessKit has no mark for text from outside the app |
| `activate`, `focus`, `set_value`, custom | `Action::Click`, `Action::Focus`, `Action::SetValue`, `set_custom_actions` |
| `toggle` | on a checkbox or a switch, `Action::Click` as a best fit: AccessKit has no toggle action, and a checkbox toggles on a click; on a disclosure, `Action::Expand` or `Action::Collapse`, chosen by its `expanded` |
| `select` | none: AccessKit has no select action; `Action::Click` on the item is a best fit |
| `dismiss` | none: AccessKit has no dismiss action |
| `scroll` | none single: AccessKit's scrolling is directional (`ScrollUp`, `ScrollDown`, `ScrollLeft`, `ScrollRight`) or aimed (`ScrollIntoView`, `ScrollToPoint`, `SetScrollOffset`), while taria's `scroll` names neither |
| roles | by name (`list_item` is `Role::ListItem`, `scrollbar` `Role::ScrollBar`), except `app` `Role::Application`, `tabs` `Role::TabList`, `select` `Role::ComboBox`, `option` `Role::ListBoxOption`, `progress_bar` `Role::ProgressIndicator`, `other` `Role::Unknown` |
| `text`, `chart`, `list` (best fits) | `Role::Label`, `Role::Figure`, and `Role::List`, or `Role::ListBox` when its items can be selected |

AccessKit counts positions, rows and columns from 0, and taria from 1, as a
person counts rows and as ARIA's `aria-posinset` and `aria-rowindex` do, so
the exporter subtracts one. A count (`set_size`, `row_count`,
`column_count`) needs no shift.

## Deferred

- Rect geometry on nodes (screen coordinates, for correlating the tree with
  rendered output).
- Nesting inference: `sem`-wrapped widgets record as a flat list today;
  hierarchy comes only from explicitly built children.
- Multiple simultaneous bridge clients per app. Taking turns works without
  it: an idle taria-mcp lets the app go (see On-demand connect and idle
  release).
- Adapters for other frameworks (Bubble Tea, Textual, Ink).

Three more, found migrating a released typing tutor (keybr-tui) to v0.1. None
landed in 0.2. All are additive, so they can land in a later 0.x without a
`PROTOCOL_VERSION` bump.

- A partially applied input. An agent sends 100 characters, the typing
  surface ends its lesson after 20, and the other 80 go nowhere. The app can
  ack `Delivered`, which is true and useless, or `Ignored`, which is false.
  A status carrying a count closes it. `InputStatus` is `#[non_exhaustive]`,
  and a bridge that cannot read the new status skips that ack and keeps the
  `Delivered` before it, which is exactly today's answer.
- A role for a bounded numeric stepper. keybr renders three (target WPM,
  fragment length, alphabet size); `select` is a stretch, since there are no
  options to pick from, and `list_item` says nothing about a number with
  bounds. Most config screens have some. A peer that does not know the role
  reads `other`. The bounds themselves have been node fields since 0.4.0,
  `value_now`, `value_min` and `value_max`, so only the role is missing.
- A bad `IdSpace` prefix fails at a lookup that may never run. `new` is const
  and infallible so a space can stand in a `const`, so a prefix carrying the
  separator builds a space that owns nothing, including the ids it builds
  itself. keybr's natural spelling, `IdSpace::new("progress-key")`, compiled,
  sat in a const, and would have answered `None` to every parse. `try_new`
  exists and is const, but nothing steers an adopter to it. A constructor that
  fails const evaluation, such as a macro wrapping `try_new` in an inline
  `const` block, fails the build instead of the app, and never panics at run
  time.
