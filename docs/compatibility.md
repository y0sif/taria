# Compatibility and limitations

What survives a version difference between two taria peers, what works on
each platform, and what taria does not do.

## Compatibility

The wire protocol is version 1 and frozen: later releases only add to it. The
Rust API may still change in a 0.x minor release, and each such change comes
with an upgrade note in CHANGELOG.md.

The minimum supported Rust version is 1.88; only a minor release raises it.
`taria-ratatui` 0.4 needs your app on ratatui 0.30.2 or a later 0.30, since
it takes that ratatui's widgets and key events.

What counts as an addition, what a peer does with a value it does not know,
and what bumps the version are in
[section 16 of the protocol spec](protocol.md#16-versioning).

| Feature | Added in | With an older bridge | With an older app |
| --- | --- | --- | --- |
| On-demand connection, `--idle-release` | 0.3.2 | The bridge holds the app until its session ends. | Not affected. |
| `relayed`, the mark on text from outside the app | 0.3.3 | Passed on in the JSON tree; the 0.3.0 to 0.3.2 outline shows `relayed=true` at the end of the line. No tool description says what it means. | Never sent, so nothing is marked. |
| Typed state ([section 6.5](protocol.md#65-typed-state)) | 0.4.0 | Passed on in the JSON tree; the outline shows `checked=false selected=true` at the end of the line, not as marks. A disabled node arrives with no actions, so `act` on it is refused as unadvertised. | States nothing, which reads as not stated, never as `false`. |
| Stamps, `after_input` ([section 5.3](protocol.md#53-after_input-stamped-snapshots)) | 0.4.0 | Answers within its 500 ms window, and reads a resend as no change. The stamp shows in the tree, and in the outline it can look like a change. | The bridge answers as 0.3.3 did and strips any stamp. |
| Windows, over a named pipe | 0.4.0 | Does not build on Windows. | Does not build on Windows. |

A Rust peer that compares whole `Snapshot`s also compares the stamp, so it
reads the resend of an unchanged tree as a change; compare `root`, as
`taria-mcp` does. In an app, the wildcard arm `#[non_exhaustive]` forces on a
`match` over `AgentInput` can swallow `Text`; the
[integration guide](integration-guide.md#the-arm-the-compiler-asks-for-hides-text)
has a lint that catches it.

## What works where

| Platform | Transport | Endpoint limit | Stopped app |
| --- | --- | --- | --- |
| Linux | Unix domain socket | 107-byte path | Named as stopped, with its pid |
| macOS | Unix domain socket | 103-byte path | Reads as held by another bridge |
| Windows | Named pipe | 256 UTF-16 code units | Reads as suspended or stuck, or as held |

Stopped-app detection reads `/proc`, so it is Linux only. The BSDs share
macOS's path limit. The prebuilt bridges are listed under
[Install](../README.md#install).

## Limitations

- One bridge per app at a time ([cli.md](cli.md#connecting-on-demand)).
- Trees are capped at 32 levels and snapshots at one line of 1 MiB. The
  adapter cuts a tree that runs over and tells the app
  ([Keep the snapshot small enough to arrive](tree-guide.md#keep-the-snapshot-small-enough-to-arrive)).
- The tree carries no geometry or rendering. An agent that needs what was
  drawn still needs a screen-level tool ([comparison.md](comparison.md)).
