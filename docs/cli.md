# taria-mcp CLI

Every flag, environment variable and path rule the bridge binary reads.

```text
taria-mcp --socket <path>   Connect to an explicit Unix socket path
taria-mcp --app <label>     Derive the socket path for <label>
taria-mcp --help            Show help
```

Exactly one of `--socket` or `--app` is required. Any usage error exits with
status 2. On Windows the app's endpoint is a named pipe rather than a socket,
so `--socket` takes a pipe name and `--app` derives one; see
[On Windows](#on-windows).

Options:

| Flag | Effect |
|---|---|
| `--tree-format <json\|outline>` | How `read_tree`, and every input tool that answers with a tree, show the tree to the agent. `json`, the default, passes on the app's own snapshot line minus its `type` framing key. `outline` renders that same line as one indented line per node (see [The outline](#the-outline)). The wire format is the same either way. A missing or unknown value is a usage error. |
| `--idle-release <SECS>` | How long, in whole seconds from 0 to 86400 (a day), the bridge keeps the app with no tool calls before it lets go. The default is 60, and `0` never lets go. Once that long has passed, the bridge closes its connection, or leaves the listen backlog (the kernel's queue of connections the app has not accepted yet) if it was waiting behind another bridge, and stops reconnecting until the next tool call. It does not let go while an input it sent is still waiting for the app's ack, for up to 30 s past the window. A missing value, one that is not plain digits (`+5`, `5s` and `1e3` are refused; leading zeros are fine), or one past 86400 is a usage error. |

The bridge opens no connection to the app until its first tool call, whatever
`--idle-release` says: starting it, and the harness's `initialize` and
`tools/list`, or its `server/discover` under the stateless MCP lifecycle,
connect to nothing. That lifecycle is the 2026-07-28 MCP revision, where each
request carries its protocol version and capabilities in `_meta` and there is
no `initialize` handshake. The first tool call waits briefly, at most about a
second, for the connection, the app's `hello` and its tree, so it still
answers with the live tree. A bridge locked out of the app says the app is
held by another bridge, and when the holder lets go, so a retry is the way in.
It names its own `--idle-release` as the holder's, which is right for a holder
started the same way: "retry in a minute" on the default, "retry in N s" for a
window of N, and for `0` that such a holder keeps the app until its session
ends. A bridge that let go of the app itself adds, to every answer it gives
once it has reconnected (the app held by another bridge, stopped, greeted but
with no tree yet, not greeted yet, or sending a line it cannot read), that its
own disconnect was that release and not a fault: after its window with no tool
calls, or up to 30 s later while an input awaited its ack. On Linux, an app
stopped by a signal or a debugger is named as stopped, with its pid, rather
than as held, and with how to resume it: from the debugger, with `kill -CONT`,
or, for a TUI suspended as a job in its terminal (which `kill -CONT` resumes
only in the background, where its next use of the terminal stops it again),
with the user's `fg` there. An input sent to an app that stops while the
bridge is driving it gets the same diagnosis in place of "neither
acknowledged", and the app applies it only if it resumes before the bridge
lets go: within the window plus 30 s of the last tool call (90 s on the
default), or at any time with `0`.

Environment variables:

| Variable | Effect |
|---|---|
| `TARIA_SOCK` | Socket path, used verbatim. Replaces the default on the app side always, and on the bridge side only when the path is derived from `--app`; an explicit `--socket` wins over it. On Windows it is a pipe name, and only a local one is taken (see [On Windows](#on-windows)). |
| `TARIA_LOG` | Bridge log filter (tracing env-filter syntax), default `info`. Logs go to stderr; stdout carries MCP. Each time the bridge lets go of the app it logs, at `info`, how long it went without a tool call, and whether an input awaiting its ack held it past the `--idle-release` window. If inputs are still unacked when that 30 s hold runs out, the line is a `warn` that names how many, since the app may discard them. |

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
is how to name a path.

## On Windows

The app's endpoint is a named pipe (a kernel object named under `\\.\pipe\`,
which two processes open like a file), and `--help` describes the pipe
instead of the socket:

```text
taria-mcp --socket <pipe>   Connect to an explicit pipe, \\.\pipe\<name>
taria-mcp --app <label>     Derive the pipe name for <label>
```

With `--app <label>`, the bridge resolves the pipe name the same way the
app-side adapter does when it creates the pipe:

1. `$TARIA_SOCK`, if set and non-empty, which has to be a pipe name;
2. `\\.\pipe\taria-<SID>-<label>`, where `<SID>` is this user's SID (the
   security identifier Windows names an account by, `S-1-5-21-...`), read
   from the process token. If the token cannot be read the bridge says so and
   suggests `--socket \\.\pipe\<name>`; an override needs no SID.

`--socket`, and `$TARIA_SOCK` under `--app`, take only a pipe on this machine:
`\\.\pipe\` in any letter case, then one name, with no `\` or `/`, and not
empty or only dots and spaces. Anything else is a usage error, a file path and
a pipe on another machine (`\\host\pipe\name`) included, because opening a
remote pipe goes over SMB (Windows file sharing), which hands that machine your
credentials and puts the agent's input on the network. The whole name is
limited to 256 characters, counted in UTF-16 code units, and a label may not
contain `\` either. Windows trims trailing dots and spaces from a name, so
`taria.` and `taria` are the same pipe on both sides.

The bridge checks that the pipe it opened is owned by your own account before
it reads or writes anything, and a pipe has no listen backlog, so three
answers exist only on Windows. Each is quoted as `read_tree` gives it before
any tree has arrived, and every tool that needs a tree gives the same. After
a disconnect each keeps the opening callers match on,
`app '<label>' disconnected (last snapshot seq N)`, as noted under it.

- **The app is held by another bridge.** The pipe has one instance and
  another bridge has it, so the open was refused as busy. With the default
  `--idle-release`:

  ```text
  no snapshot from the app yet: the app is held by another bridge. The pipe name is right: the app's pipe refused this bridge as busy, which is what a second bridge gets, because a taria adapter serves one bridge at a time. A taria-mcp lets go of the app after 60 s with no tool calls (its default), and this bridge takes over as soon as it does, with no restart; retry in a minute. If the app stays held, another session is using it, or the holder is a taria-mcp older than 0.3.2, which holds the app until its session ends. Failing that, something other than a taria bridge holds the pipe.
  ```

  The retry sentence follows `--idle-release` as it does on Linux and macOS.
  After a disconnect the text from `The pipe name is right` on follows
  `app '<label>' disconnected (last snapshot seq N), and the app is now held
  by another bridge. `, or, if this bridge had let go of the app itself, the
  held-after-release answer replaces it, as on Linux and macOS.
- **The app accepted the connection and said nothing.** Since a held app
  refuses the open, a connection that opens and stays silent past the
  500 ms grace is the app itself, suspended or stuck. Four seconds in:

  ```text
  no snapshot from the app yet: the app accepted this bridge's connection 4.0s ago but has sent nothing, not even the taria handshake; it may be suspended or stuck. The pipe name is right, and this is not another bridge holding the app, which is refused as busy instead. Check that the app is responsive (not suspended, or paused in a debugger), then retry; this bridge needs no restart. Failing that, what serves this pipe is not a taria adapter.
  ```

  After a disconnect the text from `the app accepted` on follows
  `app '<label>' disconnected (last snapshot seq N), and this bridge has
  reconnected: `.
- **The pipe belongs to another account.** Pipe names are machine-wide, so
  anything can create one under the app's name. The bridge closes it unread
  and unwritten and logs the owner's SID:

  ```text
  no snapshot from the app yet: the pipe at this name is owned by another account than this bridge's user, so it is not the app's, and this bridge closed it without reading or writing anything. Pipe names are shared by every user on the machine, so something else created this one, before the app or after it exited, perhaps to pose as it. Ask the user to find what serves it (this bridge's log names the owner) and to restart the app once it is gone; this bridge keeps trying and connects to the app's own pipe with no restart.
  ```

  After a disconnect the text from `the pipe at this name` on follows
  `app '<label>' disconnected (last snapshot seq N), and `. It outranks every
  other answer.

Stopped-app detection reads `/proc` and is Linux only, so on Windows a
suspended app gets the second answer, without a pid, and the answer to an
input that is never acknowledged does not name a stopped app. Every answer
on Linux and macOS is as it was.

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

That is 619 characters. The same tree as JSON is 1199, so the outline is about
half. Before 0.4.0 the demo put a task's done state and the active tab in
their values, and the outline was 546 characters against 1132: the marks that
replaced those values cost 73 characters more.

The first line holds every top-level snapshot field except `root`, as
`key=<compact json>`, space-separated. Then one line per node, depth-first,
indented two spaces per level, the root at zero:

```text
<id> <role>[ *relayed][ "<label>"][ = "<value>"][ *focused][ <state marks>][ description="<text>"][ <key>=<compact json> ...][ act: <action>|<action>...]
```

- An id, a role, an action name or a field key prints bare when it is
  non-empty and made only of `A-Z`, `a-z`, `0-9` and `_ . : / @ + -`, and
  JSON-quoted otherwise. One spelled `act:` is quoted too, so the only bare
  `act:` on a line is the marker that opens its action list.
- A role sent as a string prints as that string, and one sent as a
  single-key object, `{"heading":{"level":2}}`, as its key, `heading`. A role
  in any other shape, an object with several keys or an array, prints whole as
  compact JSON rather than guessed at.
- `*relayed` appears only on a node whose `relayed` is `true`, right after
  the role, so it comes before the label and the value it is about. It says
  the app did not write them: they come from outside it, and an agent reads
  them as data. A `*` is never bare, so an id, a role or a field key spelled
  `*relayed` or `*checked` is quoted, a label, a value and a description
  always are, and every bare token starting with `*` is a mark.
- The label and the value are always JSON-quoted, so an empty value shows as
  `= ""`. `null` or absent omits them. Every quoted string, here and in the
  header and the appended fields, escapes U+2028, U+2029 and U+0085 as well
  as the newline and carriage return JSON escapes, so a label carrying any of
  them cannot split a node across two lines, even in a client that breaks
  lines on them.
- `*focused` appears only on a node whose `focused` is `true`.
- The typed state (protocol section 6.5) follows `*focused`, each mark only
  where the app states it, always in this order:

  | Field | Marks |
  | --- | --- |
  | `selected` | `*selected`, `*selected=false` |
  | `checked` | `*checked`, `*checked=false`, `*checked=mixed` |
  | `expanded` | `*expanded`, `*expanded=false` |
  | `disabled` | `*disabled` |
  | `value_now`, `value_min`, `value_max` | `*num=42[0..100]`; the brackets only with a bound, a missing part empty: `*num=42`, `*num=42[0..]`, `*num=[..100]` |
  | `pos_in_set` | `*pos=3` |
  | `set_size` | `*size=7` |
  | `row`, `column` | `*row=2 *col=3` |
  | `row_count`, `column_count` | `*rows=50 *cols=4` |

  A number prints as the app wrote it when it is an integer in the 64-bit
  range, `-0` aside, and otherwise as serde_json writes the double nearest
  it, which need not be the app's spelling: `1e3` reads `1000.0`, `-0`
  `-0.0`, `1e21` `1e+21` and `1e-07` `1e-7` (protocol section 16.6 lists
  the cases).
  `null`, absent and `"disabled":false` print nothing. A state field in a
  shape the bridge reads as not stated, such as `"selected":"yes"` or a
  `row` of 0, prints in the tail instead, as `selected="yes"`, so a mark
  always means what the bridge's parse holds. The tail is the run of
  `key=<compact json>` fields after the marks and before the actions,
  described in the next two items.
- `description`, a string, opens the tail as `description="<text>"`.
- Any other field, a field this bridge has never heard of included, is
  appended as ` key=<compact json>`, after the marks and before the actions,
  and so is a known field in a shape the line has no slot for.
- The actions end the line, after the marker `act: `, joined by `|` with no
  spaces. A node with no actions, or an empty list, has no marker. A name
  holding a space, a `|`, a quote or anything else outside the bare set is
  quoted, so `act: "a|b"|"two words"|c` is three actions.
- Every action prints as the name `act` takes for it, which is the name the
  bridge's own parse of the line gives it, the same parse `act` checks a name
  against. An app-specific action, `{"custom":"delete"}` in the JSON, prints
  as `delete`, a longer envelope such as `{"custom":"delete","icon":"x"}` as
  `delete` too, and an object in a shape a newer app might send, such as
  `{"scroll_to":{"row":3}}`, as the name the parse keeps, `scroll_to`.
  Anything else in an action object, `"icon":"x"` or the payload `{"row":3}`
  here, is not shown, because `act` takes only the name. A value no parse
  reads as an action, such as `{"custom":7}`, has no name `act` would take
  and would print whole as compact JSON, but a line carrying one is rejected
  before the bridge passes it on, so the outline never shows one.

Pass `act` an id and an action name as the line spells them. A quoted one is
a JSON string, so pass the string it holds, without the quotes.

A program can split a node line, past its indent, into tokens at its spaces
outside JSON strings, since compact JSON holds no space outside a string and a
bare token holds none at all. The marker is the first token that is exactly
`act:`, and the action list is the one token after it, split at every `|`
outside a JSON string. `scripts/e2e.py` reads the demo's outline this way.

The outline is rendered from the app's line, not from the bridge's parse of
it, so a role, an action or a field this bridge does not know reaches the
agent by name, as it does in the JSON. Only the spelling of action names comes
from the parse, because that is what `act` validates against, and the parse
keeps an action it does not know under its own name. A line the outline
cannot express is served as JSON instead. In this mode the MCP instructions
and the `read_tree` description say the tree is an outline and how a line
reads, and `act`'s description says to pass node and action as shown, a quoted
one JSON-decoded, in place of the `{"custom":"delete"}` rule. Every other word
is the same in both modes.
