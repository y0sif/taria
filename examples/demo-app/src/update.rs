//! Event handling and state mutations (the Update). Never renders.
//!
//! Two entry points, both pure over [`App`]: [`update`] for terminal events
//! and [`apply_agent_input`] for taria agent input. Semantic acts address
//! nodes by the ids published in [`crate::tree::build_nodes`]; the raw-key
//! fallback lowers into the same key handler as the keyboard.
//!
//! # Typed text goes where the app puts typing
//!
//! [`AgentInput::Text`] is typing, so it is routed to this app's text-entry
//! state, the new-task input, and never through [`handle_key`]. Lowered
//! through the key handler instead, text sent while the list has focus meets
//! the list's single-key bindings: one `type_text` of "deploy" opened the
//! delete dialog on `d`, was answered `y` by its own last character, and cost
//! a task, while the bridge reported plain success because a task vanishing is
//! a changed tree. Text nothing is accepting is reported
//! [`Ignored`](InputStatus::Ignored) instead, so an agent that types at the wrong
//! moment hears about it rather than tripping bindings.
//!
//! The rule other apps should copy is not "text means the text field". It is
//! that the app decides where typed characters go: an app whose typing surface
//! is something else, a typing tutor scoring individual keystrokes for
//! instance, routes [`AgentInput::Text`] to that surface and reports Ignored
//! only where it is accepting no typing at all.
//!
//! [`AgentInput::Key`] keeps the raw lowering, deliberately. A key is a
//! keypress and is supposed to reach the bindings, wherever focus happens to
//! be; that is the difference between the two inputs.
//!
//! # Ignored input
//!
//! [`apply_agent_input`] returns the [`InputStatus`] each input has earned,
//! because some inputs are deliberately dropped (the modal gate, an unknown
//! node id, an action a node does not handle, a `set_value` carrying no value)
//! and an agent waiting on an effect from one of those should stop waiting.
//! [`TariaLayer::drain_acking`](taria_ratatui::TariaLayer::drain_acking) sends
//! the [`Ignored`](InputStatus::Ignored) ones; a
//! [`Delivered`](InputStatus::Delivered) adds nothing to the ack the layer
//! already sent when it handed the input over.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use taria::{Action, AgentInput};
use taria_ratatui::{InputStatus, text_to_keys, to_crossterm_key};

use crate::app::{App, Focus, Tab, TaskId};
use crate::events::AppEvent;
use crate::tree::TASK_IDS;

pub fn update(app: &mut App, event: AppEvent) {
    match event {
        AppEvent::Key(key) => handle_key(app, key),
        AppEvent::Tick => {}
        AppEvent::Resize => {}
    }
}

/// Apply one agent input received through the taria layer, returning the ack
/// it has earned: [`Delivered`](InputStatus::Delivered) for one that reached a
/// handler able to act on it, [`Ignored`](InputStatus::Ignored) for one this
/// app looked at and deliberately did nothing with.
///
/// A key that parses counts as delivered even if nothing is bound to it, the
/// same verdict a person gets for pressing an unbound key. What is ignored here
/// is input this app cannot act on at all: a key the shared grammar rejects or
/// the adapter cannot lower, text sent while nothing here is accepting typing,
/// and an input kind added to taria after this app was written.
///
/// The lint is scoped to this one function on purpose. Neither `rustc` nor
/// default `clippy` can tell the wildcard arm `#[non_exhaustive]` demands from
/// one swallowing a variant this app should handle. This restriction lint
/// names every known variant a wildcard covers, and works on stable, where
/// rustc's own `non_exhaustive_omitted_patterns` does not yet.
#[warn(clippy::wildcard_enum_match_arm)]
pub fn apply_agent_input(app: &mut App, input: AgentInput) -> InputStatus {
    match input {
        AgentInput::Act {
            node,
            action,
            value,
            ..
        } => apply_act(app, node.0.as_str(), action, value),
        AgentInput::Key { key, .. } => match to_crossterm_key(&key) {
            Some(key) => {
                handle_key(app, key);
                InputStatus::Delivered
            }
            None => InputStatus::Ignored,
        },
        AgentInput::Text { text, .. } => apply_text(app, &text),
        // The layer answers this variant itself and never hands it over. It
        // is named here, as an arm of its own, only so the lint above stays
        // able to see the wildcard: folded into it as `Unknown | _`, the lint
        // skips the arm and goes quiet even with `Text` missing.
        AgentInput::Unknown => InputStatus::Ignored,
        // An input kind taria added after this app was written. Reported the
        // way the app reports every other input it looks at and does not act
        // on, so the agent hears "this app did nothing with it" rather than
        // waiting out the bridge's window for an effect that cannot come.
        //
        // This is the arm the compiler demands, and it would swallow `Text`
        // just as quietly: delete the `Text` arm and the build and default
        // clippy stay green while each `type_text` is answered Ignored. That
        // is how an app migrating from v0, where `Text` did not exist, loses
        // it. Here the typing tests would notice; an app without them has
        // only the lint on this function.
        _ => InputStatus::Ignored,
    }
}

