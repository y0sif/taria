//! Semantic tree construction: the taria nodes published every frame.
//!
//! Pure (state in, nodes out) so the invariants are unit-testable. Ids are
//! stable across frames and across mutations: tabs and the input keep fixed
//! ids, a list item is `task-<task id>` so it keeps its id while it moves
//! between tabs and while other tasks are deleted, and the dialog nodes exist
//! only while the dialog is open.

use taria::id::IdSpace;
use taria::{Action, Node, Role};

use crate::app::{App, Focus, Tab};

/// The id space list items live in, shared with [`mod@crate::update`] so the
/// spelling that builds a task id and the one that parses it cannot drift.
pub const TASK_IDS: IdSpace = IdSpace::new("task");

/// Build the top-level semantic nodes for the current app state.
///
/// Invariants: exactly one node in the returned forest is focused, and while
/// the delete dialog is open only the dialog nodes advertise actions. The
/// rest of the tree stays visible (structure, labels, values) but inert,
/// mirroring how [`mod@crate::update`] gates keys and acts behind the modal.
///
/// Focus and selection are separate facts. `focused` says where a raw key
/// would land, so only one node carries it; the list's value says which row
/// the cursor sits on, and it is published in every state so moving the
/// cursor is observable even when the keyboard belongs to the input.
pub fn build_nodes(app: &App) -> Vec<Node> {
    let mut nodes = vec![
        tabs_node(app),
        list_node(app),
        input_node(app),
        quit_node(app),
    ];
    if let Some(dialog) = dialog_node(app) {
        nodes.push(dialog);
    }
    nodes
}

