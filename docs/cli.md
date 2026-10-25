# taria-mcp CLI

Every flag, environment variable and path rule the bridge binary reads.

```text
taria-mcp --socket <path>   Connect to an explicit Unix socket path
taria-mcp --app <label>     Derive the socket path for <label>
taria-mcp --help            Show help
```

Exactly one of `--socket` or `--app` is required. Any usage error exits with
status 2.

Options:

| Flag | Effect |
|---|---|
| `--tree-format <json\|outline>` | How `read_tree`, and every input tool that answers with a tree, show the tree to the agent. `json`, the default, passes on the app's own snapshot line minus its `type` framing key. `outline` renders that same line as one indented line per node (see [The outline](#the-outline)). The wire format is the same either way. A missing or unknown value is a usage error. |
| `--idle-release <SECS>` | How long, in whole seconds from 0 to 86400 (a day), the bridge keeps the app with no tool calls before it lets go. The default is 60, and `0` never lets go. Once that long has passed, the bridge closes its connection, or leaves the listen backlog (the kernel's queue of connections the app has not accepted yet) if it was waiting behind another bridge, and stops reconnecting until the next tool call. It does not let go while an input it sent is still waiting for the app's ack, for up to 30 s past the window. A missing value, one that is not plain digits (`+5`, `5s` and `1e3` are refused; leading zeros are fine), or one past 86400 is a usage error. |

The bridge opens no connection to the app until its first tool call, whatever
`--idle-release` says: starting it, and the harness's `initialize` and
`tools/list`, connect to nothing. That first call waits briefly, at most
about a second, for the connection, the app's `hello` and its tree, so it
still answers with the live tree. A bridge locked out of the app says the app
is held by another bridge, and when the holder lets go, so a retry is the way
in. It names its own `--idle-release` as the holder's, which is right for a
holder started the same way: "retry in a minute" on the default, "retry in N
s" for a window of N, and for `0` that such a holder keeps the app until its
session ends. A bridge that let go of the app itself, and came back to find it
taken or stopped, adds that its own disconnect was that release and not a
fault: after its window with no tool calls, or up to 30 s later while an input
awaited its ack. On Linux, an app stopped by a signal or a debugger is named
as stopped, with its pid, rather than as held, and with how to resume it: from
the debugger, with `kill -CONT`, or, for a TUI suspended as a job in its
terminal (which `kill -CONT` resumes only in the background, where its next
use of the terminal stops it again), with the user's `fg` there. An input sent
to an app that stops while the bridge is driving it gets the same diagnosis in
place of "neither acknowledged", and the app applies it only if it resumes
before the bridge lets go: within the window plus 30 s of the last tool call
(90 s on the default), or at any time with `0`.

Environment variables:

| Variable | Effect |
|---|---|
| `TARIA_SOCK` | Socket path, used verbatim. Replaces the default on the app side always, and on the bridge side only when the path is derived from `--app`; an explicit `--socket` wins over it. |
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

## The outline

With `--tree-format outline`, the demo app's first screen reads:

```text
protocol_version=1 seq=1
app app "taria-demo"
  tabs tabs "Tabs" = "Active"
    tab-active tab "Active" = "selected" act: select
    tab-done tab "Done" act: select
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" = "todo" *focused act: select|toggle|delete
    task-3 list_item "Wire up the MCP bridge" = "todo" act: select|toggle|delete
    task-4 list_item "Record the killer demo" = "todo" act: select|toggle|delete
  input text_input "New task" = "" act: set_value|focus
  quit button "Quit" act: activate
```

That is 546 characters. The same tree as JSON is 1132.

The first line holds every top-level snapshot field except `root`, as
`key=<compact json>`, space-separated. Then one line per node, depth-first,
indented two spaces per level, the root at zero:

```text
<id> <role>[ *relayed][ "<label>"][ = "<value>"][ *focused][ <key>=<compact json> ...][ act: <action>|<action>...]
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
  `*relayed` is quoted, and the only bare one on a line is the mark.
- The label and the value are always JSON-quoted, so an empty value shows as
  `= ""`. `null` or absent omits them. Every quoted string, here and in the
  header and the appended fields, escapes U+2028, U+2029 and U+0085 as well
  as the newline and carriage return JSON escapes, so a label carrying any of
  them cannot split a node across two lines, even in a client that breaks
  lines on them.
- `*focused` appears only on a node whose `focused` is `true`.
- Any other field, a field this bridge has never heard of included, is
  appended as ` key=<compact json>`, after `*focused` and before the actions,
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
