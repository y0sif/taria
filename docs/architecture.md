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
| Ratatui adapter | `crates/taria-ratatui` | `TariaLayer` serves the app's Unix socket, publishes snapshots, feeds agent input into the event loop, and acknowledges every input. `FrameRecorder` and `sem` record nodes per rendered frame. |
| MCP bridge | `crates/taria-mcp` | Stdio MCP server (rmcp) exposing `read_tree`, `act`, `key`, and `type_text`. A socket-manager task owns the connection to the app and republishes its acks. |
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

- Transport: Unix domain socket, newline-delimited JSON (ndjson). One JSON
  object per line.
- Up (app to bridge): one `hello` per connection, carrying `app_label` and
  `protocol_version`, then `snapshot` and `ack` messages.
- Down (bridge to app): `input` messages carrying an `id` and an
  `AgentInput`: a semantic act, a raw key, or literal text.
- Framing: both message enums are internally tagged, so every line carries a
  `type` naming its variant. That key frames the message on the transport and
  says nothing about the tree, so the bridge drops it from the snapshot it
  hands an agent. `read_tree` returns the snapshot, not the line carrying it.
- Ids: the bridge stamps each input with an `InputId` and must not reuse one
  for the lifetime of its process. An ack can outlive the connection it was
  sent on, so ids that never repeat are what stop a dead app's ack from
  answering a live input. A process-wide counter satisfies this.
- Line cap: both sides refuse incoming lines over 1 MiB and treat the
  connection as broken rather than buffering without bound.
- `seq`: increments per published snapshot within one app run. It resets
  when the app restarts, so the bridge detects change by comparing whole
  snapshots, not by `seq` ordering. `seq` signals staleness only within one
  connection.
- Dedup: a frame whose tree is identical to the previous publish is skipped
  entirely; `seq` does not move.
- `focused`: serialized on every node and accepted when absent, where absence
  reads as `false`. Nothing on the wire moves today; what the pairing buys is
  that omitting the field for the nodes that are not focused stays an additive
  change later, and a 500-node tree spends around 8 KB per publish on
  `"focused":false` against an agent's output budget. Relaxing what a peer
  accepts is only free while no peer yet relies on it, which is why it lands
  under the freeze rather than when it is needed.

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
the process from inside a library thread.

`MAX_NODE_DEPTH` is 32, a little under half the measured ceiling, and the
slack pays for the transport envelope, for a root an adapter adds above the
nodes an app hands it, and for a peer whose parser is stricter than
`serde_json`. The core crate states the limit and offers `Node::check_depth`,
which reports the depth measured and names a node found at it; it enforces
nothing, because what to do about a deep tree belongs to the adapter.

The ratatui adapter cuts. Over the limit, every node at the limit publishes
without its children, per branch, and the app is told through
`truncated_snapshots()` and `last_truncation()`. Cutting, because the other
two answers are the same failure: publishing the tree as built puts a line on
the wire the bridge cannot parse, so it skips it and goes on serving the last
tree it read, and skipping the publish is that same stale tree chosen
deliberately. Only the cut still delivers the part of the tree that is fine.
The app is the peer told about it because the app is the only one that can fix
it, and the fix is nearly always to publish what the widget draws rather than
the data behind it: for a deep tree view, the expanded path and the rows on
screen.

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
agent told never to guess an action name meets that difference on its first
custom action, so the tool description and the server's instructions both
state it rather than leaving it to be inferred.

### Version 1 is frozen

`PROTOCOL_VERSION` is 1 and the format is fixed. Within version 1, changes
must be additive. Two properties make that safe: a reader skips a line it
cannot parse instead of dropping the connection, and serde ignores unknown
fields. So a new optional field, or a whole new message variant, reaches an
older peer as something it quietly ignores.

Neither property covers a value nested inside a message the peer does want,
so the open vocabularies carry their own fallbacks. An unknown `Role`
reads as `Role::Other`. An unknown `Action` name reads as `Action::Custom`
keeping that name, so an agent can still advertise it, echo it back, and
have the app recognize it. Without those, one leaf node using a role added
later makes the whole snapshot unparseable, the reader skips every line, and
the app looks frozen with no error anywhere.

An `AgentInput` whose `kind` this build does not know is the third, and it
degrades for a sharper reason than the other two. The `InputId` sits on the
message rather than inside the input, so a `kind` that failed the line would
take the id with it, and an agent would wait out the bridge's window for an
ack that was never possible. `AgentInput::Unknown` is a `#[serde(other)]`
fallback carrying nothing at all, which is enough: the input arrives with its
id intact, the adapter answers it `ignored` on the app's behalf without
queueing it, and the agent learns in one round trip that this app can do
nothing with what it asked for. An app never sees the variant.

`InputStatus` has no fallback. A status this build does not know fails its
ack and the reader skips that line, which leaves the input reading as
unacknowledged. That is the safe reading, and the loss is one ack rather
than a tree.