fn tabs_node(app: &App) -> Node {
    let modal = app.dialog.is_some();
    let tab_child = |id: &str, tab: Tab| {
        let mut node = Node::new(id, Role::Tab).label(tab.label());
        if !modal {
            node = node.action(Action::Select);
        }
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
    let modal = app.dialog.is_some();
    let list_focused = app.focus == Focus::List && !modal;
    let items: Vec<Node> = app
        .visible_tasks()
        .enumerate()
        .map(|(pos, task)| {
            let mut node = Node::new(TASK_IDS.id(task.id), Role::ListItem)
                .label(task.title.clone())
                .value(if task.done { "done" } else { "todo" })
                .focused(list_focused && pos == app.selection);
            if !modal {
                node = node.actions([
                    Action::Select,
                    Action::Toggle,
                    Action::Custom("delete".into()),
                ]);
            }
            node
        })
        .collect();
    let mut node = Node::new("tasks", Role::List)
        .label(match app.tab {
            Tab::Active => "Active tasks",
            Tab::Done => "Done tasks",
        })
        // An empty list would leave the frame with no focused node at all;
        // parking focus on the list itself keeps exactly one node focused.
        .focused(list_focused && items.is_empty())
        .children(items);
    // Selection is not focus. The cursor sits on a row whether or not the
    // list owns the keyboard, so the list publishes the selected item's id as
    // its own value the way the tabs node publishes the active tab. Without
    // it, a `select` sent while the input has focus would move the cursor
    // without changing anything an agent can read, and the bridge would
    // truthfully report no change for an input the app handled. An empty
    // list has no row to name and so has no value, matching the tab children
    // where an absent value means "not selected".
    if let Some(selected) = app.selected_id() {
        node = node.value(TASK_IDS.id(selected));
    }
    node
}

fn input_node(app: &App) -> Node {
    let modal = app.dialog.is_some();
    let focused = app.focus == Focus::Input && !modal;
    let mut node = Node::new("input", Role::TextInput)
        .label("New task")
        .value(app.input.clone())
        .focused(focused);
    if !modal {
        node = node.action(Action::SetValue);
        // `activate` submits the draft, and an empty one submits nothing:
        // advertising it there was the tree's only advertised-but-ignored
        // pair, which teaches an agent that an advertised action may quietly
        // do nothing. The condition is `submit_input`'s own, whitespace
        // included, so the advertisement and the behaviour cannot drift.
        if !app.input.trim().is_empty() {
            node = node.action(Action::Activate);
        }
        // The way back out, advertised only while the input holds the
        // keyboard, because that is when there is something to hand back.
        // Without it the input is a focus trap: `set_value` moves focus here
        // and nothing semantic moves it away again, so every later key an
        // agent sends is typed into the draft and only the raw-key fallback
        // escapes. `dismiss` is Esc's counterpart, which the list ignores
        // too, so the advertisement and the behaviour agree.
        if focused {
            node = node.action(Action::Dismiss);
        }
    }
    node
}

/// The way out of the app, as a node rather than as a key an agent has to
/// know about.
///
/// The footer has always told a person `[q] quit`, while the tree advertised
/// no exit at all, so an agent's only way to close the demo was the raw-key
/// fallback the project's conventions keep off the primary path (and which
/// types the letter `q` whenever the input holds the keyboard). The modal
/// strips its action like every other node's, because a dialog asking about a
/// delete is not a moment to quit.
fn quit_node(app: &App) -> Node {
    let mut node = Node::new("quit", Role::Button).label("Quit");
    if app.dialog.is_none() {
        node = node.action(Action::Activate);
    }
    node
}

fn dialog_node(app: &App) -> Option<Node> {
    let id = app.dialog?;
    let title = app.task(id).map(|task| task.title.as_str()).unwrap_or("?");
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

    /// Every id in the forest, in publication order.
    fn ids(nodes: &[Node]) -> Vec<String> {
        nodes
            .iter()
            .flat_map(|n| std::iter::once(n.id.0.clone()).chain(ids(&n.children)))
            .collect()
    }

    fn act(node: &str, action: Action) -> AgentInput {
        AgentInput::act(taria::NodeId(node.into()), action, None)
    }

    fn delete(app: &mut App, node: &str) {
        apply_agent_input(app, act(node, Action::Custom("delete".into())));
        apply_agent_input(app, act("dialog-confirm", Action::Activate));
    }

    #[test]
    fn exactly_one_focused_in_list_state() {
        let app = App::new();
        let nodes = build_nodes(&app);
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(focused_id(&nodes).as_deref(), Some("task-1"));
    }

    #[test]
    fn a_fresh_app_publishes_the_same_ids_every_run() {
        // Pinned literally because these are the ids an agent script written
        // against the demo will hold. The Done seeds (2 and 5) are absent
        // because the Active tab is the one on screen.
        assert_eq!(
            ids(&build_nodes(&App::new())),
            [
                "tabs",
                "tab-active",
                "tab-done",
                "tasks",
                "task-1",
                "task-3",
                "task-4",
                "input",
                "quit",
            ]
        );
        assert_eq!(
            ids(&build_nodes(&App::new())),
            ids(&build_nodes(&App::new())),
            "a restart must republish the same ids"
        );
    }

    #[test]
    fn deleting_a_task_leaves_every_other_node_id_unchanged() {
        let mut app = App::new();
        let before = ids(&build_nodes(&app));
        let survivors: Vec<(String, Option<String>)> = find(&build_nodes(&app), "tasks")
            .unwrap()
            .children
            .iter()
            .filter(|item| item.id.0 != "task-1")
            .map(|item| (item.id.0.clone(), item.label.clone()))
            .collect();

        delete(&mut app, "task-1");

        let after = ids(&build_nodes(&app));
        assert_eq!(
            after,
            before
                .iter()
                .filter(|id| *id != "task-1")
                .cloned()
                .collect::<Vec<_>>(),
            "only the deleted id disappears; no other id shifts"
        );
        // Ids that survived must still name the same task, which is what an
        // index-derived id could not promise.
        let nodes = build_nodes(&app);
        for (id, label) in survivors {
            assert_eq!(
                find(&nodes, &id).unwrap().label,
                label,
                "{id} must still name the same task"
            );
        }
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
        app.dialog = app.selected_id();
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
            find(&build_nodes(&app), "task-1").unwrap().value.as_deref(),
            Some("todo")
        );

        apply_agent_input(&mut app, act("task-1", Action::Toggle));
        apply_agent_input(&mut app, act("tab-done", Action::Select));

        let nodes = build_nodes(&app);
        let node = find(&nodes, "task-1").expect("same id on the Done tab");
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

        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
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
        let item = find(&nodes, "task-1").unwrap();
        assert_eq!(
            item.actions,
            vec![
                Action::Select,
                Action::Toggle,
                Action::Custom("delete".into())
            ]
        );
        let input = find(&nodes, "input").unwrap();
        assert_eq!(
            input.actions,
            vec![Action::SetValue],
            "an empty draft has nothing to submit, so `activate` is not offered"
        );
        assert_eq!(input.label.as_deref(), Some("New task"));
        let quit = find(&nodes, "quit").unwrap();
        assert_eq!(quit.actions, vec![Action::Activate]);
        assert_eq!(quit.label.as_deref(), Some("Quit"));
    }

    /// `activate` on the input was the tree's one advertised-but-ignored
    /// pair: offered whatever the draft held, and ignored whenever the draft
    /// was empty, which teaches an agent that an advertised action may do
    /// nothing. It now tracks `submit_input`'s own condition, whitespace
    /// included.
    #[test]
    fn the_input_advertises_activate_only_when_there_is_something_to_submit() {
        let mut app = App::new();
        let input_actions = |app: &App| find(&build_nodes(app), "input").unwrap().actions.clone();

        assert!(!input_actions(&app).contains(&Action::Activate));

        app.input = "   ".into();
        assert!(
            !input_actions(&app).contains(&Action::Activate),
            "a whitespace-only draft submits nothing, so it advertises nothing"
        );

        app.input = " real ".into();
        assert!(
            input_actions(&app).contains(&Action::Activate),
            "a draft that would submit advertises the way to submit it"
        );

        // And what it advertises is what the app does with it.
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            crate::update::Applied::Handled
        );
        assert!(!input_actions(&app).contains(&Action::Activate));
    }

    /// The footer has told a person `[q] quit` since the demo existed, while
    /// the tree advertised no exit at all, leaving an agent the raw-key
    /// fallback the project keeps off the primary path.
    #[test]
    fn the_tree_advertises_the_way_out_of_the_app() {
        let mut app = App::new();
        let quit = |app: &App| find(&build_nodes(app), "quit").unwrap().clone();
        assert_eq!(quit(&app).actions, vec![Action::Activate]);

        // Focus is unaffected: quitting is not where a keypress lands.
        assert!(!quit(&app).focused);
        assert_eq!(count_focused(&build_nodes(&app)), 1);

        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        assert!(
            quit(&app).actions.is_empty(),
            "the modal strips it like every other node outside the dialog"
        );
    }

    #[test]
    fn dialog_strips_actions_outside_it_and_cancel_restores_them() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        let nodes = build_nodes(&app);

        for id in ["tabs", "tab-active", "tab-done", "tasks", "input", "quit"] {
            assert!(
                find(&nodes, id).unwrap().actions.is_empty(),
                "{id} must advertise no actions while the dialog is open"
            );
        }
        assert!(
            find(&nodes, "tasks")
                .unwrap()
                .children
                .iter()
                .all(|item| item.actions.is_empty()),
            "task items must advertise no actions while the dialog is open"
        );

        // Structure, labels, and values stay visible.
        let input = find(&nodes, "input").unwrap();
        assert_eq!(input.label.as_deref(), Some("New task"));
        let task = find(&nodes, "task-1").unwrap();
        assert_eq!(task.label.as_deref(), Some(app.tasks[0].title.as_str()));
        assert_eq!(task.value.as_deref(), Some("todo"));

        // The dialog keeps its own actions.
        assert_eq!(
            find(&nodes, "dialog").unwrap().actions,
            vec![Action::Dismiss]
        );
        for id in ["dialog-confirm", "dialog-cancel"] {
            assert_eq!(find(&nodes, id).unwrap().actions, vec![Action::Activate]);
        }

        // Cancelling brings the actions back.
        apply_agent_input(&mut app, act("dialog-cancel", Action::Activate));
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "input").unwrap().actions,
            vec![Action::SetValue],
            "the draft is empty, so `activate` stays unadvertised"
        );
        assert_eq!(
            find(&nodes, "quit").unwrap().actions,
            vec![Action::Activate]
        );
        assert_eq!(
            find(&nodes, "task-1").unwrap().actions,
            vec![
                Action::Select,
                Action::Toggle,
                Action::Custom("delete".into())
            ]
        );
        assert_eq!(
            find(&nodes, "tab-active").unwrap().actions,
            vec![Action::Select]
        );
    }

    #[test]
    fn list_names_the_selected_item_in_both_focus_states() {
        let mut app = App::new();
        app.selection = 2;
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "tasks").unwrap().value.as_deref(),
            Some("task-4"),
            "the list names the row the cursor sits on"
        );
        assert_eq!(focused_id(&nodes).as_deref(), Some("task-4"));

        app.focus = Focus::Input;
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "tasks").unwrap().value.as_deref(),
            Some("task-4"),
            "the cursor is still readable once the keyboard belongs to the input"
        );
        // Focus semantics are untouched: the keyboard is the input's, so no
        // item claims focus and exactly one node in the tree does.
        assert_eq!(count_focused(&nodes), 1);
        assert_eq!(focused_id(&nodes).as_deref(), Some("input"));
    }

    /// An agent that focused the input has to be able to read its way out
    /// again, or the input is a trap it can only leave through the raw-key
    /// fallback. The advertisement tracks the keyboard, because `dismiss` is
    /// Esc's counterpart and the list ignores Esc.
    #[test]
    fn the_input_advertises_the_way_out_while_it_holds_the_keyboard() {
        let mut app = App::new();
        assert_eq!(
            find(&build_nodes(&app), "input").unwrap().actions,
            vec![Action::SetValue],
            "with the list focused there is no keyboard to hand back"
        );

        // The move an agent actually makes: `set_value` takes the keyboard.
        apply_agent_input(
            &mut app,
            AgentInput::act(
                taria::NodeId("input".into()),
                Action::SetValue,
                Some("draft".into()),
            ),
        );
        let nodes = build_nodes(&app);
        assert_eq!(focused_id(&nodes).as_deref(), Some("input"));
        assert_eq!(
            find(&nodes, "input").unwrap().actions,
            vec![Action::SetValue, Action::Activate, Action::Dismiss],
            "the way out must be readable from the tree that trapped the focus"
        );

        // And it works: the draft is gone and the keyboard is the list's.
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Dismiss)),
            crate::update::Applied::Handled
        );
        let nodes = build_nodes(&app);
        assert_eq!(find(&nodes, "input").unwrap().value.as_deref(), Some(""));
        assert_eq!(focused_id(&nodes).as_deref(), Some("task-1"));
        assert_eq!(
            find(&nodes, "input").unwrap().actions,
            vec![Action::SetValue],
            "the draft went with the dismiss, and with it the way to submit it"
        );
    }

    #[test]
    fn selecting_while_the_input_has_focus_changes_the_published_tree() {
        let mut app = App::new();
        app.focus = Focus::Input;
        let before = build_nodes(&app);

        assert_eq!(
            apply_agent_input(&mut app, act("task-4", Action::Select)),
            crate::update::Applied::Handled
        );

        let after = build_nodes(&app);
        assert_ne!(
            before, after,
            "a handled select must be visible in the tree, or the bridge \
             reports no change for a state change that happened"
        );
        assert_eq!(
            find(&after, "tasks").unwrap().value.as_deref(),
            Some("task-4")
        );
        assert_eq!(count_focused(&after), 1);
        assert_eq!(focused_id(&after).as_deref(), Some("input"));
    }

    #[test]
    fn an_empty_list_names_no_selection() {
        let mut app = App::new();
        for task in &mut app.tasks {
            task.done = true;
        }
        app.clamp_selection();
        let nodes = build_nodes(&app);
        let list = find(&nodes, "tasks").unwrap();
        assert!(list.children.is_empty());
        assert_eq!(
            list.value, None,
            "with no row to name, the list publishes no selection"
        );
    }

    #[test]
    fn the_dialog_keeps_the_selection_readable() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-3", Action::Select));
        apply_agent_input(&mut app, act("task-3", Action::Custom("delete".into())));
        let nodes = build_nodes(&app);
        assert_eq!(
            find(&nodes, "tasks").unwrap().value.as_deref(),
            Some("task-3"),
            "values stay visible behind the modal, the selection with them"
        );
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
