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

Reading through the bridge keeps more than that. It relays the app's own
snapshot instead of re-serializing its parse of it, so an unknown role, action
or field reaches the agent under its real name; the degraded parse is what the
bridge validates and compares against, not what the agent reads.

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

- The adapter serves one bridge client per app at a time.
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
