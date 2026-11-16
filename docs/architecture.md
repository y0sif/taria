# Architecture

How taria's pieces fit together. [protocol.md](protocol.md) is the normative
specification of `PROTOCOL_VERSION` 1 and wins wherever the two disagree.
[integration-guide.md](integration-guide.md) covers adding taria to a ratatui
app, and [comparison.md](comparison.md) how it differs from screen-level tools.

## Components

| Component | Location | Role |
|---|---|---|
| Protocol types | `crates/taria` | `Snapshot`, `Node`, `Role`, `Action`, `AgentInput`, the `wire` messages, the `key` and `socket` modules both peers share, and `id` for app authors. Pure, no I/O, one dependency (serde). |
| Ratatui adapter | `crates/taria-ratatui` | `TariaLayer` serves the app's Unix socket, or on Windows its named pipe, publishes snapshots, feeds agent input into the event loop, and acknowledges every input. `FrameRecorder` and `sem` record nodes per rendered frame. |
| MCP bridge | `crates/taria-mcp` | Stdio MCP server (rmcp) exposing `read_tree`, `act`, `key`, and `type_text`. A socket-manager task owns the connection to the app and republishes its acks. |
| Demo | `examples/demo-app` | `taria-demo`, a task-manager TUI an agent drives end to end. |

## Data flow

```text
up:   build nodes -> Snapshot -> socket -> bridge watch channel -> read_tree
      dequeue input -> Ack -> socket -> bridge broadcast channel -> tool result
down: act/key/type_text -> validation -> Input{id} -> socket -> event loop
```

The app builds its semantic nodes for the state it just drew and publishes
them as a snapshot. The bridge keeps only the latest snapshot, so agents see
current state, never a backlog. Agent input travels the same socket in the
other direction, and the app applies it in its event loop.

Every input carries an id, and the app answers that id with an ack. The ack
separates "the app acted on this" from "the app never saw it", which a
snapshot diff cannot: an unrelated redraw looks like success, and a dropped
input like a slow one.

## Input acknowledgement

The adapter acks `delivered` and `dropped` itself, and `ignored` for an input
kind it cannot read. The app acks `ignored` for an input it chose to do
nothing with
([integration-guide.md](integration-guide.md#acknowledge-what-you-ignore)).
[Protocol section 10](protocol.md#10-input-ids-and-acknowledgement) defines
them.

## On-demand connect and idle release

The adapter serves one bridge at a time, so bridges take turns:
[cli.md](cli.md#connecting-on-demand) says when `taria-mcp` connects and lets
go.

## One connection owns its inputs

- An input belongs to the connection it arrived on. Once that connection ends,
  the adapter discards the input, unapplied and unacked.
- The bridge drops its queued inputs on reconnect, and `ack` refuses an id
  whose connection has ended, since a new bridge counts ids from the start.
- Every line written gets 5 s, and full input and ack queues drop rather than
  block, so a stuck bridge never stalls the app.

## Transports

Linux and macOS use a Unix domain socket, Windows a named pipe in byte mode,
and the bytes on the connection are the same on both. The adapter and the
bridge each keep the platform part behind a `transport/` module.
On Windows the pipe is the only instance of its name, rejects remote clients
and admits only the app's user, and the bridge checks its owner before reading
or writing. [Protocol section 14](protocol.md#14-socket-discovery-and-lifecycle)
says how the peers find each other.

## Focus contract

The adapter focuses its generated `app` root when no recorded node is focused
([protocol section 15](protocol.md#15-focus-contract)).

## Mapping to AccessKit

taria's vocabulary is close to AccessKit's on purpose, so an exporter from a
taria tree to AccessKit 0.25, and through it to AT-SPI, UIA and
NSAccessibility, is mostly a table rather than a design. None ships yet. A few
entries are only a best fit, marked so, and `select`, `dismiss` and `scroll`
have no single counterpart at all:

| taria | AccessKit 0.25 |
| --- | --- |
| `label`, `value`, `description` | `set_label`, `set_value`, `set_description` |
| `focused: true` | the `TreeUpdate`'s `focus`, not a node property |
| `selected` | `set_selected(bool)` |
| `checked` | `set_toggled(Toggled::True)`, `Toggled::False`, `Toggled::Mixed` |
| `expanded` | `set_expanded(bool)` |
| `disabled: true` | `set_disabled()` |
| `value_now`, `value_min`, `value_max` | `set_numeric_value`, `set_min_numeric_value`, `set_max_numeric_value` |
| `pos_in_set` | `set_position_in_set(pos_in_set - 1)` |
| `set_size` | `set_size_of_set` |
| `row`, `column` | `set_row_index(row - 1)`, `set_column_index(column - 1)` |
| `row_count`, `column_count` | `set_row_count`, `set_column_count` |
| `relayed` | nothing: AccessKit has no mark for text from outside the app |
| `activate`, `focus`, `set_value`, custom | `Action::Click`, `Action::Focus`, `Action::SetValue`, `set_custom_actions` |
| `toggle` | on a checkbox or a switch, `Action::Click` as a best fit: AccessKit has no toggle action, and a checkbox toggles on a click; on a disclosure, `Action::Expand` or `Action::Collapse`, chosen by its `expanded` |
| `select` | none: AccessKit has no select action; `Action::Click` on the item is a best fit |
| `dismiss` | none: AccessKit has no dismiss action |
| `scroll` | none single: AccessKit's scrolling is directional (`ScrollUp`, `ScrollDown`, `ScrollLeft`, `ScrollRight`) or aimed (`ScrollIntoView`, `ScrollToPoint`, `SetScrollOffset`), while taria's `scroll` names neither |
| roles | by name (`list_item` is `Role::ListItem`, `scrollbar` `Role::ScrollBar`), except `app` `Role::Application`, `tabs` `Role::TabList`, `select` `Role::ComboBox`, `option` `Role::ListBoxOption`, `progress_bar` `Role::ProgressIndicator`, `other` `Role::Unknown` |
| `text`, `chart`, `list` (best fits) | `Role::Label`, `Role::Figure`, and `Role::List`, or `Role::ListBox` when its items can be selected |

AccessKit counts positions, rows and columns from 0, and taria from 1, as a
person counts rows and as ARIA's `aria-posinset` and `aria-rowindex` do, so
the exporter subtracts one. A count (`set_size`, `row_count`,
`column_count`) needs no shift.

## Deferred

- Rect geometry on nodes (screen coordinates, for correlating the tree with
  rendered output).
- Nesting inference: `sem`-wrapped widgets record as a flat list today;
  hierarchy comes only from explicitly built children.
- Multiple simultaneous bridge clients per app. Taking turns works without
  it: an idle taria-mcp lets the app go (see On-demand connect and idle
  release).
- Adapters for other frameworks (Bubble Tea, Textual, Ink).
- An ack status for a partially applied input, with a count of what landed.
- A role for a bounded numeric stepper: the value fields exist, the role does
  not.
- An `IdSpace::new` whose bad prefix fails the build rather than a lookup;
  today only `try_new` checks it.
- `mode` for modal editors, without which an agent cannot tell whether a key
  it sends a vim-like editor types or runs a command; `level` and scroll
  offset on nodes.
