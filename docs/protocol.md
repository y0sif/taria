# taria wire protocol

**Status: this document specifies `PROTOCOL_VERSION` 1. Version 1 is frozen.
Only additive changes are permitted within it (section 16); anything else
bumps the version.**

It is written for someone implementing a taria peer from this document
alone: an adapter for a framework that is not ratatui, in a language that is
not Rust. Where it names what the reference peers (`taria-ratatui`,
`taria-mcp`) do, that is an example, not a rule.

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
   (section 14), or on Windows create a named pipe by the rules of section
   14.8.
2. Send exactly one `hello` as the first message on every accepted connection,
   before any other message (section 3).
3. Publish `snapshot` messages whose `protocol_version` is 1, whose `seq`
   never decreases within one run and repeats only on the resend section 5.1
   allows, and whose tree obeys section 6 and the depth limit in section 13.
4. Answer every `input` message it reads on a live connection with at least
   one `ack` naming the same `id`, `ignored` when it cannot act on it (section
   10). The exceptions are a line that did not parse (sections 2.6 and 9.4),
   an input whose connection has ended (section 11), and an input written
   before the app's `hello` (section 14.5).
5. Skip a line it cannot parse and keep the connection (section 2.6).
6. Never apply an input that arrived on a connection that has since ended
   (section 11).
7. If its `hello` says `"stamps":true`, stamp every snapshot on that
   connection by the rules of section 5.3. An app that does not say so need
   not stamp at all.

An app MAY publish no snapshots at all (it is then a connected peer with no
tree), and MAY accept only one bridge connection at a time.

### 1.4 What a conforming bridge MUST do

1. Connect to the app's socket, on Windows by the rules of section 14.8, and
   read `hello`, `snapshot` and `ack` lines.
2. Give every `input` an `id` that does not repeat for the lifetime of the
   bridge **process**, not merely of one connection (section 10.1).
3. Skip a line it cannot parse and keep the connection (section 2.6).
4. Treat the `hello` version as the peer's version. Across a mismatch it
   **SHOULD** refuse to send input (section 16.5).
5. Resolve an input on the app's last ack for that id, not the first
   (section 10.3).
6. Discard inputs queued while no app was connected, rather than delivering
   them to the next app instance (section 11).

A bridge MUST NOT send any handshake of its own (section 3.3).

### 1.5 Out of scope

The MCP tools `taria-mcp` exposes and their argument bounds are a bridge's
agent-facing API, not the wire protocol. The MCP tools table in the
[README](../README.md) lists them.

### 1.6 Normative artefacts

This document, together with the rustdoc on `crates/taria`, is the whole
normative statement of version 1. There is **no JSON Schema**. Every literal
line in this document is valid wire syntax, except the rejection vectors and
the other lines shown failing to parse.

## 2. Transport and framing

### 2.1 Transport

