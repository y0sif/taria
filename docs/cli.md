# taria-mcp CLI

Every flag, environment variable and path rule the bridge binary reads.

```text
taria-mcp --socket <path>   Connect to an explicit Unix socket path
taria-mcp --app <label>     Derive the socket path for <label>
taria-mcp --help            Show help (also -h)
```

Exactly one of `--socket` or `--app` is required. A usage error prints the
help and exits with status 2. On Windows both name a named pipe instead of a
socket; see [On Windows](#on-windows).

## Options

| Flag | Effect |
|---|---|
| `--tree-format <json\|outline>` | How `read_tree`, and every input tool that answers with a tree, show the tree to the agent. `json`, the default, passes on the app's own snapshot line minus its `type` and `after_input` keys. `outline` renders that line as one indented line per node ([The outline](#the-outline)). The wire format is the same either way. |
| `--idle-release <SECS>` | How long the bridge keeps the app with no tool calls before it lets go: whole seconds from 0 to 86400, default 60, and `0` never lets go. Plain digits only: `+5`, `5s` and `1e3` are refused. |

## Connecting on demand

A taria app serves one bridge at a time, and a harness starts every
registered server in every session, so the bridge holds the app only while
it is used. It opens no connection until its first tool call: starting it,
`initialize`, `tools/list` and the stateless lifecycle's `server/discover`
connect to nothing. The first tool call waits up to 1.2 s for the
connection, the app's `hello` and its tree, so it answers with the live tree.

After `--idle-release` with no tool calls, the bridge closes its connection,
or leaves the listen backlog if it was waiting behind another bridge, and
stops reconnecting until the next call. An input still waiting for the app's
ack holds it up to 30 s longer. A bridge locked out says the app is held by
another bridge and when the holder lets go, judged by its own
`--idle-release`, so the agent retries instead of asking you to kill
anything.

Where the platform allows it
([compatibility.md](compatibility.md#what-works-where)), the bridge tells an
app stopped by a signal, a debugger or job control from a held one, and
names it with its pid and the way to resume it: `kill -CONT` for a signal,
the debugger for a debugger stop, and `fg` in its terminal for a suspended
job.

## Environment

| Variable | Effect |
|---|---|
| `TARIA_SOCK` | Socket path, used verbatim. Replaces the default on the app side always, and on the bridge side only when the path is derived from `--app`; an explicit `--socket` wins over it. On Windows it is a pipe name, and only a local one is taken. |
| `TARIA_LOG` | Bridge log filter (tracing env-filter syntax), default `info`. Logs go to stderr; stdout carries MCP. |

## Socket path

With `--app <label>`, the bridge resolves the socket path the same way the
app-side adapter does when binding:

1. `$TARIA_SOCK`, if set and non-empty (used verbatim);
2. `$XDG_RUNTIME_DIR/taria/<label>.sock`;
3. `<temp dir>/taria-<user>/<label>.sock`, where `<user>` is the effective
   uid where it is available (through `/proc/self` on Linux), else `$USER`,
   else `$LOGNAME`, else the literal `default`.

The label is interpolated into a file name, so it has to be one. Both sides
refuse a label carrying a path separator, or `.`, `..` or empty, because the
app-side adapter binds and unlinks whatever the label resolves to. `--socket`
is how to name a path. Set `TARIA_SOCK` the same way for the app and the
bridge rather than mixing it with `--socket`.

## On Windows

The app's endpoint is a named pipe, and `--help` describes the pipe:

```text
taria-mcp --socket <pipe>   Connect to an explicit pipe, \\.\pipe\<name>
taria-mcp --app <label>     Derive the pipe name for <label>
```

With `--app <label>`, the bridge takes `$TARIA_SOCK` if it is set and
non-empty, and otherwise `\\.\pipe\taria-<SID>-<label>`, where `<SID>` is
this user's security identifier, read from the process token. If the token
cannot be read the bridge says so and suggests `--socket \\.\pipe\<name>`.
How the app names and protects the pipe is in the
[integration guide](integration-guide.md#on-windows).

`--socket`, and `$TARIA_SOCK` under `--app`, take only a pipe on this
machine: `\\.\pipe\` in any letter case, then one name, with no `\` or `/`,
and not empty or only dots and spaces. Anything else is a usage error, a pipe
on another machine (`\\host\pipe\name`) included, because opening one goes
over SMB (Windows file sharing). Windows trims trailing dots and spaces from a
name, so `taria.` and `taria` are the same pipe.

The bridge checks that the pipe is owned by your own account before it writes
to it or parses anything from it, and a pipe has no listen backlog, so four
answers exist only on Windows:

| The answer says | What it means | What to do |
|---|---|---|
| the app is held by another bridge; its pipe refused this bridge as busy | Another bridge has the pipe's one instance. | Retry once the holder lets go. If it stays held, another session is using the app, or the app is suspended. |
| the app accepted the connection but has sent nothing | The app itself is suspended or stuck, since a held app refuses the open instead. | Check that the app is responsive, not suspended or paused in a debugger, then retry. |
| the pipe is owned by another account, so it is not the app's | Something else created a pipe under the app's name. The bridge read and wrote nothing, and its log names the owner. | Find what serves the pipe, end it, and restart the app. |
| the pipe denied this bridge access | Either the pipe is not the app's, or the bridge runs with fewer rights: in a sandbox, at low integrity, or as another account. | Run the bridge as the app's user, outside any sandbox and at the app's integrity level; failing that, as the row above. |

The answers Windows shares with Linux and macOS say "pipe" and "pipe name"
where those say "socket" and "socket path".

## The outline

With `--tree-format outline`, the demo app's first screen reads:

```text
protocol_version=1 seq=1
app app "taria-demo"
  tabs tabs "Tabs" = "Active"
    tab-active tab "Active" *selected act: select
    tab-done tab "Done" *selected=false act: select
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" *focused *selected *checked=false act: select|toggle|delete
    task-3 list_item "Wire up the MCP bridge" *selected=false *checked=false act: select|toggle|delete
    task-4 list_item "Record the killer demo" *selected=false *checked=false act: select|toggle|delete
  input text_input "New task" = "" act: set_value|focus
  quit button "Quit" act: activate
```

It is about half the characters of the same tree as JSON. The first line
holds the snapshot's fields other than the tree. Each node is one line,
indented two spaces per level: its id and role, the label and `= ` value in
quotes, the marks, any other field as `key=<json>`, and the actions after
`act:`, joined by `|`.

| Mark | From the field |
|---|---|
| `*relayed`, `*focused` | the field true; `*relayed` prints right after the role, before the text it covers |
| `*selected`, `*checked`, `*expanded`, `*disabled` | the bare mark for true, `=false` for false (nothing for `disabled` false), `*checked=mixed` for mixed |
| `*num=42[0..100]` | `value_now`, then `value_min` and `value_max` in brackets when a bound is stated: `*num=42`, `*num=42[0..]` |
| `*pos=`, `*size=`, `*row=`, `*col=`, `*rows=`, `*cols=` | `pos_in_set`, `set_size`, `row`, `column`, `row_count`, `column_count` |

Every mark after `*relayed` follows the value, in the table's order. A field
that is absent prints nothing, and one in a shape the bridge reads as not
stated, such as a `row` of 0, prints as `key=<json>` with the other fields.
An id, role or key spelled like a mark is quoted, so every bare `*` token is
a mark.

Pass `act` an id and an action name as the line spells them, and a quoted
one as the string it holds. The outline is rendered from the app's own line,
so a role, action or field this bridge does not know reaches the agent by
name, and a line the outline cannot express is served as JSON. In this mode
the bridge's instructions and tool descriptions describe the outline in place
of the JSON. The full grammar is in
[`tree_format.rs`](../crates/taria-mcp/src/tree_format.rs).
