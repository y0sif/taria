//! Integration tests: a fake taria app (tokio `UnixListener`) on one side,
//! the bridge's socket manager plus the MCP handler's tool methods on the
//! other. No MCP stdio transport involved.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use taria_mcp::bridge::{self, BridgeHandle, BridgeState};
use taria_mcp::server::{ActParams, KeyParams, TariaMcpServer, TypeTextParams};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};
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
    /// Status every input read through [`FakeApp::recv_input`] is acked with,
    /// or `None` for an app that implements no acks at all (an adapter older
    /// than the ack contract, which the bridge still has to serve).
    ack_with: Option<InputStatus>,
}

impl FakeApp {
    /// Accept the bridge's connection and perform the app-side handshake:
    /// Hello, then the given initial snapshot.
    ///
    /// Acks `Delivered` by default, like the real adapter does the moment its
    /// event loop dequeues an input.
    async fn accept(listener: &UnixListener, initial: Snapshot) -> Self {
        Self::accept_speaking(listener, initial, PROTOCOL_VERSION).await
    }

    /// Accept, but declare `protocol_version` in the handshake: the peer the
    /// bridge can read snapshots from and cannot send input to.
    async fn accept_speaking(
        listener: &UnixListener,
        initial: Snapshot,
        protocol_version: u32,
    ) -> Self {
        let (stream, _addr) = timeout(WAIT, listener.accept())
            .await
            .expect("bridge should connect")
            .expect("accept");
        let mut app = Self::from_stream(stream);
        app.send(&AppToBridge::Hello {
            app_label: "fake-app".to_string(),
            protocol_version,
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
            ack_with: Some(InputStatus::Delivered),
        }
    }

    /// Ack every input read from now on with `status`.
    fn acking(mut self, status: InputStatus) -> Self {
        self.ack_with = Some(status);
        self
    }

    /// Ack nothing at all: the compatibility path, where the changed tree is
    /// the only answer the bridge can get.
    fn never_acking(mut self) -> Self {
        self.ack_with = None;
        self
    }

    async fn send(&mut self, msg: &AppToBridge) {
        let mut line = serde_json::to_string(msg).expect("serialize app message");
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .expect("write to bridge");
    }

    /// Answer an input, naming the id it was sent under.
    async fn ack(&mut self, id: InputId, status: InputStatus) {
        self.send(&AppToBridge::Ack { id, status }).await;
    }

    /// Read the next `BridgeToApp::Input` line from the bridge, acking it as
    /// this app was configured to.
    async fn recv_input(&mut self) -> (InputId, AgentInput) {
        let line = timeout(WAIT, self.lines.next_line())
            .await
            .expect("input should arrive")
            .expect("read from bridge")
            .expect("bridge closed the socket");
        let BridgeToApp::Input { id, input } =
            serde_json::from_str(&line).expect("parse bridge message");
        if let Some(status) = self.ack_with {
            self.ack(id, status).await;
        }
        (id, input)
    }

    /// Assert the bridge sends nothing more within `window`.
    async fn expect_no_input(&mut self, window: Duration) {
        let extra = timeout(window, self.lines.next_line()).await;
        assert!(extra.is_err(), "no further input expected, got {extra:?}");
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

    // Fake app: ack Delivered, then apply the input by publishing seq 2 with
    // a marker change.
    let echo = tokio::spawn(async move {
        let (_id, input) = app.recv_input().await;
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

/// An app that takes the input and does nothing visible with it: the ack is
/// the only evidence it arrived, and the result must say exactly that instead
/// of implying the app never saw it.
#[tokio::test]
async fn act_reports_delivered_without_a_tree_change() {
    let (_dir, listener, mut handle, server) = setup("act-no-change").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("static"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Acks Delivered, publishes nothing.
    let echo = tokio::spawn(async move {
        let mut app = app;
        let (_id, input) = app.recv_input().await;
        assert!(matches!(input, AgentInput::Act { .. }));
        app
    });

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act itself succeeds");
    let text = result_text(&result);
    assert!(
        text.contains("The app received this input, and its tree did not change"),
        "note: {text}"
    );
    assert!(text.contains("read_tree"), "note: {text}");

    echo.await.expect("fake app task");
}

/// The same shape from the outside - an ack and no new tree - but the app
/// took the input and exited. "Its tree did not change" would tell the agent
/// the app is sitting there having done nothing, when it is gone; the answer
/// has to be the disconnect, in the words `read_tree` uses for it.
#[tokio::test]
async fn act_reports_an_app_that_went_away_after_taking_the_input() {
    let (_dir, path) = test_socket_path("act-then-exit");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    let app = FakeApp::accept(&listener, Snapshot::new(4, demo_root("about-to-exit"))).await;
    wait_for_snapshot(&mut handle.state_rx, 4).await;

    // Acks, then exits without drawing again, taking its socket with it.
    let exit = tokio::spawn(async move {
        let mut app = app;
        app.recv_input().await;
        drop(app);
        drop(listener);
    });

    let err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("an app that exited is not a tool call that went fine");
    assert!(
        err.message
            .contains("the app received this input and then disconnected"),
        "the report must tie the departure to the input: {}",
        err.message
    );
    assert!(
        err.message
            .contains("app 'fake-app' (last snapshot seq 4) is gone"),
        "the report must name which app went away, and when: {}",
        err.message
    );
    assert!(
        !err.message.contains("did not change"),
        "an app that exited must not read as one that ignored the input: {}",
        err.message
    );

    exit.await.expect("fake app task");
}

/// An app that never reads its socket: no ack, no snapshot. The result must
/// not claim the app received anything.
#[tokio::test]
async fn act_reports_when_the_app_neither_acks_nor_changes() {
    let (_dir, listener, mut handle, server) = setup("act-silent").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("static"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("act itself succeeds");
    let text = result_text(&result);
    assert!(
        text.contains("neither acknowledged this input nor changed its tree"),
        "note: {text}"
    );

    // The input still reached the app; it simply had not been read yet.
    let (_id, input) = app.recv_input().await;
    assert!(matches!(input, AgentInput::Act { .. }));
}

/// A full input queue on the app side must surface as an error naming the
/// cause, not as a cheerful "the tree did not change".
#[tokio::test]
async fn dropped_ack_surfaces_as_an_error() {
    let (_dir, listener, mut handle, server) = setup("ack-dropped").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("full-queue")))
        .await
        .acking(InputStatus::Dropped);
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        app.recv_input().await;
        app
    });

    let err = server
        .key(Parameters(KeyParams {
            key: "j".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("a dropped input must not read as success");
    assert!(
        err.message.contains("input queue was full"),
        "err: {}",
        err.message
    );
    assert!(
        err.message.contains("fewer") || err.message.contains("slower"),
        "error should say what to do about it: {}",
        err.message
    );

    echo.await.expect("fake app task");
}

/// The app looked at the input and chose to do nothing (a modal dialog, say).
/// The agent needs to hear that, plus the tree to re-plan from - and that has
/// to be the tree that caused the ignore, not the one the input was aimed at.
/// The adapter flushes queued acks ahead of the pending snapshot in every
/// writer pass, so the causing frame always arrives *after* the ack reporting
/// the ignore; returning at the ack hands back a tree without the dialog in
/// it, under text telling the agent to re-plan from exactly that tree.
#[tokio::test]
async fn ignored_ack_reports_the_app_did_nothing_and_returns_the_fresh_tree() {
    let (_dir, listener, mut handle, server) = setup("ack-ignored").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("pre-input"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Delivered first, then refined to Ignored, and only then the frame that
    // explains the ignore: the order the real adapter produces.
    let echo = tokio::spawn(async move {
        let mut app = app;
        let (id, _input) = app.recv_input().await;
        app.ack(id, InputStatus::Ignored).await;
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("modal-open"),
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
        .expect("an ignored input is not a failure");
    let text = result_text(&result);
    assert!(
        text.contains("deliberately did nothing with it"),
        "note: {text}"
    );
    assert!(
        text.contains("modal-open") && text.contains("\"seq\":2"),
        "the tree after the note must be the one that caused the ignore: {text}"
    );
    assert!(
        !text.contains("pre-input"),
        "re-planning from the pre-input tree is the bug this guards: {text}"
    );

    echo.await.expect("fake app task");
}

/// The other half of the ignored path: an app that publishes nothing after the
/// ignore still owes the agent a tree, and the freshest one the bridge holds
/// is the tree the input was aimed at.
#[tokio::test]
async fn ignored_ack_without_a_newer_tree_falls_back_to_the_current_one() {
    let (_dir, listener, mut handle, server) = setup("ack-ignored-static").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("unchanged"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        let (id, _input) = app.recv_input().await;
        app.ack(id, InputStatus::Ignored).await;
        app
    });

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("an ignored input is not a failure");
    let text = result_text(&result);
    assert!(
        text.contains("deliberately did nothing with it"),
        "note: {text}"
    );
    assert!(
        text.contains("unchanged") && text.contains("\"seq\":1"),
        "the note must still be followed by a tree: {text}"
    );

    echo.await.expect("fake app task");
}

/// An adapter that predates acks still has to work: a changed tree is the
/// only answer it can give, and the bridge must report it as success rather
/// than complain about the missing ack.
#[tokio::test]
async fn unacked_change_still_returns_the_tree() {
    let (_dir, listener, mut handle, server) = setup("no-acks").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before")))
        .await
        .never_acking();
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        app.recv_input().await;
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("after-no-ack"),
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
        .expect("an app without acks still works");
    let text = result_text(&result);
    assert!(text.contains("after-no-ack"), "tree json: {text}");
    assert!(
        !text.contains("acknowledg"),
        "a missing ack is not a problem worth reporting: {text}"
    );

    echo.await.expect("fake app task");
}

/// Acks name an id; one for a different input says nothing about this one and
/// must not be mistaken for the answer.
#[tokio::test]
async fn acks_for_other_inputs_are_ignored() {
    let (_dir, listener, mut handle, server) = setup("ack-other-id").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before")))
        .await
        .never_acking();
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Answer an id nobody is waiting for with the harshest status there is,
    // then react to the real input normally.
    let echo = tokio::spawn(async move {
        let mut app = app;
        let (id, _input) = app.recv_input().await;
        app.ack(id.wrapping_add(1000), InputStatus::Dropped).await;
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("after-stray-ack"),
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
        .expect("a stray ack must not fail the call");
    let text = result_text(&result);
    assert!(text.contains("after-stray-ack"), "tree json: {text}");

    echo.await.expect("fake app task");
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
        let (_id, input) = app.recv_input().await;
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
            repeat: None,
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
        .key(Parameters(KeyParams {
            key: String::new(),
            repeat: None,
        }))
        .await
        .expect_err("empty key must be rejected");
    assert!(err.message.contains("non-empty"), "err: {}", err.message);
}

/// The incident this guards: an agent sent "inx", the bridge forwarded it, the
/// app's parser returned nothing, and the input vanished with no signal. The
/// bridge parses with the same grammar the app does, so it can refuse first.
#[tokio::test]
async fn key_rejects_an_unparseable_key_without_sending_it() {
    let (_dir, listener, mut handle, server) = setup("key-unparseable").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let err = server
        .key(Parameters(KeyParams {
            key: "inx".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("an unparseable key must be rejected");
    assert!(
        err.message.contains("inx"),
        "error should name the rejected key: {}",
        err.message
    );
    assert!(
        err.message.contains("ctrl+c") && err.message.contains("enter"),
        "error should describe the grammar: {}",
        err.message
    );

    app.expect_no_input(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn key_rejects_out_of_range_repeat() {
    let (_dir, _listener, _handle, server) = setup("key-bad-repeat").await;
    for repeat in [0, 65] {
        let err = server
            .key(Parameters(KeyParams {
                key: "j".to_string(),
                repeat: Some(repeat),
            }))
            .await
            .expect_err("repeat outside 1..=64 must be rejected");
        assert!(
            err.message.contains("repeat must be between 1 and 64"),
            "err: {}",
            err.message
        );
        assert!(
            err.message.contains(&repeat.to_string()),
            "error should name the rejected count: {}",
            err.message
        );
    }
}

/// "Move down five rows" is one call: five presses, five ids, one report.
#[tokio::test]
async fn key_repeat_sends_exactly_that_many_inputs() {
    let (_dir, listener, mut handle, server) = setup("key-repeat").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("list"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        let mut ids = Vec::new();
        for _ in 0..5 {
            let (id, input) = app.recv_input().await;
            assert_eq!(
                input,
                AgentInput::Key {
                    key: "down".to_string(),
                }
            );
            ids.push(id);
        }
        (app, ids)
    });

    let result = server
        .key(Parameters(KeyParams {
            key: "down".to_string(),
            repeat: Some(5),
        }))
        .await
        .expect("repeated key");
    // The app acked every press and published nothing, so the report is about
    // the last press.
    assert!(
        result_text(&result).contains("did not change"),
        "note: {}",
        result_text(&result)
    );

    let (mut app, ids) = echo.await.expect("fake app task");
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "each press needs its own id, got {ids:?}"
    );
    app.expect_no_input(Duration::from_millis(300)).await;
}

/// A burst where the last press landed hides every press dropped before it if
/// only the last id is watched. The agent has to hear how much of the call
/// actually arrived, even when the tree changed.
#[tokio::test]
async fn key_repeat_reports_presses_dropped_mid_burst() {
    let (_dir, listener, mut handle, server) = setup("key-repeat-dropped").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("list")))
        .await
        .never_acking();
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Three of the five presses overflow the app's queue; the first and last
    // land, and the tree even changes, which is exactly what hid the drops.
    let echo = tokio::spawn(async move {
        let mut app = app;
        let mut ids = Vec::new();
        for _ in 0..5 {
            let (id, _input) = app.recv_input().await;
            ids.push(id);
        }
        for id in &ids[1..4] {
            app.ack(*id, InputStatus::Dropped).await;
        }
        app.ack(ids[0], InputStatus::Delivered).await;
        app.ack(ids[4], InputStatus::Delivered).await;
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            2,
            demo_root("moved-twice"),
        )))
        .await;
        app
    });

    let err = server
        .key(Parameters(KeyParams {
            key: "down".to_string(),
            repeat: Some(5),
        }))
        .await
        .expect_err("a burst with drops in it must not read as success");
    assert!(
        err.message.contains("dropped 3 of the 5 inputs"),
        "error should count the drops: {}",
        err.message
    );
    assert!(
        err.message.contains("at most 2 landed"),
        "error should say how much of the burst arrived: {}",
        err.message
    );
    assert!(
        err.message.contains("input queue was full"),
        "error should name the cause: {}",
        err.message
    );

    echo.await.expect("fake app task");
}

/// A peer on another protocol version cannot parse the `Input` messages this
/// bridge writes, so every input it receives is skipped on its side and the
/// whole session silently does nothing. Snapshots still parse across the
/// mismatch, so `read_tree` keeps working; every input tool must fail fast,
/// name both versions, and send nothing.
#[tokio::test]
async fn protocol_mismatch_keeps_read_tree_and_refuses_every_input_tool() {
    let (_dir, listener, mut handle, server) = setup("protocol-mismatch").await;
    let other = PROTOCOL_VERSION + 1;
    let mut app = FakeApp::accept_speaking(
        &listener,
        Snapshot::new(1, demo_root("other-version")),
        other,
    )
    .await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let result = server
        .read_tree()
        .await
        .expect("a readable tree survives the mismatch");
    assert!(
        result_text(&result).contains("other-version"),
        "tree json: {}",
        result_text(&result)
    );

    let act_err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("act must refuse a peer that cannot receive input");
    let key_err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("key must refuse a peer that cannot receive input");
    let text_err = server
        .type_text(Parameters(TypeTextParams {
            text: "hello".to_string(),
        }))
        .await
        .expect_err("type_text must refuse a peer that cannot receive input");
    for err in [&act_err, &key_err, &text_err] {
        assert!(
            err.message.contains(&format!("protocol version {other}")),
            "error should name the app's version: {}",
            err.message
        );
        assert!(
            err.message
                .contains(&format!("bridge speaks {PROTOCOL_VERSION}")),
            "error should name the bridge's version: {}",
            err.message
        );
        assert!(
            err.message.contains("taria dependency"),
            "error should say what to change: {}",
            err.message
        );
        assert!(
            !err.message.contains("may not have reacted"),
            "a peer that cannot receive input must not read as a slow one: {}",
            err.message
        );
    }

    // And nothing was put on the wire for it to fail to parse.
    app.expect_no_input(Duration::from_millis(300)).await;
}

/// A full input queue with the app not draining it must not park the tool call
/// until the app comes back. The handle is wired by hand because a real
/// connection drains the queue faster than a test can fill it: a connected
/// snapshot to get past the liveness check, and a one-slot queue nobody reads.
#[tokio::test]
async fn a_full_input_queue_fails_the_call_instead_of_hanging() {
    let (_state_tx, state_rx) = watch::channel(BridgeState::Connected(Snapshot::new(
        1,
        demo_root("not-draining"),
    )));
    let (input_tx, _input_rx) = mpsc::channel(1);
    let (ack_tx, _ack_rx) = broadcast::channel(8);
    let (_protocol_tx, protocol_rx) = watch::channel(Some(PROTOCOL_VERSION));
    let handle = BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
    };
    // Occupy the only slot, so the tool's own send has nowhere to go.
    handle
        .input_tx
        .send((
            handle.next_input_id(),
            AgentInput::Key {
                key: "x".to_string(),
            },
        ))
        .await
        .expect("fill the queue");
    let server = TariaMcpServer::new(handle);

    let err = timeout(
        WAIT,
        server.key(Parameters(KeyParams {
            key: "q".to_string(),
            repeat: None,
        })),
    )
    .await
    .expect("the call must not park on a full queue")
    .expect_err("a call that could not be sent is not a success");
    assert!(
        err.message.contains("the app is not accepting input"),
        "error should say the app is not taking input: {}",
        err.message
    );
    assert!(
        err.message.contains("this input was not sent"),
        "error should say nothing was sent: {}",
        err.message
    );
}

/// The same jam, but partway through a burst: presses 1 and 2 are already on
/// the wire when press 3 finds no room. Failing out with the single-input
/// error would tell the agent nothing was sent and invite a retry of a burst
/// that partly landed, so the call has to report both halves. Wired by hand
/// for the same reason as the test above: a real app drains the queue faster
/// than a test can fill it.
#[tokio::test]
async fn a_burst_cut_short_reports_how_much_of_it_was_sent() {
    let (_state_tx, state_rx) = watch::channel(BridgeState::Connected(Snapshot::new(
        1,
        demo_root("stops-draining"),
    )));
    // Two slots and nobody reading them: the third press onwards has nowhere
    // to go.
    let (input_tx, mut input_rx) = mpsc::channel(2);
    let (ack_tx, _ack_rx) = broadcast::channel(8);
    let (_protocol_tx, protocol_rx) = watch::channel(Some(PROTOCOL_VERSION));
    let server = TariaMcpServer::new(BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
    });

    let err = timeout(
        WAIT,
        server.key(Parameters(KeyParams {
            key: "down".to_string(),
            repeat: Some(5),
        })),
    )
    .await
    .expect("the call must not park on a full queue")
    .expect_err("a burst that could not be finished is not a success");
    assert!(
        err.message.contains("2 of the 5 inputs were sent"),
        "error should count what went out: {}",
        err.message
    );
    assert!(
        err.message.contains("remaining 3 were not sent"),
        "error should count what did not: {}",
        err.message
    );
    assert!(
        err.message.contains("may already have taken effect"),
        "error should warn that the effect is partial: {}",
        err.message
    );
    assert!(
        !err.message.contains("this input was not sent"),
        "a partial burst must not read like nothing was sent: {}",
        err.message
    );

    // The claim has to be true: those two really are queued for the app.
    let mut queued = Vec::new();
    while let Ok((_id, input)) = input_rx.try_recv() {
        queued.push(input);
    }
    assert_eq!(
        queued.len(),
        2,
        "the error names 2 sent; the queue must hold exactly those: {queued:?}"
    );
}

#[tokio::test]
async fn type_text_forwards_the_whole_string_in_one_input() {
    let (_dir, listener, mut handle, server) = setup("type-text").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("empty-input"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        let (_id, input) = app.recv_input().await;
        assert_eq!(
            input,
            AgentInput::Text {
                text: "buy milk".to_string(),
            }
        );
        app.send(&AppToBridge::Snapshot(Snapshot::new(2, demo_root("typed"))))
            .await;
        app
    });

    let result = server
        .type_text(Parameters(TypeTextParams {
            text: "buy milk".to_string(),
        }))
        .await
        .expect("type_text tool");
    assert!(
        result_text(&result).contains("typed"),
        "tree json: {}",
        result_text(&result)
    );

    let mut app = echo.await.expect("fake app task");
    app.expect_no_input(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn type_text_rejects_empty_text() {
    let (_dir, _listener, _handle, server) = setup("type-text-empty").await;
    let err = server
        .type_text(Parameters(TypeTextParams {
            text: String::new(),
        }))
        .await
        .expect_err("empty text must be rejected");
    assert!(err.message.contains("non-empty"), "err: {}", err.message);
}

#[tokio::test]
async fn type_text_rejects_oversized_text() {
    let (_dir, listener, mut handle, server) = setup("type-text-huge").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("tree"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let err = server
        .type_text(Parameters(TypeTextParams {
            text: "x".repeat(4097),
        }))
        .await
        .expect_err("oversized text must be rejected");
    assert!(
        err.message.contains("4097") && err.message.contains("4096"),
        "error should name the size and the limit: {}",
        err.message
    );

    app.expect_no_input(Duration::from_millis(300)).await;
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
            repeat: None,
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
            repeat: None,
        }))
        .await
        .expect_err("key while disconnected must refuse");
    assert!(err.message.contains("disconnected"), "err: {}", err.message);

    // ...so queue stale inputs directly; the listener is still unbound, so
    // these sit in the channel until the next successful connect.
    for key in ["q", "y"] {
        let id = handle.next_input_id();
        handle
            .input_tx
            .send((
                id,
                AgentInput::Key {
                    key: key.to_string(),
                },
            ))
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
            repeat: None,
        }))
        .await
        .expect("key against the restarted app");
    let (_id, input) = app.recv_input().await;
    assert_eq!(
        input,
        AgentInput::Key {
            key: "enter".to_string(),
        },
        "stale inputs leaked into the new app instance"
    );

    // And nothing stale trails behind it.
    app.expect_no_input(Duration::from_millis(300)).await;
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
    let (_id, input) = app.recv_input().await;
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

/// After the app dies, every tool must say WHICH app went away and at what
/// seq (not the generic never-connected message), and once the app is back
/// the bridge must serve trees again.
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

    // Every tool refuses, naming the dead app and its last snapshot seq.
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
            repeat: None,
        }))
        .await
        .expect_err("key must refuse after disconnect");
    let text_err = server
        .type_text(Parameters(TypeTextParams {
            text: "hello".to_string(),
        }))
        .await
        .expect_err("type_text must refuse after disconnect");
    for err in [&read_err, &act_err, &key_err, &text_err] {
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