/// Type `text` into this app's text-entry state, one character at a time.
///
/// Routed here rather than lowered through [`handle_key`] the way
/// [`AgentInput::Key`] is: typing belongs wherever the app puts typed
/// characters, and in this app that is the new-task input alone. So nothing is
/// typed while the list has the keyboard or the modal dialog is up, and the
/// caller reports [`Ignored`](InputStatus::Ignored), rather than the text meeting
/// the list's `d` and `y` bindings and deleting a task.
///
/// A newline mid-string still submits the draft, because [`text_to_keys`]
/// lowers it to Enter and that is what Enter does here. The characters after
/// it are not typed: submitting hands the keyboard back to the list, and this
/// app has nowhere else to put typing. Two tasks are two calls.
fn apply_text(app: &mut App, text: &str) -> InputStatus {
    let keys = text_to_keys(text);
    let mut typed = false;
    for key in keys {
        if !accepts_typing(app) {
            break;
        }
        handle_input_key(app, key);
        typed = true;
    }
    if typed {
        InputStatus::Delivered
    } else {
        InputStatus::Ignored
    }
}

/// Whether the app is accepting typed characters right now.
///
/// The one surface that takes typing is the new-task input, and only while it
/// holds the keyboard: the modal dialog covers it, and the list's keys are
/// commands rather than text. The tree says the same thing, since `input` is
/// the focused node in exactly these states.
fn accepts_typing(app: &App) -> bool {
    app.dialog.is_none() && app.focus == Focus::Input
}

fn apply_act(app: &mut App, node: &str, action: Action, value: Option<String>) -> InputStatus {
    // The delete dialog is modal: mirror `handle_key` by ignoring any act
    // that targets a node outside the dialog while it is open. The tree
    // stops advertising those actions too (see `crate::tree::build_nodes`),
    // so behavior and advertisement agree.
    if app.dialog.is_some() && !matches!(node, "dialog" | "dialog-confirm" | "dialog-cancel") {
        return InputStatus::Ignored;
    }
    match (node, action) {
        ("tab-active", Action::Select) => {
            switch_tab(app, Tab::Active);
            InputStatus::Delivered
        }
        ("tab-done", Action::Select) => {
            switch_tab(app, Tab::Done);
            InputStatus::Delivered
        }
        // `set_value` with no value asks for nothing. Treating a missing
        // value as the empty string would clear the draft, an edit the agent
        // never asked for, and report it as Delivered.
        ("input", Action::SetValue) => match value {
            Some(value) => {
                app.input = value;
                app.focus = Focus::Input;
                InputStatus::Delivered
            }
            None => InputStatus::Ignored,
        },
        ("input", Action::Activate) => submit_input(app),
        ("input", Action::Dismiss) => dismiss_input(app),
        ("input", Action::Focus) => focus_input(app),
        // The advertised way out. Until this existed, the only exit was the
        // raw `q` key, which is the fallback the project's conventions keep
        // off the primary path, and which types a letter rather than quitting
        // whenever the input holds the keyboard. The footer has told a person
        // `[q] quit` all along; this is the same affordance for an agent.
        ("quit", Action::Activate) => {
            app.running = false;
            InputStatus::Delivered
        }
        // The dialog's nodes exist only while it is open, so these three are
        // acts on a node that may be gone: an agent planning from a snapshot
        // it read just before the operator pressed `n` sends one against a
        // tree that no longer has it. Reporting Delivered for that would be the
        // same lie the task arms already refuse to tell for an id that has
        // been deleted, so the verdict is theirs too.
        ("dialog-confirm", Action::Activate) => confirm_delete(app),
        ("dialog-cancel", Action::Activate) | ("dialog", Action::Dismiss) => dismiss_dialog(app),
        (node, action) => apply_task_act(app, node, action),
    }
}

