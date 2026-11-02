//! The molerat tree: the same UI `tree.rs` publishes to taria, published to
//! molerat, so one app (one `App`, one screen, one state dump) can be driven
//! by either stack.
//!
//! Every molerat action handler yields a [`Message`] that is the taria act it
//! stands for (node id, action, value). The event loop turns it into the
//! `AgentInput` taria would have delivered and passes it to the same
//! `update::apply_agent_input`, so both stacks reach exactly the same state
//! changes, the modal gate and the Ignored cases included. molerat has no ack
//! channel, so the status that function returns is dropped here.
//!
//! What is advertised mirrors `tree::build_nodes` node for node: an action is
//! registered on a molerat node exactly when the taria node advertises the
//! action it maps to (see MAPPING.md), so behind the open dialog nothing
//! outside it advertises anything, on either stack.

use molerat::Molerat;
use molerat::accesskit::{Action as AkAction, ActionData, Role};
use taria::{Action, AgentInput, NodeId};

use crate::app::{App, Focus, Tab};
use crate::tree::TASK_IDS;

/// A taria act, as a molerat handler produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    node: String,
    action: Action,
    value: Option<String>,
}

impl Message {
    /// The input taria would have delivered for the same act.
    pub fn into_input(self) -> AgentInput {
        AgentInput::act(NodeId(self.node), self.action, self.value)
    }
}

/// A handler that always yields the act `action` on taria node `node`.
fn act(node: &str, action: Action) -> impl Fn() -> Message + 'static {
    let node = node.to_string();
    move || Message {
        node: node.clone(),
        action: action.clone(),
        value: None,
    }
}

/// The molerat handle: with its MCP endpoint (at `MOLERAT_MCP_ENDPOINT_FILE`
/// when set, molerat's discovery file otherwise), or, when the endpoint
/// cannot start, without one, so molerat cannot block startup any more than
/// taria can. The error is returned for `main` to print before the
/// alternate screen.
pub fn open(name: &str) -> (Molerat<Message>, Option<std::io::Error>) {
    match Molerat::new_mcp(name) {
        Ok(molerat) => (molerat, None),
        Err(err) => (Molerat::new(name), Some(err)),
    }
}

