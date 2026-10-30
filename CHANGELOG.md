# Changelog

Notable changes to the taria crates. The shape is
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/): one section per
release, newest first, dated. Inside a release the body is grouped by audience
rather than only by kind, because a second implementer of the protocol, an
agent driving an app, and an author retrofitting taria into one each need a
different part of the same list, and splitting it into Added, Changed and
Fixed scatters every one of them across all three.

The crates follow [semantic versioning](https://semver.org/spec/v2.0.0.html).
The wire format carries its own number, `PROTOCOL_VERSION`, which moves
independently of the crate version: a crate release is not a format bump, and
a format bump would not wait for one.

## [Unreleased]

**The Rust API is cut to what a caller needs.** Each crate had grown entry
points that did one thing several ways, or that exposed internals: a
`TariaLayer` had seven ways to take input, and `taria-mcp` exported its
bridge, its tool methods and its command-line parser. They are removed or
folded together before an alpha freezes the API, and what is left has room
to grow without a break: `LayerStats` is `#[non_exhaustive]`, and a
`taria-mcp` server is set up through a builder.

This breaks Rust callers of all three crates; the table under Upgrading maps
every removed name to what replaces it. Nothing on the wire moved:
`PROTOCOL_VERSION` stays 1, and `InputId`, a newtype in Rust now, is still a
bare integer in the JSON. Every text an agent reads through `taria-mcp`, from
the instructions to each answer, is unchanged byte for byte, and so is the
binary's command line.

### Upgrading

| Was | Now |
| --- | --- |
| `taria::wire::InputId`, an alias of `u64` | a newtype: `InputId::new(n)` or `n.into()`, read with `.get()` or `u64::from(id)` |
| `TariaLayer::drain_acking(f)` | `TariaLayer::drain(f)` |
| `TariaLayer::drain(f)`, `f` returning nothing | `drain(f)`, `f` returning `InputStatus::Delivered` |
| `try_recv()` and `try_recv_with_id()` | `try_recv()`, returning `Option<(InputId, AgentInput)>` |
| `recv_timeout(t)` and `recv_timeout_with_id(t)` | `recv_timeout(t)`, returning `Option<(InputId, AgentInput)>` |
| `drain_with_ids(f)` | `while let Some((id, input)) = layer.try_recv() { .. }` |
| `dropped_inputs()`, `stale_inputs()`, `unknown_inputs()`, `dropped_acks()` | fields of the same names on `stats()` |
| `truncated_snapshots()`, `last_truncation()`, `oversized_snapshots()`, `last_oversize()` | fields of the same names on `stats()` |
| `taria_mcp::server::TariaMcpServer`, `taria_mcp::tree_format::TreeFormat`, `taria_mcp::args::DEFAULT_IDLE_RELEASE` | `taria_mcp::{TariaMcpServer, TreeFormat, DEFAULT_IDLE_RELEASE}` |
| `TariaMcpServer::on_demand(socket, format, idle)` | `TariaMcpServer::builder(socket).tree_format(format).idle_release(idle).on_demand()?` |
| `TariaMcpServer::new(h)` or `with_tree_format(h, format)`, with `h` from `bridge::spawn(socket)` | `TariaMcpServer::builder(socket).tree_format(format).connected()?` |
| `server::parse_action(name)`, `server::action_name(&action)` | `taria::Action::from_wire(name)`, `action.wire_name()` |
| the tool methods `read_tree`, `act`, `key` and `type_text`, their `*_tool_attr()` functions, and `ActParams`, `KeyParams`, `TypeTextParams` | rmcp's `ServerHandler`, through a `tools/call` request |
| `bridge::{spawn, BridgeHandle, AppSnapshot, BridgeState, LinkState}` | none: a finalizer spawns the bridge |
| `tree_format::outline(line)`, `TreeFormat::render(&snapshot)` | none |
| `args::{parse, Cli, HELP, MAX_IDLE_RELEASE_SECS, resolve_socket_path}` | none: they are the binary's own. `taria::socket::resolve_path` derives a socket path from a `user` string the caller passes, while the adapter and the binary derive theirs privately: the uid on Linux (from `/proc/self`), else `$USER`, else `$LOGNAME`. A caller passing `$USER` gets a different temp-dir fallback path on Linux than the adapter binds |
| rmcp 3 | rmcp 3.8 or later |

- **`drain` is a new method under an old name.** It is 0.3's `drain_acking`,
  so a 0.3 `drain` call still finds a `drain` and fails on its closure
  (expected `InputStatus`, found `()`), not on a missing method. Return
  `InputStatus::Delivered` from the closure for 0.3's behaviour.
- **An `InputId` is no longer a number in Rust.** The JSON is the same bare
  integer. A `tracing` field that logged the id as a number needs
  `id = id.get()`.
- **`taria-mcp` is reached through its root, and its tools through rmcp.** Its
  modules are private. In-process tool calls, watching the bridge's state and
  rendering a saved line are no longer possible from outside the crate. Each
  can come back as an addition if someone needs it: open an issue.
- **Both finalizers return `std::io::Result`.** Outside a tokio runtime they
  return an error, where 0.3's constructors panicked. `connected()` ignores
  `idle_release`: that bridge never lets go.

### For whoever drives an app

- **rmcp 3.8 answers a few requests differently; taria's own texts do not
  move.** A request with `"id":null` now gets a `-32600` (invalid request)
  reply, where 0.3.3 sent nothing. Requests before `initialize` no longer end
  the session after the third: each is answered `-32602`, and `initialize` is
  still accepted after them. rmcp's stderr report of a malformed `initialize`
  is worded differently.
- **The stateless 2026-07-28 lifecycle is accepted too.** `server/discover`,
  like `initialize`, opens no connection to the app. In that mode
  `resources/list` and `prompts/list` carry
  `"ttlMs":0,"cacheScope":"private"`.

### For an app author adopting taria

- **`stats()` gathers what the layer lost.** One call returns a `LayerStats`:
  the six counts, all zero on a disabled layer, and the two `last_*`
  details, `None` until a cut, to read once the terminal is restored, as
  before. A count added later is a new field, not a break.
- **`TariaLayer::bind_or_disabled_at(label, path)` is public.** It binds at a
  path the app chose and returns a disabled layer when that fails, as
  `bind_or_disabled` does at the resolved path.

### For a second implementer

- **`Action::from_wire` is public, and `Action::wire_name` is new.** They map
  between an action and the name the wire spells it by, so a bridge written in
  Rust no longer keeps its own copy of the names.

### The gates

- `scripts/e2e.py` keeps its 26 steps and `scripts/adversarial.py` its 24
  probes.
- New `taria-mcp` tests drive a server built with `connected()` over MCP,
  check that `server/discover` and a `tools/list` sent the 2026-07-28 way open
  no connection to the app, and check that the builder refuses outside a
  tokio runtime.

## [0.3.3] - 2026-10-25

**A node can say that its text is not the app's.** A node's label and value
carry both the app's own words and text it passes through from outside:
model output, file contents, another user's message. Nothing told an agent
which was which, so relayed text could speak in the app's voice. A node now
takes an optional `relayed: true`, and the agent is told to read such text as
data, never as instructions (issue #6).

**A bridge that cannot be served says what it sees.** A bridge that
reconnected to an app whose tree it could not read told the agent to retry
(issue #16). A bridge that had let go of the app on purpose opened its answer
with "disconnected", and agents went looking for a fault with `ss` and `ps`.
A suspended app read as held by another bridge. Each now gets its own answer.

This is the first addition to the wire since version 1 froze in 0.1.0, and
it is additive: `PROTOCOL_VERSION` stays 1. `relayed` is a new optional field
(section 16.2), omitted when false, and a bridge built before it passes the
field on in JSON and shows `relayed=true` among the fields at the end of an
outline line. The Rust API grows by the `Node::relayed` field and its setter,
and by `taria_mcp::args::MAX_IDLE_RELEASE_SECS`; `taria_mcp::args::parse`
refuses values 0.3.2 took (see Upgrading). The `read_tree` description gains
one sentence in both formats, and the outline's also gains `*relayed` in its
grammar of a line. All three crates move to 0.3.3 together, and
taria-ratatui 0.3.3 needs taria 0.3.3.

### For whoever drives an app

- **`*relayed` marks text from outside the app.** In JSON the node carries
  `"relayed":true`; the outline prints `*relayed` right after the role, which
  nothing else can print bare. The mark covers the node's label and value,
  never its id, and children do not inherit it. An unmarked node is not
  vouched for: an app built before 0.3.3 marks nothing.
- **A reconnected bridge quotes the line it could not read.** After the
  `disconnected (last snapshot seq N)` opening, it says the app sent lines on
  the new connection and this bridge could not read the last one, with the
  parse error and the same depth and version hints a bridge that never had a
  tree gives. A reason from an earlier connection is no longer held against
  the next: a connect forgets every reason but an oversized line, whose rule
  is unchanged.
- **A bridge that let go says so.** Its held answer keeps the opening and
  `, and the app is now held by another bridge.`, then says the disconnect was
  this bridge letting go on purpose after its window with no tool calls, or
  up to 30 s later while an input awaited its ack, and that it is not a
  fault. Its other answers that say it reconnected, a stopped app's among
  them, say the same right after the opening. Once a connect finds nothing
  listening, or another process listening than the one it let go of (when
  the kernel names both), it stops saying so.
- **The retry hint follows `--idle-release`.** At the default the text is as
  in 0.3.2. A bridge started with another window names it, and one started
  with `0` says it never lets go.
- **A stopped app is named, on Linux.** When a connection waits unanswered,
  or an input draws no ack, the bridge checks the app's process: its pid from
  the socket, its state from `/proc`. A job suspended in its terminal needs
  the user to run `fg` there, since `kill -CONT` resumes it only in the
  background, where its next use of the terminal stops it again; any other
  stop takes `kill -CONT <pid>`, or the debugger. An input sent to a stopped
  app waits in the socket, and the answer promises it only if the app
  resumes within the bridge's window plus 30 s of the last tool call (90 s
  on the default): once the bridge lets go, the adapter discards it. With
  `--idle-release 0` it waits for good. Other platforms keep the 0.3.2
  answers.
- **An app that never sends `hello` is held to its first snapshot's
  version.** Every snapshot carries `protocol_version`, and until a `hello`
  arrives the bridge goes by the first snapshot's, so one stating another
  version refuses input exactly as a mismatched `hello` does.

### Upgrading

- **`--idle-release` takes plain digits from 0 to 86400.** 0.3.2 accepted
  `+5`, and any value up to 2^64-1, where the largest silently meant never.
  Anything else is now a usage error and the bridge does not start. Use `0`
  for never. `taria_mcp::args::parse` returns the same error to a library
  caller, and `taria_mcp::args::MAX_IDLE_RELEASE_SECS` names the limit.

### For an app author adopting taria

- **Mark relayed text with `Node::relayed(true)`.** The integration guide's
  new "Mark relayed text" section: mark text the app did not write, keep
  speakers in structure (one unmarked node per turn with the speaker as its
  label, the words in a relayed child) rather than in prefixes the content
  can spell, and never build an id or a custom action name from relayed text.
- **Apps that draw only a panel.** A new guide section covers an app that
  leaves its output in the terminal's scrollback and has ratatui draw only a
  pinned panel, as ramvamp does: publish that output from state, as a bounded
  `log`, and report after teardown (issue #8).

### For a second implementer

- `docs/protocol.md` gains the `relayed` row in section 6.1 and section 6.4,
  "Relayed text", which section 14.3 points to.
- Sections 3.5 and 5.2 say the reference bridge reads the first snapshot's
  `protocol_version` on a connection that sent no `hello`, and Appendix B
  item 2 says so too.
- Sections 1.3 and 9.4 say an input line that does not parse loses its id
  even when its text shows one, and why no conforming sender writes one.
- Section 16.6 is renamed "Passing the line on beats degrading", so "relay"
  means the field, and it now says what the reference bridge's copy of a
  snapshot respells: keys sorted, whitespace gone, numbers and escapes
  written its own way, and a key repeated in one object kept once.
- Section 14.5 gains a paragraph on how the reference bridge, on Linux, tells
  a held app from a stopped one, and section 14.7 lists what a reconnected
  bridge, one that let go and one facing a stopped app each say.
- Appendix B gains item 11: what a bridge does with relayed text beyond
  passing the mark on.

### The gates

- `scripts/adversarial.py` grows from 23 probes to 24. The new one stops an
  app and checks that the bridge names it with its pid, and that a job
  suspended on a terminal is told to `fg`.
- `scripts/e2e.py` keeps its 26 steps. Step y expects the released answer
  from the bridge that let go, and step z, when it fails, tells a transient
  answer from a held one.

## [0.3.2] - 2026-10-24

**An idle agent session no longer holds the app.** The adapter serves one
bridge at a time, and `taria-mcp` connected at startup and never let go. A
harness starts every server it has registered in every session, so with a
project `.mcp.json` the first session opened kept the app, and the session
that wanted it had to kill a bridge. `taria-mcp` now connects on its first
tool call and lets the app go after 60 s with no tool calls.

Nothing on the wire moved. `PROTOCOL_VERSION` stays 1, no message or field
changed, and the code of `taria` and `taria-ratatui` is unchanged. The Rust
API only grows: `bridge::spawn` still connects at once and never lets go, and
`TariaMcpServer::new` and `with_tree_format` serve such a handle as in 0.3.1.
The new `TariaMcpServer::on_demand(socket, tree_format, idle_release)` is what
the binary uses. The MCP instructions and tool descriptions are unchanged,
byte for byte. All three crates move to 0.3.2 together: they share one
workspace version.

### For whoever drives an app

- **The bridge connects on its first tool call.** MCP `initialize` and
  `tools/list` open nothing, so a session that never calls a taria tool never
  takes the app. The first call waits briefly, at most about a second, for
  the connection, the app's `hello` and its tree, so it still returns the live
  tree in one call.
- **An idle bridge lets go after 60 s.** After 60 s with no tool calls it
  closes its connection, or leaves the listen backlog (the kernel's queue of
  connections the app has not accepted yet) if it was waiting there, and
  stops reconnecting, so a bridge waiting for the app gets it at once. The
  next call reconnects. It does not let go while an input it sent still
  awaits the app's ack, for at most 30 s more.
- **`--idle-release <SECS>`** sets that delay in whole seconds. The default is
  60, and `0` never lets go. The bridge connects on demand either way.
- **A bridge locked out by another says the app is held.** It says the app is
  held by another bridge, and that a taria-mcp lets go of the app after 60 s
  with no tool calls, so the agent retries instead of asking someone to kill a
  process. A bridge that lost the reconnect race when the app restarted gets
  this answer too, since that app is running. Its answer still opens
  `app '<label>' disconnected (last snapshot seq N)`, the part a caller
  matches on, and goes on `, and the app is now held by another bridge.`
- **A bridge that reconnects to a running app no longer says it may have
  exited.** This bug was already in 0.3.1: a bridge that reconnected to a
  restarted app, which had greeted it but not yet published a tree, was told
  the app may have exited. Every answer from a bridge with a connection open
  again keeps the same opening, then says what that connection shows. If the
  app has greeted it, the bridge has reconnected, nothing needs restarting,
  and a retry gets the first snapshot the app publishes. If the connection is
  under half a second old and nothing has greeted it yet, the answer says how
  many milliseconds ago it reconnected, and that this is worth one retry. Past
  that, it is the held answer above. Only a bridge with no connection open
  says "disconnected (last snapshot seq N); it may have exited", word for word
  as in 0.3.1.
- **A holder older than 0.3.2 still holds the app until its session ends.**
  Until every session runs the new bridge, disable taria with `/mcp` in the
  Claude Code sessions that do not drive the app.

### For an app author adopting taria

- **Nothing to change.** A project `.mcp.json` that registers the bridge no
  longer hands the app to whichever session opened first.

### The gates

- `scripts/adversarial.py` grows from 22 probes to 23. The new one checks that
  a release never drops an input still awaiting its ack, and the held-socket
  probe matches the new wording.
- `scripts/e2e.py` grows from 23 steps to 26. Step x starts a bridge and never
  calls it, and that bridge must not take the app. Step y gives the holder
  `--idle-release 2` and the waiting bridge 3: the waiting bridge is told the
  app is held, gets it once the first lets go, and the demo exits with no
  input lost. Step z restarts the app, and the bridge that loses the
  reconnect race must say the app is held, not that it exited.

## [0.3.1] - 2026-10-19

**An app whose tree outgrew one line keeps its bridge.** A snapshot travels as
one line, and the bridge closes the connection on a line over 1 MiB. The
adapter wrote snapshots of any size, so an app holding one unbounded value, a
draft, a log or a transcript, lost its bridge, and again on every reconnect,
which resends the latest snapshot. The agent was told the app may have exited.
The adapter now trims every snapshot to fit, and the bridge names the limit.

**A bridge waiting its turn can no longer act on a UI it never saw.** The
adapter serves one bridge at a time, and a second one waits in the listen
backlog (the kernel's queue of connections not yet accepted). Input it wrote
while waiting was applied once the first bridge left. It is now thrown away,
unacked, before the `hello` goes out. A client that connects to an idle app
keeps what it writes, as before.

**Answers the app gave on its way out arrive.** An app that answered an input
and exited in the same frame closed the connection with the answer still
queued, so the agent was told the input's fate was unknown. The adapter now
writes the queued acks before it closes, within 250 ms.

Nothing on the wire moved. `PROTOCOL_VERSION` stays 1, and no message or field
changed. `docs/protocol.md` gains a SHOULD (keep every snapshot within the
line cap) and a MAY (discard input written before the `hello`), both
describing what the reference peers do. The MAY answers a question version 1
had left open (Appendix B item 7), so it is additive. The Rust API only grows.
All three crates move to 0.3.1 together: they share one workspace version.

### For whoever drives an app

- **A tree over the line limit is named, not mistaken for an exit.** The
  bridge keeps an overrun as its reason, across reconnects, until a snapshot
  parses or a connect finds nothing listening at the socket. Until then
  `read_tree` and every input tool say the snapshot was over 1 MiB (1048576
  bytes), that the app resends it on every reconnect, and that nothing the
  agent sends can fix it. An app that overran and then exited reads as exited.
- **A second bridge is told the app has not accepted it yet**, not that it
  "never accepted" it: the app does once the first bridge leaves.
- **An input acked just before an oversized snapshot is not blamed for it.**
  The answer says the bridge closed the connection, not the app, and why. It
  opens as before, so a caller matching on the opening reads the same verdict.
- **A trimmed string says so where it was cut.** A cut value or label ends in
  `…`, and a `log` or `terminal` value, which keeps its tail, starts with it.
  Ids and custom action names are never cut, so an agent can still use them.
- **An edit past the cut of a trimmed value shows no change.** The tree is the
  one already sent, so nothing new goes out, and the input tool reports the
  input received with no change.
- **An advertised `quit` comes back acknowledged.** The ack the app queued as
  it took the input now reaches the bridge before the connection closes, and
  so does an `Ignored` it gave on its way out.

### For an app author adopting taria

- **Every snapshot is bounded to fit one line.** An unchanged frame is
  skipped; a changed one is measured exactly and, if over, cut as
  `docs/architecture.md` describes. A string of 256 bytes of JSON or less is
  never cut.
- **You are told.** `oversized_snapshots()` counts snapshots published
  trimmed, and `last_oversize()` returns a `SnapshotTooLarge`: the line's size
  as built, the limit, the strings trimmed and nodes dropped, and the node
  holding the longest string, ids and custom action names included. The fix is
  yours: publish a log's tail, cap a draft, publish the rows on screen. The
  demo prints each snapshot counter at exit when it is nonzero.
- **`truncated_snapshots()` counts snapshots, as its doc always said.** It
  counted frames: a deep tree redrawn unchanged at 60 fps added 60 a second.
  It now counts once per snapshot sent, as `oversized_snapshots()` does.
- **A tree deeper than the call stack no longer aborts the app.** The depth
  cut exists for such a tree, but 0.3.0 freed what it cut away, and any tree
  handed to a disabled layer, by recursing, which overflows the stack. Every
  tree the adapter lets go of, an unpublished frame included, is now freed
  one node at a time.
- **Input a waiting client wrote before its `hello` is discarded**, unacked,
  and counted in `stale_inputs()`. Only a client that waited in the backlog
  for the previous session to end is affected; one that waits for the `hello`
  before writing, as taria-mcp does, loses nothing.
- **Every line has 5 s in all, and `Drop` is bounded.** In 0.3.0 the 5 s write
  timeout bounded each wait for room in the socket, not the line, so a bridge
  reading slowly or not at all could hold a line, and `Drop`, for 10 s or
  more. A line now goes out in 2 KiB pieces against one 5 s deadline, and a
  bridge that misses it is dropped. Acks queued at shutdown get 250 ms and a
  pending snapshot is not sent, so `Drop` takes at most 50 ms + 5 s + 250 ms.
- **`taria::wire::MAX_LINE_BYTES`** is the 1 MiB limit as a public constant,
  shared by both reference peers, for a peer or a test that sizes against it.

### The gates

- `scripts/adversarial.py` grows from 20 probes to 22. A stand-in app sends a
  tree over the line cap, once after a tree was served and once as its first,
  and every answer has to name the limit.
- `scripts/e2e.py` grows from 22 steps to 23. Step w types the demo's draft
  past 1 MiB through `type_text`: the bridge stays up, the draft comes back
  trimmed, `set_value ""` clears it, and the demo reports the trim at exit.

## [0.3.0] - 2026-10-19

**`taria-mcp --tree-format outline` serves the tree as an outline.** The
bridge relays the app's JSON line, and most of that line is repeated field
names, `"focused":false` on nearly every node, and punctuation, which is how a
benchmark found a taria tree costing more per call than a tmux screen of the
same app. The outline carries every node, id, role, label, value, focus
state and field, and every action by the name `act` takes, in under half the
characters: the demo's first tree is 1,132 characters as JSON and 546 as an
outline. Extra keys inside an action object are not shown, because `act`
takes only the name. It is opt-in, and the default stays JSON.

**The bridge's own text is less than half as long.** The server instructions
and the four tool definitions are resent with every model call. Counted as the
instructions plus each tool's `tools/list` entry as Python's `json.dumps`
prints it, they came to 4,788 characters, the bridge's largest fixed cost,
against 2,787 for the benchmark's tmux driver. They are now 1,992 characters,
and 2,199 with the outline. `act` also says what `activate` does, which no
text said before.

Nothing on the wire moved. `PROTOCOL_VERSION` stays 1 and apps send the same
JSON; only the text the bridge hands the model changes.

**This breaks the Rust API of `taria-mcp`, so this release is 0.3.0.**
`args::Cli::Run` gained a field, and `Cli`, its `Run` variant and the new
`TreeFormat` are now `#[non_exhaustive]`, so that the next flag or format is
not another break. Code outside the crate that builds or matches them stops
compiling: a pattern `Cli::Run { socket }` fails with a missing-field error,
and a `match` over `Cli` that names only `Run` and `Help` fails as
non-exhaustive. Destructure with `..`, as `Cli::Run { socket, .. }`, and add a
wildcard arm to a `match` over `Cli` or `TreeFormat`. A struct literal of
`Cli::Run` is no longer possible outside the crate; get one from
`args::parse`. Nothing changes for whoever runs the binary.

**Upgrade `taria` and `taria-ratatui` as a pair.** All three crates move to
0.3.0 together, as they did to 0.2.1, because they share one workspace
version; neither of the other two has a code change. `taria-ratatui` 0.3.0
depends on `taria` 0.3.0, so an app that keeps `taria = "0.2"` beside it
builds two copies of the protocol crate, and the nodes it builds are not the
`Node` the adapter takes: the build fails with a type mismatch. Move both
requirements to `0.3`. An app that names only `taria-ratatui` and reaches the
types through `taria_ratatui::taria`, as the integration guide's one-line
setup does, has only that one to move.

### For whoever drives an app

- **`--tree-format <json|outline>`**, default `json`. The outline is a header
  line of the snapshot's other fields (`protocol_version=1 seq=1`), then one
  line per node, indented two spaces per level, as
  `id role "label" = "value" *focused act: a|b`, with each part present only
  when set. `docs/cli.md` has the grammar. A missing or unknown value is a
  usage error.
- **The actions end the line, labelled `act:`**, joined by `|`, so the list
  reads as the call to make rather than as a detail an agent can pass over
  for `key`. A name holding a space or a `|` is quoted, and so is an id, role
  or name spelled `act:`, so the marker cannot be forged.
- **Every tree goes out in the chosen format**: `read_tree`, and the tree in
  every `act`, `key` and `type_text` answer, the ignored note's tree included.
- **The outline is rendered from the relayed line**, so a
  role, an action or a field this bridge has never heard of still reaches the
  agent by name. Every action prints as the name `act` takes for it, spelled
  from the bridge's own parse, which is what `act` checks it against:
  `delete` rather than `{"custom":"delete"}`, `delete` for
  `{"custom":"delete","icon":"x"}`, and `scroll_to` for an object a newer app
  might send, `{"scroll_to":{"row":3}}`. A test passes every printed name
  back through `act`. An id or a label that would break the line is
  JSON-quoted, with U+2028, U+2029 and U+0085 escaped as well as the newline,
  and a line the outline cannot express is served as JSON.
- **The agent is told which format it is reading.** In outline mode the MCP
  instructions and the `read_tree` description say the tree is an outline and
  how a line reads, and `act`'s description says to pass node and action as
  shown, a quoted one JSON-decoded, since a quoted token is a JSON string.
  Every other word is the same in both modes, and a test pins each mode's
  text byte for byte.
- **The instructions give the order of work and one rule.** Read the tree,
  prefer `act` with an advertised action, fall back on `key`, and type text
  with `type_text`, not `key`, because `key`'s characters hit the app's key
  bindings. Where an app routes no typed text, `key` remains the way into its
  fields. `key`'s description says the rule again, for a client that drops
  the instructions, and `type_text`'s says the text never goes through the
  key bindings.
- **The input tools say they return the tree if it changed.** An input the
  app took with no change to show, one it neither acknowledged nor changed
  anything for, and one it acknowledged on its way out each come back as a
  note with no tree.
- **`type_text` says a newline may act as Enter and submit**, and to send
  text meant for after it in a second call. In a multi-line editor a
  newline inserts a line instead.
- **The `{"custom":"delete"}` rule lives only in `act`'s description**, in
  JSON mode. The outline prints the inner name, so it never shows the
  envelope.
- **`act` says `activate` presses a button or submits a field.**
- **Parameters carry no description but `repeat`'s "Defaults to 1."** The
  tool descriptions already cover them, and the schema still states every
  bound: 4096 characters for `value` and `text`, 64 for `key`, and 1 to 64
  presses for `repeat`.
- **Some of what the texts said is now left to the answers.** A key outside
  the grammar is still refused with the grammar in the error, and an input
  the app ignored still comes back as a result carrying the tree, not as an
  error, whose text names a `set_value` sent without a value among the
  causes. Some is no longer in any text the agent sees: how to aim
  `type_text` (an advertised focus action, or `set_value` on the target),
  that a tab arrives as Tab and carriage returns are dropped, and the
  instructions' "never guess them" about node ids and action names.
- **For a Rust caller**: `TariaMcpServer::with_tree_format` builds the handler
  in either format, `TariaMcpServer::new` still serves JSON, the renderer is
  `taria_mcp::tree_format`, and `args::Cli::Run` carries the parsed
  `tree_format` beside the socket, which is the break described above.

### The end-to-end gate

- `scripts/e2e.py` grows from 21 steps to 22. The new one starts a demo behind
  a bridge run with `--tree-format outline`, reads the outline, and selects a
  row through an `act` named from it, whose answer has to be an outline too.
  It then invokes `delete` on that row, the custom action as its line spells
  it, checks the confirm dialog opened, and closes it through the dialog's
  advertised `dismiss`, so the demo ends with no modal and the row intact.

### The benchmark

`bench/` measures what an AI agent pays to drive the demo through taria,
against the same agent reading the screen through tmux. Six tasks are graded
from the demo's own state, which `taria-demo` writes as JSON to the file
named by `TARIA_DEMO_STATE`; unset or empty, it writes nothing. Claude Haiku
5.5 and Sonnet 5.5 ran each task five times on each of three arms: the tree
as JSON, the outline, and tmux. `bench/RESULTS.md` has the method, the
caveats and every number. Medians per run, with this release's bridge,
counting the tokens the model read:

- **Haiku:** taria passed all 30 runs as JSON and all 30 as the outline, and
  tmux 29 of 30, a gap within chance at this size. tmux used 1.90x the
  tokens of taria JSON and 2.09x those of the outline, and took 2.27x JSON's
  time to finish. Its one failure was typed text meeting the list's key
  bindings, the failure `type_text` exists to prevent.
- **Sonnet:** every arm passed all 30 runs. tmux used 7.7% fewer tokens than
  taria JSON, and the outline 1.7% fewer than tmux. The time to finish
  showed no detectable difference between tmux and JSON.
- **The bridge's shorter text** cut taria JSON's tokens per run by a fifth
  against a first round run on 0.2.1's bridge: 20.0% on Haiku, 19.7% on
  Sonnet. tmux's text did not change between the rounds. Its Sonnet median
  moved 0.1%, and its Haiku median rose 10.3%, which `bench/RESULTS.md` puts
  down to run-to-run variation.
- **The raw fallbacks nearly stopped.** In the first round, taria agents made
  38 of their 449 tool calls through `key` or `type_text`; in this one, 2 of
  453. The texts, the outline's format and remote control's note all
  changed in between, so `bench/RESULTS.md` credits none of them alone.

### Each crate carries its licences

`taria`, `taria-ratatui` and `taria-mcp` now ship `LICENSE-MIT` and
`LICENSE-APACHE` in the package crates.io serves. They declared `MIT OR
Apache-2.0` before without either text, which both licences ask to travel
with copies. In the repository they are links to the root files, so there is
one copy of each.

## [0.2.1] - 2026-10-11

**Adopting taria no longer widens the host app's ratatui.** `taria-ratatui`
took ratatui with its default features while using only the crossterm event
types, `Buffer`, `Rect` and `Widget`, and cargo unions features across a
build, so an app that had trimmed its own ratatui got `all-widgets`,
`widget-calendar`, `layout-cache` and `macros` back the moment it added the
adapter, and compiled `time`, `ratatui-macros` and `critical-section` along
with them. Found adopting taria into ramvamp, whose manifest asks for
`default-features = false, features = ["crossterm"]`.

Nothing on the wire moved and no crate's Rust API changed. `PROTOCOL_VERSION`
stays 1. All three crates move to 0.2.1 together: they share one workspace
version, and the `taria-mcp` binaries the v0.2.1 tag builds should report the
release they came from.

### For an app author adopting taria

- **The adapter asks ratatui for `crossterm` and nothing else.** A crossterm
  host's ratatui now resolves to what the host asked for, which for a trimmed
  app is `crossterm` and `std`. A host on another backend still gets
  `crossterm`, which the adapter's key conversion needs.
- **The bytes on the terminal do not change.** `underline-color` is in
  `ratatui-crossterm`'s own default set and arrives through the `crossterm`
  feature the host selected itself, so a crossterm host has it with or
  without taria. Measured on a pty in ramvamp, the reset sequence is
  identical either way. What does change is speed: a trimmed host's
  `Layout::split` no longer caches.
- **One kind of build can break, and it was relying on taria by accident.** An
  app that used the calendar widget, a ratatui macro, `Layout::init_cache` or
  `Layout::DEFAULT_CACHE_SIZE` without asking for `widget-calendar`, `macros`
  or `layout-cache` compiled only because the adapter had switched it on. It
  now has to name that feature in its own manifest.

### For whoever drives an app

- **`install.sh` installs the bridge without a Rust toolchain.** It resolves
  the latest tag, or the one `TARIA_VERSION` pins, downloads the tarball for
  the platform it runs on, checks it against the sha256 the release already
  publishes, and installs `taria-mcp` into `/usr/local/bin` when that is
  writable and `~/.local/bin` otherwise. A checksum mismatch is a refusal, not
  a warning. If the checksum cannot be fetched, or neither `shasum` nor
  `sha256sum` is installed, it says so and installs anyway. Windows gets the
  reason rather than a missing-artifact error: the transport is a Unix domain
  socket.

  ```bash
  curl -sSf https://y0sif.github.io/taria/install.sh | sh
  ```

- **The copy on the site follows the repository.** A workflow copies
  `install.sh` to `gh-pages` on every push to `main` that changes it, so the
  `curl` recipe fetches the file in the repository.

### The demo is a trimmed host

`taria-demo` now takes ratatui with `default-features = false` and `features =
["crossterm"]`, the way a careful host does, and builds unchanged. It shares a
workspace with the adapter, so this repository resolves ratatui the way an
adopter's app does, and `cargo tree -e features -i ratatui -p taria-demo`
shows `crossterm` and `std` and nothing else. The check is manual: no CI step
runs it yet.

### Docs for someone landing on the repository

- **The README is a front page again**: 380 lines down to 199. The FAQ moves
  whole to `docs/faq.md`, the `taria-mcp` flags, environment variables and
  socket path resolution to `docs/cli.md`, and Compatibility and Limitations
  merge into `docs/compatibility.md`. Nothing was cut: every sentence that
  left the README is in one of those three files.
- **`docs/comparison.md` and the FAQ are headings now**, one link each at the
  foot of the README, in place of a mention mid-paragraph.
- **`docs/compatibility.md` does not restate the freeze.** Section 16 of
  `docs/protocol.md` is the normative statement on versioning, so the
  compatibility page points at it and keeps the consequences: what the bridge
  relays across a version difference, what `#[non_exhaustive]` costs a Rust
  adopter, and the wildcard arm that hides `Text`.
- **What the README keeps**: the install split, the quick start, the tool
  table, the workspace tree, and the six development commands. The prose
  around those commands was `CONTRIBUTING.md`'s, and is now a link to it.

## [0.2.0] - 2026-09-12

**The crates are on crates.io.** 0.1.0 froze the wire format and never left
this repository, so 0.2.0 is the first version cargo can resolve, and the
0.1.0 section below, which says nothing is published yet, is now a statement
about that release rather than about the project.

```bash
cargo add taria-ratatui   # in a TUI app that agents should be able to use
cargo install taria-mcp   # the bridge, for driving one
```

A tag also builds `taria-mcp` for x86_64 and aarch64 Linux and for both macOS
architectures and attaches the tarballs, each with its sha256, to a GitHub
release, so driving an app does not require a Rust toolchain.

Nothing on the wire moved. `PROTOCOL_VERSION` stays 1, no message or type
gained or lost a field, and no crate's Rust API changed. Everything below is a
specification, a description an agent reads, a test that was missing,
packaging, or one action the demo app now advertises, which `Action::Focus`
already existed to carry.

### The wire format is specified, message by message

`docs/protocol.md` is the normative per-message specification of version 1,
written for someone implementing a peer from that document alone: an adapter
for a framework that is not ratatui, in a language that is not Rust. Until now
`docs/architecture.md` said the per-message spec was deferred and that the
rustdoc on `crates/taria` was normative until it existed, which is a fine
answer for a Rust adopter and no answer at all for anyone else.

It covers framing, the handshake, every message with a literal line, both
vocabularies with their degrade rules, the ack model, connection ownership,
the key grammar, the limits with their units, socket discovery and lifecycle,
the focus contract, and what a version bump costs. Rationale stays in
`architecture.md`. Behaviour the reference peers add beyond what the protocol
requires is marked as convention and is not binding on a conforming peer, and
Rust-specific obligations are confined to one section.

Several normative facts existed only in code and are written down for the
first time: that `AgentInput` is tagged on `kind` while the message envelope
is tagged on `type`, one level apart in the same line, so a peer that
conflates them fails every input; that the 1 MiB line cap counts bytes,
excludes the newline, and closes the connection rather than skipping a line;
that depth counts the root as level one; and that an input id is a `u64`.
Every literal line in the document is real output, either lifted from the
frozen test vectors or captured from a live session against the demo. What is
genuinely undecided is listed as undecided rather than invented.

### What an agent is told about typing

The agent-facing text still carried the v0 model, in which the adapter lowered
`Text` into one key event per character. The MCP instructions said to use
`type_text` "for anything you would otherwise spell out with `key`", which is
exactly the conflation that once deleted a task, offered to every agent on
connect.

- **`type_text` now says where the characters go.** To whatever surface the
  app puts typing into, never through its key bindings. A newline arrives as
  Enter and submits, a tab arrives as Tab, carriage returns are dropped,
  characters after a submit usually have nowhere left to go, and an app
  accepting no typing answers ignored, which reaches the agent as a result
  carrying the tree rather than as an error. The lowering is named as an
  adapter convention, not a protocol rule, since the core crate does not
  dictate how an app consumes characters.
- **The server instructions describe `key` and `type_text` as the two
  different lowerings they are**, rather than as a slow path and a fast one.
- **`act` says that an action needing a value and arriving without one is
  answered ignored.**
- **The ignored result names every kind of input that can earn it.** It was
  written act-first, so it covered neither text sent into a surface taking
  none nor a key the grammar refuses.
- **`InputStatus::Ignored` documents the same list**, and the line the demo
  was alone in knowing: a key that parses but is bound to nothing is
  `Delivered`, the same verdict a person gets for pressing an unbound key.
  `Ignored` is for input the app could not act on at all, which is what makes
  it worth reporting to an agent waiting on an effect.
- **The seven built-in actions had no documentation**, `Custom` aside. For
  `Action::Focus` that left "focus the target first" as advice with no way to
  follow it. Each action now says what it does, and `Focus` says what
  advertising it promises: an act with it puts the keyboard on that node,
  and the next snapshot shows that node as the focused one, so an agent can
  check the move landed rather than assume it. That is what makes it the
  advertised way to aim typing. `set_value` is not a focus call, even in an
  app that moves the keyboard as a side effect of one.
- A docs.rs link that pointed through a private module is fixed.
- **The integration guide teaches aiming.** A new section works through
  routing typed text to a surface, with the handler and the rule that
  `set_value` is not a focus call, and the running sample advertises `focus`
  on its text input while the keyboard is elsewhere. The hand-check list at
  the end grows from five items to six.

### The demo advertises the way in

`Action::Focus` became the documented way to aim typing, and the reference app
advertised it on nothing, so the advice had no worked example and no test
behind it. A live agent found the hole immediately: it wanted the keyboard and
nothing else, saw only `set_value` advertised on the input, tried to aim with
an empty value, and ended up letting `set_value` carry the whole title because
there was no other semantic way in.

The demo's input now advertises `focus` while the keyboard is elsewhere, the
mirror of the `dismiss` it already advertised while it held the keyboard, and
the semantic form of the `i` key a person presses. Acting on it moves the
keyboard and nothing else: the draft is untouched, which is the point. It is
reported `ignored` when the input already has the keyboard and while the
dialog is up, so the advertisement and the verdict agree, and the tree stops
offering it in exactly those states.

`scripts/e2e.py` gains the step that was missing: aim with `focus`, type the
whole title with one `type_text`, submit with `activate`. The title carries
the demo's own list bindings (`d`, `j`, `k`, `q`, `y`, `i`) so a character
routed to them cannot pass unnoticed, and a second `focus` asserts the
ignored verdict against the same frame that stopped advertising it.

### The ignore path is gated

The claim that new wording makes to an agent, that text sent while nothing is
accepting typing comes back ignored with a tree rather than as an error, was
the one claim neither suite tested. `scripts/adversarial.py` gains a probe
that types `dy` at the list: the pair that opens the confirm-delete dialog and
confirms it. Lowered through the key bindings it would destroy a task and
still look like success, because the tree would change. The probe asserts that
the ack is `Ignored`, that a tree came with it, that the tree does not show
the input focused, and that every node is untouched either side of the call.

The gate is 21 end-to-end steps and 20 adversarial probes.

### Packaging and CI

- **All three crates carry a `readme`.** `taria-ratatui` and `taria-mcp` had
  none, so their crates.io pages would have rendered empty next to `taria`'s.
- **The workspace dependency on `taria` carries its own version**, and cargo
  has no way to make it follow `workspace.package`, so it is a second literal
  to bump by hand every release. Bumping `workspace.package` alone would have
  published 0.2.0 crates depending on a 0.1.0 that does not exist.
- **A tag releases binaries.** `taria-mcp` is what a person installs, so a tag
  builds it for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `x86_64-apple-darwin` and `aarch64-apple-darwin`, and attaches a tarball and
  its sha256 for each to a GitHub release. Windows is absent on purpose: the
  transport is a Unix domain socket. Publishing the crates themselves stays
  manual and in dependency order.
- **CI checks the MSRV.** The crates promise `rust-version = "1.88"` on their
  crates.io pages and nothing verified it. A `cargo check` on a pinned 1.88
  does, cheaply: what an old compiler rejects is syntax and library items, not
  test outcomes.

### Docs for someone deciding whether to adopt this

- **`docs/comparison.md`** places taria beside ht, tmux with `send-keys`, the
  tmux and PTY MCP servers, agent-tui, tui-use and agent-terminal: what each
  does, why working inside the app rather than outside it changes what an
  agent can know, and the cases where the right answer is one of the others.
  The first of those is the largest: taria reaches only apps whose authors
  adopted it, so anything you did not write and cannot patch is screen-level
  territory.
- **The README has an FAQ**, in the words people search with: ARIA for
  terminals, an MCP server for a TUI, ht and tmux `send-keys`, whether the app
  has to be ratatui, what adoption costs an author, Windows, two agents at
  once, whether the protocol is stable, and what happens to the widgets you
  have not annotated.
- **The README has install instructions**, split by which side of the socket
  the reader is on: a dependency for an app author, a binary for whoever
  drives the app.
- Three claims in the docs were wrong and are corrected: the temp-dir fallback
  path is namespaced by the effective uid, else `$USER`, else `$LOGNAME`, else
  the literal `default`, rather than by the uid alone; "exactly one focused
  node" is the app's obligation rather than something either peer enforces,
  since the adapter guarantees at least one and the bridge never reads the
  field; and eleven types are `#[non_exhaustive]`, not ten.

## [0.1.0] - 2026-09-10

The first release, and the one the wire format freezes in. Nothing is
published to crates.io yet, so both crates still come from the repository.
"v0" below means the vertical slice that preceded this release: three MCP
tools, 18 roles, no input ids, and no acknowledgement.

### The format is frozen at `PROTOCOL_VERSION` 1

Version 1 is fixed. Changes within it must be additive: a new optional field,
a new message variant, a new role, a new action name. Two properties make that
safe, and both peers already rely on them. A reader skips a line it cannot
parse instead of dropping the connection, and serde ignores unknown fields, so
an addition reaches an older peer as something it quietly ignores while the
rest of the stream keeps working.

Neither property covers a value nested inside a message the peer does want, so
the vocabularies carry their own fallbacks: an unknown `Role` reads as
`Role::Other`, an unknown `Action` name as `Action::Custom` keeping that name,
and an `AgentInput` kind this build cannot read as `AgentInput::Unknown`.
Without them, one leaf node using a role added later makes every snapshot
unparseable, the reader skips each line and goes on serving its last good
tree, and the app looks frozen with no error anywhere.

One rule is narrower than "additive". An ack answers an `InputId`, and the id
sits on the message rather than inside the input, so a new `BridgeToApp`
variant carrying agent input would reach an old adapter as a skipped line with
nothing sent back, and the agent that sent it would wait out a timeout. An
addition that carries agent input must therefore be a new `AgentInput` kind,
never a new `BridgeToApp` variant. New `BridgeToApp` variants stay available
for anything that wants no answer.

Removing a field, renaming one, making an optional field required, or changing
what an existing field means bumps `PROTOCOL_VERSION`. The last is the
dangerous one, because an old peer parses it and acts on the old meaning.

The normative statement, with the reasoning behind each clause, is the module
documentation in `crates/taria/src/wire.rs`. That is the text a second
implementer should read in full; `docs/architecture.md` explains it and
`CONTRIBUTING.md` states it as a ground rule.

### Breaking changes since v0

Anything built against the v0 slice needs these.

**The wildcard arm the compiler makes you add to every `match` over
`AgentInput` is the arm that swallows `AgentInput::Text`, so check every
wildcard you add for `Text` before you trust a green build.** The two changes
below that cause this sit in different lists: `Text` is a wire change and
`#[non_exhaustive]` a Rust one, and it is their product that bites. A v0
`match` named `Act` and `Key`; rebuilt, it fails with
``non-exhaustive patterns: `_` not covered``, an error that never names
`Text`, and the arm that silences it takes every `type_text`. On the first
real app migrated, the four edits the compiler asked for left a clean build,
a clean `clippy -D warnings`, 181 passing tests, and a `type_text` that did
nothing at all. `docs/integration-guide.md` names a clippy lint that catches
it.

On the wire:

- `BridgeToApp::Input` carries an `InputId`, and the app answers it with
  `AppToBridge::Ack` naming the same id and one of `Delivered`, `Dropped` or
  `Ignored`. This replaces inferring the outcome from snapshot diffs, which
  counted any unrelated change as success.
- `AgentInput::Text` carries literal text, so typing costs one message rather
  than one round trip per character.
- `AgentInput::Unknown` is the fallback described above. An app never sees it:
  the adapter acks it `Ignored` at the parse and never queues it, because the
  variant carries nothing an app could act on.
- The role vocabulary went from 18 names to 29, and a custom action name now
  travels folded onto the built-in envelope, so a name that later graduates to
  a built-in reaches the newer app's own arm instead of being accepted and
  silently ignored.

In Rust:

- `#[non_exhaustive]` on the eleven types a version-1 addition can reach:
  `AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`, `Action`, `Role`,
  `Node`, `Snapshot`, `key::Key`, `key::Modifiers` and `key::KeyPress`. And on
  the six struct-like variants where a new optional field would land:
  `AppToBridge::Hello` and `Ack`, `BridgeToApp::Input`, and `AgentInput`'s
  `Act`, `Key` and `Text`. Marking a type is itself a breaking change, which is
  why all of it happens before the first release rather than at the first
  addition. It costs an outside peer one wildcard arm per match, or a trailing
  `..` in a pattern, and buys back that a new variant, field, role, action or
  key is a recompile rather than a repair. On `AgentInput` the wildcard costs
  more than that for anyone coming from v0: it is where `Text` goes missing,
  as the warning at the top of this section says.
- A marked variant has no struct literal outside the crate, so each ships with
  a constructor beside it: `AppToBridge::hello` and `ack`,
  `BridgeToApp::input`, and `AgentInput::act`, `key` and `text`. `Modifiers`
  gains `NONE` and a const `new` in exchange for the struct literal it closes.
- `taria_ratatui::key::to_crossterm` returns `Option<KeyEvent>`. `None` means
  a key a newer taria's grammar learned that this adapter cannot express. It is
  reported rather than approximated, because lowering it to a near-miss event
  would put a keystroke into the app that the agent never asked for and ack it
  as delivered.
- Socket path resolution left both peers for `taria::socket::resolve_path`,
  which returns `Result<PathBuf, InvalidAppLabel>`. The label becomes a file
  name and the adapter binds and unlinks whatever comes out, so an absolute
  label, or one carrying a separator or `..`, is refused rather than resolved.
- Three modules moved into the core crate so the two peers cannot disagree
  about a string: `taria::key` (the key grammar), `taria::id` (`IdSpace`) and
  `taria::socket` (path resolution and the AF_UNIX limit).
- `serde_json` moved to dev-dependencies. The core crate defines the wire
  types and leaves encoding to the transport, so an adopter inherits one
  dependency, serde, instead of two.

In the tool surface: three tools became four. `read_tree`, `act` and `key`
are joined by `type_text`.

### What an agent gains

- **Typing costs one call.** `type_text` sends a literal string of up to 4096
  characters where a 100-character string used to be 100 `key` calls.
- **A malformed key is rejected with the grammar instead of vanishing.** `key`
  parses the string with `taria::key` before sending anything, so a key the app
  would have silently swallowed comes back as an error naming what the grammar
  accepts. An agent sent `inx` once and never learned it was dropped. `key`
  also gains `repeat`, bounded at 1 to 64, so moving five rows is one call.
- **Every result is anchored on the app's acknowledgement** rather than
  inferred from whether the tree happened to change. `Dropped` is an error.
  `Ignored` returns the tree of the frame that caused the ignore, with a note
  to re-plan from it. `Delivered` distinguishes a change from no change. A
  partial burst, acks lost to a slow read, no ack at all, and the app
  disconnecting after taking the input each report as themselves. An adapter
  that sends no acks still behaves exactly as it did before.
- **`read_tree` relays the app's own snapshot line** instead of the bridge's
  re-serialization of its parse of it, so a role, action or field this bridge
  has never heard of arrives under its real name. An app publishing
  `"role":"sparkline"` had the agent read `"role":"other"`, with the real name
  on the wire the whole time. The parse stays authoritative for what the bridge
  decides, since `act` validates node ids and advertised actions against it.
  The ndjson envelope key is stripped, because that is transport framing and
  not part of the tree.
- **A protocol mismatch splits the tool surface by direction** instead of
  swallowing input. `read_tree` keeps working for as long as the peer's
  snapshots still parse, and `act`, `key` and `type_text` refuse up front with
  an error naming both versions, having sent nothing.

### What an app author gains

- `TariaLayer::bind_or_disabled` cannot fail. A socket that will not bind
  yields an inert layer, so taria can never block an app's startup, and the
  layer stays silent about it, because printing once the app owns the alternate
  screen garbles the display. The app reports `bind_error()` where it chooses.
- `publish(nodes)`, `drain` and `drain_with_ids` absorb boilerplate every
  integration was writing by hand, and `ack(id, InputStatus::Ignored)` is how
  an app says it looked at an input and deliberately did nothing.
- `drain_acking` takes the `InputStatus` the handler returns and sends the
  `Ignored` ones itself, so saying "I looked at that and did nothing" needs no
  input id and no enum of the app's own. The demo and the typing tutor each
  wrote the same two-variant enum, and the same loop around `drain_with_ids`
  and `ack`, before it existed. `drain_with_ids` stays for an app that needs
  the id for something else.
- `taria::IdSpace` gives nodes ids that are identities rather than positions,
  which is the difference between an agent acting on the row it read and
  acting on whatever moved into that slot. It sits at the crate root beside
  `Node` and `Role`, as well as in `taria::id`. `new` stays const and
  infallible for the usual case of a prefix declared in a const; `try_new` is
  for one built at runtime.
- Socket failures have names and say what to do about themselves.
  `SocketPathTooLong` carries the path, its length and the platform's limit,
  in place of the bare "path must be shorter than SUN_LEN" that std reports,
  and `InvalidAppLabel` names a label that is not a plain file name.
- Four counters record agent traffic that went nowhere: `dropped_inputs()`,
  `stale_inputs()`, `unknown_inputs()` and `dropped_acks()`. All are monotonic
  across reconnects, all are 0 on a disabled layer, and all are read after the
  terminal is restored, because the layer never prints. `dropped_acks` is the
  one that points at the bridge rather than the app: the input landed and the
  caller was answered nowhere.
- `MAX_NODE_DEPTH` states the depth past which a snapshot exceeds what a JSON
  parser will recurse into and arrives as nothing at all. The adapter cuts
  deeper branches at publish, since a cut tree is the only outcome where the
  agent gets a current one, and reports the cut through `truncated_snapshots()`
  and `last_truncation()` to the only party that can fix the tree.

### The role vocabulary went from 18 to 29

Added: `tree`, `tree_item`, `image`, `chart`, `scrollbar`, `log`, `status`,
`terminal`, `link`, `select` and `option`.

A census of 15 real ratatui apps found 21% of on-screen widgets had no
defensible role, and 8 of ratatui's 15 publishable built-ins had no role at
all. `scrollbar` appeared in 6 of the 15 apps, more often than `tabs`, which
did have one, while `button` appeared zero times: the old vocabulary came from
an HTML forms model rather than from what terminal UIs put on screen. The
census, the ecosystem figures, and the alternative that was rejected (letting
apps name their own roles, which every comparable accessibility vocabulary
tried and abandoned) are in `docs/landscape.md`. Adding roles is additive, so
`PROTOCOL_VERSION` stays 1.

### Fixed

Defects a v0 user would have hit.

- **Node ids shifted on delete.** The demo derived a task's node id from its
  position in a vector, which is the obvious thing to write and what an adopter
  copying it would have written too. Deleting a task renumbered every later
  one, and an agent acting on an id from an earlier `read_tree` hit the wrong
  task. Ids are identities assigned at creation now, reproducible across
  restarts, and `IdSpace` is there so that stays the easy way to write them.
- **An input from a dead connection still reached the app.** The queue outlived
  the connection that filled it, so an agent's `key q` could quit the app after
  the bridge session that sent it was gone, with nothing left to report it to.
  Inputs now carry their connection's generation and are discarded on dequeue
  and counted in `stale_inputs()`, deliberately without an ack, since the peer
  that would read one is the peer that went away.
- **Running two copies hung the first forever at exit.** `Drop` woke its
  listener by connecting to the socket path, but a second instance had already
  unlinked and rebound that path, so the connect reached the second instance's
  listener, looked like success, and the join blocked on a thread parked on a
  socket nothing could reach. The listener polls a shutdown flag now, and
  binding removes only a socket, and only once a connect to it has been
  refused, so a live one is left alone.
- **`type_text` into a focused list triggered keybindings.**
  `type_text("deploy")` on a fresh demo deleted a task and the bridge reported
  success, because with the list focused `d` opened the confirm-delete dialog
  and the later `y` confirmed it. Typed text now reaches an app only where the
  app puts typing, and is acked `Ignored` otherwise. Raw `key` still lowers
  into the bindings on purpose, which is the whole difference between the two
  inputs.

### Deliberately not in this release

Node attributes: `selected` as a fact distinct from `focused`, position in a
list, scroll offset, `expanded` and `level` for trees, and `mode` for modal
editors. The role census found a second group, larger than the one the new
roles fix: 26% of widgets already have a role worth keeping and need only to
say more about themselves. That is not a naming problem, and no role added to
the vocabulary reaches it. Attributes are what it wants. They are additive
under the freeze, so they can land in a later 0.x without moving
`PROTOCOL_VERSION`, and they are held back to be designed rather than because
they were missed.

`mode` is the one that is a safety gap rather than a completeness one. Without
it, an agent typing into a vim-modal editor runs commands instead of typing.

[Unreleased]: https://github.com/y0sif/taria/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/y0sif/taria/releases/tag/v0.3.3
[0.3.2]: https://github.com/y0sif/taria/releases/tag/v0.3.2
[0.3.1]: https://github.com/y0sif/taria/releases/tag/v0.3.1
[0.3.0]: https://github.com/y0sif/taria/releases/tag/v0.3.0
[0.2.1]: https://github.com/y0sif/taria/releases/tag/v0.2.1
[0.2.0]: https://github.com/y0sif/taria/releases/tag/v0.2.0
[0.1.0]: https://github.com/y0sif/taria/releases/tag/v0.1.0
