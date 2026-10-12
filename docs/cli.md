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
| `--tree-format <json\|outline>` | How `read_tree`, and every input tool that answers with a tree, show the tree to the agent. `json`, the default, relays the app's own snapshot line minus its `type` framing key. `outline` renders that same line as one indented line per node (see [The outline](#the-outline)). The wire format is the same either way. A missing or unknown value is a usage error. |

Environment variables:

| Variable | Effect |
|---|---|
| `TARIA_SOCK` | Socket path, used verbatim. Replaces the default on the app side always, and on the bridge side only when the path is derived from `--app`; an explicit `--socket` wins over it. |
| `TARIA_LOG` | Bridge log filter (tracing env-filter syntax), default `info`. Logs go to stderr; stdout carries MCP. |

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
    tab-active tab "Active" = "selected" [select]
    tab-done tab "Done" [select]
  tasks list "Active tasks" = "task-1"
    task-1 list_item "Write the taria README" = "todo" [select toggle delete] *focused
    task-3 list_item "Wire up the MCP bridge" = "todo" [select toggle delete]
    task-4 list_item "Record the killer demo" = "todo" [select toggle delete]
  input text_input "New task" = "" [set_value focus]
  quit button "Quit" [activate]
```

That is 525 characters. The same tree as JSON is 1132.

The first line holds every top-level snapshot field except `root`, as
`key=<compact json>`, space-separated. Then one line per node, depth-first,
indented two spaces per level, the root at zero:

```text
<id> <role>[ "<label>"][ = "<value>"][ [<action> ...]][ *focused][ <key>=<compact json> ...]
```

- An id, a role, an action name or a field key prints bare when it is
  non-empty and made only of `A-Z`, `a-z`, `0-9` and `_ . : / @ + -`, and
  JSON-quoted otherwise.
- A role sent as a string prints as that string, and one sent as a
  single-key object, `{"heading":{"level":2}}`, as its key, `heading`. A role
  in any other shape, an object with several keys or an array, prints whole as
  compact JSON rather than guessed at.
- The label and the value are always JSON-quoted, so an empty value shows as
  `= ""`. `null` or absent omits them. Every quoted string, here and in the
  header and the appended fields, escapes U+2028, U+2029 and U+0085 as well
  as the newline and carriage return JSON escapes, so a label carrying any of
  them cannot split a node across two lines, even in a client that breaks
  lines on them.
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
  before it is relayed, so the outline never shows one. Pass `act` an
  id and an action name exactly as the line spells them, without the quotes if
  one is quoted.
- `*focused` appears only on a node whose `focused` is `true`.
- Any other field, a field this bridge has never heard of included, is
  appended as ` key=<compact json>`, and so is a known field in a shape the
  line has no slot for.

The outline is rendered from the app's line, not from the bridge's parse of
it, so a role, an action or a field this bridge does not know reaches the
agent by name, as it does in the JSON. Only the spelling of action names comes
from the parse, because that is what `act` validates against, and the parse
keeps an action it does not know under its own name. A line the outline
cannot express is served as JSON instead. In this mode the MCP instructions and the `read_tree`
and `act` descriptions describe the outline rather than the JSON; everything
else they say is the same.