fn apply_task_act(app: &mut App, node: &str, action: Action) -> InputStatus {
    // Ids are parsed through the same space that built them, and the task is
    // looked up by identity: an id an agent read before an unrelated delete
    // still names the task it named then, or nothing at all.
    let Some(id) = TASK_IDS
        .parse::<TaskId>(node)
        .filter(|id| app.task(*id).is_some())
    else {
        return InputStatus::Ignored;
    };
    match action {
        Action::Select => match app.visible_position(id) {
            Some(pos) => {
                app.selection = pos;
                InputStatus::Delivered
            }
            // The task exists but sits on the other tab, so there is no
            // position on screen to move the cursor to.
            None => InputStatus::Ignored,
        },
        Action::Toggle => {
            if let Some(task) = app.task_mut(id) {
                task.done = !task.done;
            }
            app.clamp_selection();
            InputStatus::Delivered
        }
        Action::Custom(name) if name == "delete" => {
            app.dialog = Some(id);
            InputStatus::Delivered
        }
        _ => InputStatus::Ignored,
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
            dismiss_input(app);
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        _ => {}
    }
}

/// Throw the draft away and hand the keyboard back to the list: what Esc does
/// for a person, and the input's `dismiss` for an agent.
///
/// The way out of the input is the reason this is a semantic action at all.
/// `set_value` moves focus here, and until `dismiss` existed nothing
/// advertised moved it back, so every later `key` an agent sent was typed into
/// the draft and the only escape was the raw-key fallback the project's
/// conventions keep off the main path.
///
/// Ignored when the input does not have the keyboard, because then there is
/// nothing to hand back. That is also what keeps it Esc's exact counterpart:
/// the list ignores Esc too, and the tree only advertises `dismiss` while the
/// input is focused.
fn dismiss_input(app: &mut App) -> InputStatus {
    if app.focus != Focus::Input {
        return InputStatus::Ignored;
    }
    app.input.clear();
    app.focus = Focus::List;
    InputStatus::Delivered
}

/// Move the keyboard to the new-task input: [`dismiss_input`]'s counterpart,
/// and the semantic form of the `i` key a person presses.
///
/// Advertised only while the keyboard is elsewhere, and reported ignored when
/// it is already here, so the advertisement and the verdict agree the way
/// `dismiss`'s do. Until this existed the only semantic way to move the
/// keyboard here was `set_value`, which moves it as a side effect of
/// replacing the draft, so an agent that wanted the keyboard and nothing else
/// had to overwrite the draft or clear it with an empty value.
fn focus_input(app: &mut App) -> InputStatus {
    if app.focus == Focus::Input {
        return InputStatus::Ignored;
    }
    app.focus = Focus::Input;
    InputStatus::Delivered
}

/// Submit the input draft as a new task and hand focus back to the list with
/// the new task selected. An empty draft adds nothing and is reported
/// ignored, so an agent that activates too early hears about it.
///
/// A new task is active, so it lives on the Active tab, and submitting from
/// the Done tab used to leave it in no published node at all: the draft
/// cleared, the tree changed, the cursor stayed put, and an agent that then
/// read the tree found nothing it had asked for and could reasonably submit
/// again. Showing the tab the task landed on is what makes the addition
/// observable, and it is what a person adding a task wants to see too.
fn submit_input(app: &mut App) -> InputStatus {
    let title = app.input.trim().to_string();
    if title.is_empty() {
        return InputStatus::Ignored;
    }
    let id = app.add_task(title);
    app.input.clear();
    app.focus = Focus::List;
    switch_tab(app, Tab::Active);
    if let Some(pos) = app.visible_position(id) {
        app.selection = pos;
    }
    InputStatus::Delivered
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
        KeyCode::Char('y') | KeyCode::Enter => {
            confirm_delete(app);
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            dismiss_dialog(app);
        }
        _ => {}
    }
}

/// Delete the task the open dialog names. Ignored when no dialog is open:
/// this handler is only ever reached that way by an agent acting on a tree
/// the dialog has since left.
fn confirm_delete(app: &mut App) -> InputStatus {
    let Some(id) = app.dialog.take() else {
        return InputStatus::Ignored;
    };
    app.remove_task(id);
    app.clamp_selection();
    InputStatus::Delivered
}

