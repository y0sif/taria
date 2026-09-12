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
   (section 14).
2. Send exactly one `hello` as the first message on every accepted connection,
   before any other message (section 3).
3. Publish `snapshot` messages whose `protocol_version` is 1, whose `seq`
   increases within one run, and whose tree obeys section 6 and the depth
   limit in section 13.
4. Answer every `input` message it reads on a live connection with at least
   one `ack` naming the same `id` (section 10). An input the app cannot act on
   is answered `ignored`, not left unanswered. The two exceptions are a line
   that did not parse, which carries no readable id (section 2.6), and an
   input whose connection has since ended, which is deliberately not acked
   (section 11).
5. Skip a line it cannot parse and keep the connection (section 2.6).
6. Never apply an input that arrived on a connection that has since ended
   (section 11).

An app MAY publish no snapshots at all (it is then a connected peer with no
tree, which the reference bridge reports as such), and MAY accept only one
bridge connection at a time.

### 1.4 What a conforming bridge MUST do

1. Connect to the app's socket and read `hello`, `snapshot` and `ack` lines.
2. Stamp every `input` with an `id` that does not repeat for the lifetime of
   the bridge **process**, not merely of one connection (section 10.1).
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
`type_text`), its 500 ms result windows, and its per-tool argument bounds are
a bridge's agent-facing API, not the wire protocol. They appear here only
where they are marked as conventions an implementer may want to match.

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

The transport is a Unix domain socket, `AF_UNIX` with type `SOCK_STREAM`. The
app binds and listens; the bridge connects. Nothing about the protocol depends
on a particular socket path, but discovery does (section 14).

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

Both peers refuse a line longer than **1 MiB (1048576 bytes)**.

- The cap is measured in **bytes**, not characters.
- The terminating `\n` does **not** count. A frame whose content is exactly
  1048576 bytes plus its newline (1048577 bytes on the wire) is accepted by
  both reference readers; content of 1048577 bytes is not.
- Exceeding the cap **MUST** be treated as a broken connection, not as a
  skippable line. A reader that reaches the cap without seeing a newline has
  no way to find the start of the next message, so both reference readers
  close the connection. The app's reader thread exits and the client is
  dropped; the bridge logs the overrun and reconnects.

Verified against the reference adapter at the boundary: an `input` line of
exactly 1048576 content bytes plus its newline was parsed and acked, and the
connection went on serving; the same line one byte longer closed the
connection, and the next write to the socket failed.

This is the reason every payload a bridge accepts from an agent has to be
bounded before it goes on the wire: one oversized `value` or one megabyte of
repeated `ctrl+` prefixes is a single legal call that cuts an app off from its
bridge.

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

| Field | Type | Required | Meaning |
|---|---|---|---|
| `type` | string | yes | `"hello"` |
| `app_label` | string | yes | Human-readable app name shown to agents, typically the binary name. |
| `protocol_version` | integer (u32) | yes | The version this app speaks. `1` for this specification. |

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
streams every subsequent publish.

### 3.5 An app that sends no hello

An app **MUST** send `hello`. A bridge **SHOULD** tolerate its absence rather
than dropping the connection: the reference bridge leaves the peer's version
unknown, which it reads as "no mismatch known", and goes on serving snapshots
and forwarding input. That path exists for adapters older than the handshake.
The cost of relying on it is that version-mismatch protection (section 16.5)
is only as good as the peer's honesty about its version.

## 4. Message catalogue

Both message enums are internally tagged: every line carries a `type` key
naming the message. That key frames the message on the transport and says
nothing about the tree, which is why a bridge relaying a snapshot to an agent
strips it.

### 4.1 App to bridge

| `type` | Direction | Purpose | Section |
|---|---|---|---|
| `hello` | app to bridge | Handshake, once per connection, first. | 3 |
| `snapshot` | app to bridge | One published state of the semantic tree. | 5 |
| `ack` | app to bridge | What became of one input. | 10 |

