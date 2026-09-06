//! Integration tests: a fake taria app (tokio `UnixListener`) on one side,
//! the bridge's socket manager plus the MCP handler's tool methods on the
//! other. No MCP stdio transport involved.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use taria::wire::{AppToBridge, BridgeToApp};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use taria_mcp::bridge::{self, BridgeHandle};
use taria_mcp::server::{ActParams, KeyParams, TariaMcpServer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::timeout;

/// Generous bound for local socket round trips.
const WAIT: Duration = Duration::from_secs(5);

/// Unique socket path per test, in the system temp dir.
fn test_socket_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("taria-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test socket dir");
    dir.join(format!("{name}.sock"))
}

/// A demo tree: an `app` root with one `btn` button advertising `activate`
/// and one `input` text input advertising `set_value`.
fn demo_root(marker: &str) -> Node {
    Node::new("app", Role::App).label(marker).children([
        Node::new("btn", Role::Button)
            .label("Save")
            .action(Action::Activate),
        Node::new("input", Role::TextInput)
            .value("draft")
            .action(Action::SetValue),
    ])
}

/// The app half of one accepted connection.
struct FakeApp {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl FakeApp {
    /// Accept the bridge's connection and perform the app-side handshake:
    /// Hello, then the given initial snapshot.
    async fn accept(listener: &UnixListener, initial: Snapshot) -> Self {
        let (stream, _addr) = timeout(WAIT, listener.accept())
            .await
            .expect("bridge should connect")
            .expect("accept");
        let mut app = Self::from_stream(stream);
        app.send(&AppToBridge::Hello {
            app_label: "fake-app".to_string(),
            protocol_version: PROTOCOL_VERSION,
        })
        .await;
        app.send(&AppToBridge::Snapshot(initial)).await;
        app
    }

    fn from_stream(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self {
            lines: BufReader::new(read).lines(),
            write,
        }
    }

    async fn send(&mut self, msg: &AppToBridge) {
        let mut line = serde_json::to_string(msg).expect("serialize app message");
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .expect("write to bridge");
    }

    /// Read the next `BridgeToApp::Input` line from the bridge.
    async fn recv_input(&mut self) -> AgentInput {
        let line = timeout(WAIT, self.lines.next_line())
            .await
            .expect("input should arrive")
            .expect("read from bridge")
            .expect("bridge closed the socket");
        let BridgeToApp::Input(input) = serde_json::from_str(&line).expect("parse bridge message");
        input
    }
}

/// Wait until the watch holds a snapshot with at least `min_seq`.
async fn wait_for_snapshot(rx: &mut watch::Receiver<Option<Snapshot>>, min_seq: u64) -> Snapshot {
    timeout(WAIT, async {
        loop {
            let hit = rx
                .borrow_and_update()
                .as_ref()
                .filter(|s| s.seq >= min_seq)
                .cloned();
            if let Some(snapshot) = hit {
                return snapshot;
            }
            rx.changed().await.expect("watch sender alive");
        }
    })
    .await
    .expect("snapshot should arrive")
}

/// Wait until the watch is cleared back to `None` (disconnect observed).
async fn wait_for_clear(rx: &mut watch::Receiver<Option<Snapshot>>) {
    timeout(WAIT, async {
        loop {
            if rx.borrow_and_update().is_none() {
                return;
            }
            rx.changed().await.expect("watch sender alive");
        }
    })
    .await
    .expect("watch should clear to None");
}

/// The text of a successful tool result's single content block.
fn result_text(result: &rmcp::model::CallToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text(text)) => &text.text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Spin up listener + bridge + server for one test.
async fn setup(name: &str) -> (UnixListener, BridgeHandle, TariaMcpServer) {
    let path = test_socket_path(name);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle.clone());
    (listener, handle, server)
}

#[tokio::test]
async fn read_tree_errors_before_any_connection() {
    // Nothing listens on this path; the manager retries in the background.
    let handle = bridge::spawn(test_socket_path("never-connects"));
    let server = TariaMcpServer::new(handle);
    let err = server.read_tree().await.expect_err("no app, no snapshot");
    assert!(
        err.message.contains("running"),
        "error should hint at the app not running: {}",
        err.message
    );
}