/// Close the dialog without deleting anything, with the same verdict rule as
/// [`confirm_delete`].
fn dismiss_dialog(app: &mut App) -> InputStatus {
    if app.dialog.take().is_none() {
        return InputStatus::Ignored;
    }
    InputStatus::Delivered
}

#[cfg(test)]
mod tests {
    use taria::NodeId;

    use super::*;

    // The seeds are tasks 1 through 5; 2 and 5 start done, so the Active tab
    // shows task-1, task-3, task-4 and the Done tab task-2, task-5.

    fn act(node: &str, action: Action) -> AgentInput {
        AgentInput::act(NodeId(node.into()), action, None)
    }

    fn act_value(node: &str, action: Action, value: &str) -> AgentInput {
        AgentInput::act(NodeId(node.into()), action, Some(value.into()))
    }

    fn key(key: &str) -> AgentInput {
        AgentInput::key(key)
    }

    fn text(text: &str) -> AgentInput {
        AgentInput::text(text)
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
            InputStatus::Delivered
        );
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input, "Ship the demo");

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            InputStatus::Delivered
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
    /// alone and say so, rather than clearing it and reporting Delivered.
    #[test]
    fn set_value_without_a_value_leaves_the_draft_alone() {
        let mut app = App::new();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "half typed"));

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::SetValue)),
            InputStatus::Ignored
        );
        assert_eq!(app.input, "half typed", "the draft must survive");

        // An explicit empty value is a different request, and still clears it.
        assert_eq!(
            apply_agent_input(&mut app, act_value("input", Action::SetValue, "")),
            InputStatus::Delivered
        );
        assert!(app.input.is_empty());
    }

    /// The tree stops advertising `activate` on an empty draft (see
    /// [`crate::tree`]), so this is now the answer to an act sent against a
    /// tree read before the draft was cleared, not to one an agent could read
    /// as available.
    #[test]
    fn activate_on_empty_input_adds_nothing() {
        let mut app = App::new();
        let before = app.tasks.len();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "   "));
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            InputStatus::Ignored,
            "an agent that activates an empty draft hears that nothing happened"
        );
        assert_eq!(app.tasks.len(), before);
    }

    /// Submitting while the Done tab was on screen used to create a task that
    /// appeared in no published node: the draft cleared and the tree changed,
    /// so the bridge reported success, while an agent reading the tree back
    /// found nothing it had asked for and could reasonably submit again. A
    /// new task is active, so the app shows the tab it landed on.
    #[test]
    fn submitting_from_the_done_tab_shows_the_task_it_created() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("tab-done", Action::Select));
        assert_eq!(app.tab, Tab::Done);

        apply_agent_input(&mut app, act_value("input", Action::SetValue, "Ship it"));
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Activate)),
            InputStatus::Delivered
        );

        let id = app.tasks.last().unwrap().id;
        assert_eq!(app.task(id).unwrap().title, "Ship it");
        assert_eq!(app.tab, Tab::Active, "the tab the new task lives on");
        assert_eq!(
            app.selected_id(),
            Some(id),
            "and the cursor names it, so the addition is readable"
        );
    }

    /// The advertised exit. Without it the only way out is the raw `q` key,
    /// which is the fallback the project keeps off the primary path, and
    /// which types a letter while the input holds the keyboard.
    #[test]
    fn agent_can_quit_through_the_quit_node() {
        let mut app = App::new();
        assert!(app.running);
        assert_eq!(
            apply_agent_input(&mut app, act("quit", Action::Activate)),
            InputStatus::Delivered
        );
        assert!(!app.running);
    }

    /// And the modal gate covers it like every other node outside the dialog:
    /// a dialog asking whether to delete a task is not a moment to quit.
    #[test]
    fn quit_is_blocked_while_the_dialog_is_open() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        assert_eq!(
            apply_agent_input(&mut app, act("quit", Action::Activate)),
            InputStatus::Ignored
        );
        assert!(app.running, "the dialog must stop the quit");
    }

    /// The path `focus` exists for: take the keyboard without touching the
    /// draft, then type into it.
    ///
    /// Until this action was advertised the only semantic way here was
    /// `set_value`, which moves the keyboard as a side effect of replacing
    /// the draft. An agent that wanted to type its own text had to overwrite
    /// the draft first, or clear it with an empty value, to move a keyboard
    /// it could then type into. A live agent hit exactly that and worked
    /// around it by letting `set_value` carry the whole title.
    #[test]
    fn focus_takes_the_keyboard_without_touching_the_draft() {
        let mut app = App::new();
        app.input = "half typed".to_string();
        assert_eq!(app.focus, Focus::List);

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Focus)),
            InputStatus::Delivered
        );
        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.input, "half typed", "focus is not an edit");

        // And now typing lands, which is the whole point of aiming: before
        // the focus act this same text would have been answered Ignored.
        assert_eq!(
            apply_agent_input(&mut app, text(" more")),
            InputStatus::Delivered
        );
        assert_eq!(app.input, "half typed more");
    }

    /// Advertised only where it does something, so reported ignored where it
    /// does not: the tree and the verdict have to agree.
    #[test]
    fn focus_is_ignored_when_the_input_already_holds_the_keyboard() {
        let mut app = App::new();
        app.focus = Focus::Input;
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Focus)),
            InputStatus::Ignored
        );
        assert_eq!(app.focus, Focus::Input, "an ignored act changes nothing");
    }

    /// The modal gate covers `focus` like every other act on a node outside
    /// the dialog, and the tree stops advertising it for the same reason.
    #[test]
    fn focus_is_ignored_while_the_dialog_is_up() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Focus)),
            InputStatus::Ignored
        );
        assert_eq!(app.focus, Focus::List, "the dialog must stop the move");
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
            InputStatus::Delivered
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
            InputStatus::Ignored
        );
        assert_eq!(app.tasks, before, "a stale id must move nothing");
    }

    #[test]
    fn unknown_ids_and_unhandled_actions_report_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, act("task-1", Action::Toggle)),
            InputStatus::Delivered,
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
                InputStatus::Ignored,
                "{input:?}"
            );
        }
    }

    #[test]
    fn the_modal_gate_reports_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into()))),
            InputStatus::Delivered
        );
        let blocked = [
            act("tab-done", Action::Select),
            act("task-3", Action::Toggle),
            act_value("input", Action::SetValue, "sneaky task"),
        ];
        for input in blocked {
            assert_eq!(
                apply_agent_input(&mut app, input.clone()),
                InputStatus::Ignored,
                "{input:?}"
            );
        }
        assert_eq!(
            apply_agent_input(&mut app, act("dialog-cancel", Action::Activate)),
            InputStatus::Delivered,
            "the dialog's own nodes still answer"
        );
    }

    #[test]
    fn an_unparseable_key_reports_ignored() {
        let mut app = App::new();
        assert_eq!(
            apply_agent_input(&mut app, key("not-a-key")),
            InputStatus::Ignored
        );
        assert_eq!(
            apply_agent_input(&mut app, key("j")),
            InputStatus::Delivered
        );
    }

    #[test]
    fn text_types_into_the_focused_input() {
        let mut app = App::new();
        // Focusing first is the agent's job; set_value on the input does it.
        apply_agent_input(&mut app, act_value("input", Action::SetValue, ""));
        assert_eq!(app.focus, Focus::Input);

        assert_eq!(
            apply_agent_input(&mut app, text("buy milk")),
            InputStatus::Delivered
        );
        assert_eq!(app.input, "buy milk");

        // A newline lowers to Enter, so one message can type and submit.
        apply_agent_input(&mut app, text("\n"));
        assert_eq!(app.tasks.last().unwrap().title, "buy milk");
        assert_eq!(app.focus, Focus::List);
    }

    /// The bug this routing exists to end: `type_text("deploy")` with the
    /// list focused used to lower to plain key events, where `d` opened the
    /// delete dialog and the `y` in the same word confirmed it. One call
    /// deleted a task and the bridge reported success, because a task
    /// vanishing is a changed tree.
    #[test]
    fn text_at_the_list_types_nothing_and_says_so() {
        let mut app = App::new();
        assert_eq!(app.focus, Focus::List);
        let before = app.tasks.clone();

        assert_eq!(
            apply_agent_input(&mut app, text("deploy")),
            InputStatus::Ignored,
            "the list accepts no typing, and the agent has to hear that"
        );
        assert_eq!(app.tasks, before, "not one task may be touched");
        assert_eq!(app.dialog, None, "no `d` may reach the delete binding");
        assert!(app.running, "no `q` may reach the quit binding");
        assert!(app.input.is_empty(), "nothing was typed anywhere");

        // The raw key fallback is unchanged: a keypress is a keypress and is
        // supposed to reach the bindings.
        assert_eq!(
            apply_agent_input(&mut app, key("d")),
            InputStatus::Delivered
        );
        assert_eq!(app.dialog, Some(1), "key d still opens the dialog");
    }

    /// The modal is the other state accepting no typing, and the one where
    /// the old lowering cost a task: `y` inside the text confirmed the
    /// delete.
    #[test]
    fn text_at_the_dialog_types_nothing_and_says_so() {
        let mut app = App::new();
        apply_agent_input(&mut app, act("task-1", Action::Custom("delete".into())));
        assert_eq!(app.dialog, Some(1));
        let before = app.tasks.clone();

        assert_eq!(
            apply_agent_input(&mut app, text("yes please")),
            InputStatus::Ignored
        );
        assert_eq!(app.dialog, Some(1), "the dialog is still asking");
        assert_eq!(app.tasks, before, "nothing was deleted");
        assert!(app.input.is_empty());
    }

    /// A newline mid-string submits, which hands the keyboard back to the
    /// list, and this app has no second typing surface for the rest. It stops
    /// there rather than typing the tail into a draft nothing is looking at.
    #[test]
    fn text_stops_where_the_app_stops_accepting_typing() {
        let mut app = App::new();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, ""));
        let before = app.tasks.len();

        assert_eq!(
            apply_agent_input(&mut app, text("first\nsecond")),
            InputStatus::Delivered
        );
        assert_eq!(app.tasks.len(), before + 1, "one task, not two");
        assert_eq!(app.tasks.last().unwrap().title, "first");
        assert_eq!(
            app.focus,
            Focus::List,
            "the submit handed the keyboard back"
        );
        assert!(
            app.input.is_empty(),
            "the tail was not typed into a new draft"
        );
    }

    #[test]
    fn empty_text_reports_ignored() {
        let mut app = App::new();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "draft"));
        assert_eq!(app.focus, Focus::Input, "the input is accepting typing");

        assert_eq!(apply_agent_input(&mut app, text("")), InputStatus::Ignored);
        assert_eq!(
            apply_agent_input(&mut app, text("\r")),
            InputStatus::Ignored,
            "a lone carriage return lowers to no key events"
        );
        assert_eq!(app.input, "draft", "neither touched the draft");
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

    /// The dialog's nodes are published only while it is open, so an act on
    /// one of them with no dialog up comes from an agent working off a
    /// snapshot the operator has already moved past. It gets the same answer
    /// a stale task id gets, rather than Delivered for a node that is gone.
    #[test]
    fn dialog_acts_with_no_dialog_open_are_ignored() {
        for input in [
            act("dialog-confirm", Action::Activate),
            act("dialog-cancel", Action::Activate),
            act("dialog", Action::Dismiss),
        ] {
            let mut app = App::new();
            let before = app.tasks.len();
            assert_eq!(app.dialog, None);
            assert_eq!(
                apply_agent_input(&mut app, input.clone()),
                InputStatus::Ignored,
                "input: {input:?}"
            );
            assert_eq!(app.tasks.len(), before, "nothing may be deleted: {input:?}");
        }
    }

    /// The counterpart of Esc, and the only advertised way out of the input.
    /// Ignored where Esc does nothing either, so an agent is not told the
    /// keyboard moved when it did not.
    #[test]
    fn dismiss_leaves_the_input_and_is_ignored_from_the_list() {
        let mut app = App::new();
        apply_agent_input(&mut app, act_value("input", Action::SetValue, "half typed"));
        assert_eq!(app.focus, Focus::Input);

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Dismiss)),
            InputStatus::Delivered
        );
        assert_eq!(app.focus, Focus::List, "the keyboard goes back to the list");
        assert!(
            app.input.is_empty(),
            "the draft is thrown away, as Esc does"
        );

        assert_eq!(
            apply_agent_input(&mut app, act("input", Action::Dismiss)),
            InputStatus::Ignored,
            "with the list focused there is nothing to hand back"
        );
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

        assert_eq!(
            apply_agent_input(&mut app, key("ctrl+q")),
            InputStatus::Delivered
        );
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
            InputStatus::Delivered
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
