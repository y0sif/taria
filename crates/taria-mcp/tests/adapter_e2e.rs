//! The real adapter against the real bridge, on every platform taria
//! supports: a `TariaLayer` in an app process of its own, and the `taria-mcp`
//! binary driven over stdio with MCP JSON-RPC, the way a harness drives it.
//!
//! No TUI, no pty and no Python, so it runs where `scripts/e2e.py` cannot.
//! The transport is the platform's own: a Unix socket in a private temp
//! directory on Unix, a named pipe on Windows.
//!
//! The app is this test binary run again with [`APP_PROCESS_ENV`] set, which
//! turns [`app_process`], an ignored test that otherwise does nothing, into
//! the app. It has to be a process of its own: a restarted app has to be a new
//! process with a new pid, a second app at a live endpoint has to be refused
//! by the endpoint and not by this process, and a bridge tells one app from
//! the next by the process at the other end.
//!
//! Answers are matched on the prefixes and phrases that callers already rely
//! on, never on whole texts, and trees are read as JSON, by node id.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use taria_ratatui::taria::{Action, AgentInput, Node, Role};
use taria_ratatui::{InputStatus, TariaLayer};

/// Generous bound for anything a process is waited on for: CI runners are
/// slow, and on a pass nothing comes near it.
const WAIT: Duration = Duration::from_secs(20);

/// Set to anything, the variable that makes [`app_process`] serve. The
/// endpoint itself travels in `TARIA_SOCK`, the variable an app reads it from.
const APP_PROCESS_ENV: &str = "TARIA_MCP_E2E_APP_PROCESS";

/// What the app process prints once it is serving, and when binding fails.
const LISTENING: &str = "e2e app: listening";
const BIND_REFUSED: &str = "e2e app: bind refused";

/// How the app process exits when its bind is refused.
const REFUSED_EXIT: i32 = 2;

/// The label the app binds under, which the bridge's answers name.
const APP_LABEL: &str = "e2e-app";

/// The rows of the app's list: ids and labels.
const ITEMS: [(&str, &str); 2] = [("item-a", "alpha"), ("item-b", "beta")];

/// What the app shows, and what agent input changes.
#[derive(Default)]
struct AppState {
    /// Index into [`ITEMS`] of the selected row.
    selected: usize,
    /// How many times `add` was activated.
    clicks: u32,
    /// What was typed into `draft`.
    draft: String,
    /// The last key the app was sent, as the bridge forwarded it.
    last_key: Option<String>,
}

impl AppState {
    fn nodes(&self) -> Vec<Node> {
        let items = ITEMS.iter().enumerate().map(|(index, (id, label))| {
            Node::new(*id, Role::ListItem)
                .label(*label)
                .selected(index == self.selected)
                .actions([Action::Select, Action::Activate])
        });
        vec![
            Node::new("list", Role::List)
                .label("items")
                .value(ITEMS[self.selected].0)
                .children(items),
            Node::new("add", Role::Button)
                .label(format!("clicked {}", self.clicks))
                .action(Action::Activate),
            // Advertises an action, so the bridge forwards an act on it, and
            // the app does nothing with it, so it answers Ignored.
            Node::new("unhandled", Role::Button)
                .label("does nothing")
                .action(Action::Activate),
            // Where typed text goes: the one node accepting typing.
            Node::new("draft", Role::TextInput)
                .label("draft")
                .value(self.draft.clone())
                .focused(true),
            Node::new("last-key", Role::Text)
                .label("last key")
                .value(self.last_key.clone().unwrap_or_else(|| "none".into())),
            // Which process this is, so a restart is visible in the tree.
            Node::new("pid", Role::Text)
                .label("pid")
                .value(std::process::id().to_string()),
        ]
    }

    fn apply(&mut self, input: AgentInput) -> InputStatus {
        match input {
            AgentInput::Act { node, action, .. } => {
                let row = ITEMS.iter().position(|(id, _)| *id == node.0);
                match (node.0.as_str(), action, row) {
                    ("add", Action::Activate, _) => self.clicks += 1,
                    (_, Action::Select | Action::Activate, Some(row)) => self.selected = row,
                    _ => return InputStatus::Ignored,
                }
            }
            AgentInput::Key { key, .. } => self.last_key = Some(key),
            AgentInput::Text { text, .. } => self.draft.push_str(&text),
            _ => return InputStatus::Ignored,
        }
        InputStatus::Delivered
    }
}

