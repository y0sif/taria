//! The `taria-mcp` binary over stdio, against a fake app: it connects on the
//! first tool call, not when it starts, not on `initialize` and not on
//! `tools/list`, and it lets go of the app after `--idle-release`, saying so
//! in its log.
//!
//! Plain threads and blocking sockets: the binary is a separate process, and
//! nothing here needs a runtime.
//!
//! Unix only: the fake app listens on a Unix socket.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use taria::wire::AppToBridge;
use taria::{Node, PROTOCOL_VERSION, Role, Snapshot};

/// Generous bound for anything the binary is waited on for.
const WAIT: Duration = Duration::from_secs(10);

/// A running `taria-mcp`, killed when dropped.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
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

impl Bridge {
    fn start(socket: &Path, idle_release: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_taria-mcp"))
            .arg("--socket")
            .arg(socket)
            .args(["--idle-release", idle_release])
            .env("TARIA_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start taria-mcp");
        let stdout = line_channel(child.stdout.take().expect("piped stdout"));
        let stderr = line_channel(child.stderr.take().expect("piped stderr"));
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            stdout,
            stderr,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write to taria-mcp");
        stdin.flush().expect("flush to taria-mcp");
    }

    /// The response to request `id`, skipping anything else on stdout.
    fn response(&self, id: u64) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .stdout
                .recv_timeout(left)
                .unwrap_or_else(|err| panic!("no response to request {id}: {err}"));
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|err| panic!("stdout carries only MCP: {err}: {line}"));
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }

    /// Every log line written so far.
    fn log(&self) -> Vec<String> {
        self.stderr.try_iter().collect()
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

/// The next connection to `listener` within `window`, if any.
fn accept_within(listener: &UnixListener, window: Duration) -> Option<UnixStream> {
    let deadline = Instant::now() + window;
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(false).expect("blocking stream");
                return Some(stream);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("accept: {err}"),
        }
    }
}

fn write_message(stream: &mut UnixStream, message: &AppToBridge) {
    let mut line = serde_json::to_string(message).expect("serialize app message");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("write to bridge");
}

/// The `_meta` a request carries in the 2026-07-28 lifecycle, which has no
/// `initialize`: the protocol version, client info and capabilities travel
/// with every request instead of once per session.
fn stateless_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "on-demand-test", "version": "0"},
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A client on the 2026-07-28 lifecycle opens with `server/discover` where an
/// older one sends `initialize`, then lists the tools with the metadata
/// inline. Neither may connect to the app, for the reason `initialize` and
/// `tools/list` may not: a harness does both for every session it starts,
/// and an app serves one bridge at a time.
#[test]
fn server_discover_and_a_stateless_tools_list_connect_to_nothing() {
    let dir = tempfile::Builder::new()
        .prefix("taria-mcp-bin-")
        .tempdir()
        .expect("create test socket dir");
    let path = dir.path().join("discover.sock");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let mut bridge = Bridge::start(&path, "60");

    let discover = bridge.call(1, "server/discover", json!({"_meta": stateless_meta()}));
    let result = &discover["result"];
    let versions = result["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("a discover result: {discover}"));
    assert!(versions.contains(&json!("2026-07-28")), "{discover}");
    assert!(result["capabilities"]["tools"].is_object(), "{discover}");
    assert!(
        result["instructions"]
            .as_str()
            .is_some_and(|text| text.starts_with("Bridge to a live terminal (TUI) app.")),
        "discover carries the server's instructions: {discover}"
    );
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("taria-mcp"),
        "{discover}"
    );

    let tools = bridge.call(2, "tools/list", json!({"_meta": stateless_meta()}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("a tool list: {tools}"))
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, ["act", "key", "read_tree", "type_text"], "{tools}");

    assert!(
        accept_within(&listener, Duration::from_millis(500)).is_none(),
        "taria-mcp connected on server/discover or tools/list, before any tool call"
    );
}

