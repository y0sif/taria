//! Event handling and state mutations (the Update). Never renders.
//!
//! Two entry points, both pure over [`App`]: [`update`] for terminal events
//! and [`apply_agent_input`] for taria agent input. Semantic acts address
//! nodes by the ids published in [`crate::tree::build_nodes`]; the raw-key
//! fallback lowers into the same key handler as the keyboard.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taria::{Action, AgentInput};
use taria_ratatui::to_crossterm_key;

use crate::app::{App, Focus, Tab, Task};
use crate::events::AppEvent;

pub fn update(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Key(key) => handle_key(app, key),
        AppEvent::Tick => {}
        AppEvent::Resize => {}
    }
}

/// Apply one agent input received through the taria layer.
pub fn apply_agent_input(app: &mut App, input: AgentInput) {
    match input {
        AgentInput::Act {
            node,
            action,
            value,
        } => apply_act(app, node.0.as_str(), action, value),
        AgentInput::Key { key } => {
            if let Some(key) = to_crossterm_key(&key) {
                handle_key(app, key);
            }
        }
    }
}

fn apply_act(app: &mut App, node: &str, action: Action, value: Option<String>) {
    // The delete dialog is modal: mirror `handle_key` by ignoring any act
    // that targets a node outside the dialog while it is open. The tree
    // stops advertising those actions too (see `crate::tree::build_nodes`),
    // so behavior and advertisement agree.
    if app.dialog.is_some() && !matches!(node, "dialog" | "dialog-confirm" | "dialog-cancel") {
        return;
    }
    match (node, action) {
        ("tab-active", Action::Select) => switch_tab(app, Tab::Active),
        ("tab-done", Action::Select) => switch_tab(app, Tab::Done),
        ("input", Action::SetValue) => {
            app.input = value.unwrap_or_default();
            app.focus = Focus::Input;
        }
        ("input", Action::Activate) => submit_input(app),
        ("dialog-confirm", Action::Activate) => confirm_delete(app),
        ("dialog-cancel", Action::Activate) | ("dialog", Action::Dismiss) => app.dialog = None,
        (node, action) => apply_task_act(app, node, action),
    }
}

fn apply_task_act(app: &mut App, node: &str, action: Action) {
    let Some(idx) = node
        .strip_prefix("task-")
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return;
    };
    if idx >= app.tasks.len() {
        return;
    }
    match action {
        Action::Select => {
            if let Some(pos) = app.visible_indices().iter().position(|&i| i == idx) {
                app.selection = pos;
            }
        }
        Action::Toggle => {
            app.tasks[idx].done = !app.tasks[idx].done;
            app.clamp_selection();
        }
        Action::Custom(name) if name == "delete" => app.dialog = Some(idx),
        _ => {}
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if app.dialog.is_some() {
        handle_dialog_key(app, key);
        return;
    }
    match app.focus {
        Focus::List => handle_list_key(app, key),
        Focus::Input => handle_input_key(app, key),
    }
}

// --- List focus ---

fn handle_list_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.running = false,
        KeyCode::Tab => switch_tab(app, app.tab.other()),
        KeyCode::Up | KeyCode::Char('k') => move_selection_up(app),
        KeyCode::Down | KeyCode::Char('j') => move_selection_down(app),
        KeyCode::Char(' ') => toggle_selected(app),
        KeyCode::Char('i') => app.focus = Focus::Input,
        KeyCode::Char('d') => {
            if let Some(idx) = app.selected_task() {
                app.dialog = Some(idx);
            }
        }
        _ => {}
    }
}

fn switch_tab(app: &mut App, tab: Tab) {
    if app.tab != tab {
        app.tab = tab;
        app.selection = 0;
    }
}

fn move_selection_up(app: &mut App) {
    let len = app.visible_indices().len();
    if len == 0 {
        return;
    }
    app.selection = if app.selection == 0 {
        len - 1
    } else {
        app.selection - 1
    };
}

fn move_selection_down(app: &mut App) {
    let len = app.visible_indices().len();
    if len == 0 {
        return;
    }
    app.selection = (app.selection + 1) % len;
}

fn toggle_selected(app: &mut App) {
    if let Some(idx) = app.selected_task() {
        app.tasks[idx].done = !app.tasks[idx].done;
        app.clamp_selection();
    }
}

// --- Input focus ---

fn handle_input_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => submit_input(app),
        KeyCode::Esc => {
            app.input.clear();
            app.focus = Focus::List;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.input.push(c);
        }
        _ => {}
    }
}

/// Submit the input draft as a new task (no-op while empty) and hand focus
/// back to the list with the new task selected.
fn submit_input(app: &mut App) {
    let title = app.input.trim().to_string();
    if title.is_empty() {
        return;
    }
    app.tasks.push(Task { title, done: false });
    app.input.clear();
    app.focus = Focus::List;
    if app.tab == Tab::Active {
        app.selection = app.visible_indices().len() - 1;
    }
}