/// Not a test of its own: run by [`AppProcess::spawn`] with
/// [`APP_PROCESS_ENV`] set, it binds a layer at `$TARIA_SOCK` and serves
/// [`AppState`] until it is killed or its parent goes away. A refused bind is
/// printed as [`BIND_REFUSED`] with the error's kind, and exits with
/// [`REFUSED_EXIT`]. Without the variable it returns at once.
#[test]
#[ignore = "helper: the other tests run it as a child process"]
fn app_process() {
    if std::env::var_os(APP_PROCESS_ENV).is_none() {
        return;
    }
    // The parent holds this process's stdin open and never writes to it, so
    // end of file means the parent is gone, even killed with no chance to
    // kill this one; without this the app would serve forever.
    thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    let endpoint = std::env::var_os("TARIA_SOCK").expect("the parent sets TARIA_SOCK");
    let mut layer = match TariaLayer::bind_at(APP_LABEL, PathBuf::from(endpoint)) {
        Ok(layer) => layer,
        Err(err) => {
            println!("{BIND_REFUSED} ({:?}): {err}", err.kind());
            let _ = std::io::stdout().flush();
            std::process::exit(REFUSED_EXIT);
        }
    };
    println!("{LISTENING}, pid {}", std::process::id());
    let _ = std::io::stdout().flush();
    let mut state = AppState::default();
    loop {
        // Apply, then publish: the first tree after a dequeue is the one the
        // bridge credits to that input, so it carries the effect.
        layer.drain(|input| state.apply(input));
        layer.publish(state.nodes());
        thread::sleep(Duration::from_millis(10));
    }
}

/// Where an app is reached: a socket path in a private temp directory on
/// Unix, a named pipe on Windows. Unique per test, so tests run in parallel.
struct Endpoint {
    path: PathBuf,
    /// Removed, with the socket in it, when the test ends.
    #[cfg(unix)]
    _dir: tempfile::TempDir,
}

impl Endpoint {
    #[cfg(unix)]
    fn new(name: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("taria-e2e-")
            .tempdir()
            .expect("create the socket directory");
        // The layer serves only from a directory no other user can enter,
        // and tempfile leaves the mode to the umask.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("make the socket directory private");
        Self {
            path: dir.path().join(format!("{name}.sock")),
            _dir: dir,
        }
    }

    #[cfg(windows)]
    fn new(_name: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: PathBuf::from(format!(r"\\.\pipe\taria-test-{}-{n}", std::process::id())),
        }
    }
}