```json
{"type":"hello","app_label":"demo-app","protocol_version":1}
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
| `snapshot` | `protocol_version` | integer | yes | never |
| `snapshot` | `seq` | integer (u64) | yes | never |
| `snapshot` | `root` | Node | yes | never |
| `ack` | `id` | integer (u64) | yes | never |
| `ack` | `status` | string | yes | never |
| `input` | `id` | integer (u64) | yes | never |
| `input` | `input` | AgentInput | yes | never |

Every field in this table is required in both directions. Verified: removing
any one of them from an otherwise valid line fails the parse with
"missing field".

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
| `seq` | integer (u64) | yes | Publish counter within one app run. Always emitted. |
| `root` | Node | yes | The root of the tree. Always emitted. |

No snapshot field is ever omitted.

### 5.1 `seq`

`seq` increments once per **published** snapshot within one run of the app. It
says which of two snapshots from the same run is the newer one and nothing
beyond that.

- A restarted app starts counting again, so a **lower `seq` with different
  content is a change, not a stale tree**. A reader **MUST NOT** order
  snapshots by `seq` across connections, and **SHOULD** detect change by
  comparing whole snapshots rather than by `seq`.
- `seq` counts publishes, not frames. A sender **MAY** skip publishing a tree
  identical to the one it last published, in which case `seq` does not move
  (the reference adapter deduplicates exactly this way, and the whole publish
  is skipped).
- No behaviour is specified for `seq` reaching the u64 maximum.

### 5.2 `protocol_version` on the snapshot

Every snapshot states the version, and **neither reference peer reads it**.
Version negotiation happens on the `hello` (section 16.5). A `Snapshot` object
carrying `"protocol_version":2` parses without complaint into this build's
types, so a reader **MUST NOT** treat this field as a validity check.

**Not specified.** What a peer should do when a snapshot's `protocol_version`
disagrees with the `hello`'s, or with its own.

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
| `focused` | boolean | no | **always emitted**, including `false` | `false` |
| `actions` | array | no | **omitted when empty** | no actions advertised |
| `children` | array | no | **omitted when empty** | a leaf |

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
- A reader **MUST** accept `actions` and `children` absent and read absence as
  empty.
- `focused`, `actions` and `children` **MUST NOT** be sent as `null`. Absent
  and empty are the two spellings; `null` fails the parse with "invalid type:
  null" in the reference implementation, and failing takes the whole snapshot
  with it.
- `id` **MUST** be a JSON string. Verified: `{"id":7,...}` fails.

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
of nodes in a tree, or on the length of a `label` or a `value`; the 1 MiB line
cap (section 2.5) bounds all of them together, and crossing it costs the
connection rather than the tree.

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
had focus drew `delivered` then `ignored`, and no snapshot followed.

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
  inside it is lost: verified, the reference adapter sent no ack for it.
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

An app **MUST NOT** let an ack be overtaken by a snapshot published after it.
An agent that saw the snapshot first could not tell whether it reflects its own
input yet. The reference adapter enforces this by giving one thread the stream
and flushing every pending ack before the pending snapshot in each pass.

The converse is not guaranteed. An ack **MAY** arrive before a snapshot that
was published earlier, because the reference adapter batches: a pass that
finds both waiting writes the acks first whatever order they were produced in.
A bridge **MUST NOT** infer, from a snapshot arriving after an ack, that the
snapshot post-dates the input.

### 10.5 No fallback for an unknown status

`status` has **no** unknown-value fallback, unlike `role` and `action`. A
status name a reader does not know fails its whole ack line, id included, and
the reader skips that line. Verified: `{"type":"ack","id":7,"status":"coalesced"}`
is a parse error, and `"Delivered"` is too, because the names are exactly the
three lowercase spellings.

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
  ends, so the next connection is not judged by the previous app's handshake.

**Reference implementation.** The adapter tags each connection with a
generation, stamps every queued input with the generation it arrived on, and
retires that generation as the reader thread exits, before the disconnect is
observable anywhere. An input from a retired generation is discarded at
dequeue and counted in `stale_inputs()`. The same bookkeeping decides who may
still be answered: `ack` refuses an id whose connection has ended, and an id
also stops being answerable once its own connection has delivered a queue's
worth (256) of newer inputs past it, which bounds what the layer remembers and
costs at most a late ack the peer already has to survive.

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
`truncated_snapshots()` and `last_truncation()`. Cutting, because the other
two answers are the same failure: publishing the tree as built puts a line on
the wire the bridge cannot parse, and skipping the publish is that same stale
tree chosen deliberately. Only the cut still delivers the part of the tree
that is fine. The fix is nearly always to publish what the widget draws rather
than the data behind it: for a deep tree view, the expanded path and the rows
on screen.

### 13.2 Line length

1 MiB, 1048576 bytes, excluding the terminating newline. See section 2.5.

### 13.3 Socket path length

`sockaddr_un.sun_path` has to hold the path and its terminating NUL, so the
limit is that buffer minus one, and the buffer is not the same size
everywhere.

| Platform | `sun_path` | Longest path, bytes |
|---|---|---|
| Linux and other unixes | 108 | **107** |
| macOS, FreeBSD, NetBSD, OpenBSD, DragonFly | 104 | **103** |

Length is the byte length of the path, the same count the kernel applies. A
path in the band between the two binds on Linux and is refused on macOS, which
is not hypothetical: the macOS temp dir is around 49 bytes, so the fallback
path plus a long app label lands in it.

Both peers **SHOULD** check this before binding or connecting rather than
letting the kernel refuse. The kernel's own refusal names neither the path,
its length, the limit, nor a way out. The bridge checking it while parsing its
arguments is what keeps an over-long path from surfacing from inside a
reconnect loop, where it reads as "the app is not running".

### 13.4 Bounds the reference bridge adds (convention)

These are agent-facing tool bounds, not wire rules, but they exist to protect
the wire and an implementer may want to match them. Each one closes a
single-legal-call hole where an unbounded argument becomes a line over the
1 MiB cap, which costs the app its bridge.

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
2. the latest snapshot, if it has one,
3. every subsequent publish, as it happens.

Multiple simultaneous bridge clients per app are a deferred feature, not a
protocol rule. With a one-at-a-time adapter, a second bridge's `connect`
succeeds into the listen backlog and is then never accepted, so that bridge
sees an open connection on which nobody says a word. A bridge **SHOULD**
report that state as itself rather than as a wrong socket path: in that state
the bridge is connected to the right path, and sending an adopter back to
check the path is the first wrong turn a retrofit takes.

**Not specified.** What a second bridge should do about a connection nobody
accepts, beyond reporting it.

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

### 14.7 Reconnection

A bridge **SHOULD** reconnect with capped backoff rather than give up: the
reference bridge starts at 250 ms, doubles, and caps at 2 s, retrying forever.
A connection that dies young without delivering a snapshot **SHOULD** keep the
backoff growing instead of resetting it, or a peer that accepts and
immediately drops pulls the bridge into a full-CPU loop.

On disconnect a bridge **SHOULD** fail tool calls fast with an error naming
the app that went away, rather than acting on a stale tree.

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
as its own readable fact: the demo puts the selected item's node id in the
list node's `value`, which is visible in the capture in section 18 as the
list's `"value":"task-1"` moving to `"value":"task-3"` after a `down` key.

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

### 16.3 The three degrade fallbacks

Neither property in 16.1 covers a value nested inside a message the peer does
want, so the open vocabularies carry their own fallbacks. A reader **MUST**
implement all three:

| Value | Unknown reads as | Section |
|---|---|---|
| `role` | `other` | 7.2 |
| `action` | a custom action **keeping the name** | 8.4 |
| `input` `kind` | an unreadable input **keeping the message's id** | 9.5 |

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

### 16.6 Relaying beats degrading

Degrading keeps the connection; relaying keeps the information. A bridge
**SHOULD** store each snapshot as **the app's own line** beside the parse of
it, and hand the line to the agent. A role, an action or a field the bridge
has never heard of then reaches the agent by name instead of flattened into
whatever its typed view could hold: an app publishing `"role":"sparkline"` had
the agent read `"role":"other"`, with the real name on the wire the whole time.

The parse stays authoritative for what the bridge **decides**, because it is
typed: validating node ids and advertised actions, and detecting change, read
the parse rather than the text. A line is relayed only after it parsed, so a
malformed one is still skipped.

The one thing a relaying bridge **SHOULD** remove from the line is the
`"type":"snapshot"` framing key, which belongs to the transport rather than to
the tree.

### 16.7 What forces a version bump

Anything that is not additive:

- removing a field,
- renaming a field,
- making an optional field required,
- changing what an existing field or variant **means**.

The last is the dangerous one, because an old peer parses the message
successfully and acts on the old meaning.

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
**eleven**: `AppToBridge`, `BridgeToApp`, `InputStatus`, `AgentInput`,
`Action`, `Role`, `Node`, `Snapshot`, `key::Key`, `key::Modifiers` and
`key::KeyPress`. The key types are on the list even though a key travels as a
string, because the string is parsed into them and an adapter takes the result
by value.

The same reasoning reaches one level in, to the variants. A new optional field
on an existing message is the format's cheapest additive change and also the
one that breaks an outside peer hardest, because it breaks everything that
builds or destructures that message. So **six** struct-like variants are
marked too: `AppToBridge::Hello`, `AppToBridge::Ack`, `BridgeToApp::Input`,
and `AgentInput::Act`, `Key` and `Text`. `AppToBridge::Snapshot` needs
neither: it is a newtype around `Snapshot`, which is marked already.

What that costs a Rust peer:

- **Build marked variants through their constructors**, since a marked variant
  has no struct literal from outside the crate: `AppToBridge::hello`, `ack`,
  `BridgeToApp::input`, `AgentInput::act`, `key`, `text`, `Snapshot::new`,
  `Node::new` plus its chainable setters, `Modifiers::NONE` / `Modifiers::new`,
  `KeyPress::new`.
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
app->bridge	{"type":"hello","app_label":"taria-demo","protocol_version":1}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":1,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","value":"selected","focused":false,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-1","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","value":"todo","focused":true,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","value":"todo","focused":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","value":"todo","focused":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":1,"input":{"kind":"act","node":"tab-done","action":"select"}}
app->bridge	{"type":"ack","id":1,"status":"delivered"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Done","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","focused":false,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","value":"selected","focused":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Done tasks","value":"task-2","focused":false,"children":[{"id":"task-2","role":"list_item","label":"Sketch the protocol wire types","value":"done","focused":true,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-5","role":"list_item","label":"Publish a snapshot per frame","value":"done","focused":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
```