#[test]
fn the_binary_connects_on_the_first_tool_call_and_lets_go_when_idle() {
    let dir = tempfile::Builder::new()
        .prefix("taria-mcp-bin-")
        .tempdir()
        .expect("create test socket dir");
    let path = dir.path().join("bin.sock");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let mut bridge = Bridge::start(&path, "1");

    let init = bridge.call(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "on-demand-test", "version": "0"},
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    bridge.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = bridge.call(2, "tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"read_tree"), "{tools}");

    // Starting, initialize and tools/list are behind it: still nothing.
    assert!(
        accept_within(&listener, Duration::from_millis(500)).is_none(),
        "taria-mcp connected before any tool call"
    );

    // The app's side of the first call: greet, publish, and hold the
    // connection until the bridge lets go of it.
    let (closed_tx, closed_rx) = mpsc::channel();
    let app = thread::spawn(move || {
        let mut stream = accept_within(&listener, WAIT).expect("the tool call should connect");
        write_message(
            &mut stream,
            &AppToBridge::hello("bin-app", PROTOCOL_VERSION),
        );
        let root = Node::new("app", Role::App).label("bin-tree");
        write_message(&mut stream, &AppToBridge::Snapshot(Snapshot::new(1, root)));
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
        let _ = closed_tx.send(Instant::now());
        listener
    });

    // Taken before the call is sent, so the release is at least the window
    // after it however late the response reaches this process: the window
    // starts when the call ends, which is after this, and the response's
    // trip back through stdout is not in the measurement at all. Measured
    // from the response instead, the margin was that trip, which a loaded
    // host can stretch past any slack worth leaving.
    let call_sent = Instant::now();
    let result = bridge.call(
        3,
        "tools/call",
        json!({"name": "read_tree", "arguments": {}}),
    );
    assert_ne!(result["result"]["isError"], json!(true), "{result}");
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a text result: {result}"));
    assert!(
        text.contains("bin-tree"),
        "the first call answers with the tree: {text}"
    );

    let closed_at = closed_rx
        .recv_timeout(WAIT)
        .expect("taria-mcp should let go of the app after --idle-release 1");
    let quiet = closed_at.duration_since(call_sent);
    assert!(
        quiet >= Duration::from_secs(1),
        "let go {quiet:?} after the last call was sent, inside the 1 s window that starts when it \
         ends"
    );
    drop(app.join().expect("fake app thread"));

    // The release is logged, so whoever reads the log knows the app is free.
    let deadline = Instant::now() + WAIT;
    let mut log = Vec::new();
    while !log
        .iter()
        .any(|line: &String| line.contains("released the app"))
    {
        assert!(
            Instant::now() < deadline,
            "no release in the log:\n{}",
            log.join("\n")
        );
        log.extend(bridge.log());
        thread::sleep(Duration::from_millis(20));
    }
    let release = log
        .iter()
        .find(|line| line.contains("released the app"))
        .expect("found above");
    assert!(release.contains("INFO"), "{release}");
    // The idle time is measured, not the window restated: at least the
    // window, and nothing held it past it.
    assert!(
        release.contains("no tool call for 1.")
            && release.contains("released the app (closed its connection to the app)")
            && !release.contains("past the"),
        "{release}"
    );
}

/// The app leaving while the bridge holds it past its window for an ack. The
/// window has closed, so the manager lets go rather than reconnect (a
/// reconnect would take the app again for a session that is not using it),
/// and its log says the app's leaving ended the hold, with how many inputs
/// were still unacked. That branch of the manager is otherwise untested: the
/// library tests see the connection end and the reconnect that does not
/// come, and only the binary's log says which release it was.
#[test]
fn the_app_leaving_during_the_ack_hold_is_logged_and_not_reconnected_to() {
    let dir = tempfile::Builder::new()
        .prefix("taria-mcp-bin-")
        .tempdir()
        .expect("create test socket dir");
    let path = dir.path().join("hold.sock");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    listener
        .set_nonblocking(true)
        .expect("non-blocking listener");
    let mut bridge = Bridge::start(&path, "1");
    let init = bridge.call(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "on-demand-test", "version": "0"},
        }),
    );
    assert!(init.get("result").is_some(), "{init}");
    bridge.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // The app's side: greet, publish, read the key and never ack it, then
    // leave once told to, and report whether the bridge came back within a
    // second of that.
    let (leave_tx, leave_rx) = mpsc::channel::<()>();
    let app = thread::spawn(move || {
        let mut stream = accept_within(&listener, WAIT).expect("the tool call should connect");
        write_message(
            &mut stream,
            &AppToBridge::hello("bin-app", PROTOCOL_VERSION),
        );
        let root = Node::new("app", Role::App).label("bin-tree");
        write_message(&mut stream, &AppToBridge::Snapshot(Snapshot::new(1, root)));
        let mut input = String::new();
        BufReader::new(&stream)
            .read_line(&mut input)
            .expect("read the bridge's input");
        leave_rx.recv_timeout(WAIT).expect("told to leave");
        // Shut down, not only dropped: a bridge the other test is spawning
        // holds a copy of this descriptor until its exec, and a close alone
        // sends no EOF while it does.
        stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shut the app's end down");
        drop(stream);
        let reconnected = accept_within(&listener, Duration::from_secs(1)).is_some();
        (input, reconnected)
    });

    let result = bridge.call(
        2,
        "tools/call",
        json!({"name": "key", "arguments": {"key": "x"}}),
    );
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a text result: {result}"));
    assert!(
        text.contains("neither acknowledged"),
        "an input the app never acked: {text}"
    );
    // Two seconds after the answer, so past the 1 s window that starts when
    // the call ends, however late the answer reached this process, and far
    // inside the 30 s an unacked input holds the app for.
    thread::sleep(Duration::from_secs(2));
    leave_tx.send(()).expect("the fake app is waiting");
    let (input, reconnected) = app.join().expect("fake app thread");
    assert!(input.contains(r#""type":"input""#), "{input}");
    assert!(
        !reconnected,
        "the bridge reconnected to an app it had no tool call for"
    );

    let wanted = "the app closed the connection during the ack hold; not reconnecting";
    let deadline = Instant::now() + WAIT;
    let mut log = Vec::new();
    while !log.iter().any(|line: &String| line.contains(wanted)) {
        assert!(
            Instant::now() < deadline,
            "no ack-hold release in the log:\n{}",
            log.join("\n")
        );
        log.extend(bridge.log());
        thread::sleep(Duration::from_millis(20));
    }
    let release = log
        .iter()
        .find(|line| line.contains(wanted))
        .expect("found above");
    assert!(
        release.contains("INFO")
            && release.contains("s past the 1 s idle window")
            && release.contains("held there while 1 input(s) awaited the app's ack")
            && release.contains(&format!("released the app ({wanted})")),
        "{release}"
    );
    assert!(
        !log.iter()
            .any(|line| line.contains("reconnecting") && !line.contains("not reconnecting")),
        "the log has a reconnect:\n{}",
        log.join("\n")
    );
}