// --- Dialog ---

fn handle_dialog_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => confirm_delete(app),
        KeyCode::Char('n') | KeyCode::Esc => app.dialog = None,
        _ => {}
    }
}

fn confirm_delete(app: &mut App) {
    if let Some(idx) = app.dialog.take() {
        if idx < app.tasks.len() {
            app.tasks.remove(idx);
        }
        app.clamp_selection();
    }
}

#[cfg(test)]
mod tests {
    use taria::NodeId;

    use super::*;

    fn act(node: &str, action: Action) -> AgentInput {
        AgentInput::Act {
            node: NodeId(node.into()),
            action,
            value: None,
        }
    }

    fn act_value(node: &str, action: Action, value: &str) -> AgentInput {
        AgentInput::Act {
            node: NodeId(node.into()),
            action,
            value: Some(value.into()),
        }
    }

    fn key(key: &str) -> AgentInput {
        AgentInput::Key { key: key.into() }
    }

    #[test]
    fn agent_adds_a_task_via_set_value_and_activate() {
        let mut app = App::new();
        let before = app.tasks.len();

        apply_agent_input(
            &mut app,
            act_value("input", Action::SetValue, "Ship the demo"),
        );
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input, "Ship the demo");

        apply_agent_input(&mut app, act("input", Action::Activate));
        assert_eq!(app.tasks.len(), before + 1);
        let task = app.tasks.last().unwrap();
        assert_eq!(task.title, "Ship the demo");
        assert!(!task.done);
        assert!(app.input.is_empty(), "input clears after submit");
        assert_eq!(app.focus, Focus::List, "focus returns to the list");
        assert_eq!(
            app.selected_task(),
            Some(app.tasks.len() - 1),
            "the new task is selected"
        );
    }

    #[test]
    fn activate_on_empty_input_adds_nothing() {
        let mut app = App::new();
        let before = app.tasks.len();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "   "));
        apply_agent_input(&mut app, act("input", Action::Activate));
        assert_eq!(app.tasks.len(), before);
    }

    #[test]
    fn agent_toggle_flips_done() {
        let mut app = App::new();
        assert!(!app.tasks[0].done);
        apply_agent_input(&mut app, act("task-0", Action::Toggle));
        assert!(app.tasks[0].done);
        apply_agent_input(&mut app, act("task-0", Action::Toggle));
        assert!(!app.tasks[0].done);
    }

    #[test]
    fn agent_select_moves_selection_to_that_task() {
        let mut app = App::new();
        // Active tab shows original indices [0, 2, 3]; task-3 sits at pos 2.
        apply_agent_input(&mut app, act("task-3", Action::Select));
        assert_eq!(app.selection, 2);
        assert_eq!(app.selected_task(), Some(3));
    }

    #[test]
    fn agent_delete_flow_confirms_through_dialog() {
        let mut app = App::new();
        let before = app.tasks.len();
        let title = app.tasks[2].title.clone();

        apply_agent_input(&mut app, act("task-2", Action::Custom("delete".into())));
        assert_eq!(app.dialog, Some(2));

        apply_agent_input(&mut app, act("dialog-confirm", Action::Activate));
        assert_eq!(app.dialog, None);
        assert_eq!(app.tasks.len(), before - 1);
        assert!(app.tasks.iter().all(|t| t.title != title));
    }

    #[test]
    fn agent_can_cancel_or_dismiss_the_dialog() {
        for cancel in [
            act("dialog-cancel", Action::Activate),
            act("dialog", Action::Dismiss),
        ] {
            let mut app = App::new();
            let before = app.tasks.len();
            apply_agent_input(&mut app, act("task-0", Action::Custom("delete".into())));
            assert_eq!(app.dialog, Some(0));
            apply_agent_input(&mut app, cancel);
            assert_eq!(app.dialog, None);
            assert_eq!(app.tasks.len(), before, "cancel must not delete");
        }
    }

    #[test]
    fn dialog_blocks_acts_on_nodes_outside_it() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-0", Action::Custom("delete".into())));
        assert_eq!(app.dialog, Some(0));
        let before = app.tasks.len();

        apply_agent_input(
            &mut app,
            act_value("input", Action::SetValue, "sneaky task"),
        );
        assert!(app.input.is_empty(), "set_value must not reach the input");
        assert_eq!(app.focus, Focus::List, "focus must not move to the input");

        apply_agent_input(&mut app, act("input", Action::Activate));
        assert_eq!(app.tasks.len(), before, "activate must not add a task");

        apply_agent_input(&mut app, act("task-2", Action::Toggle));
        assert!(!app.tasks[2].done, "toggle must not flip a task");
        apply_agent_input(&mut app, act("task-3", Action::Select));
        assert_eq!(app.selection, 0, "select must not move the selection");
        apply_agent_input(&mut app, act("tab-done", Action::Select));
        assert_eq!(app.tab, Tab::Active, "tab switch must not happen");

        assert_eq!(app.dialog, Some(0), "the dialog stays open throughout");
    }

    #[test]
    fn acts_work_again_after_dialog_cancel() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-0", Action::Custom("delete".into())));
        apply_agent_input(&mut app, act("dialog-cancel", Action::Activate));
        assert_eq!(app.dialog, None);

        apply_agent_input(
            &mut app,
            act_value("input", Action::SetValue, "back to work"),
        );
        assert_eq!(app.input, "back to work");
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn agent_tab_select_switches_visible_list() {
        let mut app = App::new();
        assert!(app.visible_indices().iter().all(|&i| !app.tasks[i].done));

        apply_agent_input(&mut app, act("tab-done", Action::Select));
        assert_eq!(app.tab, Tab::Done);
        assert!(!app.visible_indices().is_empty());
        assert!(app.visible_indices().iter().all(|&i| app.tasks[i].done));

        apply_agent_input(&mut app, act("tab-active", Action::Select));
        assert_eq!(app.tab, Tab::Active);
    }

    #[test]
    fn key_fallback_navigates_and_wraps() {
        let mut app = App::new();
        let len = app.visible_indices().len();
        assert_eq!(app.selection, 0);

        apply_agent_input(&mut app, key("j"));
        assert_eq!(app.selection, 1);
        apply_agent_input(&mut app, key("down"));
        assert_eq!(app.selection, 2);
        apply_agent_input(&mut app, key("up"));
        assert_eq!(app.selection, 1);

        apply_agent_input(&mut app, key("k"));
        apply_agent_input(&mut app, key("up"));
        assert_eq!(app.selection, len - 1, "up from the top wraps to the end");
        apply_agent_input(&mut app, key("down"));
        assert_eq!(app.selection, 0, "down from the end wraps to the top");
    }

    #[test]
    fn key_fallback_space_toggles_selected() {
        let mut app = App::new();
        let idx = app.selected_task().unwrap();
        apply_agent_input(&mut app, key("space"));
        assert!(app.tasks[idx].done);
    }

    #[test]
    fn quit_blocked_while_typing() {
        let mut app = App::new();
        apply_agent_input(&mut app, key("i"));
        assert_eq!(app.focus, Focus::Input);

        apply_agent_input(&mut app, key("q"));
        assert!(
            app.running,
            "q must type, not quit, while the input is focused"
        );
        assert_eq!(app.input, "q");

        apply_agent_input(&mut app, key("esc"));
        assert_eq!(app.focus, Focus::List);
        assert!(app.input.is_empty(), "esc discards the draft");
        apply_agent_input(&mut app, key("q"));
        assert!(!app.running);
    }

    #[test]
    fn quit_blocked_while_dialog_open() {
        let mut app = App::new();
        apply_agent_input(&mut app, key("d"));
        assert!(app.dialog.is_some());
        apply_agent_input(&mut app, key("q"));
        assert!(app.running);
        apply_agent_input(&mut app, key("esc"));
        assert_eq!(app.dialog, None);
    }

    #[test]
    fn human_delete_flow_via_dialog_keys() {
        let mut app = App::new();
        let before = app.tasks.len();
        let idx = app.selected_task().unwrap();

        apply_agent_input(&mut app, key("d"));
        assert_eq!(app.dialog, Some(idx));
        apply_agent_input(&mut app, key("n"));
        assert_eq!(app.tasks.len(), before);

        apply_agent_input(&mut app, key("d"));
        apply_agent_input(&mut app, key("y"));
        assert_eq!(app.tasks.len(), before - 1);
    }

    #[test]
    fn update_routes_terminal_key_events() {
        let mut app = App::new();
        let event = AppEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        update(&mut app, event);
        assert!(!app.running);

        let mut app = App::new();
        update(&mut app, AppEvent::Tick);
        update(&mut app, AppEvent::Resize);
        assert!(app.running, "tick and resize leave state untouched");
    }

    #[test]
    fn tab_key_switches_and_toggle_moves_task_across_tabs() {
        let mut app = App::new();
        let idx = app.selected_task().unwrap();
        apply_agent_input(&mut app, key("space"));
        assert!(app.tasks[idx].done);
        assert!(
            !app.visible_indices().contains(&idx),
            "a completed task leaves the Active tab"
        );

        apply_agent_input(&mut app, key("tab"));
        assert_eq!(app.tab, Tab::Done);
        assert!(app.visible_indices().contains(&idx));
    }
}
