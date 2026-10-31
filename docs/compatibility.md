# Compatibility and limitations

What survives a version difference between two taria peers, and what taria
does not do yet.

## Compatibility

`PROTOCOL_VERSION` is 1 and the wire format is frozen. Within version 1,
changes are additive. What counts as additive, what an older peer does with a
value it does not know, and what bumps the version are
[section 16 of the protocol spec](protocol.md#16-versioning); `wire.rs` in
`crates/taria` is the same rule in code, and
[docs/architecture.md](architecture.md) explains it. So an app built against a
later taria stays readable by an agent built against this one, at the cost of
one degraded field rather than the whole tree.

Reading through the bridge keeps more than that. It passes on the app's own
snapshot instead of re-serializing its parse of it, so an unknown role, action
or field reaches the agent under its real name; the degraded parse is what the
bridge validates and compares against, not what the agent reads. The opt-in
outline format, `--tree-format outline`, is rendered from that same line, so
it keeps the names too.

The node field `relayed`, added in 0.3.3, is that case. It marks a node whose
label and value came from outside the app, a model's output or a file's
contents, for the agent to read as data. A taria-mcp older than 0.3.3 passes
it on in the JSON tree with every other field of the app's snapshot, and 0.3.0
to 0.3.2, the releases with an outline, show it in one as `relayed=true` among
the fields at the end of the line rather than as `*relayed` after the role,
but no `read_tree` description before 0.3.3 says what the mark means. An app
built before 0.3.3 never sends it, so nothing in its tree is marked, whoever
wrote it.

The typed state, added in 0.4.0, is the same case again: `description`,
`selected`, `checked`, `expanded`, `disabled`, `value_now`, `value_min`,
`value_max`, `pos_in_set`, `set_size`, `row`, `column`, `row_count` and
`column_count`, all optional and omitted when unset ([protocol section
6.5](protocol.md#65-typed-state)). With them a node's fields are, in the
order the reference `Node` writes them: `id`, `role`, `label`, `value`,
`description`, `focused`, `relayed`, the state from `selected` to
`column_count`, `actions`, `children`. A taria-mcp from 0.1.0 to 0.3.3
passes the state on in the JSON tree, because it hands the agent the app's
own line, and its outline (0.3.0 on) shows it among the fields at the end of
the line, `checked=false selected=true`, rather than as marks such as
`*checked=false *selected`. It does not refuse `act` on a disabled node
itself, but the 0.4.0 adapter publishes a disabled node with no actions, so
the older bridge refuses the act as unadvertised. An app built before 0.4.0
states nothing, which reads as not stated, never as `false`. A state field in
a shape a 0.4.0 peer cannot read costs only that field, not the node. Two
things to know as a sender: a number outside the double range, such as
`1e999`, does not count as a wrong shape, which would cost only the field: it
fails the whole line in the reference reader, which reports "number out of
range"; and an integer field written with a fraction or an exponent,
`"row":3.0` or `"row":3e0`, reads as not stated, though its value is whole.
Python's `json` writes a float as `3.0`, so convert with `int()` first.

The stamps, added in 0.4.0 for issue #17, are the third case: `after_input`
on a snapshot, the newest input the app had dequeued when it published, and
`"stamps":true` in the hello of an app that sends it ([protocol section
5.3](protocol.md#53-after_input-stamped-snapshots)). Both are optional, and
they change one behaviour of the 0.4.0 adapter: after a dequeue, a frame
identical to the last one sends that snapshot again under the same `seq`,
with only the stamp moved. Measured against the demo, with and without
`TARIA_DEMO_SLOW_MS=800`:

| Bridge | App | What the agent gets |
| --- | --- | --- |
| taria-mcp 0.4.0 | taria-ratatui 0.4.0 | Each input's own tree: an act the demo held 800 ms answered with its effect in 0.81 s, three in a row each with its own. An input that changes nothing reads "The app received this input, and its tree did not change." within a frame (50 ms, where 0.3.3 took 503 ms), and an ignored one is answered as fast. Up to 3 s for a tree, then "published no tree after it within 3000ms". The stamp is stripped from both formats. |
| taria-mcp 0.1.0 to 0.3.3 | taria-ratatui 0.4.0 | Works as with a 0.3.3 app: the 500 ms window and its texts, and the resend reads as no change. The stamp shows: `"after_input":N` in the JSON tree, and `after_input=N` at the head of the outline (0.3.0 on), which can look like a change, since two reads of one tree differ in it, and names an input id the agent never sent ([protocol section 16.9](protocol.md#169-stamps-040)). Issue #17 stays: the 800 ms act read "did not change within 500ms", and the acts after it read as unacknowledged while the demo worked through them. |
| taria-mcp 0.4.0 | taria-ratatui 0.3.3 or older, or any app whose hello does not say `stamps` | Answered exactly as 0.3.3 answered it: the 500 ms window and every text, byte for byte. A stamp from an app that did not say it stamps is stripped and ignored. |

A Rust peer that compares whole `Snapshot`s now compares the stamp too, so
against a 0.4.0 adapter it reads the resend of an unchanged tree as a change.
Compare `root`, as taria-mcp does, or `seq` and `root` together.

In Rust the same promise is `#[non_exhaustive]` on the twelve types a version-1
addition can reach, from `Role`, `Action` and `Checked` to the two wire message
enums, and on the six struct-like variants inside them, where a new optional
field would land. So a new role, key, field or message variant costs an app that
integrated taria a recompile rather than a repair, in exchange for building
messages through their constructors and ending a destructuring pattern with
`..`. An input kind this build cannot read parses as `AgentInput::Unknown`,
which keeps the input's id, so the app can still acknowledge it instead of
leaving the agent waiting.

Migrating from v0, that same recompile hides the one step that matters: the
wildcard arm it asks for on `AgentInput` is the arm that swallows
`AgentInput::Text`, so check every wildcard you add for `Text` before you
trust a green build. The
[integration guide](integration-guide.md#the-arm-the-compiler-asks-for-hides-text)
has a lint that catches it.

## Limitations

- The adapter serves one bridge client per app at a time. Agents can take
  turns: since 0.3.2, taria-mcp connects on its first tool call and lets the
  app go after 60 s with no tool calls by default (`--idle-release`). A
  taria-mcp older than 0.3.2 holds the app until its session ends.
- An app on a different `protocol_version` can still be read for as long as
  its snapshots parse, which is the common case rather than a guarantee: a
  version bump is defined by the changes that break parsing, so a peer whose
  snapshot shape moved leaves the bridge with no tree at all and `read_tree`
  reports that none has arrived. Every input tool refuses either way,
  because the app cannot parse the input messages this bridge writes.
- Unix only for now: the adapter is built on unix-only APIs, and the
  transport is a Unix domain socket bound through them. Linux is the
  tested platform.
- Socket paths are capped by AF_UNIX at the platform's `sun_path` minus the
  terminating NUL: 107 bytes on Linux, 103 on macOS and the BSDs. Set
  `$TARIA_SOCK` to a shorter path when the default is too long, inside a
  directory only you can reach: the adapter binds only under a directory you
  own that grants no group or other access, so `/tmp` is refused.
- Trees are capped at `MAX_NODE_DEPTH`, 32 levels, because a snapshot past
  that exceeds what a JSON parser will recurse into and would arrive as
  nothing. The adapter cuts deeper branches at publish and tells the app.
- Snapshots are capped at one line of `MAX_LINE_BYTES`, 1 MiB, because a
  longer line costs the app its bridge on every reconnect. The adapter trims
  at publish and tells the app (the stages are in architecture.md, Snapshot
  size): it drops any node that could never fit, shortens the longest values
  and descriptions, then labels, marking each cut with `…`, and drops more
  nodes only when that is not enough, keeping each node's first children with
  no gap apart from a node that could never fit. Once nodes are dropped, the
  strings left are cut only as far as the smaller tree needs.