Degrading keeps the connection; relaying keeps the information. The bridge
stores each snapshot as the app's own line beside the parse of it, and it is
the line an agent reads. So a role, an action or a field this build has never
heard of reaches the agent by name instead of flattened into whatever the
typed struct could hold: an app publishing `"role":"sparkline"` had the agent
read `"role":"other"`, with the real name on the wire the whole time. The
parse stays authoritative for what the bridge decides, because it is typed:
`act` validates node ids and advertised actions against it, and change
detection compares parses rather than text. A line is relayed only after it
parsed, so a malformed one is still skipped.

Additive on the wire is not automatically additive in Rust. A peer skips a
message variant it cannot parse, but a peer rebuilt against the version that
added one meets it in an exhaustive `match` and stops compiling, and the peers
this format is written for are exactly the ones that match on these types: an
adapter dispatching `BridgeToApp`, a bridge dispatching `AppToBridge`. So every
type a version-1 addition can reach is `#[non_exhaustive]`. Eleven of them:
`AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`, `Action`, `Role`,
`Node`, `Snapshot`, `key::Key`, `key::Modifiers` and `key::KeyPress`. Marking a
type is itself a breaking change, which is why it was done before the first
release rather than at the first addition. Each costs an outside peer one
wildcard arm, or one `..` in a pattern, and buys back that a new variant,
field, role, action or key is a recompile rather than a repair. `Modifiers`
gains `NONE` and a const `new` in exchange for the struct literal it closes,
and `KeyPress` already had the const `new` its own marking needs.

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
| `ignored` | The app looked at the input and deliberately did nothing. | The app, with `layer.ack(id, InputStatus::Ignored)`. |

`delivered` says only that the input was dequeued, before the app knows what
it will do with it. An app that then does nothing (an act a modal dialog
blocks, an unknown node id, a `set_value` with no value) refines that to
`ignored`.

Acks share the connection's writer with snapshots and are flushed first in
every pass, so an ack always reaches the bridge before the snapshot
published after it. An agent that saw the snapshot first could not tell
whether it reflects its own input yet.

### What the bridge does with each outcome

An input tool sends, then watches acks and snapshots for up to 500 ms. Both
signals are needed and neither is sufficient: an ack says the app saw the
input but not what it did, a new tree says something happened but not that
this input caused it. The window is not cut short by a tree arriving without
an ack, because the ack still owed can be a `dropped`, for an input that
never reached the app while an unrelated redraw did, and answering that with
a tree would report a dropped input as applied. A `delivered` does not cut it
short either: the app can still refine it to `ignored`. What a `delivered`
never becomes is a `dropped`, which says the input never reached the app at
all.

| Outcome | Tool answer |
|---|---|
| Nothing could be handed over: the bridge's queue to the app stayed full for 500 ms | Error: this input was not sent; the app is stopped or not reading its socket. |
| A burst the bridge could not finish sending | Error: how many went out and may already have taken effect, how many did not, and that the effect is partial. |
| Acks lost (the bridge fell behind its own ack channel) | Error: what became of the inputs cannot be reported in full; call `read_tree`, send fewer inputs per call. |
| Any input of a multi-input burst `dropped` | Error: how many landed, and that the effect is partial. |
| The app acknowledged the input and then went away | Text: the app acknowledged it and disconnected, possibly because of it; no tree, because the app is gone. Not an error: the input's fate is known, and an advertised `quit` reaches this on purpose. |
| The app went away without acknowledging the input | Error: it disconnected before acknowledging this input, so whether the input was applied is not known. |
| `dropped` | Error: nothing was applied; send fewer inputs. |
| `ignored` | Text saying the app deliberately did nothing, plus the current tree to re-plan from. |
| `delivered`, tree changed | The new tree. |
| `delivered`, tree unchanged | Text: received, no change within 500 ms. |
| No ack, tree changed | The new tree. |
| No ack, no change | Text: neither acknowledged nor changed; it may be an adapter that sends no acks. If a line was rejected inside the window, that line is quoted instead, because it may have been this input's ack. |

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
  otherwise be delivered to the next app instance.

## Socket lifecycle

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
   new publish. The listener socket is non-blocking and the thread polls a
   shutdown flag every 50 ms rather than parking in `accept()`: waking a
   parked `accept()` by connecting to the socket path fails exactly when it
   matters, because by then the path may hold a socket another process bound,
   whose listener takes the wake-up while this one stays parked forever.
   Dropping the layer shuts the threads down and removes the socket file, but
   only while that file is still the one this layer bound, compared by device
   and inode: a second instance that took the path over is serving a socket of
   its own there, and unlinking it would leave that instance unreachable.

`bind_or_disabled` turns any of those failures into an inert layer instead
of an error, so taria can never stop an app from starting. Every method
stays callable on a disabled layer, and the app reads `bind_error()` when it
chooses. A refused label is the one failure with no path behind it, so that
layer reports an empty `socket_path()`: the error names the label, which is
the thing that has to change, and an invented path would only look like
somewhere the socket might be. The layer prints nothing itself: a print once
the app owns the alternate screen garbles the display.

Bridge side (`taria-mcp`):

