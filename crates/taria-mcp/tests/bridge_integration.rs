//! Integration tests: a fake taria app (tokio `UnixListener`) on one side,
//! the bridge's socket manager plus the MCP handler's tool methods on the
//! other. No MCP stdio transport involved.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use taria::wire::{AppToBridge, BridgeToApp};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use taria_mcp::bridge::{self, BridgeHandle, BridgeState};
use taria_mcp::server::{ActParams, KeyParams, TariaMcpServer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::time::timeout;

/// Generous bound for local socket round trips.
const WAIT: Duration = Duration::from_secs(5);

/// Fresh per-test socket directory. The directory and everything in it
/// (socket files included) are removed when the guard drops, even when the
/// test fails, so test runs leave no litter in the system temp dir.
fn test_socket_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("taria-mcp-test-")
        .tempdir()
        .expect("create test socket dir")
}

/// A socket path for `name` in its own self-cleaning directory. Keep the
/// returned guard alive for the duration of the test.
fn test_socket_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = test_socket_dir();
    let path = dir.path().join(format!("{name}.sock"));
    (dir, path)
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

/// Wait until the watch holds a connected snapshot with at least `min_seq`.
async fn wait_for_snapshot(rx: &mut watch::Receiver<BridgeState>, min_seq: u64) -> Snapshot {
    timeout(WAIT, async {
        loop {
            let hit = match &*rx.borrow_and_update() {
                BridgeState::Connected(s) if s.seq >= min_seq => Some(s.clone()),
                _ => None,
            };
            if let Some(snapshot) = hit {
                return snapshot;
            }
            rx.changed().await.expect("watch sender alive");
        }
    })
    .await
    .expect("snapshot should arrive")
}

/// Wait until the watch leaves `Connected` (disconnect observed).
async fn wait_for_disconnect(rx: &mut watch::Receiver<BridgeState>) {
    timeout(WAIT, async {
        loop {
            if !matches!(&*rx.borrow_and_update(), BridgeState::Connected(_)) {
                return;
            }
            rx.changed().await.expect("watch sender alive");
        }
    })
    .await
    .expect("watch should leave Connected");
}

/// The text of a successful tool result's single content block.
fn result_text(result: &rmcp::model::CallToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text(text)) => &text.text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Spin up listener + bridge + server for one test. The first element is the
/// socket directory guard; hold it (as `_dir`, not `_`) until the test ends.
async fn setup(
    name: &str,
) -> (
    tempfile::TempDir,
    UnixListener,
    BridgeHandle,
    TariaMcpServer,
) {
    let (dir, path) = test_socket_path(name);
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle.clone());
    (dir, listener, handle, server)
}

#[tokio::test]
async fn read_tree_errors_before_any_connection() {
    // Nothing listens on this path; the manager retries in the background.
    let (_dir, path) = test_socket_path("never-connects");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);
    let err = server.read_tree().await.expect_err("no app, no snapshot");
    assert!(
        err.message.contains("no snapshot") && err.message.contains("running"),
        "never-connected error should hint at the app not running: {}",
        err.message
    );
    assert!(
        !err.message.contains("disconnected"),
        "never-connected must not read as a disconnect: {}",
        err.message
    );
}

#[tokio::test]
async fn manager_connects_and_watch_gets_snapshot() {
    let (_dir, listener, mut handle, server) = setup("connects").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("hello-tree"))).await;

    let snapshot = wait_for_snapshot(&mut handle.state_rx, 1).await;
    assert_eq!(snapshot.seq, 1);
    assert_eq!(snapshot.root.id.0, "app");

    let result = server.read_tree().await.expect("read_tree after snapshot");
    let text = result_text(&result);
    assert!(text.contains("hello-tree"), "tree json: {text}");
    assert!(text.contains("btn"), "tree json: {text}");
}

