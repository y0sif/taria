//! Semantic tree construction: the taria nodes published every frame.
//!
//! Pure (state in, nodes out) so the invariants are unit-testable. Ids are
//! stable across frames: tabs and the input keep fixed ids, list items are
//! `task-<original index>` so a task keeps its id while it moves between
//! tabs, and the dialog nodes exist only while the dialog is open.

use taria::{Action, Node, Role};

use crate::app::{App, Focus, Tab};

/// Build the top-level semantic nodes for the current app state.
///
/// Invariant: exactly one node in the returned forest is focused.
pub fn build_nodes(app: &App) -> Vec<Node> {
    let mut nodes = vec![tabs_node(app), list_node(app), input_node(app)];
    if let Some(dialog) = dialog_node(app) {
        nodes.push(dialog);
    }
    nodes
}

fn tabs_node(app: &App) -> Node {
    let tab_child = |id: &str, tab: Tab| {
        let mut node = Node::new(id, Role::Tab)
            .label(tab.label())
            .action(Action::Select);
        if app.tab == tab {
            node = node.value("selected");
        }
        node
    };
    Node::new("tabs", Role::Tabs)
        .label("Tabs")
        .value(app.tab.label())
        .children([
            tab_child("tab-active", Tab::Active),
            tab_child("tab-done", Tab::Done),
        ])
}

fn list_node(app: &App) -> Node {
    let visible = app.visible_indices();
    let list_focused = app.focus == Focus::List && app.dialog.is_none();
    let items: Vec<Node> = visible
        .iter()
        .enumerate()
        .map(|(pos, &idx)| {
            let task = &app.tasks[idx];
            Node::new(format!("task-{idx}"), Role::ListItem)
                .label(task.title.clone())
                .value(if task.done { "done" } else { "todo" })
                .focused(list_focused && pos == app.selection)
                .actions([
                    Action::Select,
                    Action::Toggle,
                    Action::Custom("delete".into()),
                ])
        })
        .collect();
    Node::new("tasks", Role::List)
        .label(match app.tab {
            Tab::Active => "Active tasks",
            Tab::Done => "Done tasks",
        })
        // An empty list would leave the frame with no focused node at all;
        // parking focus on the list itself keeps exactly one node focused.
        .focused(list_focused && items.is_empty())
        .children(items)
}

fn input_node(app: &App) -> Node {
    Node::new("input", Role::TextInput)
        .label("New task")
        .value(app.input.clone())
        .focused(app.focus == Focus::Input && app.dialog.is_none())
        .actions([Action::SetValue, Action::Activate])
}

fn dialog_node(app: &App) -> Option<Node> {
    let idx = app.dialog?;
    let title = app
        .tasks
        .get(idx)
        .map(|task| task.title.as_str())
        .unwrap_or("?");
    Some(
        Node::new("dialog", Role::Dialog)
            .label(format!("Delete task '{title}'?"))
            .focused(true)
            .action(Action::Dismiss)
            .children([
                Node::new("dialog-confirm", Role::Button)
                    .label("Delete")
                    .action(Action::Activate),
                Node::new("dialog-cancel", Role::Button)
                    .label("Cancel")
                    .action(Action::Activate),
            ]),
    )
}

#[cfg(test)]
mod tests {
    use taria::AgentInput;

    use super::*;
    use crate::update::apply_agent_input;

    fn count_focused(nodes: &[Node]) -> usize {
        nodes
            .iter()
            .map(|n| usize::from(n.focused) + count_focused(&n.children))
            .sum()
    }