1. Connect, retrying forever: backoff starts at 250 ms, doubles, and caps at
   2 s. A connection that dies young without delivering a snapshot keeps the
   backoff growing instead of resetting it.
2. On disconnect, the watch flips to `Disconnected`, keeping the app label
   and the last `seq`, so tool calls fail fast with an error that says which
   app went away instead of acting on a stale tree.
3. On reconnect, inputs queued while disconnected are discarded, and the
   peer's `protocol_version` is forgotten so the next connection is not
   judged by the previous app's handshake.
4. Lines it could not read are kept, one at a time, with the reason.
5. Its own `connect` is published too: whether a connection is open, and
   since when.

Those last two exist for the same reason. "No snapshot yet" is four states
wearing one name, and the bridge holds the facts that separate them:

| What is true | What `read_tree` says |
|---|---|
| A line arrived and could not be read | The app found the socket, so the line is what is wrong; the reason is quoted, and the two things that produce it named (a tree past `MAX_NODE_DEPTH`, an app built against a taria whose snapshot shape differs from this bridge's). |
| The peer greeted this bridge and published nothing | The app is connected; what is missing is the tree. An app that binds the layer and never publishes nodes looks exactly like this. |
| A connection is open and nobody has said a word on it (past a 500 ms grace) | The path is not what is wrong. An adapter greets a bridge the moment it accepts one and serves one at a time, so this is the second bridge's view of a socket another one is holding, usually a `taria-mcp` left running by an earlier session. |
| Nothing is connected | Is the app running, and is the socket path correct? |

All four used to be answered by asking whether the socket path was right,
which is the first wrong turn a retrofit takes and the first wall a new agent
hits: in both of the middle cases the bridge is connected to that path. The
input tools ask for a tree before they send anything, so they carry whichever
of these applies rather than a second vocabulary for it.

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

### One connection owns its inputs

An input belongs to the connection it arrived on. Each connection is tagged
with a generation, every queued input carries the generation it arrived on,
and the reader retires that generation as it exits, before the disconnect is
observable anywhere. An input from a retired generation is discarded at
dequeue rather than applied, and counted in `stale_inputs()`.

Without this, a `key q` sent just before the bridge went away could quit the
app after its sender was gone, with nobody left to hear about it. The
discard is deliberately not acked: the peer that would read the ack is the
one that left. The bridge drops its own queue on reconnect for the
mirror-image reason, so both sides agree.

The same ownership decides who may still be answered. `ack` refuses an id
whose connection has ended, because ids are unique only within a bridge
process: a fresh bridge counts from the start again, so an id an app held
across a disconnect can already name a live input of the next bridge's, and
that bridge's waiter must not receive a verdict from a session it never saw.
An id also stops being answerable once its own connection has delivered a
queue's worth of newer inputs past it, which bounds what the layer remembers
and costs at most a late ack the peer already has to survive.

Guard rails on the app side: a 5 s write timeout drops a peer that stops
reading, and a bounded input queue (256 entries) drops the newest input,
acks it `dropped`, and counts it in `dropped_inputs()` instead of blocking
the socket thread. The ack queue is bounded too, at 1024, and drops from the
opposite end: the oldest, because an ack answers one specific input and the
answers an agent is still waiting on are the newest, while the oldest name
inputs whose waiter has long since timed out. It has to be bounded because a
bridge that reads steadily but far slower than an agent sends never stalls
long enough for the write timeout to fire, and would otherwise grow the app's
memory until the process is killed. Drops are counted in `dropped_acks()`,
which is a report about the bridge rather than the app: the input was applied
and the caller was answered nowhere. On the bridge side, an input waits at
most 500 ms for room in the queue to the app, so a stopped app fails a tool
call instead of hanging it.

Four counters in all record agent traffic that went nowhere:
`dropped_inputs()`, `stale_inputs()`, `unknown_inputs()` and
`dropped_acks()`. All are monotonic across reconnects, all are 0 on a
disabled layer, and all are read after the terminal is restored, because the
layer never prints.

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
selection as its own readable fact; the demo puts the selected item's node
id in the list node's `value`.

## Deferred

- Rect geometry on nodes (screen coordinates, for correlating the tree with
  rendered output).
- Nesting inference: `sem`-wrapped widgets record as a flat list today;
  hierarchy comes only from explicitly built children.
- Multiple simultaneous bridge clients per app.
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
  reads `other`. The bounds themselves belong with node attributes, which are
  deferred on their own.
- A bad `IdSpace` prefix fails at a lookup that may never run. `new` is const
  and infallible so a space can stand in a `const`, so a prefix carrying the
  separator builds a space that owns nothing, including the ids it builds
  itself. keybr's natural spelling, `IdSpace::new("progress-key")`, compiled,
  sat in a const, and would have answered `None` to every parse. `try_new`
  exists and is const, but nothing steers an adopter to it. A constructor that
  fails const evaluation, such as a macro wrapping `try_new` in an inline
  `const` block, fails the build instead of the app, and never panics at run
  time.