#[tokio::test]
async fn act_forwards_input_and_returns_updated_tree() {
    let (_dir, listener, mut handle, server) = setup("act-roundtrip").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

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
    let (_dir, listener, mut handle, server) = setup("act-no-change").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("static"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

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
    let (_dir, listener, mut handle, server) = setup("act-bad-node").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

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
    let (_dir, listener, mut handle, server) = setup("act-bad-action").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

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
    let (_dir, listener, mut handle, server) = setup("key-roundtrip").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

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
    let (_dir, _listener, _handle, server) = setup("key-empty").await;
    let err = server
        .key(Parameters(KeyParams { key: String::new() }))
        .await
        .expect_err("empty key must be rejected");
    assert!(err.message.contains("non-empty"), "err: {}", err.message);
}

#[tokio::test]
async fn key_errors_before_any_connection() {
    // Nothing listens on this path; `key` must refuse (like `act`) instead of
    // queueing input that would replay into the next app instance.
    let (_dir, path) = test_socket_path("key-never-connects");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);
    let err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
        }))
        .await
        .expect_err("key with no app connected must error");
    assert!(
        err.message.contains("running"),
        "error should hint at the app not running: {}",
        err.message
    );
}

/// Regression test for the reconnect hot loop: an app that accepts and then
/// immediately drops the connection (e.g. one whose snapshot always exceeds
/// the line cap) must not be reconnected to at full CPU. Each connection here
/// dies young without delivering a snapshot, so consecutive accepts must be
/// spaced by the growing backoff (at least 250 + 500 + 1000 ms across four
/// cycles).
#[tokio::test]
async fn accept_then_drop_connections_are_backed_off() {
    let (_dir, path) = test_socket_path("accept-drop-backoff");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let _handle = bridge::spawn(path); // keep the handle alive: dropping it ends the manager

    let started = std::time::Instant::now();
    for _ in 0..4 {
        let (stream, _addr) = timeout(WAIT, listener.accept())
            .await
            .expect("bridge should keep reconnecting")
            .expect("accept");
        drop(stream); // immediate close: an unhealthy connection
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1400),
        "4 accept-and-drop cycles finished in {elapsed:?}; reconnects are not backed off"
    );
}

/// Inputs queued while no app is connected must not replay into the next app
/// instance. `key` refuses while disconnected, so the stale inputs are queued
/// through the raw bridge handle (the race window the reconnect drain covers).
#[tokio::test]
async fn stale_inputs_do_not_replay_into_next_connection() {
    let (_dir, path) = test_socket_path("stale-inputs");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    // First instance comes up, then dies.
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("gen-one"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    drop(app);
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    wait_for_disconnect(&mut handle.state_rx).await;

    // The `key` tool refuses while down...
    let err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
        }))
        .await
        .expect_err("key while disconnected must refuse");
    assert!(err.message.contains("disconnected"), "err: {}", err.message);

    // ...so queue stale inputs directly; the listener is still unbound, so
    // these sit in the channel until the next successful connect.
    for key in ["q", "y"] {
        handle
            .input_tx
            .send(AgentInput::Key {
                key: key.to_string(),
            })
            .await
            .expect("queue input while down");
    }

    // Restart the app; the bridge must drain the stale queue on reconnect.
    let listener = UnixListener::bind(&path).expect("rebind fake app socket");
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("gen-two"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // A fresh key sent through the tool must be the FIRST input the new
    // instance sees; with no drain, the FIFO queue would deliver "q" first.
    server
        .key(Parameters(KeyParams {
            key: "enter".to_string(),
        }))
        .await
        .expect("key against the restarted app");
    let input = app.recv_input().await;
    assert_eq!(
        input,
        AgentInput::Key {
            key: "enter".to_string(),
        },
        "stale inputs leaked into the new app instance"
    );

    // And nothing stale trails behind it.
    let extra = timeout(Duration::from_millis(300), app.lines.next_line()).await;
    assert!(extra.is_err(), "no further input expected, got {extra:?}");
}

