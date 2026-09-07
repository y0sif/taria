//! Event handling and state mutations (the Update). Never renders.
//!
//! Two entry points, both pure over [`App`]: [`update`] for terminal events
//! and [`apply_agent_input`] for taria agent input. Semantic acts address
//! nodes by the ids published in [`crate::tree::build_nodes`]; the raw-key
//! and text fallbacks lower into the same key handler as the keyboard.
//!
//! # Text is typing, not appending
//!
//! [`AgentInput::Text`] is lowered with [`text_to_keys`] and each event goes
//! through the same [`handle_key`] path a person's keystroke takes. So text is
//! literal typing into whatever currently has focus, not "append to the text
//! field": sent while the list has focus it meets the list's single-key
//! bindings, where `q` quits and `d` opens the delete dialog. That is the
//! lowering doing its job. An agent that wants text in the input focuses the
//! input first, by acting `set_value` on the `input` node or sending the `i`
//! key, and only then types.
//!
//! # Ignored input
//!
//! [`apply_agent_input`] returns [`Applied`], because some inputs are
//! deliberately dropped (the modal gate, an unknown node id, an action a node
//! does not handle, a `set_value` carrying no value) and the caller acks those
//! [`Ignored`](taria_ratatui::InputStatus::Ignored) so an agent waiting on an
//! effect stops waiting.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taria::{Action, AgentInput};
use taria_ratatui::{text_to_keys, to_crossterm_key};

use crate::app::{App, Focus, Tab, TaskId};
use crate::events::AppEvent;
use crate::tree::TASK_IDS;

/// What the app did with one agent input.
///
/// Coarse on purpose: it answers the one question an agent has after sending
/// an input, which is whether waiting for an effect is worth it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The input reached a handler that could act on it.
    Handled,
    /// The app looked at the input and deliberately did nothing.
    Ignored,
}

pub fn update(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Key(key) => handle_key(app, key),
        AppEvent::Tick => {}
        AppEvent::Resize => {}
    }
}

/// Apply one agent input received through the taria layer, reporting whether
/// it changed anything.
///
/// A key that parses counts as handled even if nothing is bound to it, the
/// same verdict a person gets for pressing an unbound key; only a key the
/// grammar rejects is ignored here.
pub fn apply_agent_input(app: &mut App, input: AgentInput) -> Applied {
    match input {
        AgentInput::Act {
            node,
            action,
            value,
        } => apply_act(app, node.0.as_str(), action, value),
        AgentInput::Key { key } => match to_crossterm_key(&key) {
            Some(key) => {
                handle_key(app, key);
                Applied::Handled
            }
            None => Applied::Ignored,
        },
        AgentInput::Text { text } => {
            let keys = text_to_keys(&text);
            if keys.is_empty() {
                return Applied::Ignored;
            }
            for key in keys {
                handle_key(app, key);
            }
            Applied::Handled
        }
    }
}

fn apply_act(app: &mut App, node: &str, action: Action, value: Option<String>) -> Applied {
    // The delete dialog is modal: mirror `handle_key` by ignoring any act
    // that targets a node outside the dialog while it is open. The tree
    // stops advertising those actions too (see `crate::tree::build_nodes`),
    // so behavior and advertisement agree.
    if app.dialog.is_some() && !matches!(node, "dialog" | "dialog-confirm" | "dialog-cancel") {
        return Applied::Ignored;
    }
    match (node, action) {
        ("tab-active", Action::Select) => {
            switch_tab(app, Tab::Active);
            Applied::Handled
        }
        ("tab-done", Action::Select) => {
            switch_tab(app, Tab::Done);
            Applied::Handled
        }
        // `set_value` with no value asks for nothing. Treating a missing
        // value as the empty string would clear the draft, an edit the agent
        // never asked for, and report it as Handled.
        ("input", Action::SetValue) => match value {
            Some(value) => {
                app.input = value;
                app.focus = Focus::Input;
                Applied::Handled
            }
            None => Applied::Ignored,
        },
        ("input", Action::Activate) => submit_input(app),
        ("dialog-confirm", Action::Activate) => {
            confirm_delete(app);
            Applied::Handled
        }
        ("dialog-cancel", Action::Activate) | ("dialog", Action::Dismiss) => {
            app.dialog = None;
            Applied::Handled
        }
        (node, action) => apply_task_act(app, node, action),
    }
}

