//! Application state (the Model). All mutable state lives here.

/// One task in the manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
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
    pub tab: Tab,
    /// Index into the currently visible list (not into `tasks`).
    pub selection: usize,
    pub focus: Focus,
    /// Original `tasks` index of the task pending delete confirmation.
    pub dialog: Option<usize>,
    /// Draft text of the new-task input.
    pub input: String,
    /// Socket path shown in the title bar; set once in `main`.
    pub socket_hint: String,
}

impl App {
    pub fn new() -> Self {
        let tasks = vec![
            Task {
                title: "Write the taria README".into(),
                done: false,
            },
            Task {
                title: "Sketch the protocol wire types".into(),
                done: true,
            },
            Task {
                title: "Wire up the MCP bridge".into(),
                done: false,
            },
            Task {
                title: "Record the killer demo".into(),
                done: false,
            },
            Task {
                title: "Publish a snapshot per frame".into(),
                done: true,
            },
        ];
        Self {
            running: true,
            tasks,
            tab: Tab::Active,
            selection: 0,
            focus: Focus::List,
            dialog: None,
            input: String::new(),
            socket_hint: String::new(),
        }
    }

    /// Original `tasks` indices of the tasks on the currently visible tab,
    /// in list order. Node ids and dialog targets use these original
    /// indices so they stay stable while tasks move between tabs.
    pub fn visible_indices(&self) -> Vec<usize> {
        let want_done = self.tab == Tab::Done;
        self.tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.done == want_done)
            .map(|(i, _)| i)
            .collect()
    }

    /// Original `tasks` index of the currently selected task, if any.
    pub fn selected_task(&self) -> Option<usize> {
        self.visible_indices().get(self.selection).copied()
    }

    /// Keep `selection` valid after the visible list changed.
    pub fn clamp_selection(&mut self) {
        let len = self.visible_indices().len();
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