#[tokio::test]
async fn newline_free_flood_disconnects_the_app_connection() {
    let (_dir, listener, mut handle, _server) = setup("line-flood").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("pre-flood"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Stream a few MiB with no newline; the bridge must treat the connection
    // as broken (watch clears to None) instead of buffering it all.
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..48 {
        // 48 * 64 KiB = 3 MiB
        if app.write.write_all(&chunk).await.is_err() {
            break; // the bridge already dropped the connection
        }
    }
    wait_for_disconnect(&mut handle.state_rx).await;
}

/// Simulate an app restart mid-`act`: the fake app receives the input, then
/// its process "dies" and a fresh instance (seq starting over) comes up. The
/// listener is rebound before the old connection drops so the bridge's
/// reconnect succeeds immediately.
async fn restart_app_on_input(
    listener: UnixListener,
    path: PathBuf,
    mut app: FakeApp,
    fresh: Snapshot,
) -> FakeApp {
    let input = app.recv_input().await;
    assert!(matches!(input, AgentInput::Act { .. }));
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let listener = UnixListener::bind(&path).expect("rebind fake app socket");
    drop(app); // the bridge sees the disconnect and reconnects
    FakeApp::accept(&listener, fresh).await
}

#[tokio::test]
async fn act_returns_fresh_tree_after_app_restart() {
    let (_dir, path) = test_socket_path("restart-act");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    // The old instance is at seq 5; the restarted one starts over at seq 1.
    let app = FakeApp::accept(&listener, Snapshot::new(5, demo_root("before-restart"))).await;
    wait_for_snapshot(&mut handle.state_rx, 5).await;
    let restart = tokio::spawn(restart_app_on_input(
        listener,
        path,
        app,
        Snapshot::new(1, demo_root("after-restart")),
    ));

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act across an app restart");
    let text = result_text(&result);
    assert!(
        !text.contains("did not change"),
        "restart must not be misreported as no change: {text}"
    );
    assert!(text.contains("after-restart"), "tree json: {text}");
    assert!(text.contains("\"seq\":1"), "tree json: {text}");

    restart.await.expect("fake app restart task");
}

#[tokio::test]
async fn act_detects_restart_even_when_seq_matches() {
    let (_dir, path) = test_socket_path("restart-act-same-seq");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    // Both instances publish seq 1, but the trees differ.
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("generation-one"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    let restart = tokio::spawn(restart_app_on_input(
        listener,
        path,
        app,
        Snapshot::new(1, demo_root("generation-two")),
    ));

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act across a same-seq app restart");
    let text = result_text(&result);
    assert!(
        !text.contains("did not change"),
        "same-seq restart must not be misreported as no change: {text}"
    );
    assert!(text.contains("generation-two"), "tree json: {text}");

    restart.await.expect("fake app restart task");
}

/// After the app dies, all three tools must say WHICH app went away and at
/// what seq (not the generic never-connected message), and once the app is
/// back the bridge must serve trees again.
#[tokio::test]
async fn disconnect_reports_app_label_and_last_seq_then_recovers() {
    let (_dir, path) = test_socket_path("disconnect");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    let app = FakeApp::accept(&listener, Snapshot::new(3, demo_root("alive"))).await;
    wait_for_snapshot(&mut handle.state_rx, 3).await;
    server.read_tree().await.expect("connected read_tree works");

    // Kill the app side; the manager must record the disconnect.
    drop(app);
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    wait_for_disconnect(&mut handle.state_rx).await;

    // All three tools refuse, naming the dead app and its last snapshot seq.
    let read_err = server
        .read_tree()
        .await
        .expect_err("read_tree must fail after disconnect");
    let act_err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("act must refuse after disconnect");
    let key_err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
        }))
        .await
        .expect_err("key must refuse after disconnect");
    for err in [&read_err, &act_err, &key_err] {
        assert!(
            err.message.contains("'fake-app' disconnected"),
            "error should name the dead app: {}",
            err.message
        );
        assert!(
            err.message.contains("last snapshot seq 3"),
            "error should carry the last seq: {}",
            err.message
        );
        assert!(
            !err.message.contains("no snapshot"),
            "disconnect must not reuse the never-connected message: {}",
            err.message
        );
    }

    // The app comes back: the bridge reconnects and serves trees again.
    let listener = UnixListener::bind(&path).expect("rebind fake app socket");
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("back-again"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    let result = server
        .read_tree()
        .await
        .expect("read_tree once the app is back");
    assert!(
        result_text(&result).contains("back-again"),
        "tree json: {}",
        result_text(&result)
    );
}