#[tokio::test]
async fn manager_connects_and_watch_gets_snapshot() {
    let (listener, mut handle, server) = setup("connects").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("hello-tree"))).await;

    let snapshot = wait_for_snapshot(&mut handle.snapshot_rx, 1).await;
    assert_eq!(snapshot.seq, 1);
    assert_eq!(snapshot.root.id.0, "app");

    let result = server.read_tree().await.expect("read_tree after snapshot");
    let text = result_text(&result);
    assert!(text.contains("hello-tree"), "tree json: {text}");
    assert!(text.contains("btn"), "tree json: {text}");
}

#[tokio::test]
async fn act_forwards_input_and_returns_updated_tree() {
    let (listener, mut handle, server) = setup("act-roundtrip").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;

    // Fake app: apply the input by publishing seq 2 with a marker change.
    let echo = tokio::spawn(async move {
        let input = app.recv_input().await;
        assert_eq!(
            input,
            AgentInput::Act {
                node: taria::NodeId("btn".to_string()),
                action: Action::Activate,
                value: None,
            }
        );
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("after-activate"),
        )))
        .await;
        app
    });

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act on advertised action");
    let text = result_text(&result);
    assert!(text.contains("after-activate"), "tree json: {text}");
    assert!(text.contains("\"seq\":2"), "tree json: {text}");

    echo.await.expect("fake app task");
}

#[tokio::test]
async fn act_reports_when_tree_does_not_change() {
    let (listener, mut handle, server) = setup("act-no-change").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("static"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;

    // The fake app swallows the input and never publishes a new snapshot.
    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act itself succeeds");
    let text = result_text(&result);
    assert!(text.contains("did not change"), "note: {text}");

    // The input still reached the app.
    let input = app.recv_input().await;
    assert!(matches!(input, AgentInput::Act { .. }));
}

#[tokio::test]
async fn act_rejects_unknown_node_and_lists_valid_ids() {
    let (listener, mut handle, server) = setup("act-bad-node").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;

    let err = server
        .act(Parameters(ActParams {
            node: "missing".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("unknown node must be rejected");
    assert!(err.message.contains("missing"), "err: {}", err.message);
    for id in ["app", "btn", "input"] {
        assert!(
            err.message.contains(id),
            "error should list valid id `{id}`: {}",
            err.message
        );
    }
}

#[tokio::test]
async fn act_rejects_unadvertised_action_and_lists_advertised() {
    let (listener, mut handle, server) = setup("act-bad-action").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;

    let err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "toggle".to_string(),
            value: None,
        }))
        .await
        .expect_err("unadvertised action must be rejected");
    assert!(err.message.contains("toggle"), "err: {}", err.message);
    assert!(
        err.message.contains("activate"),
        "error should list the advertised action: {}",
        err.message
    );
}

#[tokio::test]
async fn key_forwards_input_and_returns_updated_tree() {
    let (listener, mut handle, server) = setup("key-roundtrip").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;

    let echo = tokio::spawn(async move {
        let input = app.recv_input().await;
        assert_eq!(
            input,
            AgentInput::Key {
                key: "ctrl+s".to_string(),
            }
        );
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("after-key"),
        )))
        .await;
        app
    });

    let result = server
        .key(Parameters(KeyParams {
            key: "ctrl+s".to_string(),
        }))
        .await
        .expect("key tool");
    let text = result_text(&result);
    assert!(text.contains("after-key"), "tree json: {text}");

    echo.await.expect("fake app task");
}

#[tokio::test]
async fn key_rejects_empty_key() {
    let (_listener, _handle, server) = setup("key-empty").await;
    let err = server
        .key(Parameters(KeyParams { key: String::new() }))
        .await
        .expect_err("empty key must be rejected");
    assert!(err.message.contains("non-empty"), "err: {}", err.message);
}

#[tokio::test]
async fn disconnect_clears_watch_and_read_tree_errors_again() {
    let (listener, mut handle, server) = setup("disconnect").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("alive"))).await;
    wait_for_snapshot(&mut handle.snapshot_rx, 1).await;
    server.read_tree().await.expect("connected read_tree works");

    // Kill the app side; the manager must clear the watch to None.
    drop(app);
    drop(listener);
    wait_for_clear(&mut handle.snapshot_rx).await;

    let err = server
        .read_tree()
        .await
        .expect_err("read_tree must fail after disconnect");
    assert!(err.message.contains("running"), "err: {}", err.message);
}
