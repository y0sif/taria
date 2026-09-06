//! Application state (the Model). All mutable state lives here.

/// Identity of one task, stable for as long as the task exists.
///
/// A position in [`App::tasks`] is not an identity: deleting a task shifts
/// every later position, so an agent acting on an id it read before the
/// delete would hit whichever task slid into that slot. Ids are handed out
/// once and never reused, so a stale id names nothing rather than the wrong
/// task.
pub type TaskId = u64;

/// Id the first task of a fresh [`App`] takes.
///
/// One rather than zero so a task id never coincides with a list position,
/// which is exactly the confusion the id scheme exists to end.
const FIRST_TASK_ID: TaskId = 1;

/// The tasks a fresh [`App`] starts with, as `(title, done)` in list order.
///
/// A fixed table combined with a fixed starting id is what makes ids
/// reproducible: restarting the demo republishes the same node id for the
/// same task, so an agent script written against one run still applies.
const SEED_TASKS: [(&str, bool); 5] = [
    ("Write the taria README", false),
    ("Sketch the protocol wire types", true),
    ("Wire up the MCP bridge", false),
    ("Record the killer demo", false),
    ("Publish a snapshot per frame", true),
];

/// One task in the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub done: bool,
}

/// Which list is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Active,
    Done,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Active => "Active",
            Tab::Done => "Done",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Tab::Active => Tab::Done,
            Tab::Done => Tab::Active,
        }
    }
}

/// Which region owns keyboard input (when no dialog is open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Input,
}

pub struct App {
    pub running: bool,
    pub tasks: Vec<Task>,
    /// Id the next created task takes. Private so every id comes from
    /// [`App::fresh_id`] and no two tasks can end up sharing one.
    next_id: TaskId,
    pub tab: Tab,
    /// Position of the cursor in the currently visible list. Genuinely
    /// positional: it says where the cursor sits, not which task it sits on,
    /// so it survives a mutation only after [`App::clamp_selection`].
    pub selection: usize,
    pub focus: Focus,
    /// The task pending delete confirmation, by id, so the confirmation still
    /// names the task it was opened for however the list changes underneath.
    pub dialog: Option<TaskId>,
    /// Draft text of the new-task input.
    pub input: String,
    /// Socket path shown in the title bar; set once in `main`.
    pub socket_hint: String,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            running: true,
            tasks: Vec::new(),
            next_id: FIRST_TASK_ID,
            tab: Tab::Active,
            selection: 0,
            focus: Focus::List,
            dialog: None,
            input: String::new(),
            socket_hint: String::new(),
        };
        for (title, done) in SEED_TASKS {
            let id = app.fresh_id();
            app.tasks.push(Task {
                id,
                title: title.to_string(),
                done,
            });
        }
        app
    }

    /// Take the next unused [`TaskId`]. The only source of ids.
    fn fresh_id(&mut self) -> TaskId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Append a task with a fresh id and return that id, so the caller can
    /// name the task it just created.
    pub fn add_task(&mut self, title: String) -> TaskId {
        let id = self.fresh_id();
        self.tasks.push(Task {
            id,
            title,
            done: false,
        });
        id
    }

    /// Drop the task with `id`, if it is still there.
    pub fn remove_task(&mut self, id: TaskId) {
        self.tasks.retain(|task| task.id != id);
    }

    /// The tasks on the currently visible tab, in list order.
    ///
    /// The single place the tab filter lives, so the tree, the renderer and
    /// the selection arithmetic cannot disagree about what is on screen.
    pub fn visible_tasks(&self) -> impl Iterator<Item = &Task> {
        let want_done = self.tab == Tab::Done;
        self.tasks.iter().filter(move |task| task.done == want_done)
    }

    /// How many tasks the visible tab holds.
    pub fn visible_len(&self) -> usize {
        self.visible_tasks().count()
    }

    /// Position of `id` in the visible list, or `None` when that task is on
    /// the other tab or gone.
    pub fn visible_position(&self, id: TaskId) -> Option<usize> {
        self.visible_tasks().position(|task| task.id == id)
    }

    /// Id of the selected task, or `None` when the visible tab is empty.
    pub fn selected_id(&self) -> Option<TaskId> {
        self.visible_tasks().nth(self.selection).map(|task| task.id)
    }

    /// The task with `id`, or `None` if it no longer exists.
    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }

    /// [`App::task`], mutably.
    pub fn task_mut(&mut self, id: TaskId) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// Keep `selection` valid after the visible list changed.
    pub fn clamp_selection(&mut self) {
        let len = self.visible_len();
        self.selection = if len == 0 {
            0
        } else {
            self.selection.min(len - 1)
        };
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_ids_are_reproducible_across_restarts() {
        let ids = |app: &App| app.tasks.iter().map(|task| task.id).collect::<Vec<_>>();
        assert_eq!(ids(&App::new()), vec![1, 2, 3, 4, 5]);
        assert_eq!(
            ids(&App::new()),
            ids(&App::new()),
            "a restart must reproduce the same ids"
        );
    }

    #[test]
    fn ids_are_never_reused_after_a_delete() {
        let mut app = App::new();
        let removed = app.tasks[0].id;
        app.remove_task(removed);
        let added = app.add_task("later".into());
        assert_ne!(
            added, removed,
            "a fresh id must not resurrect a deleted one"
        );
        assert!(app.tasks.iter().all(|task| task.id != removed));
    }

    #[test]
    fn visible_tasks_follow_the_tab() {
        let mut app = App::new();
        assert_eq!(
            app.visible_tasks().map(|task| task.id).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        app.tab = Tab::Done;
        assert_eq!(
            app.visible_tasks().map(|task| task.id).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert_eq!(app.visible_len(), 2);
        assert_eq!(app.visible_position(5), Some(1));
        assert_eq!(app.visible_position(1), None, "task 1 is on the other tab");
    }

    #[test]
    fn clamp_selection_pulls_the_cursor_back_into_the_list() {
        let mut app = App::new();
        app.selection = 99;
        app.clamp_selection();
        assert_eq!(app.selection, app.visible_len() - 1);

        for task in &mut app.tasks {
            task.done = true;
        }
        app.clamp_selection();
        assert_eq!(app.selection, 0, "an empty list parks the cursor at 0");
        assert_eq!(app.selected_id(), None);
    }
}
