# dual-demo: the taria tree and the molerat tree

dual-demo is taria-demo (`examples/demo-app`) with a second tree, molerat's,
published beside taria's every frame. `app.rs`, `events.rs`, `state.rs`,
`tree.rs`, `ui.rs` and `update.rs` are taria-demo's own files, included by
path; `main.rs` adds the molerat handle, its publish and its drain;
`molerat_tree.rs` builds the molerat tree.

Every molerat handler yields the taria act it stands for (node id, action,
value), which the event loop hands to the same `update::apply_agent_input`
a taria input reaches. So both stacks change one `App` through one code path,
the modal gate and the Ignored cases included, and the state dump
(`TARIA_DEMO_STATE`) grades either. A molerat action is registered exactly
where the taria node advertises the action it maps to, so the trees advertise
the same affordances in every state.

## Nodes

| taria node (role) | molerat node (AccessKit role) | state shown |
|---|---|---|
| `app` (app, "taria-demo"), added by the layer | root (Window, "taria-demo") | |
| `tabs` (tabs, "Tabs", value = tab label) | TabList "Tabs", value = tab label | |
| `tab-active`, `tab-done` (tab) | Tab "Active", Tab "Done" | `selected` on both |
| `tasks` (list, "Active tasks" / "Done tasks", value = selected task id) | ListBox, same label, no value | `focused` when the list holds the keyboard and is empty, on both |
| `task-<n>` (list_item, label = title) | ListBoxOption, label = title | `selected` and `focused` on both; done: taria `checked`, molerat `description` "Done" / "Not done" (also `toggled`, which molerat's get_ui does not show) |
| `input` (text_input, "New task", value = draft) | TextInput "New task", value = draft | `focused` on both |
| `quit` (button, "Quit") | Button "Quit" | |
| `dialog` (dialog, "Delete task '<title>'?"), only while open | Dialog, same label | `focused` on both |
| `dialog-confirm` (button, "Delete") | Button "Delete" | |
| `dialog-cancel` (button, "Cancel") | Button "Cancel" | |

## Actions

| affordance | taria act (`act` node, action) | molerat (`perform_action`) | advertised when |
|---|---|---|---|
| select a tab | `tab-active` / `tab-done`, `select` | Tab, `click` | no dialog |
| select a task | `task-<n>`, `select` | ListBoxOption, `click` | no dialog |
| toggle done | `task-<n>`, `toggle` | ListBoxOption, custom action "Toggle done" | no dialog |
| delete (opens the dialog) | `task-<n>`, `{"custom":"delete"}` | ListBoxOption, custom action "Delete" | no dialog |
| set the draft | `input`, `set_value` + value | TextInput, `setValue` + `data: {"value": ...}` (a `numericValue` is no draft: ignored, as `set_value` without a value is) | no dialog |
| submit the draft | `input`, `activate` | TextInput, custom action "Add task" | no dialog, draft not blank |
| focus the input | `input`, `focus` | TextInput, `focus` | no dialog, input not focused |
| leave the input (discards the draft, as Esc does) | `input`, `dismiss` | TextInput, `blur` | no dialog, input focused |
| quit | `quit`, `activate` | Button "Quit", `click` | no dialog |
| confirm the delete | `dialog-confirm`, `activate` | Button "Delete", `click` | dialog open |
| cancel the delete | `dialog-cancel`, `activate` | Button "Cancel", `click` | dialog open |
| close the dialog | `dialog`, `dismiss` | Dialog, custom action "Close the dialog without deleting" | dialog open |

Behind the open dialog nothing outside it advertises anything, on either
stack: no node is marked disabled, its actions are just not offered, as
taria-demo does. molerat refuses an action a node does not advertise
("action Click is not available on node N"); taria-mcp refuses it too.

## What only one stack has

- taria's raw `key` and `type_text` tools: molerat-mcp has no raw-input
  fallback, so text goes in only through `setValue`.
- taria's acks: `act` waits for the app's verdict and returns the changed
  tree, or says the input was ignored or changed nothing. molerat's
  `perform_action` answers "Action dispatched to the application" once the
  action is queued, before the app applies it (within one 50 ms tick here);
  the agent reads the effect with another `get_ui`. Inputs the app ignores
  (a stale node id, say) are dropped silently on the molerat side.
- Node ids: taria's are the app's own strings (`task-3`), molerat's are
  numbers it allocates per key path (stable for the app's life, here `task-3`
  is a key); custom actions are numbered, and `perform_action` names them by
  `customActionId`.

## Config

`TARIA_SOCK`, `TARIA_DEMO_STATE` and `TARIA_DEMO_SLOW_MS` as taria-demo reads
them (the slow hold applies to molerat actions too);
`MOLERAT_MCP_ENDPOINT_FILE` is read by molerat itself. Anything else in the
environment is ignored.