What to read out of it:

- The `hello` comes first and the bridge answers nothing.
- The first snapshot arrives unprompted, immediately after the handshake,
  because the app already had one.
- `seq` is 1, then 2. Nothing was published in between, because nothing
  changed.
- The act names a node id and an action the tree advertised on that node
  (`tab-done`, `select`). Neither was guessed.
- The ack precedes the snapshot published after it.
- The effect is legible in the next tree: the tabs node's value moved from
  `Active` to `Done`, `tab-done` gained `"value":"selected"`, and the list
  node re-labelled itself and published a different set of children. A tree
  diff, without the ack, could not have told that from an unrelated redraw.
- Exactly one node is focused in each snapshot, and it is not the root: the
  root carries `"focused":false` because a recorded node below it has focus.
- The list publishes its selection as its own `value` (`task-1`, then
  `task-2`), which is the focus-is-not-selection rule (section 15) in
  practice.

### 18.2 The four ack shapes

From a second capture against the same app, showing every acknowledgement
pattern in one session:

```
bridge->app	{"type":"input","id":1,"input":{"kind":"text","text":"hello"}}
app->bridge	{"type":"ack","id":1,"status":"delivered"}
app->bridge	{"type":"ack","id":1,"status":"ignored"}
bridge->app	{"type":"input","id":2,"input":{"kind":"paste","text":"hi","from":"clip"}}
app->bridge	{"type":"ack","id":2,"status":"ignored"}
bridge->app	{"type":"input","id":3,"input":{"kind":"key","key":"down"}}
app->bridge	{"type":"ack","id":3,"status":"delivered"}
app->bridge	{"type":"snapshot","protocol_version":1,"seq":2,"root":{"id":"app","role":"app","label":"taria-demo","focused":false,"children":[{"id":"tabs","role":"tabs","label":"Tabs","value":"Active","focused":false,"children":[{"id":"tab-active","role":"tab","label":"Active","value":"selected","focused":false,"actions":["select"]},{"id":"tab-done","role":"tab","label":"Done","focused":false,"actions":["select"]}]},{"id":"tasks","role":"list","label":"Active tasks","value":"task-3","focused":false,"children":[{"id":"task-1","role":"list_item","label":"Write the taria README","value":"todo","focused":false,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-3","role":"list_item","label":"Wire up the MCP bridge","value":"todo","focused":true,"actions":["select","toggle",{"custom":"delete"}]},{"id":"task-4","role":"list_item","label":"Record the killer demo","value":"todo","focused":false,"actions":["select","toggle",{"custom":"delete"}]}]},{"id":"input","role":"text_input","label":"New task","value":"","focused":false,"actions":["set_value"]},{"id":"quit","role":"button","label":"Quit","focused":false,"actions":["activate"]}]}}
bridge->app	{"type":"input","id":4,"input":{"kind":"act","node":"nope","action":"activate"}}
app->bridge	{"type":"ack","id":4,"status":"delivered"}
app->bridge	{"type":"ack","id":4,"status":"ignored"}
```