/// Publish the molerat tree for the state `app` is in: the counterpart of
/// `layer.publish(tree::build_nodes(app))`.
pub fn publish(molerat: &mut Molerat<Message>, app: &App) {
    let modal = app.dialog.is_some();
    let list_focused = app.focus == Focus::List && !modal;
    let input_focused = app.focus == Focus::Input && !modal;
    molerat.publish(|ui| {
        // Tabs: `tabs` / `tab-active` / `tab-done`.
        ui.container("tabs", Role::TabList, |ui| {
            for (key, tab) in [("tab-active", Tab::Active), ("tab-done", Tab::Done)] {
                let node = ui
                    .node(key, Role::Tab)
                    .label(tab.label())
                    .selected(app.tab == tab);
                if !modal {
                    node.on_click(act(key, Action::Select));
                }
            }
        })
        .label("Tabs")
        .value(app.tab.label());

        // The visible tab's tasks: `tasks` / `task-<n>`. Done is in the
        // description, because molerat's get_ui does not show `toggled`
        // (set too, for AccessKit consumers that read it).
        let list_label = match app.tab {
            Tab::Active => "Active tasks",
            Tab::Done => "Done tasks",
        };
        ui.list_box("tasks", list_label, |ui| {
            for (pos, task) in app.visible_tasks().enumerate() {
                let id = TASK_IDS.id(task.id);
                let node = ui
                    .list_box_option(id.clone(), task.title.clone())
                    .description(if task.done { "Done" } else { "Not done" })
                    .toggled(task.done)
                    .selected(pos == app.selection)
                    .focused(list_focused && pos == app.selection);
                if !modal {
                    node.on_click(act(&id, Action::Select))
                        .custom_action("toggle", "Toggle done", act(&id, Action::Toggle))
                        .custom_action(
                            "delete",
                            "Delete",
                            act(&id, Action::Custom("delete".into())),
                        );
                }
            }
        })
        .focused(list_focused && app.visible_len() == 0);

        // The new-task input: `input`.
        let input = ui
            .text_input("input", "New task", app.input.clone())
            .focused(input_focused);
        if !modal {
            let input = input.on_action(AkAction::SetValue, |request| Message {
                node: "input".to_string(),
                action: Action::SetValue,
                // A number is no draft: taria's set_value without a value,
                // which the app ignores.
                value: match request.data.as_ref() {
                    Some(ActionData::Value(value)) => Some(value.to_string()),
                    _ => None,
                },
            });
            let input = if app.input.trim().is_empty() {
                input
            } else {
                input.custom_action("submit", "Add task", act("input", Action::Activate))
            };
            if input_focused {
                input.on_action(AkAction::Blur, {
                    let dismiss = act("input", Action::Dismiss);
                    move |_| dismiss()
                });
            } else {
                input.on_focus(act("input", Action::Focus));
            }
        }

        // The way out: `quit`.
        let quit = ui.button("quit", "Quit");
        if !modal {
            quit.on_click(act("quit", Action::Activate));
        }

        // The confirm dialog: `dialog` / `dialog-confirm` / `dialog-cancel`.
        if let Some(id) = app.dialog {
            let title = app.task(id).map(|task| task.title.as_str()).unwrap_or("?");
            ui.container("dialog", Role::Dialog, |ui| {
                ui.button("dialog-confirm", "Delete")
                    .on_click(act("dialog-confirm", Action::Activate));
                ui.button("dialog-cancel", "Cancel")
                    .on_click(act("dialog-cancel", Action::Activate));
            })
            .label(format!("Delete task '{title}'?"))
            .focused(true)
            .custom_action(
                "dismiss",
                "Close the dialog without deleting",
                act("dialog", Action::Dismiss),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use molerat::accesskit::{ActionRequest, NodeId as AkNodeId, TreeId, TreeUpdate};

    use super::*;
    use crate::update::apply_agent_input;

    /// The published nodes as (key path label or role, node) pairs are hard
    /// to name, so tests find nodes by label and role.
    fn update(molerat: &Molerat<Message>) -> &TreeUpdate {
        molerat.last_update().expect("published")
    }

    fn node_id(molerat: &Molerat<Message>, role: Role, label: &str) -> Option<AkNodeId> {
        update(molerat)
            .nodes
            .iter()
            .find(|(_, n)| n.role() == role && n.label() == Some(label))
            .map(|(id, _)| *id)
    }

    fn request(node: AkNodeId, action: AkAction, data: Option<ActionData>) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: node,
            data,
        }
    }

    fn custom(molerat: &Molerat<Message>, node: AkNodeId, description: &str) -> ActionRequest {
        let (_, n) = update(molerat)
            .nodes
            .iter()
            .find(|(id, _)| *id == node)
            .unwrap();
        let id = n
            .custom_actions()
            .iter()
            .find(|a| a.description == description)
            .unwrap_or_else(|| panic!("no custom action {description:?}"))
            .id;
        request(
            node,
            AkAction::CustomAction,
            Some(ActionData::CustomAction(id)),
        )
    }

    /// Handle `req` and apply what it yields, as the event loop does.
    fn drive(molerat: &mut Molerat<Message>, app: &mut App, req: ActionRequest) -> bool {
        let handled = molerat.handle_action(req);
        let messages: Vec<Message> = molerat.messages().collect();
        for message in messages {
            apply_agent_input(app, message.into_input());
        }
        publish(molerat, app);
        handled
    }

    #[test]
    fn add_toggle_and_delete_through_molerat_change_the_app() {
        let mut app = App::new();
        let mut molerat = Molerat::<Message>::new("taria-demo");
        publish(&mut molerat, &app);

        let input = node_id(&molerat, Role::TextInput, "New task").unwrap();
        assert!(drive(
            &mut molerat,
            &mut app,
            request(
                input,
                AkAction::SetValue,
                Some(ActionData::Value("Benchmark the bridge".into()))
            )
        ));
        assert_eq!(app.input, "Benchmark the bridge");
        let req = custom(&molerat, input, "Add task");
        assert!(drive(&mut molerat, &mut app, req));
        assert_eq!(app.tasks.last().unwrap().title, "Benchmark the bridge");
        assert_eq!(app.input, "");

        let row = node_id(&molerat, Role::ListBoxOption, "Record the killer demo").unwrap();
        let req = custom(&molerat, row, "Toggle done");
        assert!(drive(&mut molerat, &mut app, req));
        assert!(
            app.tasks
                .iter()
                .any(|t| t.title == "Record the killer demo" && t.done)
        );

        let row = node_id(&molerat, Role::ListBoxOption, "Wire up the MCP bridge").unwrap();
        let req = custom(&molerat, row, "Delete");
        assert!(drive(&mut molerat, &mut app, req));
        assert_eq!(app.dialog, Some(3));
        let confirm = node_id(&molerat, Role::Button, "Delete").unwrap();
        assert!(drive(
            &mut molerat,
            &mut app,
            request(confirm, AkAction::Click, None)
        ));
        assert!(app.dialog.is_none());
        assert!(app.task(3).is_none());
    }

    #[test]
    fn behind_the_dialog_nothing_advertises_anything() {
        let mut app = App::new();
        app.dialog = Some(1);
        let mut molerat = Molerat::<Message>::new("taria-demo");
        publish(&mut molerat, &app);
        for (_, node) in &update(&molerat).nodes {
            let inside = matches!(node.role(), Role::Dialog)
                || (node.role() == Role::Button && node.label() != Some("Quit"));
            if !inside {
                assert!(
                    node.custom_actions().is_empty()
                        && !node.supports_action(AkAction::Click)
                        && !node.supports_action(AkAction::Focus)
                        && !node.supports_action(AkAction::SetValue),
                    "{:?} {:?} advertises an action behind the dialog",
                    node.role(),
                    node.label()
                );
            }
        }
        let quit = node_id(&molerat, Role::Button, "Quit").unwrap();
        assert!(!drive(
            &mut molerat,
            &mut app,
            request(quit, AkAction::Click, None)
        ));
        assert!(app.running);
    }

    #[test]
    fn the_input_advertises_what_the_taria_input_does() {
        let mut app = App::new();
        let mut molerat = Molerat::<Message>::new("taria-demo");
        publish(&mut molerat, &app);
        let input = node_id(&molerat, Role::TextInput, "New task").unwrap();
        let find = |molerat: &Molerat<Message>| {
            update(molerat)
                .nodes
                .iter()
                .find(|(id, _)| *id == input)
                .unwrap()
                .1
                .clone()
        };
        let node = find(&molerat);
        assert!(node.supports_action(AkAction::SetValue) && node.supports_action(AkAction::Focus));
        assert!(!node.supports_action(AkAction::Blur) && node.custom_actions().is_empty());

        assert!(drive(
            &mut molerat,
            &mut app,
            request(input, AkAction::Focus, None)
        ));
        assert_eq!(app.focus, Focus::Input);
        let node = find(&molerat);
        assert!(node.supports_action(AkAction::Blur) && !node.supports_action(AkAction::Focus));
        assert_eq!(update(&molerat).focus, input);

        assert!(drive(
            &mut molerat,
            &mut app,
            request(input, AkAction::Blur, None)
        ));
        assert_eq!(app.focus, Focus::List);
    }
}