    fn find<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
        nodes.iter().find_map(|n| {
            (n.id.0 == id)
                .then_some(n)
                .or_else(|| find(&n.children, id))
        })
    }

    fn focused_id(nodes: &[Node]) -> Option<String> {
        nodes.iter().find_map(|n| {
            n.focused
                .then(|| n.id.0.clone())
                .or_else(|| focused_id(&n.children))
        })
    }

    fn act(node: &str, action: Action) -> AgentInput {
        AgentInput::Act {
            node: taria::NodeId(node.into()),
            action,
            value: None,
        }
    }

    #[test]
    fn exactly_one_focused_in_list_state() {
        let app = App::new();
        let nodes = build_nodes(&app);
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(focused_id(&nodes).as_deref(), Some("task-0"));
    }

    #[test]
    fn exactly_one_focused_in_input_state() {
        let mut app = App::new();
        app.focus = Focus::Input;
        let nodes = build_nodes(&app);
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(focused_id(&nodes).as_deref(), Some("input"));
    }

    #[test]
    fn exactly_one_focused_in_dialog_state() {
        let mut app = App::new();
        app.dialog = Some(0);
        let nodes = build_nodes(&app);
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(focused_id(&nodes).as_deref(), Some("dialog"));
        let dialog = find(&nodes, "dialog").unwrap();
        assert!(dialog.children.iter().all(|c| !c.focused));
    }

    #[test]
    fn exactly_one_focused_with_empty_visible_list() {
        let mut app = App::new();
        for task in &mut app.tasks {
            task.done = true;
        }
        app.clamp_selection();
        let nodes = build_nodes(&app);
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(
            focused_id(&nodes).as_deref(),
            Some("tasks"),
            "an empty list parks focus on the list node"
        );
    }

    #[test]
    fn task_ids_stay_stable_across_toggle_and_tab_switch() {
        let mut app = App::new();
        let title = app.tasks[0].title.clone();
        assert_eq!(
            find(&build_nodes(&app), "task-0").unwrap().value.as_deref(),
            Some("todo")
        );

        apply_agent_input(&mut app, act("task-0", Action::Toggle));
        apply_agent_input(&mut app, act("tab-done", Action::Select));

        let nodes = build_nodes(&app);
        let node = find(&nodes, "task-0").expect("same id on the Done tab");
        assert_eq!(node.label.as_deref(), Some(title.as_str()));
        assert_eq!(node.value.as_deref(), Some("done"));
    }

    #[test]
    fn tabs_reflect_selection() {
        let mut app = App::new();
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "tabs").unwrap().value.as_deref(),
            Some("Active")
        );
        assert_eq!(
            find(&nodes, "tab-active").unwrap().value.as_deref(),
            Some("selected")
        );
        assert_eq!(find(&nodes, "tab-done").unwrap().value, None);

        app.tab = Tab::Done;
        let nodes = build_nodes(&app);
        assert_eq!(find(&nodes, "tabs").unwrap().value.as_deref(), Some("Done"));
        assert_eq!(find(&nodes, "tab-active").unwrap().value, None);
        assert_eq!(
            find(&nodes, "tab-done").unwrap().value.as_deref(),
            Some("selected")
        );
        assert_eq!(
            find(&nodes, "tasks").unwrap().label.as_deref(),
            Some("Done tasks")
        );
    }

    #[test]
    fn dialog_node_appears_and_disappears() {
        let mut app = App::new();
        assert!(find(&build_nodes(&app), "dialog").is_none());

        apply_agent_input(&mut app, act("task-0", Action::Custom("delete".into())));
        let nodes = build_nodes(&app);
        let dialog = find(&nodes, "dialog").unwrap();
        assert_eq!(
            dialog.label.as_deref(),
            Some(format!("Delete task '{}'?", app.tasks[0].title).as_str())
        );
        assert!(dialog.actions.contains(&Action::Dismiss));
        assert_eq!(
            find(&nodes, "dialog-confirm").unwrap().label.as_deref(),
            Some("Delete")
        );
        assert_eq!(
            find(&nodes, "dialog-cancel").unwrap().label.as_deref(),
            Some("Cancel")
        );

        apply_agent_input(&mut app, act("dialog", Action::Dismiss));
        assert!(find(&build_nodes(&app), "dialog").is_none());
    }

    #[test]
    fn list_items_advertise_their_actions() {
        let app = App::new();
        let nodes = build_nodes(&app);
        let item = find(&nodes, "task-0").unwrap();
        assert_eq!(
            item.actions,
            vec![
                Action::Select,
                Action::Toggle,
                Action::Custom("delete".into())
            ]
        );
        let input = find(&nodes, "input").unwrap();
        assert_eq!(input.actions, vec![Action::SetValue, Action::Activate]);
        assert_eq!(input.label.as_deref(), Some("New task"));
    }

    #[test]
    fn input_value_tracks_the_draft() {
        let mut app = App::new();
        app.input = "half-typed".into();
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "input").unwrap().value.as_deref(),
            Some("half-typed")
        );
    }
}
