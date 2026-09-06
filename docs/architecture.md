# Architecture

How the pieces fit together. For why taria exists, see `landscape.md`. For
adding taria to an existing ratatui app, see `integration-guide.md`.

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

### Version 1 is frozen

`PROTOCOL_VERSION` is 1 and the format is fixed. Within version 1, changes
must be additive. Two properties make that safe: a reader skips a line it
cannot parse instead of dropping the connection, and serde ignores unknown
fields. So a new optional field, or a whole new message variant, reaches an
older peer as something it quietly ignores.

Neither property covers a value nested inside a message the peer does want,
so the two open vocabularies carry their own fallbacks. An unknown `Role`
reads as `Role::Other`. An unknown `Action` name reads as `Action::Custom`
keeping that name, so an agent can still advertise it, echo it back, and
have the app recognize it. Without those, one leaf node using a role added
later makes the whole snapshot unparseable, the reader skips every line, and
the app looks frozen with no error anywhere.

`InputStatus` has no fallback. A status this build does not know fails its
ack and the reader skips that line, which leaves the input reading as
unacknowledged. That is the safe reading, and the loss is one ack rather
than a tree.

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
| The app went away and did not come back | Error: it received the input and then disconnected, or disconnected before acknowledging it. |
| `dropped` | Error: nothing was applied; send fewer inputs. |
| `ignored` | Text saying the app deliberately did nothing, plus the current tree to re-plan from. |
| `delivered`, tree changed | The new tree. |
| `delivered`, tree unchanged | Text: received, no change within 500 ms. |
| No ack, tree changed | The new tree. |
| No ack, no change | Text: neither acknowledged nor changed; it may be an adapter that sends no acks. |

The order matters. The two send failures come first: an input that never left
the bridge has no ack to wait for, and a burst cut short still waits out the
window, because its earlier copies can be dropped too, but answers with the
partial-send error whatever else it sees. "Some of this landed and some was
never sent" is the only true report of it. Below those, a drop outranks a
departure, because "never applied" stays true whether or not the app is still
there. A departure outranks a tree, because a tree would describe a UI that
no longer exists. Lost acks outrank the rest, because any of them could have
been a `dropped` this call was watching for.

An adapter that sends no acks at all still works, which is what the last two
rows are for.

### Tool-side validation

Rejecting bad input at the bridge costs one error instead of one round trip
and a silence.

- `act` checks the node id against the latest tree and lists the valid ids
  if it misses. It checks the action against that node's advertised actions
  and lists them if it misses.
- `key` parses the key string with `taria::key`, the same parser the adapter
  lowers with, so a key the app would refuse is refused here with the
  grammar in the error. `repeat` is 1 to 64.
- `type_text` takes up to 4096 characters. The adapter lowers it into one
  key event per character.
- Every input tool refuses while no app is connected. A queued input would
  otherwise be delivered to the next app instance.

## Socket lifecycle

App side (`TariaLayer::bind`, or `bind_or_disabled`):

1. Resolve the path with `taria::socket::resolve_path`: `$TARIA_SOCK`
   verbatim, else `$XDG_RUNTIME_DIR/taria/<label>.sock`, else
   `<temp dir>/taria-<uid>/<label>.sock`.
2. Check its length. `sockaddr_un.sun_path` holds 108 bytes including the
   NUL, so 107 is the limit, and the kernel's own refusal names neither the
   path, its length, the limit, nor a way out. taria checks first and its
   error names all four. The bridge checks the same limit while parsing its
   arguments, so an over-long path never surfaces from inside the reconnect
   loop, where it would read as "the app is not running".
3. Create the parent directory with mode `0700` and vet it: a real
   directory (not a symlink), owned by the current user, no group or other
   permission bits. Binding is refused otherwise, so another local user
   cannot swap the socket.
4. Remove a stale socket file, bind, and serve one client at a time from a
   listener thread. Each connection gets the `hello` plus the latest
   snapshot, then streams every new publish. Dropping the layer shuts the
   threads down and removes the socket file.

`bind_or_disabled` turns any of those failures into an inert layer instead
of an error, so taria can never stop an app from starting. Every method
stays callable on a disabled layer, and the app reads `bind_error()` when it
chooses. The layer prints nothing itself: a print once the app owns the
alternate screen garbles the display.

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

Guard rails on the app side: a 5 s write timeout drops a peer that stops
reading, and a bounded input queue (256 entries) drops the newest input,
acks it `dropped`, and counts it in `dropped_inputs()` instead of blocking
the socket thread. On the bridge side, an input waits at most 500 ms for
room in the queue to the app, so a stopped app fails a tool call instead of
hanging it.

## Focus contract

Every snapshot carries exactly one focused node.

- The adapter guarantees at least one: the auto-generated `app` root is
  focused only when no recorded node (or descendant) is.
- The app is responsible for recording at most one. The demo unit-tests the
  invariant in every state: list, input, modal dialog, and empty list (focus
  parks on the list node itself).

Focus tells the agent where raw keys and typed text would land, which is
what makes the `key` and `type_text` fallbacks usable.

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
