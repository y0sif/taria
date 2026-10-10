//! The state dump: the demo's model as JSON, for harnesses that need to know
//! what an agent actually did rather than what the screen or the tree says it
//! did.
//!
//! Pure (state in, string out) like [`mod@crate::tree`]; the file I/O lives in
//! `main`, which writes this only when `TARIA_DEMO_STATE` names a path. The
//! shape is a contract with `bench/`, so it is spelled out once, here:
//!
//! ```json
//! {"tasks":[{"id":"task-1","title":"Write the taria README","done":false}],
//!  "tab":"active","dialog":null,"draft":"","quit":false}
//! ```
//!
//! `tasks` is every task in stored order, whichever tab shows it; `tab` is
//! `"active"` or `"done"`; `dialog` is `null` or `{"task":"<task id>"}` for
//! the task pending delete confirmation; `draft` is the new-task input's text;
//! `quit` is true once the app has been told to exit. Task ids are the node
//! ids the tree publishes, so a harness can hold one against the other.

use serde::Serialize;

use crate::app::{App, Tab};
use crate::tree::TASK_IDS;

#[derive(Serialize)]
struct StateDump<'a> {
    tasks: Vec<TaskDump<'a>>,
    tab: &'static str,
    dialog: Option<DialogDump>,
    draft: &'a str,
    quit: bool,
}

#[derive(Serialize)]
struct TaskDump<'a> {
    id: String,
    title: &'a str,
    done: bool,
}

#[derive(Serialize)]
struct DialogDump {
    task: String,
}

/// Serialize `app` into the state-dump JSON, one line, no trailing newline.
///
/// Structs rather than `json!`, so the field order is the order above and
/// cannot move with a feature some other crate in the build turns on.
pub fn to_json(app: &App) -> serde_json::Result<String> {
    serde_json::to_string(&StateDump {
        tasks: app
            .tasks
            .iter()
            .map(|task| TaskDump {
                id: TASK_IDS.id(task.id),
                title: &task.title,
                done: task.done,
            })
            .collect(),
        tab: match app.tab {
            Tab::Active => "active",
            Tab::Done => "done",
        },
        dialog: app.dialog.map(|id| DialogDump {
            task: TASK_IDS.id(id),
        }),
        draft: &app.input,
        quit: !app.running,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn dump(app: &App) -> Value {
        serde_json::from_str(&to_json(app).unwrap()).unwrap()
    }

    /// The seed, byte for byte: field names, field order and the id spelling
    /// are the contract a harness parses, so a drift in any of them has to
    /// fail here rather than in a benchmark run.
    #[test]
    fn a_fresh_app_dumps_the_seed_exactly() {
        assert_eq!(
            to_json(&App::new()).unwrap(),
            concat!(
                r#"{"tasks":["#,
                r#"{"id":"task-1","title":"Write the taria README","done":false},"#,
                r#"{"id":"task-2","title":"Sketch the protocol wire types","done":true},"#,
                r#"{"id":"task-3","title":"Wire up the MCP bridge","done":false},"#,
                r#"{"id":"task-4","title":"Record the killer demo","done":false},"#,
                r#"{"id":"task-5","title":"Publish a snapshot per frame","done":true}"#,
                r#"],"tab":"active","dialog":null,"draft":"","quit":false}"#,
            )
        );
    }

    /// Every task in stored order, not the visible tab's slice of them: a
    /// harness checking a delete has to see the tasks the screen is hiding.
    #[test]
    fn tasks_are_dumped_in_stored_order_whatever_the_tab() {
        let mut app = App::new();
        app.tab = Tab::Done;
        app.remove_task(3);
        let ids: Vec<Value> = dump(&app)["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].clone())
            .collect();
        assert_eq!(ids, ["task-1", "task-2", "task-4", "task-5"]);
        assert_eq!(dump(&app)["tab"], "done");
    }

    #[test]
    fn the_dialog_names_its_task_by_node_id() {
        let mut app = App::new();
        app.dialog = Some(3);
        assert_eq!(dump(&app)["dialog"], json!({"task": "task-3"}));
    }

    /// The draft is agent-controlled text, so it has to survive the trip
    /// whatever it holds.
    #[test]
    fn the_draft_round_trips_whatever_it_holds() {
        let mut app = App::new();
        app.input = "say \"hi\"\\ \n\tüñí 🦀".to_string();
        assert_eq!(dump(&app)["draft"], app.input.as_str());
    }

    #[test]
    fn quit_follows_running() {
        let mut app = App::new();
        assert_eq!(dump(&app)["quit"], false);
        app.running = false;
        assert_eq!(dump(&app)["quit"], true);
    }
}