- **id 1**: text sent while nothing was accepting typing. Two acks, last wins.
- **id 2**: a `kind` this app has never heard of. One ack, `ignored`, sent
  where the line was parsed, with no `delivered` before it because the input
  was never queued.
- **id 3**: a key that moved the cursor. `delivered`, then the tree that shows
  the move, in that order.
- **id 4**: an act naming a node that does not exist. `delivered`, then
  `ignored`.

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
- **Every 500 ms window, queue size, backoff and retry figure** in the
  reference peers. Nothing in the protocol specifies timing.
- **The reference bridge's tool argument bounds** (section 13.4).

## 19. Appendix B: what is not specified

Collected for scanning. Each is a question version 1 does not answer, not a
behaviour a peer may rely on.

1. **A second `hello` on one connection.** Neither reference peer accepts or
   rejects one explicitly (section 3.2).
2. **A snapshot's `protocol_version` disagreeing with the `hello`'s.** Nothing
   reads the snapshot field (section 5.2).
3. **`seq` at the u64 maximum** (section 5.1).
4. **Duplicate keys naming a field the reader does not know.** A duplicate of
   a known field fails the line in the reference implementation; a sender
   **MUST NOT** emit either kind (section 2.2).
5. **An ack's order relative to a snapshot published before it.** Only the
   other direction is guaranteed (section 10.4).
6. **A snapshot carrying zero or more than one focused node** (section 15).
7. **Whether a bridge may send input before it has seen a `hello` or any
   snapshot.** The reference bridge refuses at the tool layer, but nothing on
   the wire forbids it.
8. **What a second bridge should do when its connection is never accepted**
   (section 14.5). Multiple simultaneous clients are deferred.
9. **Any timing requirement.** No keepalive, no heartbeat, no deadline for an
   ack, no maximum publish rate.
10. **Bounds on `app_label`, `label`, `value`, node count and children count.**
    Only the 1 MiB line cap bounds them, and crossing it costs the connection
    (sections 3.1, 6.3).
