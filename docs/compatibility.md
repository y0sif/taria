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

In Rust the same promise is `#[non_exhaustive]` on the eleven types a version-1
addition can reach, from `Role` and `Action` to the two wire message enums,
and on the six struct-like variants inside them, where a new optional field
would land. So a new role, key, field or message variant costs an app that
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
  and labels, marking each cut with `…`, and drops more nodes only when that
  is not enough, keeping each node's first children with no gap apart from a
  node that could never fit. Once nodes are dropped, the strings left are cut
  only as far as the smaller tree needs.
