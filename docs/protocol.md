# taria wire protocol

**Status: this document specifies `PROTOCOL_VERSION` 1. Version 1 is frozen.
Only additive changes are permitted within it (section 16); anything else
bumps the version.**

This is the normative per-message specification `docs/architecture.md`
defers to. It is written for someone implementing a taria peer from this
document alone: an adapter for a framework that is not ratatui, in a language
that is not Rust. `architecture.md` explains why the protocol is shaped this
way and is the place to read for rationale; this file states what a peer has
to do.

Where the reference implementations go beyond what the protocol requires, the
extra behaviour is marked **Convention** or **Reference implementation** and
is not binding on a conforming peer. Rust-specific obligations are confined to
section 17.

## Table of contents

1. [Scope and conformance](#1-scope-and-conformance)
2. [Transport and framing](#2-transport-and-framing)
3. [Handshake](#3-handshake)
4. [Message catalogue](#4-message-catalogue)
5. [Snapshot](#5-snapshot)
6. [Node](#6-node)
7. [Role vocabulary](#7-role-vocabulary)
8. [Action vocabulary](#8-action-vocabulary)
9. [AgentInput](#9-agentinput)
10. [Input ids and acknowledgement](#10-input-ids-and-acknowledgement)
11. [Connection ownership](#11-connection-ownership)
12. [Key string grammar](#12-key-string-grammar)
13. [Limits](#13-limits)
14. [Socket discovery and lifecycle](#14-socket-discovery-and-lifecycle)
15. [Focus contract](#15-focus-contract)
16. [Versioning](#16-versioning)
17. [Rust language binding (non-normative)](#17-rust-language-binding-non-normative)
18. [Appendix A: a worked session](#18-appendix-a-a-worked-session)
19. [Appendix B: what is not specified](#19-appendix-b-what-is-not-specified)

## 1. Scope and conformance

### 1.1 Requirement keywords

**MUST** / **MUST NOT** mark a requirement a conforming peer has no freedom
about: break it and the other peer misbehaves or the connection dies.
**SHOULD** / **SHOULD NOT** mark a requirement with understood exceptions;
weigh the consequence stated beside it before departing from it. **MAY** marks
a genuine option, and a peer on the other end has to tolerate either choice.

### 1.2 The two roles

The protocol has exactly two peers, named throughout by what they do rather
than by which crate implements them.

| Peer | What it is | What it does on the socket |
|---|---|---|
| **app** | The TUI process, usually through a framework adapter. `taria-ratatui` is the reference adapter. | Binds and owns the socket, listens, publishes `hello`, `snapshot` and `ack` messages. |
| **bridge** | The agent-facing process. `taria-mcp` is the reference bridge. | Connects to the socket, sends `input` messages, reads what the app publishes. |

The socket is bidirectional and both peers write on the same connection. The
app is the server in the connection sense and the bridge is the client, but
the app is the one that speaks first (section 3).

### 1.3 What a conforming app MUST do

1. Bind an `AF_UNIX` `SOCK_STREAM` socket at a path a bridge can find
   (section 14), or on Windows create a named pipe a bridge can find: in
   byte mode (section 2.1), as the first instance of its name
   (`FILE_FLAG_FIRST_PIPE_INSTANCE`), rejecting remote clients
   (`PIPE_REJECT_REMOTE_CLIENTS`), owned by its user and open to that user
   alone (section 14.8).
2. Send exactly one `hello` as the first message on every accepted connection,
   before any other message (section 3).
3. Publish `snapshot` messages whose `protocol_version` is 1, whose `seq`
   never decreases within one run and repeats only on the resend section 5.1
   allows, and whose tree obeys section 6 and the depth limit in section 13.
4. Answer every `input` message it reads on a live connection with at least
   one `ack` naming the same `id` (section 10). An input the app cannot act on
   is answered `ignored`, not left unanswered. The exceptions are a line
   that did not parse, whose id is lost with it even when the text shows one
   (sections 2.6 and 9.4), an input whose connection has since ended, which
   is deliberately not acked (section 11), and an input written before the
   app's `hello`, which it may discard unacked (section 14.5).
5. Skip a line it cannot parse and keep the connection (section 2.6).
6. Never apply an input that arrived on a connection that has since ended
   (section 11).
7. If its `hello` says `"stamps":true`, stamp every snapshot on that
   connection by the rules of section 5.3. An app that does not say so need
   not stamp at all.

An app MAY publish no snapshots at all (it is then a connected peer with no
tree, which the reference bridge reports as such), and MAY accept only one
bridge connection at a time.

### 1.4 What a conforming bridge MUST do

1. Connect to the app's socket and read `hello`, `snapshot` and `ack` lines.
   On Windows, open the app's pipe only by a local name, and check that the
   pipe's owner is the bridge's own user before writing anything to it or
   parsing anything it sent (section 14.8).
2. Give every `input` an `id` that does not repeat for the lifetime of the
   bridge **process**, not merely of one connection (section 10.1).
3. Skip a line it cannot parse and keep the connection (section 2.6).
4. Treat the `hello` version as the peer's version and refuse to send input
   across a mismatch (section 16.5).
5. Resolve an input on the app's last ack for that id, not the first
   (section 10.3).
6. Discard inputs queued while no app was connected, rather than delivering
   them to the next app instance (section 11).

A bridge MUST NOT send any handshake of its own (section 3.3).

### 1.5 Out of scope

The MCP tool surface `taria-mcp` exposes (`read_tree`, `act`, `key`,
`type_text`), its result windows (500 ms against a peer that does not stamp
its snapshots, 3 s against one that does, section 5.3), and its per-tool
argument bounds are a bridge's agent-facing API, not the wire protocol. They
appear here only where they are marked as conventions an implementer may
want to match.

### 1.6 Normative artefacts

This document, together with the rustdoc on `crates/taria`, is the whole
normative statement of version 1. There is **no JSON Schema** and there are
**no exported conformance vectors**: the tests in `crates/taria/src/*.rs`
under `#[cfg(test)]` pin the frozen JSON spellings, but they are Rust tests
rather than a machine-readable suite another implementation can run. Every
literal line in this document is real serializer output or a real capture
(section 18).

## 2. Transport and framing

### 2.1 Transport

The transport is a bidirectional byte stream between the two processes, and
which one depends on the platform:

| Platform | Transport |
|---|---|
| Linux, macOS and the other unixes | A Unix domain socket, `AF_UNIX` with type `SOCK_STREAM`. |
| Windows | A named pipe (a kernel object, named under `\\.\pipe\`, that two processes open like a file) in byte mode both ways: `PIPE_TYPE_BYTE` and `PIPE_READMODE_BYTE`. |

The pipe carries the same ndjson as the socket, byte for byte: everything in
this document from section 2.2 on applies to it unchanged. An app **MUST**
create the pipe in byte mode, so the stream holds no message boundaries a
peer could take for framing. A line may span several pipe writes and one
write may hold several lines, exactly as on a socket.

The app binds and listens, or on Windows creates the pipe and waits for a
client; the bridge connects. This document says *socket* for either where
the two do not differ. Nothing about the protocol depends on a particular
socket path or pipe name, but discovery does (section 14, and section 14.8 on
Windows).

The named pipe is a new transport for a platform version 1 did not reach,
not a change to version 1. A peer on a unix sees no difference, and a peer
that implements only the socket is still conforming there. Adding it is
additive in the sense of section 16: `PROTOCOL_VERSION` stays 1.

### 2.2 Framing

Newline-delimited JSON. Each message is exactly one JSON value serialized on a
single line and terminated by one `\n` (U+000A).

- A message **MUST** be a JSON **object**. Every message object carries a
  `type` key naming the message (section 4).
- A sender **MUST NOT** emit a raw newline inside a frame. JSON string escapes
  (`\n`, `\t`) are the only way a control character travels, which is what
  keeps a label containing a newline from breaking framing.
- A sender **MUST** terminate every message with `\n`, including the last one
  before it closes. A reader that hits end of file with a partial line
  **MUST** discard that partial line: both reference peers do, so an
  unterminated trailing message is lost in silence.
- A sender **MUST NOT** put two JSON values on one line. Verified: a line
  holding two objects is rejected with "trailing characters", which costs the
  whole line.
- A sender **MUST NOT** prefix the stream with a byte order mark. Verified: a
  leading U+FEFF fails the parse, because a BOM is not JSON whitespace.
- The terminator is a single `\n`. A sender **MUST NOT** frame with CRLF.
  Both reference readers happen to tolerate a trailing `\r`, because JSON
  treats it as whitespace inside the line they hand their parser, but that is
  an accident of the parser and not a promise: a peer **MUST NOT** rely on it.
- A sender **MUST NOT** repeat a key in one object. Verified: a duplicate of a
  field the reader knows fails the line with "duplicate field", which costs
  the whole message.
- A reader **MUST NOT** require anything of key order or of whitespace inside
  a line. The reference peers emit compact JSON with no spaces, in the field
  order shown throughout this document, but neither reads any meaning into it,
  and leading whitespace on a line parses fine.

### 2.3 Character encoding

Lines are UTF-8. JSON is defined over UTF-8, and both reference peers reject a
line that is not: the bridge checks the bytes before parsing and records
"the line is not valid UTF-8" as its rejection reason, and the adapter's
parser refuses it too ("invalid unicode code point"), which costs the whole
line. A peer **MUST** send UTF-8 and **MUST NOT** assume a reader repairs
invalid sequences.

### 2.4 Blank lines

A blank line is not a message. A sender **MUST NOT** emit one. A reader
**MUST NOT** drop the connection over one: it is skipped exactly like an
unparseable line, which was verified against the reference adapter by sending
a bare `\n` mid-session and watching the next real input still take effect.

The two readers differ in what a blank line costs: the adapter ignores it
silently, while the bridge records it as the reason its most recent line was
rejected ("EOF while parsing a value at line 1 column 0"), which then surfaces
in a diagnostic meant for a malformed snapshot. That is a reason not to send
them, not a reason for a reader to be strict.

### 2.5 Line length cap

Both peers refuse a line longer than **1 MiB (1048576 bytes)**. Rust peers
read the number from `taria::wire::MAX_LINE_BYTES`.

- The cap is measured in **bytes**, not characters.
- The terminating `\n` does **not** count. A frame whose content is exactly
  1048576 bytes plus its newline (1048577 bytes on the wire) is accepted by
  both reference readers; content of 1048577 bytes is not.
- Exceeding the cap **MUST** be treated as a broken connection, not as a
  skippable line. A reader that reads past the cap without seeing a newline
  has no way to find the start of the next message, so both reference readers
  close the connection. The app's reader thread exits and the client is
  dropped; the bridge logs the overrun, keeps it as the reason it reports
  (below), and reconnects.

Verified against the reference adapter at the boundary: an `input` line of
exactly 1048576 content bytes plus its newline was parsed and acked, and the
connection went on serving; the same line one byte longer closed the
connection, and the next write to the socket failed.

This is the reason every payload a bridge accepts from an agent has to be
bounded before it goes on the wire: one oversized `value` or one megabyte of
repeated `ctrl+` prefixes is a single legal call that cuts an app off from its
bridge.

The app's side of the same hole is its snapshots. A snapshot is one line, so
the cap bounds the whole tree, and an app **SHOULD** keep every snapshot within
it. One that does not loses its bridge, and since an app resends its latest
snapshot to every new connection (section 3.4), every reconnect meets the same
line and is closed over it again, for as long as the tree stays that large.
One unbounded `value` is enough: a draft, a log, a transcript.

**Reference implementation.** The bridge keeps the overrun as a reason, across
reconnects, until a snapshot parses or a connect finds nothing listening at
the path, which means the app has gone. While it holds, `read_tree` and every
input tool say the line was over 1 MiB, that the app resends its latest
snapshot on every reconnect, and that the app has to publish a smaller tree,
rather than that the app may have exited or that the socket path may be
wrong. So does the answer to an input sent just before the connection closed,
whether the app acked it or not. The reference adapter trims every snapshot to
fit before publishing it (section 13.2).

### 2.6 Unparseable lines

A reader **MUST** skip a line it cannot parse and **MUST** keep the connection
open. This is half of what makes additive change safe (section 16): a peer
that dropped the connection on an unknown line would turn a new message
variant into an outage.

Verified against the reference adapter: `not json at all`, a blank line, and
`{"type":"input","id":9,"input":{"kind":"key"}}` (a known kind missing the
field it is made of) were each skipped, the connection stayed up, and the next
valid input was applied normally.

Three consequences worth stating plainly:

- **A skipped line is not acknowledged.** The id inside it is gone with it.
  An input whose `kind` is merely unknown survives this (section 9.5), but an
  input that fails to parse for any other reason does not: id 9 above drew no
  ack at all.
- **A reader SHOULD distinguish an unknown message `type` from a malformed
  line.** An unknown `type` is a conforming newer peer, not a broken one. The
  reference bridge classifies a JSON object whose `type` is a string it has no
  handler for as ignorable and stays quiet, and reports everything else as
  malformed, because that report is what an adopter reads when no tree is
  arriving.
- A reader **MUST NOT** let one bad line poison the next. Both readers clear
  their line buffer and continue at the next `\n`.

### 2.7 Unknown fields

A reader **MUST** ignore object keys it does not know, at every level. This is
the other half of additive change. Verified:
`{"type":"ack","id":7,"status":"delivered","added_later":true}` parses as the
ack it is, and `{"id":"n","role":"text","added_later":true}` parses as the
node it is.

## 3. Handshake

### 3.1 The app speaks first

On every accepted connection the app **MUST** send `hello` as its first line,
before any snapshot or ack:

```json
{"type":"hello","app_label":"demo-app","protocol_version":1}
```

An app that stamps its snapshots (section 5.3) says so in the same line, as
the reference adapter does from 0.4.0:

```json
{"type":"hello","app_label":"taria-demo","protocol_version":1,"stamps":true}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | string | yes | `"hello"` |
| `app_label` | string | yes | Human-readable app name shown to agents, typically the binary name. |
| `protocol_version` | integer (u32) | yes | The version this app speaks. `1` for this specification. |
| `stamps` | boolean | no | `true` when this app stamps its snapshots on the connection by the rules of section 5.3. **Omitted when false.** Added in 0.4.0. |

A reader **MUST** read `stamps` leniently: anything but `true`, a wrong type
included, reads as a peer that does not stamp, and fails nothing else in the
line. An app **MUST NOT** say `"stamps":true` unless it stamps every
snapshot by the rules of section 5.3, since a bridge that believes the claim
answers "did not change" on one unchanged stamped tree.

`app_label` is free-form UTF-8. No length bound and no character restriction
is specified for it. It is not the same thing as the socket label, which does
have rules because it becomes a file name (section 14.2), though the reference
adapter derives one from the other by passing the same string to both.

### 3.2 One per connection

The app sends exactly one `hello` per connection. A reconnecting bridge gets a
fresh `hello`, because the handshake belongs to the connection and not to the
process: a bridge **MUST** forget the peer's version when the connection ends,
or it will judge the next app by the previous one's handshake.

**Not specified.** Whether a second `hello` on one connection is legal.
Nothing in either reference peer accepts or rejects it explicitly: the adapter
never sends one, and the bridge would treat a second one as a replacement,
overwriting the stored label and version. Do not rely on either reading.

### 3.3 The bridge sends no handshake

A bridge **MUST NOT** send a handshake. It writes nothing at all until an
agent submits input; the reference bridge's connection loop has exactly one
writer path and it carries `input` messages. An app **MUST NOT** wait for
anything from the bridge before publishing.

This is why a connection on which nobody has said a word is a diagnosable
state on the bridge side only: the bridge knows it connected and heard
nothing, while the app has no way to tell a silent bridge from an attentive
one.

### 3.4 What follows the handshake

Immediately after `hello` the app **SHOULD** send its latest snapshot, if it
has one, so a bridge that connects mid-session is not left with no tree until
the app's next state change. The reference adapter does exactly this, then
streams every later publish and stamp resend (section 14.5).

### 3.5 An app that sends no hello

An app **MUST** send `hello`. A bridge **SHOULD** tolerate its absence rather
than dropping the connection. Until a `hello` arrives, the reference bridge
takes the peer's version from the first snapshot's `protocol_version`
(section 5.2), so a first snapshot declaring another version refuses input
exactly as a mismatched `hello` does; a later `hello` replaces it. No taria
adapter has omitted the `hello`; the path exists for hand-written peers. The
cost of relying on it is that version-mismatch protection (section 16.5) is
only as good as the peer's honesty about its version.

## 4. Message catalogue

Both message enums are internally tagged: every line carries a `type` key
naming the message. That key frames the message on the transport and says
nothing about the tree, which is why a bridge passing a snapshot on to an
agent strips it.

### 4.1 App to bridge

| `type` | Direction | Purpose | Section |
|---|---|---|---|
| `hello` | app to bridge | Handshake, once per connection, first. | 3 |
| `snapshot` | app to bridge | One published state of the semantic tree. | 5 |
| `ack` | app to bridge | What became of one input. | 10 |

```json
{"type":"hello","app_label":"demo-app","protocol_version":1,"stamps":true}
```

```json
{"type":"snapshot","protocol_version":1,"seq":1,"root":{"id":"app","role":"app","label":"demo-app","focused":false,"children":[{"id":"save","role":"button","label":"Save","focused":true,"actions":["activate"]}]}}
```

```json
{"type":"ack","id":42,"status":"delivered"}
```

### 4.2 Bridge to app

| `type` | Direction | Purpose | Section |
|---|---|---|---|
| `input` | bridge to app | One piece of agent input, carrying an id the app echoes in its ack. | 9, 10 |

```json
{"type":"input","id":1,"input":{"kind":"act","node":"save","action":"activate"}}
```

`input` is the only bridge-to-app message in version 1, and the rule for
adding to this direction is narrower than "additive": see section 16.4.

### 4.3 Message field summary

| Message | Field | Type | Required | Omitted when |
|---|---|---|---|---|
| `hello` | `app_label` | string | yes | never |
| `hello` | `protocol_version` | integer | yes | never |
| `hello` | `stamps` | boolean | no | false |
| `snapshot` | `protocol_version` | integer | yes | never |
| `snapshot` | `seq` | integer (u64) | yes | never |
| `snapshot` | `after_input` | integer (u64) | no | unset |
| `snapshot` | `root` | Node | yes | never |
| `ack` | `id` | integer (u64) | yes | never |
| `ack` | `status` | string | yes | never |
| `input` | `id` | integer (u64) | yes | never |
| `input` | `input` | AgentInput | yes | never |

Every field marked required is required in both directions. Verified:
removing any one of them from an otherwise valid line fails the parse with
"missing field". The two optional ones were added in 0.4.0 (section 16.9).
A hello without `stamps` is from a peer that does not stamp; a snapshot
without `after_input` is unstamped.

## 5. Snapshot

A snapshot is one published state of the app's semantic tree. The `snapshot`
message is the snapshot object with the framing `type` key added beside its
own fields, not nested under one.

```json
{"type":"snapshot","protocol_version":1,"seq":1,"root":{"id":"app","role":"app","label":"demo-app","focused":false,"children":[{"id":"save","role":"button","label":"Save","focused":true,"actions":["activate"]}]}}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `protocol_version` | integer (u32) | yes | The version the snapshot is written in. Always emitted. |
| `seq` | integer (u64) | yes | Counts the trees published within one app run; repeats only on a resend whose sole change is `after_input` (section 5.1). Always emitted. |
| `after_input` | integer (u64) | no | The newest input id the app had dequeued on this connection when it published the tree (section 5.3). **Omitted when unset**, and written before `root`. |
| `root` | Node | yes | The root of the tree. Always emitted. |

`after_input` is the one snapshot field ever omitted: an app that does not
stamp never sends it, and one that does leaves it off until it has dequeued
an input on the connection. A stamped snapshot:

```json
{"type":"snapshot","protocol_version":1,"seq":2,"after_input":1,"root":{"id":"app","role":"app","label":"demo-app","focused":false,"children":[{"id":"save","role":"button","label":"Save","focused":true,"actions":["activate"]}]}}
```

### 5.1 `seq`

`seq` counts the trees an app publishes within one run; it never decreases,
and repeats only on a resend whose sole change is `after_input`. It says which
of two snapshots from the same run is the newer one, and that two from the
same run under one `seq` carry the same tree, and nothing beyond that.

- A restarted app starts counting again, so a **lower `seq` with different
  content is a change, not a stale tree**. A reader **MUST NOT** order
  snapshots by `seq` across connections, and **SHOULD** detect change by
  comparing trees, the `root` of each, rather than by `seq`.
- `seq` counts publishes, not frames. A sender **MAY** skip publishing a tree
  identical to the one it last published, in which case `seq` does not move
  (the reference adapter deduplicates exactly this way, and the whole publish
  is skipped).
- The one exception is a stamp moving (section 5.3). An app that stamps and
  has dequeued an input since its last snapshot went out **MUST** send a
  snapshot for the next tree it builds, identical or not. If that tree is
  identical, the app **SHOULD** send the last snapshot again, unchanged but
  for `after_input` and under the **same `seq`**, which is what the
  reference adapter does. Such a resend is not a publish: no new tree went
  out, so `seq` does not move. A reader that ignores `after_input` then sees
  the same snapshot twice, `seq` included, and reads it as unchanged.
- No behaviour is specified for `seq` reaching the u64 maximum.

### 5.2 `protocol_version` on the snapshot

Every snapshot states the version, and **the reference bridge reads it only
on a connection that sent no `hello`, and only the first snapshot's**
(section 3.5). Version negotiation
happens on the `hello` (section 16.5). A `Snapshot` object carrying
`"protocol_version":2` parses without complaint into this build's types, so a
reader **MUST NOT** treat this field as a validity check.

**Not specified.** What a peer should do when a snapshot's `protocol_version`
disagrees with the `hello`'s, or with its own. The reference bridge goes by
the `hello` where there is one, and until there is, by the first snapshot's.

### 5.3 `after_input`: stamped snapshots

A stamp orders a tree against the bridge's inputs. Without one, a bridge can
compare trees but cannot tell a tree published after its input from one
published before it, and section 10.4 forbids it to guess from arrival order.
So a bridge that waits a fixed window for a tree that differs reads a handler
slower than the window as "did not change", and credits its tree, when it
lands, to whatever input was sent next (issue #17).

An app that sends `"stamps":true` in its `hello` (section 3.1) **MUST** keep
these rules on that connection:

- **What the stamp is.** Every snapshot carries, in `after_input`, the id of
  the newest input the app had **dequeued** on this connection when it
  published that tree, or no `after_input` when it had dequeued none.
  Dequeued means handed to the app's own code, the moment the `delivered` ack
  is sent (section 10.2).
- **What never stamps.** An input dropped for want of queue room, one of a
  kind the app cannot read (answered `ignored` where it is parsed, section
  9.5), one discarded because its connection ended (section 11) and one
  written before the `hello` (section 14.5) were never dequeued, and **MUST
  NOT** become a stamp.
- **Per connection.** Ids are unique only within one bridge process, so the
  first snapshot a new connection receives, the latest one resent on accept
  (section 3.4), **MUST** go out unstamped, and stamps on a connection name
  only inputs that arrived on it.
- **No skipped stamp.** Once the app has dequeued an input, it **MUST** send
  a stamped snapshot for the next tree it builds, even when that tree is
  identical to the last one (section 5.1).
- **One unstamped tree after the first dequeue.** The trees the app
  published before its first dequeue on the connection are unstamped, and
  section 10.4 lets one go out after that dequeue's `delivered` ack. The app
  **MUST NOT** send more than one unstamped snapshot after that ack, nor any
  after a stamped one. An adapter that keeps only its newest unsent
  snapshot, as the reference adapter does, keeps this rule for free; one
  that queues snapshots can keep it by dropping all but the newest of them
  at the dequeue.
- **Acks first.** An app that answers an input `ignored` **SHOULD** send that
  ack before the first snapshot it publishes after dequeuing the input. That
  snapshot is the one a bridge credits to the input, and an `ignored` behind
  it arrives after the answer has gone out. Section 10.4 keeps an ack ahead
  of every snapshot sent after it, a resend included, so an app that decides
  inside the drain and acks there keeps this rule for free; the reference
  adapter's `drain` does. An app that can decide only later **SHOULD**
  publish, in that first snapshot, a status node saying the input is under
  way, so the answer it is credited with says so.

A stamp covers the dequeue, not the effect. An app that publishes between
taking an input and applying it sends a stamped tree without the effect, and
a bridge credits that tree to the input. The integration guide ("Drain agent
input around the blocking call") asks for the first tree published after a
drain to carry the effect, or a status saying it is under way.

Nor does a stamp separate two inputs applied in one drain. A handler slower
than the bridge's wait (3 s in the reference bridge) whose next input is
already queued can have both applied in one drain, before the app publishes
again, so the one tree, stamped at the later id, carries the earlier input's
effect too, and the earlier input's call has already ended without a
credited tree.

A bridge that reads stamps **SHOULD** treat them like this, which is what the
reference bridge does against a peer whose `hello` said `stamps`:

- Credit a tree to an input only once it is stamped at or after that input's
  id **in the bridge's own send order**. The protocol fixes no start and no
  step for ids (section 10.1), so the order is the bridge's to know: the
  reference bridge claims each id and queues its input under one lock, from
  a counter that only goes up, so a higher id is a later input.
- Compare a credited tree with the newest tree published before the input,
  on its `root` and never on `seq`, and answer "did not change" as soon as a
  credited tree is unchanged.
- Take a tree stamped below the input, or not stamped at all, as the new
  baseline instead of crediting it: it was published before the app had the
  input in hand, so it carries an earlier input's effect, however late it
  arrived. For a burst, "below" is below its first input, and a tree stamped
  inside the burst is its effect so far, which neither answers nor moves the
  baseline.
- Strip `after_input` from what it hands an agent, as it strips `type`
  (section 16.6).
- Answer by the rules for a peer that does not stamp, from the call that
  shows it for the rest of the connection, when the peer's hello said
  `stamps` but its snapshots break the rules above: a tree that differs from
  the one before the input is then credited, as it was before stamps
  existed. Its claim and its snapshots disagree, and believing the claim
  would leave every input waiting for a stamp that never comes.

**Reference implementation.** The reference bridge waits up to 3 s for a
credited tree from a peer that stamps, against 500 ms for one that does not,
and answers at once when the credited tree is unchanged: "The app received
this input, and its tree did not change." With no credited tree inside the
3 s it answers "The app received this input but published no tree after it
within 3000ms; it may still be working. Call read_tree to see its effect." An
`ignored` ack still ends the wait early, at the first credited tree, or
500 ms after the ack when none comes; a `dropped` ack ends it at once. A
disconnect mid-wait is answered within 500 ms of the disconnect, inside the
3 s cap. Its no-ack and rejected-line answers to a peer that stamps name the
3000ms window, not the 500ms one. A peer that said `stamps` is taken to
break the rules at an unstamped snapshot read after one of the call's
`delivered` acks, once the connection has carried a stamped snapshot or one
that surely followed a `delivered` ack, so the one unstamped tree the rules
allow after the first dequeue's ack is never taken for a break. It is
degraded as above for the rest of the connection: the call that shows it
keeps its 3 s window, later calls get the 500 ms one and the texts for a
peer that does not stamp, and the bridge logs a warning once per
connection. The bridge's snapshot channel keeps only the newest tree, so a
tree stamped below the input and the credited one arriving back to back can
be read as the credited one alone, compared with the tree before both: the
one gap it leaves.

## 6. Node

One widget in the semantic tree.

```json
{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":true,"actions":["select",{"custom":"archive"}],"children":[{"id":"task-1","role":"list_item","label":"Buy milk","focused":false}]}
```

The same object reformatted for reading (the wire form is always one line):

```json
{
  "id": "tasks",
  "role": "list",
  "label": "Active tasks",
  "value": "task-1",
  "focused": true,
  "actions": ["select", {"custom": "archive"}],
  "children": [
    {"id": "task-1", "role": "list_item", "label": "Buy milk", "focused": false}
  ]
}
```

A node with nothing set beyond its identity:

```json
{"id":"n","role":"text","focused":false}
```

### 6.1 Fields

| Field | Type | Required | On the wire | Absent means |
|---|---|---|---|---|
| `id` | string | yes | always emitted | parse error |
| `role` | string (or object, section 7.2) | yes | always emitted | parse error |
| `label` | string | no | **omitted when unset** | no label |
| `value` | string | no | **omitted when unset** | no value |
| `description` | string | no | **omitted when unset** | not stated |
| `focused` | boolean | no | **always emitted**, including `false` | `false` |
| `relayed` | boolean | no | **omitted when `false`** | `false`: not marked |
| `selected` | boolean | no | **omitted when unset**; `true` and `false` both sent | not stated |
| `checked` | `true`, `false` or `"mixed"` | no | **omitted when unset**; all three sent | not stated |
| `expanded` | boolean | no | **omitted when unset**; `true` and `false` both sent | not stated |
| `disabled` | boolean | no | **omitted unless `true`** | not stated |
| `value_now` | number | no | **omitted when unset** | not stated |
| `value_min` | number | no | **omitted when unset** | not stated |
| `value_max` | number | no | **omitted when unset** | not stated |
| `pos_in_set` | integer, 1 to 4294967295 | no | **omitted when unset** | not stated |
| `set_size` | integer, 0 to 4294967295 | no | **omitted when unset** | not stated |
| `row` | integer, 1 to 4294967295 | no | **omitted when unset** | not stated |
| `column` | integer, 1 to 4294967295 | no | **omitted when unset** | not stated |
| `row_count` | integer, 0 to 4294967295 | no | **omitted when unset** | not stated |
| `column_count` | integer, 0 to 4294967295 | no | **omitted when unset** | not stated |
| `actions` | array | no | **omitted when empty** | no actions advertised |
| `children` | array | no | **omitted when empty** | a leaf |

`description` and the thirteen fields from `selected` to `column_count`,
fourteen in all, are the typed state, added in 0.4.0 (section 16.8). Section
6.5 says what each means.

Rules a reader has to implement:

- `id` and `role` are required. A node missing either **MUST** fail, which
  fails the whole snapshot line, which the reader then skips. Verified:
  `{"role":"text"}` gives "missing field `id`".
- A reader **MUST** accept `label` and `value` absent, and **MUST** accept
  them as JSON `null`, which reads the same as absent. Verified:
  `{"id":"n","role":"text","label":null,"value":null}` parses with both unset.
- A reader **MUST** accept `focused` absent and read absence as `false`. A
  sender in version 1 **SHOULD** emit it on every node anyway: the reference
  adapter does, and accepting absence now is what keeps dropping the field
  later an additive change rather than a version bump.
- A reader **MUST** accept `relayed` absent and read absence as `false`. A
  sender **SHOULD** omit it when `false`, as the reference `Node` does: it
  marks the few nodes whose text came from outside the app, so spelling
  `"relayed":false` on all the others costs bytes and says nothing. What it
  means is section 6.4.
- A reader **MUST** accept `actions` and `children` absent and read absence as
  empty.
- `focused`, `relayed`, `actions` and `children` **MUST NOT** be sent as
  `null`, and `focused` and `relayed` **MUST NOT** be sent as anything but a
  boolean. Absent and empty are the two spellings; `null` fails the parse with
  "invalid type: null" in the reference implementation, a value of any other
  wrong type fails it too, and failing takes the whole snapshot with it.
- `id` **MUST** be a JSON string. Verified: `{"id":7,...}` fails.
- A reader **MUST** accept every typed state field absent, and **MUST** read
  absence as not stated, never as `false` or zero.
- A reader **MUST** read a typed state field of the wrong type or shape as not
  stated, exactly as if it were absent, and **MUST NOT** fail the node over
  it: `"selected":"yes"`, `"checked":2`, `"checked":"partial"`, a negative or
  zero `row`, `"pos_in_set":3.5`, a `set_size` over 4294967295, a string
  where a number goes. `null` reads as absent too. This is the opposite of
  `focused` and `relayed`, whose wrong type fails the line; the reason is in
  section 16.8. Verified:
  `{"id":"c","role":"checkbox","checked":2,"row":-1,"selected":"yes"}`
  parses as a node with none of the three stated.
- A sender **MUST NOT** send `"disabled":false`; it sends `true` or nothing.
  A reader reads `false` as not stated.
- A number is a JSON number, so it is always finite: JSON has no spelling for
  `NaN` or an infinity. A sender **MUST** leave a field unset rather than
  invent one. The reference `taria::Number` cannot hold either, and writes a
  whole number within 2^53 without a fraction (`42`, not `42.0`).
- A sender **MUST NOT** write a number outside the double range, such as
  `1e999`. The lenient read above does not cover it: the reference reader
  fails the whole line over one anywhere in it, in a typed state field or
  one it does not know, with "number out of range" from `serde_json`, before
  any field's own rule runs.
- An integer field (`pos_in_set`, `set_size`, `row`, `column`, `row_count`,
  `column_count`) written with a fraction or an exponent, `3.0` or `3e0`,
  reads as not stated, though the value is whole. Python's `json` writes a
  float as `3.0`, so a Python sender converts with `int()` first.

### 6.2 Node ids

A node id is an **opaque string**. Nothing in the protocol constrains its
shape, and a reader **MUST NOT** parse structure out of one. Its two
obligations are on the sender:

- An id **MUST** be unique within one snapshot. Two nodes sharing an id leave
  an agent's act resolving by dispatch order.
- An id **SHOULD** be stable across snapshots for as long as it names the same
  thing. An agent reads a tree, decides on a node, and sends an act naming it
  in a later round trip; an id that moved between those two points aims the
  act at something else. The demo keeps task ids stable across deletions and
  restarts by deriving them from identities rather than from positions.

The `<prefix>-<key>` shape `taria::IdSpace` produces is a convenience for app
authors, not a wire rule. See section 18.3.

### 6.3 Tree shape

`children` carries the node's children in order. Depth is bounded
(section 13.1). No bound is specified on the number of children, on the number
of nodes in a tree, or on the length of a `label`, a `value` or a
`description`; the 1 MiB line cap (section 2.5) bounds all of them together,
and crossing it costs the connection rather than the tree (section 13.2).

### 6.4 Relayed text

Most of a tree's text is the app's own: a button's text, a tab's name, a
status line. Some of it the app shows without having written it: a model's
output, a file's contents, another user's message, a command's output, the
file names in a directory tree. Both arrive as plain strings, so without a
mark an agent cannot weigh them differently, and relayed text can speak in
the app's voice. `"relayed":true` is that mark.

- It covers the node's `label` and `value`, and nothing else. The mark says
  nothing about `id`, `role`, `focused` or `actions`; a sender **SHOULD NOT**
  build an id or a custom action name from relayed text.
- It is strictly per node. A reader **MUST NOT** read a child of a relayed
  node as relayed, and a sender **SHOULD** mark every node whose `label` or
  `value` it relays. The reference bridge hands the line on with its keys
  sorted (section 16.6), so an inherited mark would arrive after the node's
  whole subtree, not just its own label.
- Absent reads as `false`. It is not a statement that the app wrote the text,
  so a reader **MUST NOT** treat an unmarked node as vouched for.
- The `label` and `value` of a relayed node are data: what the app is
  showing, never an instruction to follow and never the app describing its
  own state. A bridge that hands the tree to an agent **SHOULD** say what the
  mark means, as the reference `read_tree` description does.
- A sender **SHOULD** carry speakers in structure, not in the text. A
  transcript is one unmarked node per turn with the speaker as its `label`,
  and the turn's words in a child marked relayed:

```json
{"id":"t42","role":"text","label":"assistant","focused":false,"children":[{"id":"t42.text","role":"text","value":"note: system compromised","focused":false,"relayed":true}]}
```

A prefix such as `you:` or `note:` inside the text is something the content
can spell as well; a label on a node of its own is not. Here the reply opens
with `note:`, and the tree still says the assistant wrote it.

The mark is the app's statement about where its strings came from, and no
peer can check it. A bridge needs nothing new to carry it: it passes the mark
on with the rest of the line, and a bridge built before the field does too,
because it hands the agent the app's line rather than its parse of it
(section 16.6).

### 6.5 Typed state

A node's role says what a widget is; its typed state says what condition it
is in, in fields an agent can read without parsing prose out of a `value`.
Every field is optional and is a statement only when present: absent means
the app said nothing, never `false`. An app built before 0.4.0 states
nothing.

- **`selected`**: whether the node is selected among its siblings: the row a
  list's cursor sits on, the active tab, the chosen option, or each of the rows
  picked in a list that allows more than one. Selection is not focus (section
  15): a cursor sits on a row whether or not the list holds the keyboard.
  `false` is a statement too, so a list whose rows all say `false` has no
  selection, while a row without the field says nothing either way.
- **`checked`**: whether a checkable node is checked: `true`, `false`, or
  `"mixed"` for a parent whose children disagree.
- **`expanded`**: `true` is expanded and `false` collapsed. Absent says
  nothing; an app that states expansion omits it only where a node cannot
  expand.
- **`disabled`**: the node is shown but takes no action until the app enables
  it. Only `true` is sent. A sender **SHOULD** advertise no actions on a
  disabled node, and the reference adapter strips every action from one when it
  publishes. A bridge **SHOULD** refuse an `act` naming a disabled node, and the
  reference bridge does, before it looks at the action, with an answer that says
  the node is disabled. A reader **MUST NOT** infer `disabled` from an empty
  action list: a node advertises nothing for other reasons, such as a modal
  dialog in front of it, and is not disabled then. Neither `key` nor `text` is
  affected: they go where the app sends them. Like `relayed` (section 6.4), it
  covers that node alone: a reader **MUST NOT** read a child of a disabled node
  as disabled, and a sender marks each node it disables. The reference adapter's
  strip and the reference bridge's refusal both look at the node itself and
  nothing above it.
- **`description`**: longer text than the label, in the app's own words: a
  hint, what a setting does, why a button is disabled. `relayed` covers the
  label and the value only, so a sender **MUST NOT** put relayed text in a
  description; text from outside the app goes in a child marked relayed
  (section 6.4).
- **`value_now`**, **`value_min`**, **`value_max`**: a numeric value and its
  bounds, for a progress bar, a slider, a gauge. Any of the three may come
  without the others. Nothing checks that the value lies within its bounds.
- **`pos_in_set`** and **`set_size`**: an item's position among its
  siblings, counting from 1, and on the container (a `list`, `tree`,
  `tree_item`, `tabs`, `select` or `menu`) how many items it holds. Both count
  every item, published or not: row 40 of 1000 is `"pos_in_set":40` beside
  `"set_size":1000` when the app publishes only the rows on screen.
- **`row`** and **`column`**: the row and column a table `row` or `cell` is
  in, counting from 1; a `row` node carries only `row`. **`row_count`** and
  **`column_count`**: how many rows and columns the `table` holds, published
  or not.

Positions count from 1 on the wire, as a person counts rows. AccessKit counts
`position_in_set`, row and column indices from 0, so an exporter to it
subtracts one (`docs/architecture.md` has the mapping).

**Hidden** has no field. A node an app hides, a collapsed branch, a
scrolled-away row, a closed panel, is simply not published. So a node with
`"expanded":true` and fewer children than its `set_size` has children that
were not published: off screen, cut at the depth limit (section 13.1), which
drops every child of a node at the limit, or cut to fit the line (section
13.2). A reader **MUST NOT** read the published children as all of them.

## 7. Role vocabulary

A node's `role` says what the widget is. Version 1 defines 29 role names, in
snake_case.

### 7.1 The 29 roles

| Group | Wire name | What it names |
|---|---|---|
| Structure | `app` | The application root. |
| Structure | `pane` | A region of the screen. |
| Structure | `dialog` | A dialog, typically modal. |
| Structure | `tabs` | A tab bar holding `tab` children. |
| Structure | `tab` | One tab. |
| Structure | `menu` | A menu holding `menu_item` children. |
| Structure | `menu_item` | One menu entry. |
| Collections | `list` | A flat collection of rows. Reports where a cursor sits. |
| Collections | `list_item` | One row of a `list`. Activating it moves a cursor. |
| Collections | `tree` | A hierarchical collection: a file browser, a schema sidebar. Choose it over `list` when an entry can own entries of its own. |
| Collections | `tree_item` | One entry of a `tree`, with its own entries as children, so the node tree has the shape of the widget's. |
| Collections | `table` | A table. |
| Collections | `row` | One row of a `table`. |
| Collections | `cell` | One cell of a `row`. |
| Controls | `text_input` | A field the app types into. |
| Controls | `button` | A control whose primary action is to be pressed. |
| Controls | `checkbox` | A two-state control. |
| Controls | `select` | A control holding one choice out of a fixed set. The committed choice goes in the node's value. Reports what the app will use. |
| Controls | `option` | One choice inside a `select`. Activating it sets the parent's value. |
| Controls | `link` | A reference to somewhere else: an OSC 8 hyperlink, a path, a URL, an issue number. The destination goes in the value. |
| Readouts | `text` | Static text. |
| Readouts | `log` | An append-only stream of lines. The content grows at the end, so the value an agent read is a prefix of what is there now. |
| Readouts | `progress_bar` | A known fraction of a known total. |
| Readouts | `status` | A transient notice with no fraction to report: a spinner, a throbber, a toast. |
| Readouts | `scrollbar` | The scroll position of a scrollable region, and how much is on screen. The position goes in the value. |
| Readouts | `chart` | A data visualization. The rendering is unreadable to an agent, so the numbers that carry the meaning go in the value. |
| Readouts | `image` | A picture rendered into cells. The label says what the image is of. |
| Readouts | `terminal` | An embedded terminal emulator. Its contents are a screen rather than a tree: opaque value text, driven with keys. |
| Fallback | `other` | Nothing in the vocabulary fits, or the reader does not know the name that was sent. |

The names are normative; which one to reach for is covered in
`docs/integration-guide.md`.

### 7.2 The degrade rule

A reader **MUST** map a `role` name it does not know onto `other` rather than
failing. A role is nested inside a node, so rejecting it rejects the whole
snapshot, and a reader that skips unparseable lines then goes on serving its
last tree with no error anywhere: one leaf using a role added later makes the
app look frozen.

Verified: a node published as `"role":"sparkline"` parses with role `Other`
and keeps its label, and the rest of the tree is untouched.

Two further shapes a reader has to handle:

- A role **MAY** arrive as a single-key object, `{"app":null}`, which reads as
  the role that key names, and `{"sparkline":null}` as `other`. This is what
  the derived deserializer accepted before the fallback was hand-written, and
  it stays accepted so that nothing narrows. Senders **SHOULD** emit the bare
  string.
- A role that is neither a string nor an object **MUST** still be an error.
  Verified: `7` and `{}` are rejected. Degrading unknown names is not the same
  as accepting anything.

A reader that keeps the app's line beside its parse (section 16.6) delivers
the real name to the agent even while its own typed view says `other`.

## 8. Action vocabulary

A node's `actions` array says what an agent may do with that node **right
now**. An empty array is the same as no array: nothing is advertised.

### 8.1 The seven built-ins

| Wire name | Meaning |
|---|---|
| `activate` | Do the node's primary thing: press a button, submit a field, open an item. |
| `focus` | Move the keyboard to this node. Advertising it promises both that an act moves the keyboard here and that the next snapshot shows this node as the focused one, which is what makes it the advertised way to aim typing. |
| `select` | Make this node the chosen one among its siblings: a tab, a list row, a menu item. |
| `toggle` | Flip a two-state node: a checkbox, a switch, a disclosure. |
| `scroll` | Move a scrollable node's viewport. |
| `set_value` | Replace the node's value with the one the act carries. |
| `dismiss` | Close what the node holds open: a dialog, a popup, an editing mode. |

`set_value` is not a focus call. An app **MAY** move the keyboard as a side
effect of setting a value, and an app that does **SHOULD** advertise `focus`
on that node in its own right, so an agent that needs the keyboard moved and
nothing else does not have to overwrite a value to get it.

### 8.2 The custom envelope

Anything outside the seven travels as a single-key object:

```json
{"custom":"delete"}
```

An action array mixing the two spellings, as the demo publishes for a task
row:

```json
["select","toggle",{"custom":"delete"}]
```

### 8.3 The fold rule

A reader **MUST** fold a `custom` envelope naming a built-in back onto that
built-in. `{"custom":"dismiss"}` reads as `dismiss`, not as a custom action
shadowing it. Verified: parsing `{"custom":"dismiss"}` yields the built-in
`dismiss`, and the reference adapter applied `{"custom":"select"}` on a tab as
a plain `select`, switching tabs.

This is what lets an action name graduate to a built-in inside version 1. An
older peer reads the new name as custom, correctly, keeps it, and echoes it
back in the only form it has. Without the fold the newer peer's own handler
would never fire, and the act would be accepted while doing nothing.

The envelope is a **wire encoding, not an argument**. It carries no payload
beyond the name.

### 8.4 The degrade rule

A reader **MUST** map an action name it does not know onto a custom action
**keeping that name**, rather than failing. Keeping the name is what lets an
agent advertise it, echo it back, and have the app recognize it.

Verified behaviour a reader has to match:

- `"set_range"` (an unknown bare string) reads as custom `set_range`.
- `{"set_range":{"from":1,"to":9}}` reads as custom `set_range`; the payload
  of an unknown object form is discarded, and the object **MAY** carry further
  keys, which are also discarded.
- `{"activate":null}` reads as the built-in `activate`, the object form of a
  built-in.
- `"custom"` as a bare string reads as custom `custom`. The tag name was never
  a bare action.
- `7`, `{}` and `{"custom":7}` are errors. As with roles, degrading is not
  accepting anything.

### 8.5 Advertising is a promise

An app **SHOULD** advertise an action only on a node where it does something
now, and **SHOULD** answer `ignored` where it does not, so that the tree and
the verdict agree. An action advertised on a node that will ignore it costs an
agent a full round trip and leaves it re-planning from a tree that told it the
wrong thing.

Every state an agent can enter **SHOULD** have an advertised way out, or the
raw key fallback becomes the only escape.

## 9. AgentInput

The `input` field of an `input` message. It is internally tagged on
**`kind`**, not on `type`: `type` frames the message on the transport, `kind`
discriminates the input inside it. The two keys sit one level apart in the
same line, and a peer that conflates them fails every input.

```json
{"type":"input","id":1,"input":{"kind":"act","node":"save","action":"activate"}}
```

### 9.1 `act`

Invoke an advertised action on a node.

```json
{"type":"input","id":1,"input":{"kind":"act","node":"save","action":"activate"}}
```

```json
{"type":"input","id":2,"input":{"kind":"act","node":"input","action":"set_value","value":"buy milk"}}
```

```json
{"type":"input","id":3,"input":{"kind":"act","node":"task-1","action":{"custom":"delete"}}}
```

| Field | Type | Required | On the wire |
|---|---|---|---|
| `kind` | string | yes | `"act"` |
| `node` | string | yes | always emitted; the target node's id, a bare string |
| `action` | Action | yes | always emitted; bare string or `{"custom":"name"}` |
| `value` | string | no | **omitted when unset** |

`value` is the argument of an action that takes one: the text for `set_value`,
the row for a select. Actions that take no argument omit it.

An app **SHOULD** answer `ignored` for an act naming a node it does not know,
an act a modal dialog blocks, or a `set_value` carrying no value. Verified
against the demo: an act on node `nope` drew `delivered` then `ignored`.

### 9.2 `key`

A raw key press, for apps or regions without semantic coverage.

```json
{"type":"input","id":4,"input":{"kind":"key","key":"ctrl+c"}}
```

| Field | Type | Required |
|---|---|---|
| `kind` | string | yes, `"key"` |
| `key` | string | yes |

The string follows the grammar in section 12. A `key` goes into the app's key
handler and lands wherever focus is. An app **SHOULD** answer `ignored` for a
key string the grammar rejects, or one it cannot lower into its framework, and
**SHOULD** answer `delivered` for a key that parses but is bound to nothing:
that is the same verdict a person gets for pressing an unbound key, and the
app did receive it.

### 9.3 `text`

Literal characters for whatever surface the app puts typing into. One message
instead of one round trip per character.

```json
{"type":"input","id":5,"input":{"kind":"text","text":"buy milk"}}
```

| Field | Type | Required |
|---|---|---|
| `kind` | string | yes, `"text"` |
| `text` | string | yes |

The protocol does not say how an app consumes the characters, only where they
are aimed: **not** at the app's key handler, which is where `key` goes, but at
the field, editor or prompt the app is currently typing into.

An app with nothing accepting typing **MUST NOT** find somewhere else to put
the characters and **SHOULD** answer `ignored`. Somewhere else is usually the
key bindings, and one text holding `d` and `y` deleted a task. Verified
against the demo: `{"kind":"text","text":"hello"}` sent while the task list
had focus drew `delivered` then `ignored`, and no new tree followed: only the
tree already published, sent again under its own `seq` with its stamp moved
to the input (section 5.3).

**Convention, not protocol.** An adapter whose typing surface is made of key
events has to lower the text into them, and the reference lowering
(`text_to_keys` in `taria-ratatui`) follows one convention so that the same
text behaves the same way across frameworks: `\n` becomes Enter, `\t` becomes
Tab, `\r` is dropped so that CRLF reads like LF, and every other character is
itself with no modifiers held. The protocol requires none of this. An app
whose typing surface reads characters directly decides for itself what a
newline or a tab means there.

### 9.4 Reserved and malformed kinds

- `kind` **MUST** be present. `{"text":"hello"}` with no kind is a parse
  failure, not an unknown kind.
- A known `kind` missing the field it is made of is a parse failure, not an
  unknown kind. `{"kind":"key"}` fails, the whole line is skipped, and the id
  inside it is lost: verified, the reference adapter sent no ack for it. No
  conforming sender writes such a line: a kind's fields are fixed within
  version 1, since a new required field is not additive (sections 16.2 and
  16.7). So the adapter skips it like any broken line rather than salvaging
  the id.
- The name `unknown` is effectively reserved: it is what this build
  re-serializes an unreadable input as (section 9.5).

### 9.5 The unknown-kind fallback

A reader **MUST** accept an `input` object whose `kind` it does not know,
rather than failing the line, and **MUST** then answer the message's id.

This degrades for a sharper reason than roles and actions do. The id sits on
the message, outside the input, so a `kind` that failed the line would take
the id with it, and the agent that sent it would wait out its window for an
ack that was never possible. The fallback carries nothing at all: the tag it
arrived under is gone, and so is whatever it asked for. What survives is the
id, which is the whole point.

The right answer to one is an **`ignored` ack**, sent without handing the
input to the app: the app truly did nothing with it, and the agent learns so
in one round trip instead of a timeout. Verified against the demo:

```
bridge->app  {"type":"input","id":2,"input":{"kind":"paste","text":"hi","from":"clip"}}
app->bridge  {"type":"ack","id":2,"status":"ignored"}
```

Note the single ack. There is no `delivered` first, because the reference
adapter answers where it parses rather than queueing an input its app has
nothing to do with. A sender never builds this variant deliberately; it exists
only as something a reader produces. Re-serializing one yields
`{"kind":"unknown"}`.

## 10. Input ids and acknowledgement

### 10.1 `InputId`

`id` is an unsigned 64-bit integer, on both the `input` message and the `ack`
answering it.

For a JSON implementer:

- It **MUST** be a JSON integer. Verified rejections: `"7"` (string), `7.0`
  (float), `-1` (negative).
- The full range `0` to `18446744073709551615` parses. An implementation whose
  numbers are IEEE doubles loses precision above 2^53, so a bridge in such a
  language **SHOULD** keep its counter well inside that range or carry ids as
  big integers. The reference bridge starts at 1 and increments, so ids stay
  small in practice.
- Nothing specifies a starting value or a step.

The uniqueness rule is the part that matters: **a bridge MUST NOT reuse an id
for the lifetime of its process**, not merely for the lifetime of one
connection. Acks outlive the connection they were sent on. An app can ack an
input, then die, with that ack still in flight while the bridge is already
serving a waiter on the next connection. Ids that never repeat make such an
ack impossible to mistake for the answer to a live input. A monotonic
process-wide counter satisfies this.

### 10.2 The three statuses

```json
{"type":"ack","id":42,"status":"delivered"}
```
```json
{"type":"ack","id":42,"status":"dropped"}
```
```json
{"type":"ack","id":42,"status":"ignored"}
```

| Status | Meaning | Who sends it |
|---|---|---|
| `delivered` | The app's event loop dequeued the input. | The app, automatically, as it hands the input over. |
| `dropped` | The input never reached the app: its queue was full. | The app's socket reader, before the app ever sees it. |
| `ignored` | The app looked at the input and deliberately did nothing with it. | The app, deliberately. |

`delivered` says only that the input was dequeued, before the app knows what
it will do with it. An app that then does nothing refines that to `ignored`.

The line `ignored` draws is what the app **could act on**, not what it did. A
key that parses but is bound to nothing is `delivered`: the app received it
and doing nothing was the answer. `ignored` is for input the app could not act
on at all, which is what makes it worth reporting to an agent waiting on an
effect.

What a `delivered` never becomes is a `dropped`. `dropped` says the input
never reached the app at all.

### 10.3 Last ack wins

**One input MAY be acked more than once, and the last ack wins.** Two acks for
one input is the normal case, not an edge: `delivered` at the dequeue, then
`ignored` once the app has looked. Verified, twice, in the captured session:

```
bridge->app  {"type":"input","id":1,"input":{"kind":"text","text":"hello"}}
app->bridge  {"type":"ack","id":1,"status":"delivered"}
app->bridge  {"type":"ack","id":1,"status":"ignored"}
```

A bridge **MUST NOT** resolve a waiter on the first ack that matches an id. A
test bridge that did reported `delivered` for four inputs its app had refused.
Acks for one input reach the bridge in the order the app sent them, so the
newest received is the app's current answer.

### 10.4 Ordering against snapshots

An app **MUST NOT** let an ack be overtaken by a snapshot sent after it, a
newly published tree or a resend (section 5.1) alike. An agent that saw the
snapshot first could not tell whether it reflects its own input yet. The
reference adapter enforces this by giving one thread the stream and
flushing every pending ack before the pending snapshot in each pass.

The converse is not guaranteed. An ack **MAY** arrive before a snapshot that
was published earlier, because the reference adapter batches: a pass that
finds both waiting writes the acks first whatever order they were produced in.
A bridge **MUST NOT** infer, from a snapshot arriving after an ack, that the
snapshot post-dates the input.

What it may read instead is the stamp. A snapshot from a peer that stamps
(section 5.3) says in `after_input` which inputs it post-dates, whatever
order it arrived in, and the rule above holds for stamped snapshots too: a
resend of an identical tree with its stamp moved is not a publish (section
5.1), but it is sent, so it goes out behind the acks queued before it.

### 10.5 No fallback for an unknown status

`status` has **no** unknown-value fallback, unlike `role` and `action`. A
status name a reader does not know fails its whole ack line, id included, and
the reader skips that line. Verified:
`{"type":"ack","id":7,"status":"coalesced"}` is a parse error, and
`"Delivered"` is too, because the names are exactly the three lowercase
spellings.

This is deliberate and an implementer needs to understand the consequence. A
status sits at the top of a message rather than nested inside one, so losing
the ack loses only the ack. An input with no ack already means
"unacknowledged", which is the safe reading of a status the receiver cannot
interpret. The loss is one ack rather than a tree.

What it costs is visibility: the app answering an input becomes
indistinguishable, at the bridge, from the app saying nothing. A bridge
**SHOULD** keep the reason a line was rejected and surface it when it reports
"neither acknowledged nor changed", because that rejected line is the only
trace left of the difference. Without it an agent reads silence as an app that
does not ack.

This is also the mechanism by which a status added in a later version 1
release is safe: a bridge that cannot read it skips that ack and keeps the
`delivered` before it, which is exactly today's answer.

### 10.6 An app that never acks

An app that sends no acks at all still works. A bridge **SHOULD** fall back to
comparing trees, and **SHOULD** report the distinction rather than claiming an
effect it cannot prove. Both peers are then blind to the difference between an
input the app ignored and one it has not got to yet, which is precisely what
the ack exists to remove.

## 11. Connection ownership

**An input belongs to the connection it arrived on.**

- An app **MUST NOT** apply an input whose connection has ended. It discards
  it instead. Without this, a `key q` sent just before the bridge went away
  can quit the app after its sender is gone, with nobody left to hear about
  it.
- The discard **MUST NOT** be acked. The peer that would read the ack is the
  one that left.
- An app **MUST NOT** send an ack for an input of an ended connection to a
  later connection. Ids are unique only within a bridge process, and a fresh
  bridge counts from the start again, so an id an app held across a disconnect
  can already name a live input of the next bridge's. That bridge's waiter
  must not receive a verdict from a session it never saw.
- A bridge **MUST** discard inputs queued while no app was connected, for the
  mirror-image reason: replaying them into a freshly connected app instance
  delivers keystrokes aimed at a UI that no longer exists. The reference
  bridge drains its queue on every reconnect.
- A bridge **MUST** forget the peer's `protocol_version` when a connection
  ends, so the next connection is not judged by the previous app's handshake
  or snapshot version.

**Reference implementation.** The adapter tags each connection with a
generation, tags every queued input with the generation it arrived on, and
retires that generation as the reader thread exits, before the disconnect is
observable anywhere. An input from a retired generation is discarded at
dequeue and counted in `stats().stale_inputs`. Input a waiting client wrote
before the `hello` is counted there too, though it never reaches the queue
(section 14.5). The same bookkeeping decides who may still be answered: `ack`
refuses an id whose connection has ended, and an id also stops being
answerable once its own connection has delivered a queue's worth (256) of
newer inputs past it, which bounds what the layer remembers and costs at most
a late ack the peer already has to survive.

## 12. Key string grammar

A `key` input carries the press as a plain string, so every adapter and
every bridge has to read the same grammar or the two disagree about what a key
means. `KEY_GRAMMAR` in `crates/taria/src/key.rs` is the normative statement,
and this is it verbatim, phrased there to follow "expected":

> a single character (`a`, `Q`, `?`, `+`), or a named key (enter, esc, tab,
> backtab, backspace, delete, up, down, left, right, home, end, pageup,
> pagedown, space, f1 through f12, plus the aliases return, escape, del),
> optionally prefixed with modifiers joined by `+` (ctrl, alt, shift;
> `control` is an alias for ctrl). Names and modifiers are case-insensitive,
> a single character keeps its case. Examples: `q`, `Q`, `ctrl+c`,
> `alt+enter`, `ctrl+shift+p`, `space`

### 12.1 Rules the sentence leaves implicit

1. **A `+` that opens or closes the rest is the base key, not a separator.**
   So `ctrl++` is ctrl plus the `+` character, while `+a` and `ctrl+` are
   errors.
2. **`shift+tab` and `backtab` are the same press**, because a terminal
   delivers shift+tab as a distinct backwards tab. Both parse to backtab with
   shift set, and backtab renders without a `shift+` prefix.
3. **Leading and trailing whitespace is trimmed**, except that a lone space is
   the space character. `space` and a single `" "` are the same press, `" a "`
   parses as `a`, and two spaces are an error because trimming leaves nothing.
4. **Function keys are `f1` through `f12` and nothing else.** `f0`, `f13`,
   `f+1`, `f01` and `f0000001` are all errors, deliberately: each of them
   would render back as `f1`, leaving an agent unable to derive the canonical
   form from what it sent.
5. **Modifier peeling is unbounded.** The grammar strips modifier prefixes
   with no limit, which is why a bridge accepting a key from an agent
   **SHOULD** bound the string's length before parsing it: a megabyte of
   `ctrl+` parses to the key it ends in and then serializes to a line neither
   peer will read (section 2.5).
6. **Modifier order does not matter on the way in.** `shift+alt+ctrl+enter`
   parses; it renders as `ctrl+alt+shift+enter`.
7. **A modifier name is case-insensitive, a single character is not.**
   `CTRL+c` parses as ctrl plus `c`; `Q` and `q` are different presses.

### 12.2 The conformance contract

A Rust peer parses this with `taria::key`, which is why the bridge's verdict
on a key and the app's are identical by construction. An adapter in another
language reimplements it, and **the parse/render round-trip is the contract to
reimplement against: every press the parser can produce MUST render to a
string that parses back to the same press.**

The canonical rendering is modifiers in ctrl, alt, shift order, then the base
key; `Char(' ')` renders as `space` and never as a literal space, so that a
modified press such as `ctrl+space` stays parseable; backtab renders as
`backtab` with no `shift+` prefix, and parses back with shift already set.

Verified round trips:

| Input | Parses to | Renders as |
|---|---|---|
| `shift+tab` | backtab, shift | `backtab` |
| `backtab` | backtab, shift | `backtab` |
| `ctrl++` | `+`, ctrl | `ctrl++` |
| `" "` (a single space) | space character | `space` |
| `space` | space character | `space` |
| `CTRL+c` | `c`, ctrl | `ctrl+c` |
| `shift+alt+ctrl+enter` | enter, ctrl+alt+shift | `ctrl+alt+shift+enter` |
| `F12` | f12 | `f12` |

Verified rejections: `+a`, `ctrl+`, `f0`, `f13`, `f01`, `ab`, `meta+x`, the
empty string.

### 12.3 What a key does

An app **SHOULD** deliver a key into its own key handling, at focus, exactly
as if the user had pressed it. An app that cannot express a parsed key in its
framework **SHOULD** answer `ignored` rather than approximate it: lowering it
to a near-miss event puts a keystroke into the app that the agent never asked
for and reports it as applied.

The key grammar is expected to grow inside version 1 (insert, the keypad,
media keys). A key added to it is additive on the wire, because a key travels
as a string, so an app built against an older grammar sees a string it cannot
parse and answers `ignored`.

## 13. Limits

### 13.1 Tree depth

`MAX_NODE_DEPTH` is **32**.

Depth counts **the snapshot root as level 1**, so the limit allows the root
plus 31 levels of descendants. A leaf on its own has depth 1. Depth is the
longest branch: a root with a leaf child and a three-node chain has depth 4.

A sender **MUST NOT** publish a snapshot whose tree is deeper than 32, and a
reader **MUST** be able to parse one that is exactly 32.

The limit exists because a snapshot is one JSON object and JSON parsers bound
how far they recurse into one. `serde_json`, which both reference peers use,
stops at 128 nested values, and every node costs two of them (its own object
and its `children` array). Measured through a whole `{"type":"snapshot",...}`
line rather than a bare node, 63 nested nodes parse and 64 fail. 32 is a
little under half that, and the slack pays for the transport envelope, for a
root an adapter adds above the nodes an app hands it, and for a peer whose
parser is stricter than `serde_json`.

Crossing it **reports nothing anywhere**. The reader skips the line it cannot
parse exactly as it skips a truncated one, so a deep first snapshot leaves the
bridge saying it has no tree while the app is connected and healthy, and a
deep later snapshot leaves it serving the last shallow tree with nothing
marking it stale. This is why the obligation sits on the sender.

**What to do about a tree that is too deep is the sender's choice**, not a
protocol rule. The reference adapter **cuts**: every node at the limit
publishes without its children, per branch, and the app is told through
`stats()`'s `truncated_snapshots` and `last_truncation` fields. Cutting,
because the other two answers are the same failure: publishing the tree as
built puts a line on the wire the bridge cannot parse, and skipping the
publish is that same stale tree chosen deliberately. Only the cut still
delivers the part of the tree that is fine. The fix is nearly always to
publish what the widget draws rather than the data behind it: for a deep tree
view, the expanded path and the rows on screen.

### 13.2 Line length

1 MiB, 1048576 bytes, excluding the terminating newline, as
`taria::wire::MAX_LINE_BYTES`. See section 2.5.

The line that grows is a snapshot, which section 2.5 asks an app to keep
within the cap. Unlike a tree that is too deep, a tree that is too large is
not skipped: it ends the connection, and does again on every reconnect
(section 2.5).

**What to do about a snapshot that is too large is the sender's choice**, as
it is for depth, not a protocol rule. The reference adapter **trims**, and
marks a cut string with `…` (U+2026). A `log` or `terminal` value keeps its
end and starts with the marker, because the end is where those grow; every
other string keeps its start. Ids and custom action names are never changed,
so whatever an agent can read it can still act on. `docs/architecture.md`
(Snapshot size) gives the order the adapter cuts in and how it tells the app.

### 13.3 Socket path and pipe name length

`sockaddr_un.sun_path` has to hold the path and its terminating NUL, so the
limit is that buffer minus one, and the buffer is not the same size
everywhere. Windows has no `sun_path`: its endpoint is a named pipe (section
14.8), and the limit is on the pipe's whole name.

| Platform | Buffer | Longest endpoint |
|---|---|---|
| Linux and other unixes | `sun_path`, 108 | **107** bytes |
| macOS, FreeBSD, NetBSD, OpenBSD, DragonFly | `sun_path`, 104 | **103** bytes |
| Windows | none | **256** UTF-16 code units, the whole pipe name, `\\.\pipe\` included |

On a unix, length is the byte length of the path, the same count the kernel
applies. A path in the band between the two binds on Linux and is refused on
macOS, which is not hypothetical: the macOS temp dir is around 49 bytes, so
the fallback path plus a long app label lands in it.

On Windows, length is counted the way Windows counts the name, in UTF-16 code
units (a character outside the Basic Multilingual Plane, such as most emoji,
counts two). The 256 covers the prefix, so the default name
`\\.\pipe\taria-<SID>-<label>` spends 16 units on its fixed parts and the
SID's own length on the user (a SID is the account's security id, section
14.8): a typical account's SID is around 45 characters, which leaves a label
about 190. A label that holds `\` is refused before the length is counted
(section 14.8).

Both peers **SHOULD** check this before binding or connecting rather than
letting the kernel refuse. The kernel's own refusal names neither the path,
its length, the limit, nor a way out. The bridge checking it while parsing its
arguments is what keeps an over-long path from surfacing from inside a
reconnect loop, where it reads as "the app is not running".

### 13.4 Bounds the reference bridge adds (convention)

These are agent-facing tool bounds, not wire rules, but they exist to protect
the wire and an implementer may want to match them. Each one closes a
single-legal-call hole where an unbounded argument becomes a line over the
1 MiB cap, which costs the app its bridge. The reference adapter closes the
same hole on its side by trimming its snapshots (section 13.2).

| Bound | Value | Why |
|---|---|---|
| `act` `value` length | 4096 characters | Same bound as `type_text`, because it answers the same question. |
| `type_text` payload | 4096 characters | |
| `key` string length | 64 characters, checked before the parse | Modifier peeling is unbounded (section 12.1). |
| `key` repeat count | 1 to 64 | |

## 14. Socket discovery and lifecycle

### 14.1 Path resolution

Both peers derive the same default path from the same environment, or they
never meet. Precedence, highest first:

1. **`$TARIA_SOCK` verbatim**, when set and non-empty. An explicit path is an
   override, so it is used exactly as given.
2. **`$XDG_RUNTIME_DIR/taria/<label>.sock`**, when `$XDG_RUNTIME_DIR` is set
   and non-empty. This is the per-user runtime directory, already private and
   already cleaned up at logout.
3. **`<temp dir>/taria-<user>/<label>.sock`**. The temp dir is shared, so
   `<user>` namespaces it to keep one user's sockets out of another's reach.

An empty variable counts as unset in both branches.

`<user>` is derived identically by both reference peers: the effective uid
where it is available (through `/proc/self` on Linux), else `$USER`, else
`$LOGNAME`, else the literal `default`. This derivation lives in each peer
rather than in the shared crate, so a third-party adapter that wants to be
discoverable through the fallback branch **MUST** match it.

This resolution, sections 14.3 and 14.4, and the socket file of section 14.6
are about a file on a unix. On Windows the endpoint is a named pipe, which is
named, protected and freed differently; section 14.8 says how.

### 14.2 The label rule

The label is formatted into a file name, so it has to be one. A label that is
empty, `.`, `..`, or that contains `/` or a NUL byte **MUST** be refused
rather than resolved.

This is checked **before** the precedence above, so a label is acceptable or
not on its own terms and cannot pass on a machine that happens to set
`$TARIA_SOCK` and fail on the next one.

The reason is that the app binds **and unlinks** what the label resolves to.
`/etc/cron.d/evil` as a label makes the path join discard the whole resolution
and keep the absolute path, and `../../../tmp/pwn` walks out of the runtime
directory. The label is argv-sourced, so this is not a privilege boundary; it
is the difference between a bad argument reported as one and a bad argument
deleting a file.

A label may hold anything else a file name may hold: `my app`, `app.v2`,
`..app`, `app..`, `-` and `app:1` all resolve. The check is about components,
not about characters someone finds surprising. An app that wants a path this
rejects passes the path itself.

The reference bridge refuses the same labels while parsing `--app`, where
`--socket` remains the way to name a path.

On Windows the label ends a pipe name rather than a file name, and a label
that also contains `\` **MUST** be refused, since Windows reads `\` as a
separator there too (section 14.8).

### 14.3 Directory vetting

Before binding, the app **SHOULD** create the socket's parent directory with
mode `0700` and then vet it: it **MUST** be a real directory rather than a
symlink, **MUST** be owned by the current user, and **MUST** have no group or
other permission bits. Binding is refused otherwise.

The parent directory decides who can replace or redirect the socket. A
symlink, another user's ownership, or a group-writable mode each let a local
attacker swap the socket for their own and impersonate the app, or intercept
the bridge. The check uses the link's own metadata, not the target's, so a
planted symlink is seen as itself.

That is impersonation at the socket. Text inside the real app's tree that
speaks as the app, a model's output or a file's contents, is the content-level
case, and section 6.4 covers it.

### 14.4 Freeing the path

Binding needs the path to be absent, so a socket left behind by a run that was
killed has to be unlinked first. That unlink has to be earned:

- **A socket nothing is listening on** is unlinked and the bind proceeds.
- **A socket another instance is serving** is refused (`AddrInUse`), naming
  the path and saying to give the second instance one of its own. Taking it
  over would leave the first instance running, believing it is reachable,
  while every bridge connects to the newcomer.
- **Anything that is not a socket** is refused (`AlreadyExists`) and left
  untouched. The path can come verbatim from `$TARIA_SOCK`, and a typo there
  is not a reason to delete a file.

The liveness probe is a `connect` that is opened and immediately closed. A
peer serving the path sees a connection that opens and closes, which is what
it does with any client that goes away. An answer that does not arrive within
a short timeout (250 ms in the reference adapter) refuses the bind rather than
unlinking a socket that may still be serving.

### 14.5 Serving

The app listens and **MAY** serve one client at a time; the reference adapter
does. On accept it sends, in this order:

1. the `hello`,
2. the latest snapshot, if it has one, unstamped (section 5.3),
3. every snapshot published after that, as it happens, and every resend of
   the last one with only its stamp moved, which is not a publish (section
   5.1).

Multiple simultaneous bridge clients per app are a deferred feature, not a
protocol rule. With a one-at-a-time adapter, a second bridge's `connect`
succeeds into the listen backlog (the kernel's queue of connections the app
has not accepted yet) and is not accepted until the first bridge leaves, so
until then that bridge sees an open connection on which nobody says a word. A
bridge **SHOULD** report that state as itself rather than as a wrong socket
path: in that state the bridge is connected to the right path, and sending an
adopter back to check the path is the first wrong turn a retrofit takes.

Anything a waiting bridge writes sits unread in the kernel's socket buffer
until its turn comes, and would then be read as the new session's first lines.
Nothing written before the `hello` can be a reply to it: that bridge has seen
no tree, so its inputs name nodes and keys it guessed at. An app **MAY**
discard input written before its `hello`, without acking it, since the session
it would be answered in has not begun. A bridge that writes nothing before the
`hello`, as the reference bridge does, loses nothing.

**Reference implementation.** The adapter discards only for a client that was
already waiting in the backlog when the previous session ended. Just before
writing that client's `hello`, it reads off whatever is already waiting and
discards it, counting each complete `input` line, of any kind, in
`stats().stale_inputs`. If what it read stops partway through a line, the rest
of that line is skipped when it arrives. More than 1 MiB waiting is treated like
a line over the cap and closes the connection. The first connection of a
process, and a client that connects to an idle app, waited behind nobody and
keep what they wrote. The adapter tells the two apart by whether an accept
has found the backlog empty since the last session ended, so a client that
connects the instant a session ends counts as one that waited.

**Not specified.** What a second bridge should do while its connection waits,
or on Windows after its open is refused as busy, beyond reporting it.

**Reference implementation.** The reference bridge takes turns, which is its
own policy and not a rule. It opens no connection until its first tool call,
so a bridge that is started and never used never takes the app. A bridge
that waits says the app is held by another bridge, and when the holder lets
go, so the agent retries: after 60 s with no tool calls on the default, or
after the waiting bridge's own `--idle-release`, its best guess at a holder
started the same way. Once the holder does (section 14.7), the waiting bridge
is accepted. A waiting bridge that itself goes a whole window with no tool
calls leaves the backlog.

A stopped app keeps its socket listening and accepts nothing, which leaves a
connection exactly as silent as a held app does. On Linux the reference bridge
tells the two apart by the listener's state. It takes the listener's pid from
`SO_PEERCRED`, the pid the kernel records for the other end of a Unix socket
(for a connection still in the backlog, the process listening), and reads that
process's state from `/proc/<pid>/stat`. A stopped app is named with its pid
and how to resume it. A job suspended in its terminal, stopped outside the
terminal's foreground process group, resumes only with `fg` there:
`kill -CONT` resumes it only in the background, where its next use of the
terminal stops it again. A tracing stop (state `t`) ends from the debugger,
and any other stop with `kill -CONT`. Nothing on the wire carries any of it,
and on macOS and the other unixes a stopped app reads as held.

**On Windows there is no backlog**, and both of the silences above mean
something else. A one-instance pipe (section 14.8) has no queue to wait in:
while a bridge is served, a second bridge's open fails at once with
`ERROR_PIPE_BUSY`, and no connection opens at all. That refusal is the only
sign of an app held by another bridge, and a bridge **SHOULD** report it as
the app held, not as a wrong pipe name or an app that has gone, and
**SHOULD** retry it as it retries any connect (section 14.7). So:

- A second bridge never has a connection to write into before its turn, and
  the discard of input written before the `hello`, above, never applies.
- An open connection that stays silent is not a bridge waiting its turn: the
  app's pipe took it, and an app greets a bridge as soon as it takes one. A
  bridge **SHOULD NOT** report that silence as the app held. It is the app
  itself, suspended (by a debugger or a process tool), or stuck before its
  `hello`, or a pipe that is not a taria app at all.

**Reference implementation.** The reference bridge retries a busy open for
250 ms (section 14.8), and answers a refusal that outlasts that with the held
answer, in the same words and the same retry advice as on a unix, with the
pipe's refusal as the evidence in place of the silent connection's age, and
last that the app may instead be suspended or stuck: an app stopped after a
session cannot listen again, so its pipe stays busy. The exception is a busy
refusal while a pipe it refused still stands (section 14.8), which keeps that
pipe's answer instead. It answers the silent pipe with the app suspended or
stuck, the pipe name right, and a retry once the app responds. Stopped-app
detection needs `/proc`, so on Windows a suspended app is named only as
possibly suspended, never with its pid's state. Every answer that names the
endpoint names the pipe and the pipe name where a unix answer names the
socket and the socket path, and is otherwise the same text.

### 14.6 Shutdown

The app **SHOULD** remove the socket file on exit, but only while that file is
still the one it bound, compared by device and inode. A second instance that
took the path over is serving a socket of its own there, and unlinking it
would leave that instance unreachable.

Binding **MUST NOT** be able to park an app during startup. That is the one
thing taria promises never to do, and it is why the reference adapter turns
every bind failure into an inert layer rather than an error, polls a shutdown
flag instead of parking in `accept()`, and runs its liveness probe with a
timeout on a thread of its own.

**Reference implementation.** At shutdown the adapter writes the acks still
queued for a connected bridge before it closes the connection, so an app that
answers an input and exits in the same frame does not leave the agent told
that input's fate is unknown. The flush gets 250 ms in all, and a snapshot
still waiting is not sent: it describes a UI that is about to be gone. Exit
stays bounded: at worst one 50 ms poll of the shutdown flag, the 5 s the line
being written may take, and the 250 ms flush. Every line the adapter writes
has 5 s in all, however large it is and however the bridge reads, and a
bridge that cannot take a whole line in that time is dropped.

### 14.7 Reconnection

A bridge **SHOULD** reconnect with capped backoff rather than give up: the
reference bridge starts at 250 ms, doubles, and caps at 2 s, retrying while
tool calls want the app. A connection that dies young without delivering a
snapshot **SHOULD** keep the backoff growing instead of resetting it, or a
peer that accepts and immediately drops pulls the bridge into a full-CPU
loop.

On disconnect a bridge **SHOULD** fail tool calls fast with an error naming
the app that went away, rather than acting on a stale tree.

**Reference implementation.** The reference bridge connects on demand. It
opens no connection until its first tool call, which waits for the `hello` and
the latest snapshot the app sends on accept (section 3.4), so that call still
answers with the live tree. The wait normally takes milliseconds and never
runs past 1.2 s. After 60 s with no tool calls it closes its connection, or
leaves the listen backlog, and stops reconnecting until the next tool call, so
a bridge waiting for the app gets it. Stopping while no call wants the app is
not giving up: the next call reconnects. `--idle-release` sets the delay, and
`0` never lets go. It does not let go while an input it sent is still waiting
for its ack, for up to 30 s past the delay, because the app discards an input
whose connection has ended (section 11). A bridge that reconnects into the
backlog behind another, as the one that loses the race does when an app
restarts, says the app is held by another bridge rather than that it may have
exited. One that reconnects to an app that greets it but has not published yet
says that, and that a retry is served the first snapshot. One that reconnects
and cannot read what the app sends quotes the line it could not read: a
connect forgets every reason it held but an oversized line's (section 13.2),
so the reason quoted is the new connection's. On Linux, one whose connection
is silent because the app is stopped names it as stopped, with its pid and how
to resume it (section 14.5). One that let go of the app after its window says,
in each of these answers, that the disconnect was its own release, not a fault,
unless it has seen signs of a new instance since: nothing listening, or another
pid listening than the one it let go of. None of this is a protocol rule. To
the app, a bridge that let go is one that left.

On Windows the same bridge reads a reconnect refused as busy as the app held
by another bridge, the Windows form of landing in the backlog, and a
reconnect that opens and stays silent as the app suspended or stuck (section
14.5). It takes the peer's pid from `GetNamedPipeServerProcessId` rather
than `SO_PEERCRED`, for telling one instance from the next. Since a pipe's
name lives while any handle to the pipe is open (section 14.8), the bridge
closes its end as soon as it reads end of stream, so that a restarted app can
create the pipe again.

### 14.8 Windows discovery

On Windows the endpoint is a named pipe (section 2.1), and this section
replaces sections 14.1, 14.3 and 14.4 there, and the socket file of section
14.6. Sections 14.2, 14.5, 14.6 and 14.7 otherwise apply, with the
differences they and this section state. Nothing here changes a byte on the
connection: it is a transport a peer adds, and version 1 is unchanged.

Two Windows terms recur. A **SID** (security identifier) is the string that
names an account, `S-1-5-21-...`. A **DACL** (discretionary access control
list) is the part of an object's security descriptor that lists who may open
it and how.

**The name.** Both peers derive the same default name, or they never meet:

1. **`$TARIA_SOCK`**, when set and non-empty, if it passes the rule below.
2. **`\\.\pipe\taria-<SID>-<label>`**, where `<SID>` is the SID string of the
   user the process runs as, read from the process token (not a thread's
   impersonation token) and written as `ConvertSidToStringSidW` writes it.

Pipe names share one namespace per machine rather than living in a per-user
directory, so the SID is in the name to keep one user's pipes apart from
another's. A peer that wants to be found by the default **MUST** derive it
exactly this way. The name only keeps users apart; it does not keep anyone
out. What does is four things below, together: the first-instance flag, the
reject-remote flag, the DACL, and the bridge's owner check.

**The override.** A peer **MUST** honour `$TARIA_SOCK` only when it names a
pipe on the local machine: it starts with `\\.\pipe\`, in any letter case,
since Windows matches the prefix without regard to case (`\\.\PIPE\x` is
`\\.\pipe\x`), and what follows is one name, with no `\` or `/`, no NUL, and
not empty or only dots and spaces. Anything else **MUST** be refused, not
fallen back from: the user named an endpoint, and serving or connecting
somewhere else would leave the two peers apart with nothing said. A remote
name, `\\host\pipe\x`, is the reason for the rule. Opening a pipe on another
machine goes over SMB (Windows file sharing), which sends that machine the
user's credentials and puts the agent's input on the network. The other
refusals close the ways around the prefix: Windows normalises a `\\.\` name
before opening it, folding `/` into `\` and resolving `..`, so
`\\.\pipe\..\UNC\host\share` passes a plain prefix check and leaves for SMB.
The same rule holds for a name passed whole, such as the reference bridge's
`--socket` and the adapter's `bind_at`.

That normalisation also trims trailing dots and spaces, so `\\.\pipe\taria.`
and `\\.\pipe\taria` name the same pipe, and a label `app.` the same pipe as
`app`. Both peers pass the name to Windows, which normalises it alike for
each, so they still meet. A peer **SHOULD** pass the name as UTF-16 to the
wide-character (`W`) functions: the ANSI ones convert it through the code
page first, and a best-fit conversion can turn a character the rule allowed
into a `\`.

**The label** is held to section 14.2's rule, and **MUST** also be refused if
it contains `\`. The whole name is limited to 256 UTF-16 code units (section
13.3).

**Creating the pipe.** The app:

- **MUST** create it as the first instance of its name
  (`FILE_FLAG_FIRST_PIPE_INSTANCE`), which fails while any instance of that
  name exists. That is what refuses a second app on a live name, as section
  14.4's `AddrInUse` does on a unix, and what keeps an app from joining, as a
  second instance, a pipe something else created first under its name.
  Taking a name over is impossible here, so there is nothing to free.
- **MUST** reject remote clients (`PIPE_REJECT_REMOTE_CLIENTS`).
- **MUST** create it with a security descriptor whose owner is the user, and
  whose DACL grants that user alone, protected from inherited entries. This
  is section 14.3's private directory in another form: it decides who can
  open the app's pipe.
- **SHOULD** allow one instance (`nMaxInstances` 1). A second instance would
  let a second bridge connect into it and wait unserved, which section 14.5
  says a bridge reads as a stuck app, not a held one.
- **MAY** reuse the one instance across sessions: disconnect the last client
  (`DisconnectNamedPipe`) and listen again (`ConnectNamedPipe`). The
  reference adapter does.

**Opening the pipe.** The bridge:

- **MUST** check, after the open and before it writes anything to the pipe
  or parses anything it sent, that the pipe's owner is its own user's SID,
  and otherwise close it with nothing written to it and nothing it sent
  parsed. Any process can create a pipe under the app's name before the app
  does, or after it exits, and an agent that read it would take its tree for
  the app's and send it input. An owner that is a group the user belongs to
  is not the user. A runtime that starts a read into its own buffer as it
  registers the pipe, as tokio's does, still conforms, provided what that
  read took is dropped with the pipe and never parsed.
- **SHOULD** read an open refused as access denied (`ERROR_ACCESS_DENIED`)
  as a pipe it cannot use, not as no app there, and **SHOULD NOT** report it
  as surely not the app's, as it may a pipe another account owns. The app's
  pipe grants its user access (above), but a bridge running as that user is
  still denied it in a sandbox (a restricted token or an AppContainer) or at
  low integrity, and a bridge running as another account, given the name
  whole, is denied it too. So access denied means either a pipe the app did
  not create, or a bridge with fewer rights than the app's user, and a
  report of it names both, and what to check: that the bridge runs as the
  app's user, outside any sandbox and at the app's integrity level.
- **SHOULD** open it at the `SECURITY_IDENTIFICATION` impersonation level, so
  that what serves the pipe can learn who the bridge is but cannot act as it.
- **SHOULD** retry an open refused as busy (`ERROR_PIPE_BUSY`) for a short
  while before it reads the refusal as the app held by another bridge.
  Between two sessions the one instance is busy for a moment: the last
  bridge has closed its end, and the app has yet to disconnect it and
  listen again. Refused at once, a bridge arriving in that moment would
  report a free app as held.
- **SHOULD** read a refusal as busy that outlasts the retry as the app held
  by another bridge, unless a refusal of the pipe stands (below), and a
  missing name (`ERROR_FILE_NOT_FOUND`) as no app there (section 14.5).
- **SHOULD** keep reporting a pipe it refused, one another account owns or
  one that denied it access, on the busy refusals that follow, until a
  connect passes the owner check, finds nothing listening, or the bridge
  stops trying, as the reference bridge does when it lets go of the app
  after its idle window (section 14.7). A pipe that takes one open, fails
  the owner check and is closed stays busy until its server disconnects that
  open, which an impostor need never do, and read as the app held, its busy
  refusals would send the agent to wait for an app that is not there. The
  rule ends with the trying because what it stands over is what the bridge
  has tried since: a bridge that stops trying cannot see the impostor exit
  and the app come back, held by another bridge, and its next busy refusal
  is read afresh.

**The pipe's lifetime.** A pipe, and its name with it, lives while any handle
to it is open, on either end. An app that exits while a bridge still holds
its end leaves the name taken until that bridge closes, and a restarted app
is refused as if the old one were still serving. A bridge **SHOULD** close
its end as soon as it reads end of stream. An app **SHOULD** retry, for a
short while, a create that fails because the name is in use
(`ERROR_ACCESS_DENIED` from the first-instance flag, or `ERROR_PIPE_BUSY`)
before it reports the name in use, so that an app restarted at once
outlasts that bridge instead of running with taria off for the whole of its
run. The retry has to stay short, since binding must not park the app's
startup (section 14.6), and a second app on a name that is still served is
refused all the same, that much later.

**Shutdown.** A pipe leaves no file behind, so section 14.6's rule on removing
the socket file has nothing to act on. The acks flushed at shutdown (section
14.6) still have to reach the bridge: an app **SHOULD** close its end at
shutdown rather than disconnect the client, since closing leaves what the
app wrote readable by the bridge until it has read it, and
`DisconnectNamedPipe` discards it. It **SHOULD NOT** wait for the bridge to
read it either (`FlushFileBuffers`), which can wait forever and so park the
app's exit.

**Reference implementation.** `taria::socket` holds the pure parts both
reference peers use: `pipe_name` builds the default name, `check_pipe_name`
holds a name given whole to the override rule, and `resolve_pipe_name` is the
precedence above. They build and are tested on every platform. The adapter
creates the pipe with `PIPE_ACCESS_DUPLEX`, byte mode, one instance, the
first-instance and reject-remote flags, and a DACL granting the user
`GENERIC_READ | GENERIC_WRITE`, and retries a create that finds the name in
use for 500 ms, 20 ms between tries, before it reports `AddrInUse`. The
bridge opens it with tokio's named-pipe client, retries a busy open for
250 ms, 10 ms between tries, checks the owner with `GetSecurityInfo`, and
answers an open refused as access denied with both readings, the bridge's
rights first. It keeps a refused pipe's answer over every failed connect
after it but nothing listening, a busy refusal included, and drops it when
it lets go of the app after its idle window, the one way it stops trying.

## 15. Focus contract

**Every snapshot should carry exactly one focused node, and this is the
app's obligation rather than anything either peer enforces.**

- An app **MUST** publish at most one node with `focused: true`.
- An app **SHOULD** publish at least one. A tree with no focus tells an agent
  nothing about where a raw key would land. The reference adapter guarantees
  it by focusing the root it generates when no recorded node, or descendant of
  one, is focused.

Neither reference peer validates this. The bridge does not read `focused` at
all beyond describing it to an agent, so an app that publishes two focused
nodes produces a tree that parses and misleads.

**Not specified.** What a reader should do with a snapshot carrying zero or
more than one focused node.

Focus tells the agent where a raw `key` would land, which is what makes the
key fallback usable. It is also how an agent aims `text`, but only because an
app that is accepting typing normally focuses the surface taking it. What
decides where typed characters go is the app, not the protocol.

**Focus is not selection.** Focus says where a key would go; a cursor sits on
a row whether or not that list owns the keyboard. An app that publishes only
focus makes a moved cursor invisible, and a bridge then truthfully reports
"the tree did not change" for an input the app handled. Publish the selection
as its own readable fact, in `selected` (section 6.5): the demo marks the
cursor row `"selected":true` and every other row `"selected":false`, and also
names the selected row's id in the list node's `value`, as it did before
`selected` existed. Both are visible in the capture in section 18, where a
`down` key moves `"selected":true` from `task-1` to `task-3` and the list's
`"value":"task-1"` to `"value":"task-3"`.

## 16. Versioning

### 16.1 Version 1 is frozen

`PROTOCOL_VERSION` is 1 and the format is fixed. Within version 1, changes
**MUST** be additive.

Two properties make that safe, and both are obligations on every reader
(sections 2.6 and 2.7): a reader skips a line it cannot parse instead of
dropping the connection, and a reader ignores unknown fields. So a new
optional field, or a whole new message variant, reaches an older peer as
something it quietly ignores.

### 16.2 What is additive

- A new **optional field** on an existing message, omitted when unset.
- A new **message variant** (subject to section 16.4).
- A new **role** name.
- A new **action** name.
- A new input **`kind`**.
- A new ack **`status`** value.
- A new **key name** in the grammar (section 12.3).

### 16.3 The four degrade fallbacks

Neither property in 16.1 covers a value nested inside a message the peer does
want, so the open vocabularies and the typed state carry their own
fallbacks. A reader **MUST** implement all four:

| Value | Unknown reads as | Section |
|---|---|---|
| `role` | `other` | 7.2 |
| `action` | a custom action **keeping the name** | 8.4 |
| `input` `kind` | an unreadable input **keeping the message's id** | 9.5 |
| a typed state field, such as a `checked` of `"partial"` | not stated, **keeping the node** | 6.1 |

An ack's `status` deliberately has **no** fallback (section 10.5), because it
sits at the top of a message rather than nested inside one, so losing it loses
only one ack.

### 16.4 The narrower rule for bridge-to-app messages

Quietly ignoring the line is not the same as quietly ignoring the request, and
in the bridge-to-app direction that difference is the whole point.

An ack answers an id, and an id belongs to an `input` message. **A variant
that is not an `input` cannot be acknowledged**: an old app skips it with
nothing sent back, and the agent that sent it waits out a timeout. So the rule
for this direction is narrower than additive:

> An addition that carries agent input **MUST** be a new input `kind`, never
> a new bridge-to-app message variant.

A new kind reaches an old app inside a message it already reads, degrades to
the unknown-kind fallback, and comes back as an ack the agent can act on. New
bridge-to-app variants stay available for anything that is not input and wants
no answer.

Verified: the reference adapter, sent `{"type":"future","payload":1}`, skipped
it without an ack and kept serving.

### 16.5 Version mismatch

Peers on different versions disagree about the shape of every message, so an
app on another version cannot parse a single input this bridge sends it.

A bridge **SHOULD** split its tool surface by direction on a mismatch:

- **Keep the connection** and log a warning.
- **Keep reading**, for as long as the peer's snapshots still parse.
- **Refuse every input path up front**, with an error naming both versions,
  having sent nothing. Forwarding input across a mismatch leaves the agent
  waiting on a session that can never react.

Reading across a mismatch is the common case, not a guarantee. A bump is
defined by the changes that break parsing, so a peer that moved a field of
`Snapshot` delivers lines the bridge skips one by one, holds no tree at all,
and answers "no snapshot from the app yet" rather than with a degraded one.
What survives a bump is whatever the two versions still happen to spell the
same way.

An app has no comparable decision to make: it reads the bridge's version
nowhere, because the bridge sends no handshake.

### 16.6 Passing the line on beats degrading

Degrading keeps the connection; passing the line on keeps the information. A
bridge **SHOULD** store each snapshot as **the app's own line** beside the
parse of it, and hand the line to the agent. A role, an action or a field the
bridge has never heard of then reaches the agent by name instead of flattened
into whatever its typed view could hold: an app publishing
`"role":"sparkline"` had the agent read `"role":"other"`, with the real name
on the wire the whole time.
`taria-mcp --tree-format outline`, which hands the agent an outline instead of
the line, renders it from the line for the same reason.

The parse stays authoritative for what the bridge **decides**, because it is
typed: validating node ids and advertised actions, and detecting change, read
the parse rather than the text. A line is passed on only after it parsed, so
a malformed one is still skipped.

The two things a bridge **SHOULD** remove from the line it passes on are the
`"type":"snapshot"` framing key, which belongs to the transport rather than to
the tree, and the `after_input` stamp (section 5.3), which orders the tree
against the bridge's inputs and is for the bridge to reason about: an agent
shown it reads an id it never sent as a fact about the app, and a tree sent
again with only its stamp moved as one that changed. The reference bridge
removes both, from every peer and in both tree formats, by parsing the line
into a JSON value and writing that out again, which keeps every other key but
not the app's spelling of the line: keys come out sorted and whitespace gone,
string escapes are rewritten (`"\u00e9"` becomes `"é"`), and of a key
repeated in one object only the last value is kept, though a repeated field
the bridge knows never gets that far, because it fails the parse (section
2.2). Numbers are rewritten too. An integer in the 64-bit range comes through
as written, `-0` aside; any other number is read as the double nearest it and
printed as serde_json writes that double: `1e3` becomes `1000.0`, `-0`
becomes `-0.0`, `1e21` becomes `1e+21`, `1e-07` becomes `1e-7`, and an
integer outside the 64-bit range becomes a float, keeping about 17
significant digits. The read is exact since 0.4.0, so `1.4000000000000001`
comes through digit for digit, where before 0.4.0 it read `1.4`. A sender
**MUST NOT** rely on any of these spellings reaching the agent.

### 16.7 What forces a version bump

Anything that is not additive:

- removing a field,
- renaming a field,
- making an optional field required,
- changing what an existing field or variant **means**.

The last is the dangerous one, because an old peer parses the message
successfully and acts on the old meaning.

### 16.8 Typed state (0.4.0)

The second addition since version 1 froze, after `relayed` in 0.3.3: fourteen
optional node fields, `description` and the thirteen from `selected` to
`column_count` (sections 6.1 and 6.5). Each is a new optional field, omitted
when unset, so the addition is additive under section 16.2 and
`PROTOCOL_VERSION` stays 1.

- **An older reader** ignores the fields as unknown (section 2.7) and parses
  every node as before. It reads no state, so to it a node is exactly what a
  0.3.3 app would have sent, `value` and all.
- **An older reference bridge** from 0.1.0 to 0.3.3, which hands the agent the
  app's own line (section 16.6), passes the fields on in the JSON tree with the
  rest of it. Its outline, in 0.3.0 to 0.3.3, shows them among the fields at the
  end of the line, `checked=false selected=true`, rather than as marks, and its
  `read_tree` description says nothing about them. It does not refuse `act` on a
  disabled node, but against the reference adapter there is nothing to send: the
  node advertises no actions, so its own validation refuses the act as
  unadvertised.
- **An older app** sends none of them, so every node reads as stating
  nothing, which is what absent means.
- **Leniently read.** A field added later fails the node when it is read
  strictly, and a failed node fails the line, which the reader skips: the
  agent is left on a stale tree with nothing saying why, over one field.
  `relayed` is read strictly because a mark that is not a boolean is no
  clearer about who wrote the text; state is not like that, so every field
  added in 0.4.0 reads a wrong shape as not stated (section 6.1), and
  `relayed` keeps its 0.3.3 rule.
- **A repeated key now fails the line.** A 0.4.0 reader fails a line whose
  node repeats one of the new keys, `"checked"` twice, with "duplicate
  field", where a 0.3.3 reader skipped both copies as unknown. Section 2.2
  already forbids a sender to repeat a key, so no conforming line changes
  meaning; only a reader's tolerance of that mistake is gone.
- **What changed for the demo.** The reference demo now states a task's done
  state as `checked` instead of the value `"todo"` or `"done"`, and the
  active tab and the cursor row as `selected` instead of a tab's value
  `"selected"`. It keeps the list's and the tab bar's values. A reader that
  matched the old values reads the demo, and only the demo, differently.

### 16.9 Stamps (0.4.0)

The third addition since version 1 froze: `after_input` on the snapshot and
`stamps` on the hello (sections 3.1 and 5.3), for issue #17. Each is a new
optional field, omitted when unset, so the addition is additive under
section 16.2 and `PROTOCOL_VERSION` stays 1. What changed beside the fields
is one behaviour, which section 5.1 now allows: an app that stamps sends its
latest snapshot again, under the same `seq`, when the app has dequeued an
input since it went out.

That resend moves what `seq` promises, from one number per snapshot sent to
one per tree published, and section 16.7 makes a change of meaning the one
that forces a bump. It does not force one here. Within one run `seq` still
never decreases, so a higher one is still the newer tree. One inference
does stop being true: until 0.4.0, item 3 of section 1.3 said `seq`
"increases within one run", which let a reader assume no two snapshots
share a `seq`. Such a reader, meeting the repeat, at worst drops it as a
duplicate, which is harmless, because the tree is unchanged: the resend
differs only in `after_input`, which an older reader never reads. Across
runs `seq` promised nothing before and promises nothing now (section 5.1).

- **An older reader** ignores both fields as unknown (section 2.7). It reads
  the hello it always read, and every snapshot as the snapshot it was. The
  resend of an identical tree is, to it, the same snapshot twice: `seq` and
  every field it knows are unchanged, so a reader that compares whole
  snapshots reads no change, and a reader that compares `seq` none either.
- **An older reference bridge** from 0.1.0 to 0.3.3 hands the agent the app's
  own line (section 16.6), so its JSON tree shows `"after_input":N` once the
  app has dequeued an input, and its outline, in 0.3.0 to 0.3.3, shows
  `after_input=N` among the header's fields. To an agent that can look like
  a change, since two reads of one tree differ in it, and it names an input
  id the agent never sent: ids are the bridge's, and no tool shows them.
  Its answers do not move: it still waits its 500 ms and still reads a slow
  handler as "did not change", the behaviour issue #17 is about, which only
  a bridge that reads the stamp avoids.
- **An older app** sends neither field. A bridge that reads stamps answers it
  as it answered every peer before stamps existed, window and texts
  included: the reference bridge does so byte for byte.
- **An app that claims stamps and does not stamp** says `"stamps":true` and
  sends unstamped snapshots section 5.3 does not allow: one after a stamped
  snapshot, or more than one after its first dequeue's `delivered` ack,
  which an adapter that queues snapshots can do without meaning to. A
  bridge that reads stamps degrades the rest of the connection, from the
  call that shows it, to the rules for a peer that does not stamp, so a
  tree that differs is credited as it was before stamps existed, rather
  than leaving every input waiting for a stamp that never comes. The
  reference bridge also logs a warning, since the claim is the app's bug.
  Both rules bind only an app that says `stamps`, which no app before 0.4.0
  does.
- **Leniently read.** A `stamps` that is not `true`, and an `after_input`
  that is not an unsigned 64-bit integer, read as absent rather than failing
  the line: the hello would otherwise take the version and label with it,
  and the snapshot its whole tree.

## 17. Rust language binding (non-normative)

**This section is a language binding, not a wire rule.** A peer in another
language can ignore all of it. It is here because the obligations it describes
are easy to mistake for protocol requirements when reading `crates/taria`.

Additive on the wire is not automatically additive in Rust. A peer skips a
message variant it cannot parse, but a peer **rebuilt** against the version
that added one meets it in an exhaustive `match` and stops compiling, and the
peers this format is written for are exactly the ones that match on these
types.

So every type a version-1 addition can reach is `#[non_exhaustive]`. There are
**twelve**: `AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`,
`Action`, `Role`, `Node`, `Checked`, `Snapshot`, `key::Key`, `key::Modifiers`
and `key::KeyPress`. The key types are on the list even though a key travels
as a string, because the string is parsed into them and an adapter takes the
result by value.

The same reasoning reaches one level in, to the variants. A new optional field
on an existing message is the format's cheapest additive change and also the
one that breaks an outside peer hardest, because it breaks everything that
builds or destructures that message. So **six** struct-like variants are
marked too: `AppToBridge::Hello`, `AppToBridge::Ack`, `BridgeToApp::Input`,
and `AgentInput::Act`, `Key` and `Text`. `AppToBridge::Snapshot` needs
neither: it is a newtype around `Snapshot`, which is marked already.

What that costs a Rust peer:

- **Build marked variants through their constructors**, since a marked variant
  has no struct literal from outside the crate: `AppToBridge::hello` (with
  `.stamps(true)` for a peer that stamps), `ack`, `BridgeToApp::input`,
  `AgentInput::act`, `key`, `text`, `Snapshot::new` (with `.after(id)` for
  a stamp), `Node::new` plus its chainable setters, `Modifiers::NONE` /
  `Modifiers::new`, `KeyPress::new`.
- **End a destructuring pattern with `..`**, so a field added later does not
  break the match.
- **Carry a wildcard arm** on any `match` over these enums. The cost of the
  wildcard is real: on `AgentInput` it is the arm that swallows `Text`, which
  is how a demo can look complete while typing goes nowhere. The demo carries
  `clippy::wildcard_enum_match_arm` to catch exactly that.

Two more Rust-only facts:

- `Role` and `Action` use hand-written deserializers that call
  `deserialize_any`, so they decode from **self-describing formats only**.
  ndjson is one; bincode and its relatives are not.
- `key::Key`, `key::Modifiers` and `key::KeyPress` are deliberately **not**
  serde-serializable. The wire form of a key is the string, and a second
  encoding would let the two drift.

## 18. Appendix A: a worked session

### 18.1 A complete connection

Captured from the reference adapter by connecting a plain `AF_UNIX` client to
a running `taria-demo` and recording every byte in both directions. Each line
is verbatim; the tab-separated direction column is not on the wire, and each
message is followed by a single `\n`.

```
app->bridge	{"type":"hello","app_label":"taria-demo","protocol_version":1,"stamps":true}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":true,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","focused":true,"selected":true,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":1,"input":{"kind":"act","node":"tab-done","action":"select"}}
app->bridge	{"type":"ack","id":1,"status":"delivered"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"after_input":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Done","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":false,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":true,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Done tasks","value":"task-2","focused":false,"children":[{"id":"task-2","role":"list_item","label":"Sketch the protocol wire types","focused":true,"selected":true,"checked":true,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-5","role":"list_item","label":"Publish a snapshot per frame","focused":false,"selected":false,"checked":true,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
```

What to read out of it:

- The `hello` comes first and the bridge answers nothing. It says
  `"stamps":true`: every snapshot on this connection is stamped (section 5.3).
- The first snapshot arrives unprompted, immediately after the handshake,
  because the app already had one. It carries no `after_input`: nothing has
  been dequeued on this connection yet.
- `seq` is 1, then 2. Nothing was published in between, because nothing
  changed.
- The act names a node id and an action the tree advertised on that node
  (`tab-done`, `select`). Neither was guessed.
- The ack precedes the snapshot published after it, and that snapshot is
  stamped `"after_input":1`: published after the app dequeued input 1, so a
  bridge can credit it to that input whenever it arrives.
- The effect is legible in the next tree: the tabs node's value moved from
  `Active` to `Done`, `"selected":true` moved from `tab-active` to
  `tab-done`, and the list node re-labelled itself and published a different
  set of children, whose `"checked":true` says they are done. A tree diff,
  without the ack, could not have told that from an unrelated redraw.
- Exactly one node is focused in each snapshot, and it is not the root: the
  root carries `"focused":false` because a recorded node below it has focus.
- The list publishes its selection twice: the cursor row says
  `"selected":true` and every other row `"selected":false` (section 6.5), and
  the list names the row in its own `value` (`task-1`, then `task-2`). That
  is the focus-is-not-selection rule (section 15) in practice.
- `selected`, `checked` and the rest of the typed state sit between
  `focused` and `actions` in the app's line, while `description` comes before
  `focused`. Every field is omitted where the app states nothing: no node
  here says `expanded` or `disabled`.

### 18.2 The four ack shapes

From a second capture against the same app, showing every acknowledgement
pattern in one session. The `hello` and the unstamped `seq` 1 the app sent on
accept, the same two lines that open section 18.1, are left out:

```
bridge->app	{"type":"input","id":1,"input":{"kind":"text","text":"hello"}}
app->bridge	{"type":"ack","id":1,"status":"delivered"}
app->bridge	{"type":"ack","id":1,"status":"ignored"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":1,"after_input":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":true,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","focused":true,"selected":true,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":2,"input":{"kind":"paste","text":"hi","from":"clip"}}
app->bridge	{"type":"ack","id":2,"status":"ignored"}
bridge->app	{"type":"input","id":3,"input":{"kind":"key","key":"down"}}
app->bridge	{"type":"ack","id":3,"status":"delivered"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"after_input":3,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":true,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-3","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","focused":true,"selected":true,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":4,"input":{"kind":"act","node":"nope","action":"activate"}}
app->bridge	{"type":"ack","id":4,"status":"delivered"}
app->bridge	{"type":"ack","id":4,"status":"ignored"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"after_input":4,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":true,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-3","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","focused":true,"selected":true,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
```

- **id 1**: text sent while nothing was accepting typing. Two acks, last wins.
  Then the tree already published, sent again under its own `seq` 1 with
  only `"after_input":1` added: the app dequeued the input and its tree did
  not change (sections 5.1 and 5.3).
- **id 2**: a `kind` this app has never heard of. One ack, `ignored`, sent
  where the line was parsed, with no `delivered` before it because the input
  was never queued. Never dequeued, so it stamps nothing, and no tree
  follows.
- **id 3**: a key that moved the cursor. `delivered`, then the tree that shows
  the move, in that order, stamped 3.
- **id 4**: an act naming a node that does not exist. `delivered`, then
  `ignored`, then the tree of `seq` 2 again, stamped 4.

The same session also confirms the skip rules: a blank line, `not json at
all`, `{"type":"input","id":9,"input":{"kind":"key"}}` (a known kind missing
its field) and `{"type":"future","payload":1}` were each sent mid-session,
each drew no ack and no complaint, and the connection stayed up for the inputs
that followed.

### 18.3 What is not wire-normative

- **Node ids are opaque strings.** The `<prefix>-<key>` shape
  `taria::IdSpace` produces, and the rule that an id belongs to the space
  named before its **first** separator, are an app-side convenience for
  getting from a node id back to the thing it named. No reader parses ids, and
  a conforming app may shape them however it likes.
- **The auto-generated `app` root** in the captures above is the reference
  adapter wrapping the nodes the app recorded. The protocol requires a root
  node, not that particular one.
- **`text_to_keys`** and its `\n` to Enter, `\t` to Tab, `\r` dropped
  convention (section 9.3).
- **Every 500 ms and 3 s window, queue size, backoff and retry figure** in
  the reference peers, and the reference bridge's 60 s idle release (section
  14.7). Nothing in the protocol specifies timing.
- **The reference bridge's tool argument bounds** (section 13.4).

## 19. Appendix B: what is not specified

Collected for scanning. Each is a question version 1 does not answer, not a
behaviour a peer may rely on.

1. **A second `hello` on one connection.** Neither reference peer accepts or
   rejects one explicitly (section 3.2).
2. **A snapshot's `protocol_version` disagreeing with the `hello`'s.** The
   reference bridge goes by the `hello`, and before one by the first
   snapshot's (sections 3.5, 5.2).
3. **`seq` at the u64 maximum** (section 5.1).
4. **Duplicate keys naming a field the reader does not know.** A duplicate of
   a known field fails the line in the reference implementation; a sender
   **MUST NOT** emit either kind (section 2.2).
5. **An ack's order relative to a snapshot published before it.** Only the
   other direction is guaranteed (section 10.4).
6. **A snapshot carrying zero or more than one focused node** (section 15).
7. **Whether a bridge may send input before it has seen a `hello` or any
   snapshot.** The reference bridge refuses at the tool layer, but nothing on
   the wire forbids it. A bridge cannot rely on it: an app may discard such
   input (section 14.5).
8. **What a second bridge should do while its connection waits to be
   accepted** (section 14.5). Multiple simultaneous clients are deferred. The
   reference bridge's turn-taking is its own policy (sections 14.5 and 14.7).
9. **Any timing requirement.** No keepalive, no heartbeat, no deadline for an
   ack, no maximum publish rate.
10. **Bounds on `app_label`, `label`, `value`, node count and children count.**
    Only the 1 MiB line cap bounds them, and section 2.5 asks an app to keep
    its snapshot within it (sections 3.1, 6.3, 13.2).
11. **What a bridge does with relayed text beyond passing the mark on**, such
    as fencing or escaping it in what an agent reads. The reference bridge
    alters no text: it hands the mark on, and its outline prints it as
    `*relayed` (section 6.4).
12. **Typed state that contradicts itself**: a `value_now` outside its
    bounds, a `pos_in_set` past its parent's `set_size`, several siblings
    `selected` in a widget that selects one, `checked` on a role that cannot
    be checked. Neither reference peer checks any of it (section 6.5).
13. **A stamp that breaks the rules of section 5.3**: one naming an id the
    bridge never sent on the connection, one that goes down, or an
    `after_input` from a peer whose `hello` did not say `stamps`. The
    reference bridge strips the field from what the agent reads either way,
    reads a stamp only from a peer that said it stamps, and compares it as a
    number against its own ids.