The transport is a bidirectional byte stream between the two processes. On
Linux, macOS and the other unixes it is a Unix domain socket, `AF_UNIX` with
type `SOCK_STREAM`. On Windows it is a named pipe (a kernel object, named
under `\\.\pipe\`, that two processes open like a file).

The pipe carries the same ndjson as the socket, byte for byte: everything from
section 2.2 on applies to it unchanged. An app **MUST** create the pipe in
byte mode both ways (`PIPE_TYPE_BYTE` and `PIPE_READMODE_BYTE`), so the stream
holds no message boundaries a peer could take for framing.

The app binds and listens, or on Windows creates the pipe and waits for a
client; the bridge connects. This document says *socket* for either where
the two do not differ. Discovery is section 14.

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
  **MUST** discard that partial line, so an unterminated trailing message is
  lost in silence.
- A sender **MUST NOT** put two JSON values on one line: the whole line fails.
- A sender **MUST NOT** prefix the stream with a byte order mark: U+FEFF is
  not JSON whitespace, so it fails the parse.
- The terminator is a single `\n`. A sender **MUST NOT** frame with CRLF, and
  a peer **MUST NOT** rely on a reader tolerating a trailing `\r`.
- A sender **MUST NOT** repeat a key in one object. A duplicate of a field the
  reader knows may fail the whole line, as it does in the reference readers.
- A reader **MUST NOT** require anything of key order or of whitespace inside
  a line.

### 2.3 Character encoding

Lines are UTF-8, and a line that is not fails whole. A peer **MUST** send
UTF-8 and **MUST NOT** assume a reader repairs invalid sequences.

### 2.4 Blank lines

A blank line is not a message. A sender **MUST NOT** emit one. A reader
**MUST NOT** drop the connection over one: it is skipped exactly like an
unparseable line (section 2.6).

### 2.5 Line length cap

Both peers refuse a line longer than **1 MiB (1048576 bytes)**. Rust peers
read the number from `taria::wire::MAX_LINE_BYTES`.

- The cap is measured in **bytes**, not characters.
- The terminating `\n` does **not** count. Content of exactly 1048576 bytes
  plus its newline (1048577 bytes on the wire) is within the cap; content of
  1048577 bytes is not.
- Exceeding the cap **MUST** be treated as a broken connection, not as a
  skippable line. A reader that reads past the cap without seeing a newline
  has no way to find the start of the next message, so it closes the
  connection.

A bridge therefore bounds every payload it accepts from an agent before it
goes on the wire (section 12.1, rule 5). On the app's side, a snapshot is one
line, so the cap bounds the whole tree, and an app **SHOULD** keep every
snapshot within it. One that does not loses its bridge on every reconnect,
since an app resends its latest snapshot to every new connection (section
3.4).

### 2.6 Unparseable lines

A reader **MUST** skip a line it cannot parse and **MUST** keep the connection
open. This is half of what makes additive change safe (section 16): a peer
that dropped the connection on an unknown line would turn a new message
variant into an outage.

Three consequences worth stating plainly:

- **A skipped line is not acknowledged.** The id inside it is gone with it.
  An input whose `kind` is merely unknown survives this (section 9.5), but an
  input that fails to parse for any other reason, such as
  `{"type":"input","id":9,"input":{"kind":"key"}}`, draws no ack at all.
- **A reader SHOULD distinguish an unknown message `type` from a malformed
  line.** An unknown `type` is a conforming newer peer, not a broken one, so
  a reader stays quiet about it and reports the rest as malformed: that report
  is what an adopter reads when no tree is arriving.
- A reader **MUST NOT** let one bad line poison the next: it continues at the
  next `\n`.

### 2.7 Unknown fields

A reader **MUST** ignore object keys it does not know, at every level. This is
the other half of additive change. So
`{"type":"ack","id":7,"status":"delivered","added_later":true}` is the ack it
would be without the extra key.

## 3. Handshake

### 3.1 The app speaks first

On every accepted connection the app **MUST** send `hello` as its first line,
before any snapshot or ack:

```json
{"type":"hello","app_label":"demo-app","protocol_version":1}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | string | yes | `"hello"` |
| `app_label` | string | yes | Human-readable app name shown to agents, typically the binary name. |
| `protocol_version` | integer (u32) | yes | The version this app speaks. `1` for this specification. |
| `stamps` | boolean | no | `true` when this app stamps its snapshots on the connection by the rules of section 5.3. **Omitted when false.** |

A reader **MUST** read `stamps` leniently: anything but `true`, a wrong type
included, reads as a peer that does not stamp, and fails nothing else in the
line. An app **MUST NOT** say `"stamps":true` unless it stamps every
snapshot by the rules of section 5.3, since a bridge that believes the claim
answers "did not change" on one unchanged stamped tree.

`app_label` is free-form UTF-8. No length bound and no character restriction
is specified for it. It is not the same thing as the socket label, which does
have rules because it becomes a file name (section 14.2).

### 3.2 One per connection

The app sends exactly one `hello` per connection. A reconnecting bridge gets a
fresh `hello`, because the handshake belongs to the connection and not to the
process: a bridge **MUST** forget the peer's version when the connection ends,
or it will judge the next app by the previous one's handshake. Whether a
second `hello` on one connection is legal is not specified (Appendix B).

### 3.3 The bridge sends no handshake

A bridge **MUST NOT** send a handshake. It writes nothing at all until an
agent submits input. An app **MUST NOT** wait for anything from the bridge
before publishing.

### 3.4 What follows the handshake

Immediately after `hello` the app **SHOULD** send its latest snapshot, if it
has one, so a bridge that connects mid-session is not left with no tree until
the app's next state change. Section 14.5 gives the order of what follows.

### 3.5 An app that sends no hello

An app **MUST** send `hello`. A bridge **SHOULD** tolerate its absence rather
than dropping the connection. Without a `hello`, version-mismatch protection
(section 16.5) is only as good as the version the peer's snapshots state
(section 5.2).

## 4. Message catalogue

Both message enums are internally tagged: every line carries a `type` key
naming the message. That key frames the message on the transport and says
nothing about the tree (section 16.6).

### 4.1 App to bridge

| `type` | Direction | Purpose | Section |
|---|---|---|---|
| `hello` | app to bridge | Handshake, once per connection, first. | 3 |
| `snapshot` | app to bridge | One published state of the semantic tree. | 5 |
| `ack` | app to bridge | What became of one input. | 10 |

### 4.2 Bridge to app

| `type` | Direction | Purpose | Section |
|---|---|---|---|
| `input` | bridge to app | One piece of agent input, carrying an id the app echoes in its ack. | 9, 10 |

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

Every field marked required is required in both directions: a line missing
one fails the parse. A hello without `stamps` is from a peer that does not
stamp; a snapshot without `after_input` is unstamped.

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
an input on the connection. Section 18 shows stamped snapshots.

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
  identical to the one it last published, in which case `seq` does not move.
- The one exception is a stamp moving: section 5.3 says when a stamping app
  has to send a snapshot for an identical tree. It **SHOULD** then send the
  last snapshot again, unchanged but for `after_input` and under the **same
  `seq`**. Such a resend is not a publish, so `seq` does not move, and a reader
  that ignores `after_input` reads it as unchanged.
- No behaviour is specified for `seq` reaching the u64 maximum.

### 5.2 `protocol_version` on the snapshot

Every snapshot states the version, but version negotiation happens on the
`hello` (sections 3.5 and 16.5). A snapshot carrying `"protocol_version":2`
still parses as a version 1 snapshot, so a reader **MUST NOT** treat this
field as a validity check.

### 5.3 `after_input`: stamped snapshots

A stamp orders a tree against the bridge's inputs. Without one, a bridge can
compare trees but cannot tell a tree published after its input from one
published before it, and section 10.4 forbids it to guess from arrival order.
A reader **MUST** read `after_input` leniently: a value that is not an
unsigned integer reads as unstamped and fails nothing else in the line.

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
  after a stamped one. An adapter that queues snapshots can keep this rule by
  dropping all but the newest of them at the dequeue.
- **Acks first.** An app that answers an input `ignored` **SHOULD** send that
  ack before the first snapshot it publishes after dequeuing the input. That
  snapshot is the one a bridge credits to the input, and an `ignored` behind
  it arrives after the answer has gone out. An app that can decide only later
  **SHOULD** publish, in that first snapshot, a status node saying the input
  is under way.

A stamp covers the dequeue, not the effect. An app that publishes between
taking an input and applying it sends a stamped tree without the effect, and
a bridge credits that tree to the input. Nor does a stamp separate two inputs
applied in one drain: the one tree, stamped at the later id, carries both.

A bridge that reads stamps **SHOULD** treat them like this:

- Credit a tree to an input only once it is stamped at or after that input's
  id **in the bridge's own send order**. The protocol fixes no start and no
  step for ids (section 10.1), so the order is the bridge's to know.
- Compare a credited tree with the newest tree published before the input,
  on its `root` and never on `seq`, and answer "did not change" as soon as a
  credited tree is unchanged.
- Take a tree stamped below the input, or not stamped at all, as the new
  baseline instead of crediting it: it was published before the app had the
  input. For a burst, "below" is below its first input, and a tree stamped
  inside the burst neither answers nor moves the baseline.
- Strip `after_input` from what it hands an agent, as it strips `type`
  (section 16.6).
- Answer by the rules for a peer that does not stamp, from the call that
  shows it for the rest of the connection, when the peer's hello said
  `stamps` but its snapshots break the rules above: a tree that differs from
  the one before the input is then credited. Believing the claim would leave
  every input waiting for a stamp that never comes.

## 6. Node

One widget in the semantic tree.

```json
{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":true,"actions":["select",{"custom":"archive"}],"children":[{"id":"task-1","role":"list_item","label":"Buy milk","focused":false}]}
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

`description` and the thirteen fields from `selected` to `column_count` are
the typed state (section 6.5).

Rules a reader has to implement:

- `id` and `role` are required. A node missing either **MUST** fail, which
  fails the whole snapshot line, which the reader then skips.
- A reader **MUST** accept `label` and `value` absent, and **MUST** accept
  them as JSON `null`, which reads the same as absent.
- A reader **MUST** accept `focused` absent and read absence as `false`. A
  sender in version 1 **SHOULD** emit it on every node anyway: accepting
  absence now is what keeps dropping the field later an additive change.
- A reader **MUST** accept `relayed` absent and read absence as `false`. A
  sender **SHOULD** omit it when `false`: it marks the few nodes whose text
  came from outside the app (section 6.4).
- A reader **MUST** accept `actions` and `children` absent and read absence as
  empty.
- `focused`, `relayed`, `actions` and `children` **MUST NOT** be sent as
  `null`, and `focused` and `relayed` **MUST NOT** be sent as anything but a
  boolean. Absent and empty are the two spellings; `null` or any other wrong
  type fails the parse, and takes the whole snapshot with it.
- `id` **MUST** be a JSON string.
- A reader **MUST** accept every typed state field absent, and **MUST** read
  absence as not stated, never as `false` or zero.
- A reader **MUST** read a typed state field of the wrong type or shape as not
  stated, exactly as if it were absent, and **MUST NOT** fail the node over
  it: `"selected":"yes"`, `"checked":2`, `"checked":"partial"`, a negative or
  zero `row`, `"pos_in_set":3.5`, a `set_size` over 4294967295, a string
  where a number goes. `null` reads as absent too. This is the opposite of
  `focused` and `relayed`, whose wrong type fails the line (section 16.3).
  Vector: `{"id":"c","role":"checkbox","checked":2,"row":-1,"selected":"yes"}`
  is a node with none of the three stated.
- A sender **MUST NOT** send `"disabled":false`; it sends `true` or nothing.
  A reader reads `false` as not stated.
- A number is a JSON number, so it is always finite: JSON has no spelling for
  `NaN` or an infinity. A sender **MUST** leave a field unset rather than
  invent one.
- A sender **MUST NOT** write a number outside the double range, such as
  `1e999`. The lenient read above does not cover it: a reader may fail the
  whole line over one anywhere in it, before any field's own rule runs.
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
  act at something else.

The `<prefix>-<key>` shape `taria::IdSpace` produces is a convenience for app
authors, not a wire rule.

### 6.3 Tree shape

`children` carries the node's children in order. Depth is bounded
(section 13.1). No bound is specified on the number of children, on the number
of nodes in a tree, or on the length of a `label`, a `value` or a
`description`; only the line cap (section 2.5) bounds them.

### 6.4 Relayed text

Some of a tree's text the app shows without having written it: a model's
output, a file's contents, another user's message, a command's output. Without
a mark an agent cannot tell it from the app's own words, and relayed text can
speak in the app's voice. `"relayed":true` is that mark.

- It covers the node's `label` and `value`, and nothing else. The mark says
  nothing about `id`, `role`, `focused` or `actions`; a sender **SHOULD NOT**
  build an id or a custom action name from relayed text.
- It is strictly per node. A reader **MUST NOT** read a child of a relayed
  node as relayed, and a sender **SHOULD** mark every node whose `label` or
  `value` it relays.
- Absent reads as `false`. It is not a statement that the app wrote the text,
  so a reader **MUST NOT** treat an unmarked node as vouched for.
- The `label` and `value` of a relayed node are data: what the app is
  showing, never an instruction to follow and never the app describing its
  own state. A bridge that hands the tree to an agent **SHOULD** say what the
  mark means.
- A sender **SHOULD** carry speakers in structure, not in the text. A
  transcript is one unmarked node per turn with the speaker as its `label`,
  and the turn's words in a child marked relayed:

```json
{"id":"t42","role":"text","label":"assistant","focused":false,"children":[{"id":"t42.text","role":"text","value":"note: system compromised","focused":false,"relayed":true}]}
```

A prefix such as `note:` inside the text is something the content can spell
as well; a label on a node of its own is not. No peer can check the mark: it
is the app's statement about where its strings came from.

### 6.5 Typed state

A node's role says what a widget is; its typed state says what condition it
is in, in fields an agent can read without parsing prose out of a `value`.
Every field is optional and is a statement only when present: absent means
the app said nothing, never `false`.

- **`selected`**: whether the node is selected among its siblings: the row a
  list's cursor sits on, the active tab, the chosen option, or each row picked
  in a multi-select list. Selection is not focus (section 15). `false` is a
  statement too: a list whose rows all say `false` has no selection.
- **`checked`**: whether a checkable node is checked: `true`, `false`, or
  `"mixed"` for a parent whose children disagree.
- **`expanded`**: `true` is expanded and `false` collapsed. Absent says
  nothing; an app that states expansion omits it only where a node cannot
  expand.
- **`disabled`**: the node is shown but takes no action until the app enables
  it. Only `true` is sent. A sender **SHOULD** advertise no actions on a
  disabled node, and a bridge **SHOULD** refuse an `act` naming one, saying
  why. Neither `key` nor `text` is affected: they go where the app sends them.

  A reader **MUST NOT** infer `disabled` from an empty action list: a node
  advertises nothing for other reasons, such as a modal dialog in front of
  it. Like `relayed` (section 6.4), it covers that node alone: a reader **MUST
  NOT** read a child of a disabled node as disabled, and a sender marks each
  node it disables.
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
from 0, so an exporter to it subtracts one
([architecture.md](architecture.md#mapping-to-accesskit) has the mapping).

**Hidden** has no field: a node an app hides, a collapsed branch, a
scrolled-away row, a closed panel, is simply not published. A node's
published children can be fewer than its `set_size`: off screen, or cut to
fit the depth limit or the line. A reader **MUST NOT** read the published
children as all of them.

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
[tree-guide.md](tree-guide.md#pick-the-role-that-says-what-the-widget-is).

### 7.2 The degrade rule

A reader **MUST** map a `role` name it does not know onto `other` rather than
failing. A role is nested inside a node, so rejecting it rejects the whole
snapshot, and a reader that skips unparseable lines then goes on serving its
last tree with no error anywhere: one leaf using a role added later makes the
app look frozen.

Two further shapes a reader has to handle:

- A role **MAY** arrive as a single-key object, `{"app":null}`, which reads as
  the role that key names, and `{"sparkline":null}` as `other`. Senders
  **SHOULD** emit the bare string.
- A role that is neither a string nor an object **MUST** still be an error:
  `7` and `{}` are rejected. Degrading unknown names is not the same as
  accepting anything.

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
shadowing it.

This is what lets an action name graduate to a built-in inside version 1: an
older peer echoes the new name back as custom, and the fold makes the newer
peer's own handler fire.

The envelope is a **wire encoding, not an argument**. It carries no payload
beyond the name.

### 8.4 The degrade rule

A reader **MUST** map an action name it does not know onto a custom action
**keeping that name**, rather than failing. Keeping the name is what lets an
agent advertise it, echo it back, and have the app recognize it.

Vectors a reader has to match:

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
the verdict agree.

Every state an agent can enter **SHOULD** have an advertised way out, or the
raw key fallback becomes the only escape.

## 9. AgentInput

The `input` field of an `input` message. It is internally tagged on
**`kind`**, not on `type`: `type` frames the message on the transport, `kind`
discriminates the input inside it. The two keys sit one level apart in the
same line, and a peer that conflates them fails every input.

### 9.1 `act`

Invoke an advertised action on a node.

```json
{"type":"input","id":1,"input":{"kind":"act","node":"save","action":"activate"}}
{"type":"input","id":2,"input":{"kind":"act","node":"input","action":"set_value","value":"buy milk"}}
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
an act a modal dialog blocks, or a `set_value` carrying no value.

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
**SHOULD** answer `delivered` for a key that parses but is bound to nothing.

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
key bindings, where typed letters become commands.

**Convention, not protocol:** an adapter that lowers text into key events
makes `\n` Enter and `\t` Tab, drops `\r`, and types every other character
with no modifiers, so the same text behaves the same across frameworks.

### 9.4 Reserved and malformed kinds

- `kind` **MUST** be present. `{"text":"hello"}` with no kind is a parse
  failure, not an unknown kind.
- A known `kind` missing the field it is made of is a parse failure, not an
  unknown kind. `{"kind":"key"}` fails, the whole line is skipped, and the id
  inside it is lost. No conforming sender writes such a line: a kind's fields
  are fixed within version 1 (sections 16.2 and 16.7).
- The name `unknown` is effectively reserved: it is what the reference
  binding re-serializes an unreadable input as (section 9.5).

### 9.5 The unknown-kind fallback

A reader **MUST** accept an `input` object whose `kind` it does not know,
rather than failing the line, and **MUST** then answer the message's id.

The id sits on the message, outside the input, so a `kind` that failed the
line would take the id with it, and the agent would wait for an ack that was
never possible. The fallback carries nothing but that id.

The right answer to one is a single **`ignored` ack**, sent where the line is
parsed, with no `delivered` before it: the app truly did nothing with the
input, and the agent learns so in one round trip instead of a timeout (id 2
in section 18.2). A sender never builds this variant deliberately; it exists
only as something a reader produces. Re-serializing one yields
`{"kind":"unknown"}`.

## 10. Input ids and acknowledgement

### 10.1 `InputId`

`id` is an unsigned 64-bit integer, on both the `input` message and the `ack`
answering it.

For a JSON implementer:

- It **MUST** be a JSON integer. Rejection vectors: `"7"` (string), `7.0`
  (float), `-1` (negative).
- The full range `0` to `18446744073709551615` parses. An implementation whose
  numbers are IEEE doubles loses precision above 2^53, so a bridge in such a
  language **SHOULD** keep its counter well inside that range or carry ids as
  big integers.
- Nothing specifies a starting value or a step.

The uniqueness rule is the part that matters: **a bridge MUST NOT reuse an id
for the lifetime of its process**, not merely for the lifetime of one
connection. An app can ack an input, then die, with that ack still in flight
while the bridge serves a waiter on the next connection, and ids that never
repeat keep that ack from answering a live input.

### 10.2 The three statuses

```json
{"type":"ack","id":42,"status":"delivered"}
{"type":"ack","id":42,"status":"dropped"}
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
key that parses but is bound to nothing is `delivered`: doing nothing was the
answer. `ignored` is for input the app could not act on at all.

What a `delivered` never becomes is a `dropped`. `dropped` says the input
never reached the app at all.

### 10.3 Last ack wins

**One input MAY be acked more than once, and the last ack wins.** Two acks for
one input is the normal case, not an edge: `delivered` at the dequeue, then
`ignored` once the app has looked (ids 1 and 4 in section 18.2).

A bridge **MUST NOT** resolve a waiter on the first ack that matches an id.
Acks for one input reach the bridge in the order the app sent them, so the
newest received is the app's current answer.

### 10.4 Ordering against snapshots

An app **MUST NOT** let an ack be overtaken by a snapshot sent after it, a
newly published tree or a resend (section 5.1) alike. An agent that saw the
snapshot first could not tell whether it reflects its own input yet.

The converse is not guaranteed. An ack **MAY** arrive before a snapshot that
was published earlier, since an app may write its pending acks first. A
bridge **MUST NOT** infer, from a snapshot arriving after an ack, that the
snapshot post-dates the input. What it may read instead is the stamp
(section 5.3).

### 10.5 No fallback for an unknown status

`status` has **no** unknown-value fallback, unlike `role` and `action`. A
status name a reader does not know fails its whole ack line, id included, and
the reader skips that line. Vectors: `{"type":"ack","id":7,"status":"coalesced"}`
is a parse error, and `"Delivered"` is too, because the names are exactly the
three lowercase spellings.

Losing the ack loses only the ack, and an unacknowledged input is the safe
reading. So a status added within version 1 is safe: a bridge that cannot
read it keeps the `delivered` before it. The cost is that the app's answer
looks like silence, so a bridge **SHOULD** keep the reason a line was rejected
and surface it when it reports an input "neither acknowledged nor changed".

### 10.6 An app that never acks

An app that sends no acks at all still works. A bridge **SHOULD** fall back to
comparing trees, and **SHOULD** report the distinction rather than claiming an
effect it cannot prove.

## 11. Connection ownership

**An input belongs to the connection it arrived on.**

- An app **MUST NOT** apply an input whose connection has ended. It discards
  it instead. Without this, a `key q` sent just before the bridge went away
  can quit the app after its sender is gone, with nobody left to hear about
  it.
- The discard **MUST NOT** be acked. The peer that would read the ack is the
  one that left.
- An app **MUST NOT** send an ack for an input of an ended connection to a
  later connection. Ids are unique only within a bridge process, so the id
  can already name a live input of the next bridge's.
- A bridge **MUST** discard inputs queued while no app was connected, for the
  mirror-image reason: replaying them into a freshly connected app instance
  delivers keystrokes aimed at a UI that no longer exists.
- A bridge **MUST** forget the peer's `protocol_version` when a connection
  ends, so the next connection is not judged by the previous app's handshake
  or snapshot version.

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
5. **Modifier peeling is unbounded**, so a bridge accepting a key from an
   agent **SHOULD** bound the string's length before parsing it: a megabyte of
   `ctrl+` parses to the key it ends in, on a line over the cap (section 2.5).
6. **Modifier order does not matter on the way in.** `shift+alt+ctrl+enter`
   parses; it renders as `ctrl+alt+shift+enter`.
7. **A modifier name is case-insensitive, a single character is not.**
   `CTRL+c` parses as ctrl plus `c`; `Q` and `q` are different presses.

### 12.2 The conformance contract

A peer in another language reimplements the grammar, and **the parse/render
round-trip is the contract to reimplement against: every press the parser can
produce MUST render to a string that parses back to the same press.**

The canonical rendering is modifiers in ctrl, alt, shift order, then the base
key; `Char(' ')` renders as `space` and never as a literal space, so that a
modified press such as `ctrl+space` stays parseable; backtab renders as
`backtab` with no `shift+` prefix, and parses back with shift already set.

Round-trip vectors:

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

Rejection vectors: `+a`, `ctrl+`, `f0`, `f13`, `f01`, `ab`, `meta+x`, the
empty string.

### 12.3 What a key does

An app **SHOULD** deliver a key into its own key handling, at focus, exactly
as if the user had pressed it. An app that cannot express a parsed key in its
framework **SHOULD** answer `ignored` rather than approximate it: lowering it
to a near-miss event puts a keystroke into the app that the agent never asked
for and reports it as applied.

The key grammar may grow inside version 1 (insert, the keypad, media keys).
A key added to it is additive, because an app built against an older grammar
sees a string it cannot parse and answers `ignored`.

## 13. Limits

### 13.1 Tree depth

`MAX_NODE_DEPTH` is **32**.

Depth counts **the snapshot root as level 1**, so the limit allows the root
plus 31 levels of descendants. A leaf on its own has depth 1. Depth is the
longest branch: a root with a leaf child and a three-node chain has depth 4.

A sender **MUST NOT** publish a snapshot whose tree is deeper than 32, and a
reader **MUST** be able to parse one that is exactly 32.

The limit exists because a snapshot is one JSON object and JSON parsers bound
how far they recurse into one. Crossing it **reports nothing anywhere**: the
reader skips the line it cannot parse and goes on serving its last tree. This
is why the obligation sits on the sender, and what to do about a tree that is
too deep is the sender's choice.

### 13.2 Line length

The line cap is section 2.5. Unlike a tree that is too deep, a snapshot too
large for a line is not skipped: it ends the connection, on every reconnect.
What to do about one is the sender's choice;
[tree-guide.md](tree-guide.md#keep-the-snapshot-small-enough-to-arrive)
covers it for app authors.

### 13.3 Socket path and pipe name length

`sockaddr_un.sun_path` holds the path and its terminating NUL, so the limit
is that buffer minus one. On Windows the limit is on the pipe's whole name.

| Platform | Buffer | Longest endpoint |
|---|---|---|
| Linux and other unixes | `sun_path`, 108 | **107** bytes |
| macOS, FreeBSD, NetBSD, OpenBSD, DragonFly | `sun_path`, 104 | **103** bytes |
| Windows | none | **256** UTF-16 code units, the whole pipe name, `\\.\pipe\` included |

On a unix, length is the byte length of the path, the same count the kernel
applies. A path between the two limits binds on Linux and is refused on
macOS.

On Windows, length is counted in UTF-16 code units (a character outside the
Basic Multilingual Plane, such as most emoji, counts two). The 256 covers the
prefix, so the default name `\\.\pipe\taria-<SID>-<label>` spends 16 units on
its fixed parts and leaves the label 240 less the SID's length (section 14.8):
196 units beside a 44-character SID such as
`S-1-5-21-3623811015-3361044348-30300820-1013`, and 188 beside the longest
`S-1-5-21-` account SID, 52 characters.

Both peers **SHOULD** check this before binding or connecting rather than
letting the kernel refuse, since the kernel's refusal names neither the path,
its length, the limit, nor a way out.

### 13.4 Bounds the reference bridge adds (convention)

The reference bridge's tool bounds are agent-facing, not wire rules (section
1.5).

## 14. Socket discovery and lifecycle

### 14.1 Path resolution

Both peers derive the same default path from the same environment, or they
never meet. Precedence, highest first:

1. **`$TARIA_SOCK` verbatim**, when set and non-empty.
2. **`$XDG_RUNTIME_DIR/taria/<label>.sock`**, when `$XDG_RUNTIME_DIR` is set
   and non-empty.
3. **`<temp dir>/taria-<user>/<label>.sock`**, where `<user>` keeps one
   user's sockets apart from another's in the shared temp dir.

`<user>` is the effective uid where it is available (through `/proc/self` on
Linux), else `$USER`, else `$LOGNAME`, else the literal `default`. This
derivation lives in each peer rather than in the shared crate, so a
third-party adapter that wants to be discoverable through the fallback branch
**MUST** match it. On Windows, section 14.8 replaces this resolution.

### 14.2 The label rule

The label is formatted into a file name, so it has to be one. A label that is
empty, `.`, `..`, or that contains `/` or a NUL byte **MUST** be refused
rather than resolved.

This is checked **before** the precedence above, so a label is acceptable or
not on its own terms and cannot pass on a machine that happens to set
`$TARIA_SOCK` and fail on the next one.

The app binds **and unlinks** what the label resolves to, so a label such as
`/etc/cron.d/evil` or `../../../tmp/pwn` would otherwise name a file outside
the runtime directory.

A label may hold anything else a file name may hold: `my app`, `app.v2`,
`..app`, `app..`, `-` and `app:1` all resolve. An app that wants a path this
rejects passes the path itself. On Windows a label holding `\` is refused too
(section 14.8).

### 14.3 Directory vetting

Before binding, the app **SHOULD** create the socket's parent directory with
mode `0700` and then vet it: it **MUST** be a real directory rather than a
symlink, **MUST** be owned by the current user, and **MUST** have no group or
other permission bits. Binding is refused otherwise.

The parent directory decides who can replace or redirect the socket: a
symlink, another user's ownership, or a group-writable mode each let a local
attacker swap the socket for their own. The check uses the link's own
metadata, not the target's.

### 14.4 Freeing the path

Binding needs the path to be absent, so a socket left behind by a run that was
killed has to be unlinked first. That unlink has to be earned:

- **A socket nothing is listening on** is unlinked and the bind proceeds.
- **A socket another instance is serving** is refused (`AddrInUse`), naming
  the path and saying to give the second instance one of its own. Taking it
  over would leave the first instance unreachable.
- **Anything that is not a socket** is refused (`AlreadyExists`) and left
  untouched. The path can come verbatim from `$TARIA_SOCK`, and a typo there
  is not a reason to delete a file.

The liveness probe is a `connect` that is opened and immediately closed. An
answer that does not arrive within a short timeout refuses the bind rather
than unlinking a socket that may still be serving.

### 14.5 Serving

The app listens and **MAY** serve one client at a time. On accept it sends,
in this order:

1. the `hello`,
2. the latest snapshot, if it has one, unstamped (section 5.3),
3. every snapshot published after that, as it happens, and every resend of
   the last one with only its stamp moved, which is not a publish (section
   5.1).

With a one-at-a-time app, a second bridge's `connect` succeeds into the
listen backlog (the kernel's queue of connections the app has not accepted
yet) and is not accepted until the first bridge leaves. Until then that bridge
sees an open connection on which nobody says a word. A bridge **SHOULD**
report that state as itself rather than as a wrong socket path, since the
path is right.

Anything a waiting bridge writes sits in the kernel's socket buffer until its
turn, and nothing written before the `hello` can be a reply to a tree. An app
**MAY** discard input written before its `hello`, without acking it. A bridge
that writes nothing before the `hello` loses nothing.

On Windows there is no backlog, and section 14.8 says what a busy refusal and
a silent pipe mean there.

### 14.6 Shutdown

The app **SHOULD** remove the socket file on exit, but only while that file is
still the one it bound, compared by device and inode. A second instance that
took the path over is serving a socket of its own there, and unlinking it
would leave that instance unreachable.

Binding **MUST NOT** be able to park an app during startup: a failure to
bind leaves the app running without taria rather than waiting.

An app that writes its pending acks before it closes the connection at
shutdown keeps the answer to an input it applied in its last frame. A snapshot
still waiting then is not worth sending: it describes a UI about to be gone.

### 14.7 Reconnection

A bridge **SHOULD** reconnect with capped backoff rather than give up. A
connection that dies young without delivering a snapshot **SHOULD** keep the
backoff growing instead of resetting it, or a peer that accepts and
immediately drops pulls the bridge into a full-CPU loop.

On disconnect a bridge **SHOULD** fail tool calls fast with an error naming
the app that went away, rather than acting on a stale tree. A bridge that
stops reconnecting while nothing wants the app, and connects again when
something does, has not given up: to the app, it is a bridge that left.

### 14.8 Windows discovery

On Windows the endpoint is a named pipe (section 2.1), and this section
replaces sections 14.1, 14.3 and 14.4 there, and the socket file of section
14.6. Sections 14.2, 14.5, 14.6 and 14.7 otherwise apply, with the
differences this section states.

Two Windows terms recur. A **SID** (security identifier) is the string that
names an account, `S-1-5-21-...`. A **DACL** (discretionary access control
list) is the part of an object's security descriptor that lists who may open
it and how.

**The name.** Both peers derive the same default name, or they never meet:

1. **`$TARIA_SOCK`**, when set and non-empty, if it passes the rule below.
2. **`\\.\pipe\taria-<SID>-<label>`**, where `<SID>` is the SID string of the
   user the process runs as, read from the process token (not a thread's
   impersonation token) and written as `ConvertSidToStringSidW` writes it.

Pipe names share one namespace per machine, so the SID keeps one user's pipes
apart from another's. A peer that wants to be found by the default **MUST**
derive it exactly this way. What keeps others out is not the name but four
things below: the first-instance flag, the reject-remote flag, the DACL, and
the bridge's owner check.

**The override.** A peer **MUST** honour `$TARIA_SOCK` only when it names a
pipe on the local machine: it starts with `\\.\pipe\`, in any letter case
(`\\.\PIPE\x` is `\\.\pipe\x`), and what follows is one name, with no `\` or
`/`, no NUL, and not empty or only dots and spaces. Anything else **MUST** be
refused, not fallen back from: the user named an endpoint, and serving or
connecting somewhere else would leave the two peers apart with nothing said.
The same rule holds for a name passed whole, such as a command-line argument.

A remote name, `\\host\pipe\x`, is the reason for the rule: opening it goes
over SMB (Windows file sharing), which sends that machine the user's
credentials and puts the agent's input on the network. The other refusals
close the ways around the prefix, since Windows folds `/` into `\` and
resolves `..` before opening, so `\\.\pipe\..\UNC\host\share` leaves for SMB.

That normalisation also trims trailing dots and spaces, so a label `app.`
names the same pipe as `app`, for both peers alike. A peer **SHOULD** pass the
name as UTF-16 to the wide-character (`W`) functions: a best-fit conversion
through the ANSI code page can turn a character the rule allowed into a `\`.

**The label** is held to section 14.2's rule, and **MUST** also be refused if
it contains `\`. The whole name is limited to 256 UTF-16 code units (section
13.3).

**Creating the pipe.** The app:

- **MUST** create it as the first instance of its name
  (`FILE_FLAG_FIRST_PIPE_INSTANCE`), which fails while any instance of that
  name exists. That refuses a second app on a live name, and keeps an app from
  joining a pipe something else created first under its name.
- **MUST** reject remote clients (`PIPE_REJECT_REMOTE_CLIENTS`).
- **MUST** create it with a security descriptor whose owner is the user, and
  whose DACL grants that user alone, protected from inherited entries.
- **SHOULD** allow one instance (`nMaxInstances` 1). A second instance would
  let a second bridge connect into it and wait unserved, which a bridge reads
  as a stuck app, not a held one (below).
- **MAY** reuse the one instance across sessions: disconnect the last client
  (`DisconnectNamedPipe`) and listen again (`ConnectNamedPipe`).

**No backlog.** A one-instance pipe has no queue to wait in: while a bridge is
served, a second bridge's open fails at once with `ERROR_PIPE_BUSY`, and no
connection opens. That refusal is the only sign of an app held by another
bridge, and a second bridge never has a connection to write into before its
turn.

**Opening the pipe.** The bridge:

- **MUST** check, after the open and before it writes anything to the pipe
  or parses anything it sent, that the pipe's owner is its own user's SID,
  and otherwise close it with nothing written to it and nothing it sent
  parsed. Any process can create a pipe under the app's name before the app
  does, or after it exits. An owner that is a group the user belongs to is
  not the user. A runtime that reads into its own buffer as it registers the
  pipe still conforms, provided that read is dropped unparsed.
- **SHOULD** read an open refused as access denied (`ERROR_ACCESS_DENIED`)
  as a pipe it cannot use, not as no app there, and **SHOULD NOT** report it
  as surely not the app's, as it may a pipe another account owns. Access
  denied means either a pipe the app did not create, or a bridge with fewer
  rights than the app's user: another account, a sandbox (a restricted token
  or an AppContainer), or low integrity. A report of it names both.
- **SHOULD** open it at the `SECURITY_IDENTIFICATION` impersonation level, so
  that what serves the pipe can learn who the bridge is but cannot act as it.
- **SHOULD** retry an open refused as busy (`ERROR_PIPE_BUSY`) for a short
  while before it reads the refusal as the app held by another bridge, since
  the one instance is busy for a moment between two sessions.
- **SHOULD** read a refusal as busy that outlasts the retry as the app held
  by another bridge, not as a wrong pipe name or an app that has gone, unless
  a refusal of the pipe stands (below). Either way it keeps retrying as it
  retries any connect (section 14.7). It **SHOULD** read a missing name
  (`ERROR_FILE_NOT_FOUND`) as no app there.
- **SHOULD NOT** read an open connection that stays silent as the app held:
  the app's pipe took it, and an app greets a bridge as soon as it takes one.
  It is the app itself, suspended or stuck before its `hello`, or a pipe that
  is not a taria app at all.
- **SHOULD** keep reporting a pipe it refused, one another account owns or
  one that denied it access, on the busy refusals that follow, until a
  connect passes the owner check, finds nothing listening, or the bridge
  stops trying: an impostor's pipe stays busy for as long as it likes.

**The pipe's lifetime.** A pipe, and its name with it, lives while any handle
to it is open, on either end. An app that exits while a bridge still holds
its end leaves the name taken until that bridge closes, and a restarted app
is refused as if the old one were still serving. A bridge **SHOULD** close
its end as soon as it reads end of stream.

An app **SHOULD** retry, for a short while, a create that fails because the
name is in use (`ERROR_ACCESS_DENIED` from the first-instance flag, or
`ERROR_PIPE_BUSY`) before it reports the name in use, so that an app
restarted at once outlasts that bridge. The retry has to stay short, since
binding must not park the app's startup (section 14.6).

**Shutdown.** A pipe leaves no file behind, so section 14.6's rule on removing
the socket file has nothing to act on. Acks written at shutdown (section
14.6) still have to reach the bridge: an app **SHOULD** close its end at
shutdown rather than disconnect the client, since closing leaves what the
app wrote readable by the bridge until it has read it, and
`DisconnectNamedPipe` discards it. It **SHOULD NOT** wait for the bridge to
read it either (`FlushFileBuffers`), which can wait forever and so park the
app's exit.

## 15. Focus contract

**Every snapshot should carry exactly one focused node, and this is the
app's obligation rather than anything either peer enforces.**

- An app **MUST** publish at most one node with `focused: true`.
- An app **SHOULD** publish at least one. A tree with no focus tells an agent
  nothing about where a raw key would land.

Nothing checks this, so an app that publishes two focused nodes produces a
tree that parses and misleads.

Focus tells the agent where a raw `key` would land. It is also how an agent
aims `text`, because an app accepting typing normally focuses the surface
taking it, but what decides where typed characters go is the app.

**Focus is not selection.** A cursor sits on a row whether or not that list
owns the keyboard, so an app that publishes only focus makes a moved cursor
invisible. Publish the selection in `selected` (section 6.5), as the capture
in section 18.2 does when a `down` key moves it from `task-1` to `task-3`.

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

### 16.3 The degrade fallbacks

Neither property in 16.1 covers a value inside a message the peer does want,
so the open vocabularies, the typed state and the stamps carry their own
fallbacks. A reader **MUST** implement all six:

| Value | Unknown name reads as | Section |
|---|---|---|
| `role` | `other` | 7.2 |
| `action` | a custom action **keeping the name** | 8.4 |
| `input` `kind` | an unreadable input **keeping the message's id** | 9.5 |
| a typed state field of the wrong shape, such as a `checked` of `"partial"` | not stated, **keeping the node** | 6.1 |
| the hello's `stamps`, of any value but `true` | a peer that does not stamp, **keeping the hello** | 3.1 |
| the snapshot's `after_input` of the wrong shape | unstamped, **keeping the tree** | 5.3 |

Read strictly, a field added later would fail its node or its message, and
the reader would skip the line: an agent left on a stale tree over one field.
`focused` and `relayed` are read strictly, since a mark that is not a boolean
says nothing usable. An ack's `status` has **no** fallback (section 10.5):
losing it loses only one ack.

### 16.4 The narrower rule for bridge-to-app messages

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

### 16.5 Version mismatch

Peers on different versions disagree about the shape of every message, so an
app on another version cannot parse a single input a bridge sends it.

A bridge **SHOULD** split its tool surface by direction on a mismatch:

- **Keep the connection** and log a warning.
- **Keep reading**, for as long as the peer's snapshots still parse.
- **Refuse every input path up front**, with an error naming both versions,
  having sent nothing. Forwarding input across a mismatch leaves the agent
  waiting on a session that can never react.

Reading across a mismatch is the common case, not a guarantee: a bump is
defined by the changes that break parsing, so a peer's snapshots may all fail
to parse.

### 16.6 Passing the line on beats degrading

Degrading keeps the connection; passing the line on keeps the information. A
bridge **SHOULD** store each snapshot as **the app's own line** beside the
parse of it, and hand the line to the agent. A role, an action or a field the
bridge has never heard of then reaches the agent by name instead of flattened
into whatever its typed view could hold.

The parse stays authoritative for what the bridge **decides**, because it is
typed: validating node ids and advertised actions, and detecting change, read
the parse rather than the text. A line is passed on only after it parsed, so
a malformed one is still skipped.

The two things a bridge **SHOULD** remove from the line it passes on are the
`"type":"snapshot"` framing key, which belongs to the transport, and the
`after_input` stamp (section 5.3): an agent shown it reads an id it never sent
as a fact about the app, and a resend with only its stamp moved as a change.

A bridge that removes them by parsing the line and writing it out again keeps
every other key but not the app's spelling: key order, whitespace, string
escapes and the spelling of numbers can all change. A sender **MUST NOT**
rely on its spelling reaching the agent.

### 16.7 What forces a version bump

Anything that is not additive:

- removing a field,
- renaming a field,
- making an optional field required,
- changing what an existing field or variant **means**.

The last is the dangerous one, because an old peer parses the message
successfully and acts on the old meaning.

### 16.8 Typed state (0.4.0)

What typed state means for older peers is in [compatibility.md](compatibility.md).

### 16.9 Stamps (0.4.0)

What stamps mean for older peers is in [compatibility.md](compatibility.md).

## 17. Rust language binding (non-normative)

The Rust API (`#[non_exhaustive]` types, constructors, wildcard arms) is
documented in the [`taria` crate docs](https://docs.rs/taria).

## 18. Appendix A: a worked session

### 18.1 A complete connection

Every byte in both directions between a plain `AF_UNIX` client and a running
`taria-demo`. The tab-separated direction column is not on the wire, and each
message is followed by a single `\n`.

```
app->bridge	{"type":"hello","app_label":"taria-demo","protocol_version":1,"stamps":true}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":true,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","focused":true,"selected":true,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","focused":false,"selected":false,"checked":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":1,"input":{"kind":"act","node":"tab-done","action":"select"}}
app->bridge	{"type":"ack","id":1,"status":"delivered"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"after_input":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Done","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"selected":false,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"selected":true,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Done tasks","value":"task-2","focused":false,"children":[{"id":"task-2","role":"list_item","label":"Sketch the protocol wire types","focused":true,"selected":true,"checked":true,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-5","role":"list_item","label":"Publish a snapshot per frame","focused":false,"selected":false,"checked":true,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value","focus"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
```

What to read out of it:

- The `hello` comes first and the bridge answers nothing. The first snapshot
  follows unprompted and unstamped: nothing has been dequeued yet.
- The ack precedes the snapshot published after it, and that snapshot is
  stamped `"after_input":1`, so a bridge can credit it to input 1 whenever it
  arrives.
- Exactly one node is focused in each snapshot. The `app` root is the
  reference adapter's wrapper around the nodes the app recorded; the protocol
  requires a root node, not that one.

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
  Then the tree already published, resent under its own `seq` 1 with only
  `"after_input":1` added (sections 5.1 and 5.3).
- **id 2**: a `kind` this app has never heard of. One `ignored` ack, sent
  where the line was parsed. Never dequeued, so it stamps nothing, and no
  tree follows.
- **id 3**: a key that moved the cursor. `delivered`, then the tree that shows
  the move, stamped 3.
- **id 4**: an act naming a node that does not exist. `delivered`, then
  `ignored`, then the tree of `seq` 2 again, stamped 4.

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
   a known field may fail the line; section 2.2 forbids a sender either kind.
5. **An ack's order relative to a snapshot published before it.** Only the
   other direction is guaranteed (section 10.4).
6. **A snapshot carrying zero or more than one focused node** (section 15).
7. **Whether a bridge may send input before it has seen a `hello` or any
   snapshot.** The reference bridge refuses at the tool layer, but nothing on
   the wire forbids it. A bridge cannot rely on it: an app may discard such
   input (section 14.5).
8. **What a second bridge should do while its connection waits to be
   accepted, or on Windows after its open is refused as busy**, beyond
   reporting it (sections 14.5 and 14.8). Multiple simultaneous clients are
   deferred.
9. **Any timing requirement.** No keepalive, no heartbeat, no deadline for an
   ack, no maximum publish rate.
10. **Bounds on `app_label`, `label`, `value`, node count and children count.**
    Only the line cap bounds them (section 2.5).
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
    `after_input` from a peer whose `hello` did not say `stamps`.