/// Forward every line `from` writes to a channel, from a thread of its own.
fn line_channel(from: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(from).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// An app process serving at an endpoint, killed when dropped.
struct AppProcess {
    child: Child,
    pid: u32,
    /// The write end of the process's stdin, its lifeline: the process exits
    /// once this closes, which happens when this process dies.
    _lifeline: ChildStdin,
    lines: mpsc::Receiver<String>,
    /// Everything the process printed so far, for failure messages.
    seen: Vec<String>,
}

impl AppProcess {
    /// Start one at `endpoint` without waiting for it to bind.
    fn spawn(endpoint: &Endpoint) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                "app_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(APP_PROCESS_ENV, "1")
            .env("TARIA_SOCK", &endpoint.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the app process");
        let lifeline = child.stdin.take().expect("the app's stdin is piped");
        let lines = line_channel(child.stdout.take().expect("the app's stdout is piped"));
        let pid = child.id();
        Self {
            child,
            pid,
            _lifeline: lifeline,
            lines,
            seen: Vec::new(),
        }
    }

    /// Start one at `endpoint`, and wait until it serves.
    fn start(endpoint: &Endpoint) -> Self {
        let mut app = Self::spawn(endpoint);
        app.wait_for(LISTENING);
        app
    }

    /// Wait for the process to print a line containing `marker`, and return
    /// that line.
    fn wait_for(&mut self, marker: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(marker) {
                        return line;
                    }
                }
                Err(err) => panic!(
                    "the app process never printed {marker:?} ({err}); it printed:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    /// Wait for the process to exit by itself.
    fn exit_status(&mut self) -> ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll the app process") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the app process did not exit; it printed:\n{}",
                self.seen.join("\n")
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kill the process and wait until it is gone.
    fn kill(mut self) {
        self.child.kill().expect("kill the app process");
        self.child.wait().expect("reap the app process");
    }
}

impl Drop for AppProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A running `taria-mcp`, driven over stdio, killed when dropped.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: mpsc::Receiver<String>,
    log: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

impl Bridge {
    /// Start one pointed at `endpoint` with `--idle-release <idle_release>`,
    /// and complete the MCP handshake.
    fn start(endpoint: &Endpoint, idle_release: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_taria-mcp"))
            .arg("--socket")
            .arg(&endpoint.path)
            .args(["--idle-release", idle_release])
            .env("TARIA_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start taria-mcp");
        let stdout = line_channel(child.stdout.take().expect("piped stdout"));
        let stderr = child.stderr.take().expect("piped stderr");
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        // Read on as it is written, so a full pipe never blocks the bridge.
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                sink.lock().expect("log lock").push(line);
            }
        });
        let stdin = child.stdin.take();
        let mut bridge = Self {
            child,
            stdin,
            stdout,
            log,
            next_id: 1,
        };
        let init = bridge.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "adapter-e2e", "version": "0"},
            }),
        );
        assert!(init.get("result").is_some(), "{init}");
        bridge.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        bridge
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write to taria-mcp");
        stdin.flush().expect("flush to taria-mcp");
    }

    /// Send a request and wait for its response, skipping anything else on
    /// stdout.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.stdout.recv_timeout(left).unwrap_or_else(|err| {
                panic!(
                    "no response to {method} (id {id}): {err}; log:\n{}",
                    self.log()
                )
            });
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("stdout carries only MCP: {err}: {line}"));
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    /// Call a tool: the text of its result, or the message of its error.
    fn tool(&mut self, name: &str, arguments: Value) -> Result<String, String> {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        if let Some(result) = response.get("result") {
            let text = result["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("a text result: {response}"))
                .to_string();
            if result["isError"] == json!(true) {
                Err(text)
            } else {
                Ok(text)
            }
        } else if let Some(message) = response["error"]["message"].as_str() {
            Err(message.to_string())
        } else {
            panic!("neither a result nor an error: {response}")
        }
    }

    fn read_tree(&mut self) -> Result<String, String> {
        self.tool("read_tree", json!({}))
    }

    fn act(&mut self, node: &str, action: &str) -> Result<String, String> {
        self.tool("act", json!({"node": node, "action": action}))
    }

    /// Every log line written so far.
    fn log(&self) -> String {
        self.log.lock().expect("log lock").join("\n")
    }

    /// Wait for a log line containing `marker`.
    fn wait_for_log(&self, marker: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.log().contains(marker) {
            assert!(
                Instant::now() < deadline,
                "no {marker:?} in the log:\n{}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Closing stdin ends the session; the kill is for a binary that does
        // not take the hint, so no test leaves a process behind.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The tree an answer carries: the answer itself when it is one, or what
/// follows the line that introduces it.
fn tree_in(answer: &str) -> Value {
    let start = if answer.starts_with('{') {
        0
    } else {
        answer
            .find("\n{")
            .map(|at| at + 1)
            .unwrap_or_else(|| panic!("no tree in the answer: {answer}"))
    };
    serde_json::Deserializer::from_str(&answer[start..])
        .into_iter::<Value>()
        .next()
        .and_then(Result::ok)
        .unwrap_or_else(|| panic!("the tree in the answer is not JSON: {answer}"))
}

/// The node `id` in `tree`, wherever it is.
fn node<'a>(tree: &'a Value, id: &str) -> &'a Value {
    fn find<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
        if node["id"].as_str() == Some(id) {
            return Some(node);
        }
        node["children"]
            .as_array()?
            .iter()
            .find_map(|child| find(child, id))
    }
    find(&tree["root"], id).unwrap_or_else(|| panic!("no node {id:?} in {tree}"))
}

/// The value of node `id` in `tree`.
fn value<'a>(tree: &'a Value, id: &str) -> &'a str {
    node(tree, id)["value"]
        .as_str()
        .unwrap_or_else(|| panic!("node {id:?} has no value: {tree}"))
}