fn apply_task_act(app: &mut App, node: &str, action: Action) -> Applied {
    // Ids are parsed through the same space that built them, and the task is
    // looked up by identity: an id an agent read before an unrelated delete
    // still names the task it named then, or nothing at all.
    let Some(id) = TASK_IDS
        .parse::<TaskId>(node)
        .filter(|id| app.task(*id).is_some())
    else {
        return Applied::Ignored;
    };
    match action {
        Action::Select => match app.visible_position(id) {
            Some(pos) => {
                app.selection = pos;
                Applied::Handled
            }
            // The task exists but sits on the other tab, so there is no
            // position on screen to move the cursor to.
            None => Applied::Ignored,
        },
        Action::Toggle => {
            if let Some(task) = app.task_mut(id) {
                task.done = !task.done;
            }
            app.clamp_selection();
            Applied::Handled
        }
        Action::Custom(name) if name == "delete" => {
            app.dialog = Some(id);
            Applied::Handled
        }
        _ => Applied::Ignored,
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

/// Whether this press is the plain key the demo's bindings name: one carrying
/// no modifier at all.
///
/// Every modifier, not just ctrl and alt, and every key code, not just
/// characters. `taria::key` parses `shift+q` into `Char('q')` with shift, a
/// press no terminal produces, and a ctrl-or-alt-only rule let it quit the
/// app; the same rule let `ctrl+enter` submit the draft and `ctrl+esc` throw
/// it away, because the guard only ever sat on the `Char` arms. Naming the
/// modifiers that disqualify a press is the shape that keeps growing holes,
/// so this names the one combination that qualifies instead.
///
/// Typing is the one thing this rule must not decide: see [`typed_char`].
fn is_plain(key: KeyEvent) -> bool {
    key.modifiers.is_empty()
}

/// The character this press types, or `None` if it types nothing.
///
/// Shift is allowed here and nowhere else, because a terminal reports an
/// uppercase letter as `Char('A')` with shift set (crossterm adds it for any
/// uppercase character), so requiring no modifiers would stop a person typing
/// capitals. The character itself already says which one it is, which is why
/// shift can be ignored rather than checked against it. Ctrl and alt are not
/// text: `ctrl+c` is not the letter `c`, and typing it into the draft is the
/// one thing an agent sending it cannot have meant.
fn typed_char(key: KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() => Some(c),
        _ => None,
    }
}

// --- List focus ---

fn handle_list_key(app: &mut App, key: KeyEvent) {
    // A modified press is not the plain key it is built from: `ctrl+q` and
    // `shift+q` must not quit, and `ctrl+j` must not move the cursor, or an
    // agent reaching for a chord some other part of a UI wants closes this
    // app by accident. Falling through here leaves it unhandled the way any
    // unbound press is, which the caller still reports as received.
    if !is_plain(key) {
        return;
    }
    match key.code {
        KeyCode::Char('q') => app.running = false,
        KeyCode::Tab => switch_tab(app, app.tab.other()),
        KeyCode::Up | KeyCode::Char('k') => move_selection_up(app),
        KeyCode::Down | KeyCode::Char('j') => move_selection_down(app),
        KeyCode::Char(' ') => toggle_selected(app),
        KeyCode::Char('i') => app.focus = Focus::Input,
        KeyCode::Char('d') => app.dialog = app.selected_id(),
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
    let len = app.visible_len();
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
    let len = app.visible_len();
    if len == 0 {
        return;
    }
    app.selection = (app.selection + 1) % len;
}

fn toggle_selected(app: &mut App) {
    let Some(id) = app.selected_id() else { return };
    if let Some(task) = app.task_mut(id) {
        task.done = !task.done;
    }
    app.clamp_selection();
}

// --- Input focus ---

fn handle_input_key(app: &mut App, key: KeyEvent) {
    // Typing is decided first, because it is the one binding here that shift
    // may reach: an uppercase letter arrives with shift set and is still text.
    if let Some(c) = typed_char(key) {
        app.input.push(c);
        return;
    }
    // Everything else is the plain key or nothing: `ctrl+enter` is not Enter
    // and must not submit the draft, and `ctrl+esc` is not Esc and must not
    // throw it away.
    if !is_plain(key) {
        return;
    }
    match key.code {
        KeyCode::Enter => {
            submit_input(app);
        }
        KeyCode::Esc => {
            app.input.clear();
            app.focus = Focus::List;
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
}

/// Submit the input draft as a new task and hand focus back to the list with
/// the new task selected. An empty draft adds nothing and is reported
/// ignored, so an agent that activates too early hears about it.
fn submit_input(app: &mut App) -> Applied {
    let title = app.input.trim().to_string();
    if title.is_empty() {
        return Applied::Ignored;
    }
    let id = app.add_task(title);
    app.input.clear();
    app.focus = Focus::List;
    // A new task is active, so it appears only on the Active tab; select it
    // there and leave the cursor alone on the Done tab.
    if let Some(pos) = app.visible_position(id) {
        app.selection = pos;
    }
    Applied::Handled
}

// --- Dialog ---

fn handle_dialog_key(app: &mut App, key: KeyEvent) {
    // The same rule as the list, and it matters more here: neither `ctrl+y`
    // nor `shift+y` is the `y` that deletes a task, and `ctrl+enter` is not
    // the Enter that confirms it.
    if !is_plain(key) {
        return;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => confirm_delete(app),
        KeyCode::Char('n') | KeyCode::Esc => app.dialog = None,
        _ => {}
    }
}

fn confirm_delete(app: &mut App) {
    if let Some(id) = app.dialog.take() {
        app.remove_task(id);
        app.clamp_selection();
    }
}

#[cfg(test)]
mod tests {
    use taria::NodeId;

    use super::*;

    // The seeds are tasks 1 through 5; 2 and 5 start done, so the Active tab
    // shows task-1, task-3, task-4 and the Done tab task-2, task-5.

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

    fn text(text: &str) -> AgentInput {
        AgentInput::Text { text: text.into() }
    }

    fn delete(app: &mut App, node: &str) {
        apply_agent_input(app, act(node, Action::Custom("delete".into())));
        apply_agent_input(app, act("dialog-confirm", Action::Activate));
    }

    #[test]
    fn agent_adds_a_task_via_set_value_and_activate() {
        let mut app = App::new();
        let before = app.tasks.len();

        assert_eq!(
            apply_agent_input(
                &mut app,
                act_value("input", Action::SetValue, "Ship the demo")
            ),
            Applied::Handled
        );
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input, "Ship the demo");

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            Applied::Handled
        );
        assert_eq!(app.tasks.len(), before + 1);
        let task = app.tasks.last().unwrap();
        assert_eq!(task.title, "Ship the demo");
        assert!(!task.done);
        let new_id = task.id;
        assert_eq!(new_id, 6, "a new task takes the id after the seeds");
        assert!(app.input.is_empty(), "input clears after submit");
        assert_eq!(app.focus, Focus::List, "focus returns to the list");
        assert_eq!(app.selected_id(), Some(new_id), "the new task is selected");
    }

    /// `set_value` with no value asks for nothing, so it must leave the draft
    /// alone and say so, rather than clearing it and reporting Handled.
    #[test]
    fn set_value_without_a_value_leaves_the_draft_alone() {
        let mut app = App::new();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "half typed"));

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::SetValue)),
            Applied::Ignored
        );
        assert_eq!(app.input, "half typed", "the draft must survive");

        // An explicit empty value is a different request, and still clears it.
        assert_eq!(
            apply_agent_input(&mut app, act_value("input", Action::SetValue, "")),
            Applied::Handled
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn activate_on_empty_input_adds_nothing() {
        let mut app = App::new();
        let before = app.tasks.len();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "   "));
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            Applied::Ignored,
            "an agent that activates an empty draft hears that nothing happened"
        );
        assert_eq!(app.tasks.len(), before);
    }

    #[test]
    fn agent_toggle_flips_done() {
        let mut app = App::new();
        assert!(!app.task(1).unwrap().done);
        apply_agent_input(&mut app, act("task-1", Action::Toggle));
        assert!(app.task(1).unwrap().done);
        apply_agent_input(&mut app, act("task-1", Action::Toggle));
        assert!(!app.task(1).unwrap().done);
    }

    #[test]
    fn agent_select_moves_selection_to_that_task() {
        let mut app = App::new();
        // The Active tab shows tasks 1, 3 and 4, so task-4 sits at position 2.
        apply_agent_input(&mut app, act("task-4", Action::Select));
        assert_eq!(app.selection, 2);
        assert_eq!(app.selected_id(), Some(4));
    }

    #[test]
    fn agent_delete_flow_confirms_through_dialog() {
        let mut app = App::new();
        let before = app.tasks.len();
        let title = app.task(3).unwrap().title.clone();

        apply_agent_input(&mut app, act("task-3", Action::Custom("delete".into())));
        assert_eq!(
            app.dialog,
            Some(3),
            "the dialog holds the task id, so the list may move under it"
        );

        apply_agent_input(&mut app, act("dialog-confirm", Action::Activate));
        assert_eq!(app.dialog, None);
        assert_eq!(app.tasks.len(), before - 1);
        assert!(app.task(3).is_none());
        assert!(app.tasks.iter().all(|t| t.title != title));
    }

    #[test]
    fn an_act_after_an_unrelated_delete_still_targets_the_same_task() {
        let mut app = App::new();
        let title = app.task(4).unwrap().title.clone();

        // An agent reads the tree, then an earlier task is deleted. With ids
        // derived from a position, "task-4" would now name a different task.
        delete(&mut app, "task-1");

        assert_eq!(
            apply_agent_input(&mut app, act("task-4", Action::Toggle)),
            Applied::Handled
        );
        let task = app.task(4).unwrap();
        assert_eq!(task.title, title, "the id still names the same task");
        assert!(task.done);
        assert!(!app.task(3).unwrap().done, "no other task was touched");
    }

    #[test]
    fn an_id_from_a_deleted_task_names_nothing() {
        let mut app = App::new();
        delete(&mut app, "task-1");
        let before = app.tasks.clone();

        assert_eq!(
            apply_agent_input(&mut app, act("task-1", Action::Toggle)),
            Applied::Ignored
        );
        assert_eq!(app.tasks, before, "a stale id must move nothing");
    }

    #[test]
    fn unknown_ids_and_unhandled_actions_report_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, act("task-1", Action::Toggle)),
            Applied::Handled,
            "a real act on a real node is handled"
        );
        let ignored = [
            act("task-99", Action::Toggle),
            act("task-nope", Action::Toggle),
            act("nonesuch", Action::Activate),
            act("task-3", Action::Scroll),
            act("tab-done", Action::Toggle),
        ];
        for input in ignored {
            assert_eq!(
                apply_agent_input(&mut app, input.clone()),
                Applied::Ignored,
                "{input:?}"
            );
        }
    }

    #[test]
    fn the_modal_gate_reports_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into()))),
            Applied::Handled
        );
        let blocked = [
            act("tab-done", Action::Select),
            act("task-3", Action::Toggle),
            act_value("input", Action::SetValue, "sneaky task"),
        ];
        for input in blocked {
            assert_eq!(
                apply_agent_input(&mut app, input.clone()),
                Applied::Ignored,
                "{input:?}"
            );
        }
        assert_eq!(
            apply_agent_input(&mut app, act("dialog-cancel", Action::Activate)),
            Applied::Handled,
            "the dialog's own nodes still answer"
        );
    }

    #[test]
    fn an_unparseable_key_reports_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, key("not-a-key")),
            Applied::Ignored
        );
        assert_eq!(apply_agent_input(&mut app, key("j")), Applied::Handled);
    }

    #[test]
    fn text_types_into_the_focused_input() {
        let mut app = App::new();
        // Focusing first is the agent's job; set_value on the input does it.
        apply_agent_input(&mut app, act_value("input", Action::SetValue, ""));
        assert_eq!(app.focus, Focus::Input);

        assert_eq!(
            apply_agent_input(&mut app, text("buy milk")),
            Applied::Handled
        );
        assert_eq!(app.input, "buy milk");

        // A newline lowers to Enter, so one message can type and submit.
        apply_agent_input(&mut app, text("\n"));
        assert_eq!(app.tasks.last().unwrap().title, "buy milk");
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn text_is_literal_typing_so_the_list_sees_its_own_bindings() {
        // The consequence documented in the module header: text lowers to key
        // events for whatever has focus, so text sent while the list has
        // focus meets the list's single-key bindings instead of appending
        // anywhere. Correct lowering, not a bug.
        let mut app = App::new();
        assert_eq!(app.focus, Focus::List);

        assert_eq!(apply_agent_input(&mut app, text("d")), Applied::Handled);
        assert_eq!(app.dialog, Some(1), "d opened the delete dialog");
        apply_agent_input(&mut app, text("n"));
        assert_eq!(app.dialog, None, "n cancelled it");

        apply_agent_input(&mut app, text("q"));
        assert!(!app.running, "q quit instead of typing a letter");
        assert!(app.input.is_empty(), "nothing reached the input");
    }

    #[test]
    fn empty_text_reports_ignored() {
        let mut app = App::new();
        assert_eq!(apply_agent_input(&mut app, text("")), Applied::Ignored);
        assert_eq!(
            apply_agent_input(&mut app, text("\r")),
            Applied::Ignored,
            "a lone carriage return lowers to no key events"
        );
    }

    #[test]
    fn agent_can_cancel_or_dismiss_the_dialog() {
        for cancel in [
            act("dialog-cancel", Action::Activate),
            act("dialog", Action::Dismiss),
        ] {
            let mut app = App::new();
            let before = app.tasks.len();
            apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
            assert_eq!(app.dialog, Some(1));
            apply_agent_input(&mut app, cancel);
            assert_eq!(app.dialog, None);
            assert_eq!(app.tasks.len(), before, "cancel must not delete");
        }
    }

    #[test]
    fn dialog_blocks_acts_on_nodes_outside_it() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        assert_eq!(app.dialog, Some(1));
        let before = app.tasks.len();

        apply_agent_input(
            &mut app,
            act_value("input", Action::SetValue, "sneaky task"),
        );
        assert!(app.input.is_empty(), "set_value must not reach the input");
        assert_eq!(app.focus, Focus::List, "focus must not move to the input");

        apply_agent_input(&mut app, act("input", Action::Activate));
        assert_eq!(app.tasks.len(), before, "activate must not add a task");

        apply_agent_input(&mut app, act("task-3", Action::Toggle));
        assert!(!app.task(3).unwrap().done, "toggle must not flip a task");
        apply_agent_input(&mut app, act("task-4", Action::Select));
        assert_eq!(app.selection, 0, "select must not move the selection");
        apply_agent_input(&mut app, act("tab-done", Action::Select));
        assert_eq!(app.tab, Tab::Active, "tab switch must not happen");

        assert_eq!(app.dialog, Some(1), "the dialog stays open throughout");
    }

    #[test]
    fn acts_work_again_after_dialog_cancel() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
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
        assert!(app.visible_tasks().all(|task| !task.done));

        apply_agent_input(&mut app, act("tab-done", Action::Select));
        assert_eq!(app.tab, Tab::Done);
        assert!(app.visible_len() > 0);
        assert!(app.visible_tasks().all(|task| task.done));

        apply_agent_input(&mut app, act("tab-active", Action::Select));
        assert_eq!(app.tab, Tab::Active);
    }

    #[test]
    fn key_fallback_navigates_and_wraps() {
        let mut app = App::new();
        let len = app.visible_len();
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
        let id = app.selected_id().unwrap();
        apply_agent_input(&mut app, key("space"));
        assert!(app.task(id).unwrap().done);
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

    /// A control chord is not the letter it is built from. An agent that
    /// sends `ctrl+q` (or has a chord bound elsewhere in its own habits) must
    /// not close the app or move the cursor by accident, and the demo answers
    /// the way it answers any unbound press: received, nothing done.
    #[test]
    fn control_chords_are_not_the_plain_bindings() {
        let mut app = App::new();

        assert_eq!(apply_agent_input(&mut app, key("ctrl+q")), Applied::Handled);
        assert!(app.running, "ctrl+q must not quit");

        let selection = app.selection;
        apply_agent_input(&mut app, key("ctrl+j"));
        apply_agent_input(&mut app, key("ctrl+k"));
        apply_agent_input(&mut app, key("alt+down"));
        assert_eq!(app.selection, selection, "chords must not move the cursor");

        apply_agent_input(&mut app, key("ctrl+i"));
        assert_eq!(app.focus, Focus::List, "ctrl+i must not focus the input");

        apply_agent_input(&mut app, key("ctrl+d"));
        assert_eq!(app.dialog, None, "ctrl+d must not open the delete dialog");

        // The plain keys still work, so the guard blocks chords and nothing
        // else.
        apply_agent_input(&mut app, key("j"));
        assert_eq!(app.selection, selection + 1);
        apply_agent_input(&mut app, key("q"));
        assert!(!app.running, "q still quits");
    }

    /// The dialog is modal and its `y` deletes, so a chord reaching it is the
    /// same accident with a worse ending.
    #[test]
    fn control_chords_do_not_answer_the_delete_dialog() {
        let mut app = App::new();
        let before = app.tasks.len();
        apply_agent_input(&mut app, key("d"));
        assert!(app.dialog.is_some());

        apply_agent_input(&mut app, key("ctrl+y"));
        assert!(app.dialog.is_some(), "ctrl+y must not confirm the delete");
        assert_eq!(app.tasks.len(), before, "nothing was deleted");

        apply_agent_input(&mut app, key("ctrl+n"));
        assert!(app.dialog.is_some(), "ctrl+n must not cancel either");

        apply_agent_input(&mut app, key("y"));
        assert_eq!(app.tasks.len(), before - 1, "plain y still confirms");
    }

    /// Shift is a modifier like any other. `taria::key` parses `shift+q` into
    /// `Char('q')` with shift, a press no terminal produces, and a guard that
    /// only knew ctrl and alt let it quit the app and let `shift+y` answer the
    /// delete dialog.
    #[test]
    fn shift_is_not_the_plain_binding_either() {
        let mut app = App::new();

        assert_eq!(
            apply_agent_input(&mut app, key("shift+q")),
            Applied::Handled
        );
        assert!(app.running, "shift+q must not quit");

        let selection = app.selection;
        apply_agent_input(&mut app, key("shift+j"));
        apply_agent_input(&mut app, key("shift+down"));
        assert_eq!(app.selection, selection, "shift must not move the cursor");

        apply_agent_input(&mut app, key("shift+i"));
        assert_eq!(app.focus, Focus::List, "shift+i must not focus the input");
        apply_agent_input(&mut app, key("shift+space"));
        assert!(
            !app.task(app.selected_id().unwrap()).unwrap().done,
            "shift+space must not toggle a task"
        );

        let before = app.tasks.len();
        apply_agent_input(&mut app, key("shift+d"));
        assert_eq!(app.dialog, None, "shift+d must not open the delete dialog");

        // And on the dialog, where the mistake deletes data.
        apply_agent_input(&mut app, key("d"));
        assert!(app.dialog.is_some());
        apply_agent_input(&mut app, key("shift+y"));
        assert!(app.dialog.is_some(), "shift+y must not confirm the delete");
        assert_eq!(app.tasks.len(), before, "nothing was deleted");
        apply_agent_input(&mut app, key("shift+n"));
        assert!(app.dialog.is_some(), "shift+n must not cancel either");
        apply_agent_input(&mut app, key("esc"));
        assert_eq!(app.dialog, None, "plain esc still cancels");
    }

    /// The guard used to sit on the `Char` arms alone, so every named key was
    /// the plain binding whatever it carried: `ctrl+enter` submitted the draft
    /// and `ctrl+esc` threw it away.
    #[test]
    fn modified_named_keys_are_not_the_plain_bindings() {
        let mut app = App::new();
        let before = app.tasks.len();
        apply_agent_input(&mut app, key("i"));
        apply_agent_input(&mut app, text("draft"));
        assert_eq!(app.input, "draft");

        for chord in ["ctrl+enter", "alt+enter", "shift+enter"] {
            apply_agent_input(&mut app, key(chord));
            assert_eq!(app.tasks.len(), before, "{chord} must not submit the draft");
            assert_eq!(app.input, "draft", "{chord} must not clear the draft");
        }
        for chord in ["ctrl+esc", "alt+esc", "shift+esc"] {
            apply_agent_input(&mut app, key(chord));
            assert_eq!(app.input, "draft", "{chord} must not discard the draft");
            assert_eq!(app.focus, Focus::Input, "{chord} must not leave the input");
        }
        for chord in ["ctrl+backspace", "shift+backspace"] {
            apply_agent_input(&mut app, key(chord));
            assert_eq!(app.input, "draft", "{chord} must not edit the draft");
        }

        // The plain keys still do their jobs.
        apply_agent_input(&mut app, key("backspace"));
        assert_eq!(app.input, "draf");
        apply_agent_input(&mut app, key("enter"));
        assert_eq!(app.tasks.len(), before + 1, "plain enter still submits");
        assert_eq!(app.tasks.last().unwrap().title, "draf");

        // The dialog's Enter is the same binding with worse consequences.
        apply_agent_input(&mut app, key("d"));
        assert!(app.dialog.is_some());
        apply_agent_input(&mut app, key("ctrl+enter"));
        assert!(app.dialog.is_some(), "ctrl+enter must not confirm a delete");
        apply_agent_input(&mut app, key("ctrl+esc"));
        assert!(app.dialog.is_some(), "ctrl+esc must not cancel it either");
        apply_agent_input(&mut app, key("esc"));
        assert_eq!(app.dialog, None);
    }

    /// The reason [`typed_char`] lets shift through: a terminal reports an
    /// uppercase letter as `Char('A')` with shift set, so a flat "no
    /// modifiers" rule would stop a person typing capitals. The agent paths
    /// send uppercase without shift and must keep working too.
    #[test]
    fn uppercase_still_types_however_it_arrives() {
        let mut app = App::new();
        apply_agent_input(&mut app, key("i"));

        // What a terminal delivers for shift+a.
        update(
            &mut app,
            AppEvent::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        );
        // What `taria::key` and `text_to_keys` deliver for the same letter.
        apply_agent_input(&mut app, key("B"));
        apply_agent_input(&mut app, text("Cd"));
        assert_eq!(app.input, "ABCd");

        // A chord carrying a character is still not text.
        apply_agent_input(&mut app, key("ctrl+e"));
        apply_agent_input(&mut app, key("alt+f"));
        assert_eq!(app.input, "ABCd", "ctrl and alt must not reach the draft");
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
        let id = app.selected_id().unwrap();

        apply_agent_input(&mut app, key("d"));
        assert_eq!(app.dialog, Some(id));
        apply_agent_input(&mut app, key("n"));
        assert_eq!(app.tasks.len(), before);

        apply_agent_input(&mut app, key("d"));
        apply_agent_input(&mut app, key("y"));
        assert_eq!(app.tasks.len(), before - 1);
        assert!(app.task(id).is_none());
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
        let id = app.selected_id().unwrap();
        apply_agent_input(&mut app, key("space"));
        assert!(app.task(id).unwrap().done);
        assert_eq!(
            app.visible_position(id),
            None,
            "a completed task leaves the Active tab"
        );

        apply_agent_input(&mut app, key("tab"));
        assert_eq!(app.tab, Tab::Done);
        assert!(app.visible_position(id).is_some());
    }
}