/// The pid of the app that published `tree`.
fn pid_of(tree: &Value) -> u32 {
    value(tree, "pid")
        .parse()
        .expect("the pid node holds a pid")
}

/// Call `read_tree` until it serves a tree published by `pid`, and return
/// that tree; every other answer on the way is a retry.
fn tree_from(bridge: &mut Bridge, pid: u32) -> Value {
    let deadline = Instant::now() + WAIT;
    let mut last;
    loop {
        last = match bridge.read_tree() {
            Ok(text) => {
                let tree = tree_in(&text);
                if pid_of(&tree) == pid {
                    return tree;
                }
                format!("a tree from pid {}", pid_of(&tree))
            }
            Err(message) => message,
        };
        assert!(
            Instant::now() < deadline,
            "read_tree never served pid {pid}'s tree; last answer: {last}\nlog:\n{}",
            bridge.log()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// `read_tree`, `act`, `key` and `type_text` each reach the app and come back
/// with what it did: a changed tree inside the answer, or the ignored answer
/// with the tree after it.
#[test]
fn every_tool_reaches_the_app_and_answers_with_its_tree() {
    let endpoint = Endpoint::new("tools");
    let app = AppProcess::start(&endpoint);
    let mut bridge = Bridge::start(&endpoint, "60");

    let tree = tree_in(&bridge.read_tree().expect("read_tree serves the app"));
    assert_eq!(pid_of(&tree), app.pid, "{tree}");
    assert_eq!(tree["root"]["label"], json!(APP_LABEL), "{tree}");
    assert_eq!(node(&tree, "item-a")["selected"], json!(true), "{tree}");
    assert_eq!(node(&tree, "add")["label"], json!("clicked 0"), "{tree}");
    assert_eq!(node(&tree, "add")["actions"], json!(["activate"]), "{tree}");

    let answer = bridge.act("add", "activate").expect("act on add");
    let tree = tree_in(&answer);
    assert_eq!(
        node(&tree, "add")["label"],
        json!("clicked 1"),
        "the act's answer carries the new tree: {answer}"
    );

    let answer = bridge.act("item-b", "select").expect("act on item-b");
    let tree = tree_in(&answer);
    assert_eq!(value(&tree, "list"), "item-b", "{answer}");
    assert_eq!(node(&tree, "item-b")["selected"], json!(true), "{answer}");
    assert_ne!(node(&tree, "item-a")["selected"], json!(true), "{answer}");

    let answer = bridge
        .act("unhandled", "activate")
        .expect("an ignored act is an answer, not an error");
    assert!(
        answer.starts_with("The app received this input and deliberately did nothing with it"),
        "{answer}"
    );
    let tree = tree_in(&answer);
    assert_eq!(
        node(&tree, "add")["label"],
        json!("clicked 1"),
        "the ignored answer carries the current tree: {answer}"
    );

    let answer = bridge
        .tool("key", json!({"key": "x"}))
        .expect("key reaches the app");
    assert_eq!(value(&tree_in(&answer), "last-key"), "x", "{answer}");

    let answer = bridge
        .tool("type_text", json!({"text": "hello"}))
        .expect("type_text reaches the app");
    let tree = tree_in(&answer);
    assert_eq!(value(&tree, "draft"), "hello", "{answer}");
    assert_eq!(
        value(&tree, "last-key"),
        "x",
        "typed text went to the input, not the key handler: {answer}"
    );

    let tree = tree_in(&bridge.read_tree().expect("read_tree after the inputs"));
    assert_eq!(node(&tree, "add")["label"], json!("clicked 1"), "{tree}");
    assert_eq!(value(&tree, "list"), "item-b", "{tree}");
    assert_eq!(value(&tree, "draft"), "hello", "{tree}");
}

/// The adapter serves one bridge at a time. A second `taria-mcp` against an
/// app the first holds is told the app is held by another bridge, and gets in
/// once the first lets go after its idle window.
#[test]
fn a_second_bridge_is_held_until_the_first_lets_go() {
    let endpoint = Endpoint::new("held");
    let app = AppProcess::start(&endpoint);
    // Both started before either connects (a bridge connects on its first
    // tool call), so the second's start-up is not spent out of the first's
    // one-second window.
    let mut first = Bridge::start(&endpoint, "1");
    let mut second = Bridge::start(&endpoint, "60");
    let tree = tree_in(&first.read_tree().expect("the first bridge gets the app"));
    assert_eq!(pid_of(&tree), app.pid, "{tree}");

    let held = thread::scope(|scope| {
        // Keep the first bridge's session busy while the second is refused,
        // so its window cannot close under a slow runner and hand the second
        // the app before it was ever held.
        let (stop, stopped) = mpsc::channel::<()>();
        let keeper = &mut first;
        scope.spawn(move || {
            while matches!(
                stopped.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                keeper
                    .read_tree()
                    .expect("the first bridge keeps the app while the second waits");
            }
        });
        let deadline = Instant::now() + WAIT;
        let held = loop {
            match second.read_tree() {
                // A connection younger than the grace is worth one retry
                // before it is called held.
                Err(message) if message.contains("worth one retry") => {}
                Err(message) => break message,
                Ok(text) => panic!("the second bridge got the app while the first held it: {text}"),
            }
            assert!(
                Instant::now() < deadline,
                "the second bridge was never diagnosed"
            );
            thread::sleep(Duration::from_millis(50));
        };
        let _ = stop.send(());
        held
    });
    assert!(
        held.contains("held by another bridge"),
        "the second bridge is told the app is held: {held}"
    );

    // The first bridge now makes no call, lets go after its window, and the
    // second is served the tree.
    let tree = tree_from(&mut second, app.pid);
    assert_eq!(node(&tree, "add")["label"], json!("clicked 0"), "{tree}");
    first.wait_for_log("released the app");
}

/// An app killed and started again is a new process at the same endpoint.
/// The bridge reconnects on its own and serves the new process's tree.
#[test]
fn the_bridge_reconnects_to_a_restarted_app() {
    let endpoint = Endpoint::new("restart");
    let first_app = AppProcess::start(&endpoint);
    let first_pid = first_app.pid;
    let mut bridge = Bridge::start(&endpoint, "60");
    let tree = tree_in(&bridge.read_tree().expect("the bridge gets the app"));
    assert_eq!(pid_of(&tree), first_pid, "{tree}");

    // Gone for good before the next one starts: `kill` waits on the process.
    first_app.kill();
    // Started at once, which is part of what this tests. A Unix socket
    // outlives a killed app, as a file nobody listens on, and the new process
    // clears it and binds. A pipe's name stays in use until the bridge has
    // closed its end of the killed app's instance as well, and the new
    // process's bind waits that moment out rather than starting with taria
    // off.
    let second_app = AppProcess::start(&endpoint);
    assert_ne!(second_app.pid, first_pid);

    let tree = tree_from(&mut bridge, second_app.pid);
    let answer = bridge.act("add", "activate").expect("act on the new app");
    assert_eq!(
        node(&tree_in(&answer), "add")["label"],
        json!("clicked 1"),
        "inputs reach the new process: {answer}"
    );
    assert_eq!(node(&tree, "add")["label"], json!("clicked 0"), "{tree}");
}

/// A second app binding an endpoint a live app serves is refused with
/// `AddrInUse`, and the first goes on being served there.
#[test]
fn a_second_app_at_a_live_endpoint_is_refused() {
    let endpoint = Endpoint::new("taken");
    let app = AppProcess::start(&endpoint);

    let mut second = AppProcess::spawn(&endpoint);
    let refused = second.wait_for(BIND_REFUSED);
    assert!(
        refused.contains(&format!("{BIND_REFUSED} (AddrInUse)")),
        "the second bind is refused as AddrInUse: {refused}"
    );
    let status = second.exit_status();
    assert_eq!(status.code(), Some(REFUSED_EXIT), "{refused}");

    let mut bridge = Bridge::start(&endpoint, "60");
    let tree = tree_in(&bridge.read_tree().expect("the first app is still served"));
    assert_eq!(pid_of(&tree), app.pid, "{tree}");
}
