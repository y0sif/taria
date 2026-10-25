//! Integration tests: a fake taria app (tokio `UnixListener`) on one side,
//! the bridge's socket manager plus the MCP handler's tool methods on the
//! other. No MCP stdio transport involved.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus, MAX_LINE_BYTES};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use taria_mcp::bridge::{self, AppSnapshot, BridgeHandle, BridgeState, LinkState};
use taria_mcp::server::{ActParams, KeyParams, TariaMcpServer, TypeTextParams};
use taria_mcp::tree_format::TreeFormat;
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
        app.send(&AppToBridge::hello("fake-app", protocol_version))
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
        let line = serde_json::to_string(msg).expect("serialize app message");
        self.send_raw(&line).await;
    }

    /// Send one line exactly as given, without going through this build's
    /// types. The only way to play an app whose format this build cannot
    /// spell: a newer vocabulary, a field added later, or simply a serializer
    /// that lays its keys out differently.
    async fn send_raw(&mut self, line: &str) {
        let mut line = line.to_string();
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .expect("write to bridge");
    }

    /// Publish a tree whose line is over [`MAX_LINE_BYTES`], the way an app
    /// with an unbounded draft does once the draft grows past it.
    ///
    /// The write is allowed to fail partway: the bridge stops reading at its
    /// cap and closes the connection, so a broken pipe here is the outcome
    /// these tests are about rather than a fault in the fake.
    async fn send_oversized(&mut self, seq: u64) {
        let mut line = oversized_snapshot_line(seq);
        line.push('\n');
        let _ = self.write.write_all(line.as_bytes()).await;
    }

    /// Wait until the bridge closes this connection.
    async fn wait_closed(&mut self) {
        timeout(WAIT, async {
            while let Ok(Some(_)) = self.lines.next_line().await {}
        })
        .await
        .expect("the bridge should close the connection");
    }

    /// Answer an input, naming the id it was sent under.
    async fn ack(&mut self, id: InputId, status: InputStatus) {
        self.send(&AppToBridge::ack(id, status)).await;
    }

    /// Read the next `BridgeToApp::Input` line from the bridge, acking it as
    /// this app was configured to.
    async fn recv_input(&mut self) -> (InputId, AgentInput) {
        let line = timeout(WAIT, self.lines.next_line())
            .await
            .expect("input should arrive")
            .expect("read from bridge")
            .expect("bridge closed the socket");
        let BridgeToApp::Input { id, input, .. } =
            serde_json::from_str(&line).expect("parse bridge message")
        else {
            panic!("expected an input message, got: {line}");
        };
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

/// One snapshot in the form the watch holds it: the line an app would have
/// sent, beside the parse of that line. Tests that wire a `BridgeHandle` by
/// hand publish through this rather than inventing a line, so what they put in
/// the watch is what a real connection would have put there.
fn published(parsed: Snapshot) -> AppSnapshot {
    let line = serde_json::to_string(&AppToBridge::Snapshot(parsed.clone()))
        .expect("a snapshot serializes");
    AppSnapshot::from_line(&line, parsed)
}

/// A snapshot line over [`MAX_LINE_BYTES`]: a well-formed tree whose one text
/// input holds a value the app never bounded, which is how a real app gets
/// here. Well-formed on purpose, so the only thing wrong with it is its size.
fn oversized_snapshot_line(seq: u64) -> String {
    let root = Node::new("app", Role::App).label("too-big").child(
        Node::new("draft", Role::TextInput)
            .value("x".repeat(MAX_LINE_BYTES))
            .action(Action::SetValue),
    );
    let line = serde_json::to_string(&AppToBridge::Snapshot(Snapshot::new(seq, root)))
        .expect("an oversized snapshot serializes");
    assert!(line.len() > MAX_LINE_BYTES, "the line must be over the cap");
    line
}

/// The words every answer about a line over the cap carries.
const OVER_THE_LIMIT: &str = "over 1 MiB (1048576 bytes)";

/// Wait until the watch holds a connected snapshot with at least `min_seq`.
async fn wait_for_snapshot(rx: &mut watch::Receiver<BridgeState>, min_seq: u64) -> AppSnapshot {
    timeout(WAIT, async {
        loop {
            let hit = match &*rx.borrow_and_update() {
                BridgeState::Connected(s) if s.parsed.seq >= min_seq => Some(s.clone()),
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

/// Wait until the bridge holds an open connection again.
async fn wait_for_link_open(handle: &BridgeHandle) {
    let mut link = handle.link_rx.clone();
    timeout(
        WAIT,
        link.wait_for(|link| matches!(link, LinkState::Open { .. })),
    )
    .await
    .expect("the bridge should reconnect")
    .expect("the bridge task is alive");
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

/// The first error a retrofitting adopter meets must not send them after a
/// socket path the app plainly found.
///
/// An app whose snapshots the bridge rejects leaves the state at
/// `BridgeState::Never`, exactly like an app that never started, and the old
/// message asked whether the app was running and the path correct. Both were
/// yes. The bridge knew why it threw the line away and said so only to its
/// own log, which nobody retrofitting an app is reading.
#[tokio::test]
async fn read_tree_names_the_parse_failure_rather_than_the_socket_path() {
    let (_dir, path) = test_socket_path("unreadable");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);

    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("bridge should connect")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::hello("deep-app", PROTOCOL_VERSION))
        .await;
    // The failure `MAX_NODE_DEPTH` exists to keep off the wire: a tree past
    // what a JSON parser will recurse into. It is sent raw, because this
    // build's own serializer is happy to write a line its reader cannot read,
    // which is precisely how an app gets into this state.
    let mut deep = Node::new("leaf", Role::TreeItem);
    for level in (0..200).rev() {
        deep = Node::new(format!("n{level}"), Role::TreeItem).child(deep);
    }
    let line = serde_json::to_string(&AppToBridge::Snapshot(Snapshot::new(1, deep)))
        .expect("a deep snapshot serializes even though it will not parse");
    app.send_raw(&line).await;

    // Poll: the reject is recorded by the manager task, not by this one.
    let err = timeout(WAIT, async {
        loop {
            let err = server
                .read_tree()
                .await
                .expect_err("no snapshot ever became available");
            // Polled on the answer this test is about, not on the absence of
            // the socket-path question: the handshake this app sent first
            // already takes that question away, and stopping there would
            // assert against the wrong one of the two answers.
            if err.message.contains("reached this socket") {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rejected line should reach the tool surface");

    assert!(
        err.message.contains("reached this socket"),
        "the app was there, and the error has to say so: {}",
        err.message
    );
    assert!(
        err.message.contains("recursion limit"),
        "the reason the line was thrown away is the whole point: {}",
        err.message
    );
    assert!(
        !err.message.contains("is the socket path correct"),
        "the socket path is the one thing already proven fine: {}",
        err.message
    );

    // An app that then publishes something readable is served normally: the
    // rejection is an explanation for having no tree, never a sticky failure.
    app.send(&AppToBridge::Snapshot(Snapshot::new(
        2,
        demo_root("recovered"),
    )))
    .await;
    let tree = timeout(WAIT, async {
        loop {
            if let Ok(result) = server.read_tree().await {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the app's next readable snapshot should be served");
    assert!(
        format!("{tree:?}").contains("recovered"),
        "a readable snapshot after a rejected one is served as usual"
    );
}

/// An app whose very first tree is over the line cap. It greets every bridge
/// that connects and sends the same tree, so the bridge closes the connection,
/// backs off, reconnects and is closed again, and an agent polling `read_tree`
/// lands in every phase of that loop: between attempts with nothing connected,
/// on a connection just opened, and after a fresh handshake. Each phase had an
/// answer of its own, and none was the size: "is the socket path correct?",
/// "worth one retry", an app that "never publishes nodes".
#[tokio::test]
async fn read_tree_names_an_oversized_first_snapshot_on_every_reconnect() {
    let (_dir, path) = test_socket_path("oversized-first");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);

    let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = accepts.clone();
    let app = tokio::spawn(async move {
        while let Ok((stream, _addr)) = listener.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut app = FakeApp::from_stream(stream);
            app.send(&AppToBridge::hello("big-app", PROTOCOL_VERSION))
                .await;
            app.send_oversized(1).await;
        }
    });

    // Until the first overflow the answers are about the connection, which
    // is right: nothing has been rejected yet.
    timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("no tree fits");
            if err.message.contains(OVER_THE_LIMIT) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the oversized line should reach the tool surface");

    // From then on, every answer names the size, across a span longer than
    // the bridge's 2 s backoff cap, so it covers several reconnects.
    let span = Duration::from_millis(2_500);
    let started = Instant::now();
    let mut polls = 0;
    while started.elapsed() < span {
        let err = server
            .read_tree()
            .await
            .expect_err("a tree over the cap is never served");
        assert!(
            err.message.starts_with("no snapshot from the app yet")
                && err.message.contains(OVER_THE_LIMIT)
                && err.message.contains("The socket path is right"),
            "poll {polls}: the size is the answer: {}",
            err.message
        );
        // The last is the rejected-line answer's depth hint, which is about
        // a line that arrived whole and failed to parse.
        for wrong in [
            "is the socket path correct",
            "worth one retry",
            "never publishes nodes",
            "levels deep",
        ] {
            assert!(
                !err.message.contains(wrong),
                "poll {polls}: `{wrong}` is another state's answer: {}",
                err.message
            );
        }
        polls += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let accepted = accepts.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        accepted >= 3,
        "the span must cover reconnects, but the app accepted {accepted} connections"
    );
    app.abort();
}

/// An app that overflowed and then went away, taking its listener with it.
/// The size answer says the app is running and resends the tree on every
/// reconnect, and that nothing the agent sends can fix it. Kept after the app
/// exits, it held an agent to that story about an app that was gone, until
/// some app sent a tree that fit. A connect that finds nothing listening is
/// the bridge's evidence the app has gone, and the plain departure answer
/// takes over from it.
///
/// `unlink` picks how the app leaves: removing its socket file (`connect`
/// finds no file) or leaving it behind, the way a crash does (`connect` is
/// refused).
async fn forgets_the_size_once_the_app_is_gone(name: &str, unlink: bool) {
    let (_dir, path) = test_socket_path(name);
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());

    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before-paste"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    app.send_oversized(2).await;
    app.wait_closed().await;

    // The size is recorded while the app is still there: the bridge's
    // reconnect lands in the listener's backlog.
    let opening = "app 'fake-app' disconnected (last snapshot seq 1); ";
    timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("no tree fits");
            if err.message.starts_with(opening) && err.message.contains(OVER_THE_LIMIT) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the oversized line should reach the tool surface");

    drop(app);
    drop(listener);
    if unlink {
        std::fs::remove_file(&path).expect("remove the app's socket file");
    }

    // The backlogged connection is reset with the listener, and the next
    // connect finds nothing listening, so this is one backoff away.
    let plain = "app 'fake-app' disconnected (last snapshot seq 1); it may have exited. The \
                 bridge reconnects automatically; retry once the app is back.";
    let err = timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("the app is gone");
            if !err.message.contains(OVER_THE_LIMIT) {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the size should be forgotten once nothing listens");
    assert_eq!(err.message, plain);

    // And it stays forgotten across the failed reconnects that follow: nothing
    // is there to send the tree again.
    for poll in 0..10 {
        let err = server.read_tree().await.expect_err("the app is gone");
        assert_eq!(err.message, plain, "poll {poll}");
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
}

#[tokio::test]
async fn read_tree_forgets_the_size_once_the_app_removed_its_socket() {
    forgets_the_size_once_the_app_is_gone("oversized-then-unlinked", true).await;
}

#[tokio::test]
async fn read_tree_forgets_the_size_once_the_app_left_a_stale_socket() {
    forgets_the_size_once_the_app_is_gone("oversized-then-stale", false).await;
}

/// A message whose `type` this bridge has no variant for is additive under the
/// freeze, so it must be ignored like an unknown enum variant, not reported as
/// a malformed line. Regression for the v0.1 review: an internally tagged enum
/// fails to parse such a line, so it used to land in the rejected-line path and
/// misdiagnose a conforming newer app to the agent.
#[tokio::test]
async fn read_tree_ignores_an_unknown_future_message_type() {
    let (_dir, path) = test_socket_path("future-msg");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);

    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("bridge should connect")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::hello("future-app", PROTOCOL_VERSION))
        .await;
    // Well-formed, and its `type` is one a later version 1 app added that this
    // bridge does not know. Sent raw because this build has no variant to
    // serialize it from, which is exactly how the situation arises.
    app.send_raw(r#"{"type":"capability","features":["paste"]}"#)
        .await;

    // The lines are processed in order on the one stream, so once the manager
    // has had a moment the unknown line has been seen. The app sent no
    // snapshot, so read_tree still errors, but with the plain "no snapshot yet"
    // answer, never the "reached this socket ... malformed" one an unknown
    // message used to trigger.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let err = server
        .read_tree()
        .await
        .expect_err("no snapshot was ever sent");
    assert!(
        !err.message.contains("reached this socket"),
        "an unknown future message must not surface as a rejected line: {}",
        err.message
    );

    // And a real snapshot after the unknown line is served normally: ignoring
    // it did not break the stream.
    app.send(&AppToBridge::Snapshot(Snapshot::new(
        1,
        demo_root("after-unknown"),
    )))
    .await;
    let tree = timeout(WAIT, async {
        loop {
            if let Ok(result) = server.read_tree().await {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the app's snapshot after an unknown line should be served");
    assert!(
        format!("{tree:?}").contains("after-unknown"),
        "an unknown line must not block a later snapshot"
    );
}

/// The state that stopped the first agent to meet this project before it
/// could try anything: a bridge whose `connect` succeeded onto a socket that
/// was not going to accept it while another bridge held the app.
///
/// The adapter serves one bridge at a time, so a second bridge's connection is
/// completed by the kernel into the listen backlog and left unread until the
/// first bridge leaves: no handshake, no snapshot, and the same "is the socket
/// path correct?" an app that was never started gets. Both answers were yes,
/// and the real cause was another session's `taria-mcp` still holding the app.
/// The bridge knows its own connect succeeded, which is the whole difference.
///
/// Since 0.3.2 the answer says the app is held by another bridge and that a
/// taria-mcp lets go of it after a quiet minute, so the way out is a retry,
/// not a hunt for the other process.
#[tokio::test]
async fn read_tree_names_a_connection_the_app_never_accepted() {
    let (_dir, path) = test_socket_path("never-accepted");
    // Bound and listening, never accepted. Held for the whole test: dropping
    // it would close the socket and turn this into an ordinary disconnect.
    let _listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle);

    // Polled: the connection has to be open past the bridge's grace before
    // its silence means anything, and the manager opens it on its own
    // schedule.
    let err = timeout(WAIT, async {
        loop {
            let err = server
                .read_tree()
                .await
                .expect_err("nothing will ever publish a tree here");
            if err.message.contains("held by another bridge") {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a connection nobody accepts should be diagnosed, not waited on forever");

    assert!(
        err.message
            .starts_with("no snapshot from the app yet: the app is held by another bridge. ")
            && err.message.contains("serves one bridge at a time")
            && err.message.contains("lets go of the app after 60 s"),
        "the answer has to name the cause and what to do about it: {}",
        err.message
    );
    assert!(
        !err.message.contains("is the socket path correct")
            && !err.message.contains("may have exited"),
        "the bridge is connected to that path, and the app is running: {}",
        err.message
    );

    // The input tools ask for a tree before they send anything, so they carry
    // the same diagnosis rather than a second vocabulary for it.
    let err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("there is no app to send a key to");
    assert!(
        err.message.contains("held by another bridge"),
        "an input tool must diagnose this as read_tree does: {}",
        err.message
    );
}

#[tokio::test]
async fn manager_connects_and_watch_gets_snapshot() {
    let (_dir, listener, mut handle, server) = setup("connects").await;
    let _app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("hello-tree"))).await;

    let snapshot = wait_for_snapshot(&mut handle.state_rx, 1).await;
    assert_eq!(snapshot.parsed.seq, 1);
    assert_eq!(snapshot.parsed.root.id.0, "app");

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
            AgentInput::act(taria::NodeId("btn".to_string()), Action::Activate, None)
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
///
/// A result and not an error: the input was delivered and acknowledged, so
/// its fate is known, and an agent invoking an advertised `quit` reaches this
/// state on purpose. Raised, that working quit read as a failed call to
/// anything branching on the error flag.
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

    let result = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect("an input the app acknowledged is not a failed call, even when it was the last");
    let text = result_text(&result);
    assert!(
        text.contains("The app acknowledged this input and then disconnected"),
        "the report must tie the departure to the input: {text}"
    );
    assert!(
        text.contains("app 'fake-app' (last snapshot seq 4) is gone"),
        "the report must name which app went away, and when: {text}"
    );
    assert!(
        !text.contains("did not change"),
        "an app that exited must not read as one that ignored the input: {text}"
    );

    exit.await.expect("fake app task");
}

/// The input that pushed the tree over the cap: the app acks it and publishes
/// the tree it led to, which the bridge closes the connection over. The app
/// never went anywhere, so "may have exited, possibly because of this input"
/// sends the agent to wait for an app that is running and will resend the
/// same tree to every reconnect. The answer names the size instead, and stays
/// a result: the input's fate is known.
#[tokio::test]
async fn act_names_the_oversized_tree_that_followed_the_input() {
    let (_dir, path) = test_socket_path("act-then-oversized");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle.clone());

    let app = FakeApp::accept(&listener, Snapshot::new(4, demo_root("before-paste"))).await;
    wait_for_snapshot(&mut handle.state_rx, 4).await;

    // The listener is handed back rather than dropped, like a running app's:
    // the bridge's reconnect lands in its backlog and is not answered inside
    // the call's window.
    let paste = tokio::spawn(async move {
        let mut app = app;
        app.recv_input().await;
        app.send_oversized(5).await;
        (app, listener)
    });

    let result = server
        .act(Parameters(ActParams {
            node: "input".to_string(),
            action: "set_value".to_string(),
            value: Some("a long paste".to_string()),
        }))
        .await
        .expect("an input the app acknowledged is not a failed call");
    let text = result_text(&result);
    assert!(
        text.starts_with("The app acknowledged this input and then disconnected: ")
            && text.contains("app 'fake-app' (last snapshot seq 4) is gone"),
        "the opening every acknowledged departure has: {text}"
    );
    assert!(
        text.contains(OVER_THE_LIMIT) && text.contains("The bridge closed the connection"),
        "the report must name the size as the cause: {text}"
    );
    assert!(
        !text.contains("may have exited"),
        "the app did not exit: {text}"
    );

    paste.await.expect("fake app task");
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
        assert_eq!(input, AgentInput::key("ctrl+s"));
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

/// An acknowledgement this bridge cannot read arrives as silence, and the
/// report has to say so rather than let it pass for an unreactive app.
///
/// `InputStatus` has no fallback variant by design, so a status name added
/// later fails the whole `Ack` line and takes the input id down with it. The
/// bridge therefore never sees an ack, and the burst looks exactly like an
/// app that answers nothing. The rejected line is the only trace left of the
/// difference.
#[tokio::test]
async fn an_acknowledgement_this_bridge_cannot_read_is_reported_as_more_than_silence() {
    let (_dir, listener, mut handle, server) = setup("unknown-status").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before")))
        .await
        .never_acking();
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    // Answer the input with a status from a taria newer than this bridge, and
    // change nothing else: no ack the bridge can read, and no new tree.
    let echo = tokio::spawn(async move {
        let (id, _input) = app.recv_input().await;
        app.send_raw(&format!(
            r#"{{"type":"ack","id":{id},"status":"coalesced"}}"#
        ))
        .await;
        app
    });

    let result = server
        .key(Parameters(KeyParams {
            key: "x".to_string(),
            repeat: None,
        }))
        .await
        .expect("key tool answers even when the ack is unreadable");
    let text = result_text(&result);
    assert!(
        text.contains("could not read"),
        "silence that was really a rejected line has to say so: {text}"
    );
    assert!(
        text.contains("acknowledgement"),
        "the report should name what the unreadable line probably was: {text}"
    );

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
            assert_eq!(input, AgentInput::key("down"));
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
    let (_state_tx, state_rx) = watch::channel(BridgeState::Connected(published(Snapshot::new(
        1,
        demo_root("not-draining"),
    ))));
    let (input_tx, _input_rx) = mpsc::channel(1);
    let (ack_tx, _ack_rx) = broadcast::channel(8);
    let (_protocol_tx, protocol_rx) = watch::channel(Some(PROTOCOL_VERSION));
    let (_rejected_tx, rejected_rx) = watch::channel(None);
    // Connected, like the snapshot above says: the link channel describes the
    // socket this hand-built handle is standing in for.
    let (_link_tx, link_rx) = watch::channel(LinkState::Open {
        since: Instant::now(),
    });
    let handle = BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
        rejected_rx,
        link_rx,
    };
    // Occupy the only slot, so the tool's own send has nowhere to go.
    handle
        .input_tx
        .send((handle.next_input_id(), AgentInput::key("x")))
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
    let (_state_tx, state_rx) = watch::channel(BridgeState::Connected(published(Snapshot::new(
        1,
        demo_root("stops-draining"),
    ))));
    // Two slots and nobody reading them: the third press onwards has nowhere
    // to go.
    let (input_tx, mut input_rx) = mpsc::channel(2);
    let (ack_tx, _ack_rx) = broadcast::channel(8);
    let (_protocol_tx, protocol_rx) = watch::channel(Some(PROTOCOL_VERSION));
    let (_rejected_tx, rejected_rx) = watch::channel(None);
    let (_link_tx, link_rx) = watch::channel(LinkState::Open {
        since: Instant::now(),
    });
    let server = TariaMcpServer::new(BridgeHandle {
        state_rx,
        input_tx,
        ack_tx,
        protocol_rx,
        rejected_rx,
        link_rx,
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
        assert_eq!(input, AgentInput::text("buy milk"));
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
            .send((id, AgentInput::key(key)))
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
        AgentInput::key("enter"),
        "stale inputs leaked into the new app instance"
    );

    // And nothing stale trails behind it.
    app.expect_no_input(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn newline_free_flood_disconnects_the_app_connection() {
    let (_dir, listener, mut handle, server) = setup("line-flood").await;
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

    // And says why: the bridge closed the connection over the line's size,
    // which "it may have exited" would have put down to the app.
    let err = server
        .read_tree()
        .await
        .expect_err("no tree after the connection broke");
    assert!(
        err.message
            .starts_with("app 'fake-app' disconnected (last snapshot seq 1); ")
            && err.message.contains(OVER_THE_LIMIT),
        "read_tree must name the line limit: {}",
        err.message
    );
}

/// An app that served a tree and then outgrew the cap. Every tool keeps the
/// opening a disconnect has, and names the size where "it may have exited"
/// used to be, including after the reconnect that greets the bridge and sends
/// the same tree again: the handshake is not a tree, and must not clear it.
#[tokio::test]
async fn a_tree_that_outgrows_the_cap_is_named_by_every_tool() {
    let (_dir, path) = test_socket_path("outgrown");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle.clone());

    let mut app = FakeApp::accept(&listener, Snapshot::new(3, demo_root("small"))).await;
    wait_for_snapshot(&mut handle.state_rx, 3).await;
    app.send_oversized(4).await;
    wait_for_disconnect(&mut handle.state_rx).await;

    // The bridge reconnects at once after a connection that delivered a tree,
    // and a running app resends its latest one.
    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("the bridge should reconnect")
        .expect("accept");
    let mut again = FakeApp::from_stream(stream);
    again
        .send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .await;
    again.send_oversized(4).await;
    again.wait_closed().await;

    let read_err = server
        .read_tree()
        .await
        .expect_err("read_tree has no tree to serve");
    let act_err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("act must refuse while the tree does not fit");
    for err in [&read_err, &act_err] {
        assert!(
            err.message
                .starts_with("app 'fake-app' disconnected (last snapshot seq 3); "),
            "the disconnect opening stays: {}",
            err.message
        );
        assert!(
            err.message.contains(OVER_THE_LIMIT) && err.message.contains("on every reconnect"),
            "the size is the cause, and it repeats: {}",
            err.message
        );
        assert!(
            !err.message.contains("may have exited"),
            "the app did not exit: {}",
            err.message
        );
    }
    drop(app);
}

/// The size is a reason for as long as the tree is too big, and no longer. A
/// tree that fits again is served and clears it, so a later plain disconnect
/// reads as the departure it is, not as the old size.
#[tokio::test]
async fn a_tree_that_fits_again_clears_the_size_from_the_next_disconnect() {
    let (_dir, path) = test_socket_path("refits");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path);
    let server = TariaMcpServer::new(handle.clone());

    let mut app = FakeApp::accept(&listener, Snapshot::new(3, demo_root("small"))).await;
    wait_for_snapshot(&mut handle.state_rx, 3).await;
    app.send_oversized(4).await;
    wait_for_disconnect(&mut handle.state_rx).await;
    let err = server
        .read_tree()
        .await
        .expect_err("no tree while it does not fit");
    assert!(err.message.contains(OVER_THE_LIMIT), "{}", err.message);

    // The app bounds its draft and publishes a tree that fits.
    let refit = FakeApp::accept(&listener, Snapshot::new(5, demo_root("bounded"))).await;
    wait_for_snapshot(&mut handle.state_rx, 5).await;
    let tree = server
        .read_tree()
        .await
        .expect("a tree that fits is served");
    assert!(result_text(&tree).contains("bounded"), "{tree:?}");

    // Then exits.
    drop(refit);
    wait_for_disconnect(&mut handle.state_rx).await;
    let err = server
        .read_tree()
        .await
        .expect_err("no tree after the app went");
    assert_eq!(
        err.message,
        "app 'fake-app' disconnected (last snapshot seq 5); it may have exited. The bridge \
         reconnects automatically; retry once the app is back.",
        "a plain departure must not inherit the old size"
    );
    drop(app);
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

/// One line from an app on a later version of the format: a role and an action
/// this build has never heard of, a node field it has no place for, and a key
/// order and spacing no serializer here would produce. Every one of those is
/// information the bridge is passing on, not information it owns.
const NEWER_APP_LINE: &str = concat!(
    r#"{"seq": 2, "type": "snapshot", "protocol_version": 1, "root":"#,
    r#" {"id": "app", "role": "app", "focused": false, "children": ["#,
    r#"{"id": "chart", "role": "sparkline", "label": "cpu", "focused": true,"#,
    r#" "actions": ["zoom"], "sample_hz": 30}]}}"#
);

/// One tool result as information rather than bytes.
fn as_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text)
        .unwrap_or_else(|err| panic!("a tool result must be json: {err}: {text}"))
}

/// What an app line must look like once it reaches the agent: everything the
/// app sent, minus the `"type"` key that frames the message on the wire and is
/// no part of the tree.
fn without_envelope(line: &str) -> serde_json::Value {
    let mut value = as_json(line);
    let framing = value
        .as_object_mut()
        .expect("a snapshot line is a json object")
        .remove("type");
    assert_eq!(
        framing,
        Some(serde_json::json!("snapshot")),
        "the app's line must carry the framing key these tests are about"
    );
    value
}

/// The bridge passes on the app's own snapshot line; it does not re-serialize
/// its own parse of it, and it leaves out the envelope the message arrived in.
///
/// Deserializing into `Snapshot` and serializing back is lossy by
/// construction: it can only emit what this build's types can hold. An app
/// publishing `"role":"sparkline"` had the agent read `"role":"other"`, and the
/// real name had been on the wire the whole time. Everything the typed struct
/// cannot spell went the same way, which made the format's promise that
/// additive changes are safe true of the connection and false of the
/// information travelling over it.
///
/// So the claim is about information, not bytes: every key the app sent still
/// reaches the agent saying the same thing, the framing key does not, and key
/// order and whitespace are nobody's contract.
#[tokio::test]
async fn read_tree_relays_the_apps_tree_without_the_transport_envelope() {
    let (_dir, listener, mut handle, server) = setup("relay-verbatim").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("first"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    app.send_raw(NEWER_APP_LINE).await;
    wait_for_snapshot(&mut handle.state_rx, 2).await;

    let result = server.read_tree().await.expect("read_tree after snapshot");
    let text = result_text(&result);
    let relayed = as_json(text);
    assert_eq!(
        relayed,
        without_envelope(NEWER_APP_LINE),
        "read_tree must hand back everything the app sent, and nothing it did not"
    );
    assert!(
        relayed.get("type").is_none(),
        "the transport's framing key must not reach the agent: {text}"
    );

    // Spelled out, because these three are the losses passing the line on
    // exists to prevent and an equality failure alone would not name them.
    let chart = &relayed["root"]["children"][0];
    assert_eq!(
        chart["role"], "sparkline",
        "the role the app published must reach the agent by name: {text}"
    );
    assert_eq!(
        chart["actions"],
        serde_json::json!(["zoom"]),
        "the action the app published must reach the agent by name: {text}"
    );
    assert_eq!(
        chart["sample_hz"], 30,
        "a field added after this build must survive being passed on: {text}"
    );
}

/// The parse stays authoritative for what the bridge decides. The line the
/// agent reads is untyped, so `act` cannot validate against it: node ids and
/// advertised actions still come from the parse, where the unknown action is
/// `Custom("zoom")` and reaches the app under the name the tree showed.
#[tokio::test]
async fn the_parse_still_validates_acts_against_the_relayed_tree() {
    let (_dir, listener, mut handle, server) = setup("relay-validate").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("first"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    app.send_raw(NEWER_APP_LINE).await;
    wait_for_snapshot(&mut handle.state_rx, 2).await;

    // The app is handed back rather than dropped: a closed socket would end
    // the connection and turn the second act below into a disconnect report.
    let echo = tokio::spawn(async move {
        let (_id, input) = app.recv_input().await;
        (app, input)
    });

    server
        .act(Parameters(ActParams {
            node: "chart".to_string(),
            action: "zoom".to_string(),
            value: None,
        }))
        .await
        .expect("an action the tree passed on advertises");
    let (_app, input) = echo.await.expect("fake app task");
    assert_eq!(
        input,
        AgentInput::act(
            taria::NodeId("chart".to_string()),
            Action::Custom("zoom".to_string()),
            None
        )
    );

    // And a node the line passed on does not carry is still refused, from the
    // same parse.
    let err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("the newer tree has no `btn`");
    assert!(
        err.message.contains("unknown node id `btn`"),
        "validation must follow the current tree: {}",
        err.message
    );
}

/// The tools that answer with a tree pass the app's line on too, not just
/// `read_tree`. Anything that re-serialized here would put the agent back
/// where it started one call later.
#[tokio::test]
async fn a_tool_result_relays_the_tree_the_app_published() {
    let (_dir, listener, mut handle, server) = setup("relay-tool-result").await;
    let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("before"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let echo = tokio::spawn(async move {
        let mut app = app;
        let (_id, _input) = app.recv_input().await;
        app.send_raw(NEWER_APP_LINE).await;
        app
    });

    let result = server
        .key(Parameters(KeyParams {
            key: "z".to_string(),
            repeat: None,
        }))
        .await
        .expect("key against a connected app");
    assert_eq!(
        as_json(result_text(&result)),
        without_envelope(NEWER_APP_LINE),
        "the tree a tool answers with must be the one the app published"
    );

    echo.await.expect("fake app task");
}

/// `--tree-format outline` changes the text an agent reads and nothing else:
/// the outline is rendered from the line passed on, so the newer app's role,
/// action and field reach the agent by name there too, and the tree an input
/// tool answers with comes out in the same format as `read_tree`'s.
#[tokio::test]
async fn outline_mode_renders_the_relayed_tree_for_every_tool() {
    let (_dir, path) = test_socket_path("outline-relay");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path);
    let server = TariaMcpServer::with_tree_format(handle.clone(), TreeFormat::Outline);
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("first"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    app.send_raw(NEWER_APP_LINE).await;
    wait_for_snapshot(&mut handle.state_rx, 2).await;

    let result = server.read_tree().await.expect("read_tree after snapshot");
    assert_eq!(
        result_text(&result),
        "protocol_version=1 seq=2\napp app\n  chart sparkline \"cpu\" *focused sample_hz=30 act: zoom",
        "the outline must carry what the app sent, by name"
    );

    // The act is validated against the parse exactly as in the JSON mode,
    // and its answer is the next tree, as an outline.
    let echo = tokio::spawn(async move {
        let (_id, input) = app.recv_input().await;
        app.send(&AppToBridge::Snapshot(Snapshot::new(
            3,
            demo_root("zoomed"),
        )))
        .await;
        (app, input)
    });
    let result = server
        .act(Parameters(ActParams {
            node: "chart".to_string(),
            action: "zoom".to_string(),
            value: None,
        }))
        .await
        .expect("an action the outline advertises, passed as it spells it");
    assert_eq!(
        result_text(&result),
        "protocol_version=1 seq=3\napp app \"zoomed\"\n  btn button \"Save\" act: activate\n  \
         input text_input = \"draft\" act: set_value"
    );
    let (_app, input) = echo.await.expect("fake app task");
    assert_eq!(
        input,
        AgentInput::act(
            taria::NodeId("chart".to_string()),
            Action::Custom("zoom".to_string()),
            None
        )
    );
}

/// A line that failed to parse is still skipped, not passed on. The raw line is
/// the agent's view of the app, and forwarding one the bridge could not read
/// would hand the agent something no peer has vouched for; the last good tree
/// is the honest answer until a readable one arrives.
#[tokio::test]
async fn a_line_that_does_not_parse_is_never_relayed() {
    let (_dir, listener, mut handle, server) = setup("relay-skips-junk").await;
    let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("good-tree"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    app.send_raw(r#"{"type":"snapshot","protocol_version":1,"seq":2"#)
        .await;
    app.send_raw(r#"{"type":"snapshot","protocol_version":1,"seq":3,"root":42}"#)
        .await;
    // Nothing to wait for, so give the manager a window to have passed them on
    // if it were going to.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let text = result_text(&server.read_tree().await.expect("read_tree")).to_string();
    assert!(
        text.contains("good-tree") && text.contains(r#""seq":1"#),
        "a malformed line must leave the last good tree standing: {text}"
    );
}

// On-demand bridges: what the `taria-mcp` binary runs since 0.3.2. They
// connect on the first tool call and let go of the app once no call has run
// for their idle window, so an idle session no longer locks every other one
// out of an app that serves one bridge at a time (issue #13).

/// An idle window the on-demand tests can wait out: long enough that a test's
/// own calls run inside it.
const SHORT_IDLE: Duration = Duration::from_millis(300);

/// What a held answer says of when the holder lets go, from a bridge started
/// with a 30 s window: that a holder started the same way lets go after 30 s,
/// and to retry then. The 60 s default is named as the other possibility.
const HELD_ON_30_S: &str = "A taria-mcp started like this one lets go of the app after 30 s with \
no tool calls (one on the default, after 60 s), and this bridge takes over as soon as the holder \
does, with no restart; retry in 30 s.";

fn on_demand(path: PathBuf, idle_release: Duration) -> TariaMcpServer {
    TariaMcpServer::on_demand(path, TreeFormat::Json, idle_release)
}

/// An app that serves one bridge at a time, the way the adapter does, for as
/// many bridges as connect. Each gets `hello` (declaring `protocol_version`)
/// and a snapshot numbered by its turn and marked `gen-N`, and is held until
/// the bridge closes it. The first receiver gets each turn as its connection
/// closes; the second gets every input that reached the app, which acks it
/// `Delivered`.
fn serve_generations(
    listener: UnixListener,
    protocol_version: u32,
) -> (
    mpsc::UnboundedReceiver<u64>,
    mpsc::UnboundedReceiver<AgentInput>,
) {
    let (closed_tx, closed_rx) = mpsc::unbounded_channel();
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut turn = 0;
        while let Ok((stream, _addr)) = listener.accept().await {
            turn += 1;
            let mut app = FakeApp::from_stream(stream);
            app.send(&AppToBridge::hello("fake-app", protocol_version))
                .await;
            app.send(&AppToBridge::Snapshot(Snapshot::new(
                turn,
                demo_root(&format!("gen-{turn}")),
            )))
            .await;
            while let Ok(Some(line)) = app.lines.next_line().await {
                if let Ok(BridgeToApp::Input { id, input, .. }) = serde_json::from_str(&line) {
                    app.ack(id, InputStatus::Delivered).await;
                    let _ = input_tx.send(input);
                }
            }
            if closed_tx.send(turn).is_err() {
                return;
            }
        }
    });
    (closed_rx, input_rx)
}

/// Wait for [`serve_generations`] to see a bridge close its connection, and
/// return that connection's turn.
async fn wait_released(closed: &mut mpsc::UnboundedReceiver<u64>) -> u64 {
    timeout(WAIT, closed.recv())
        .await
        .expect("the bridge should let go of the app")
        .expect("the fake app is still serving")
}

/// A harness starts every configured MCP server with its session, and lists
/// its tools, long before an agent calls one. None of that may connect: an
/// app serves one bridge at a time, and a bridge that connected at startup
/// held it for a session that might never use it. The first call connects,
/// and still answers with the tree, in that one call.
#[tokio::test]
async fn on_demand_connects_on_the_first_tool_call_and_not_before() {
    use rmcp::ServerHandler;

    let (_dir, path) = test_socket_path("lazy");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, Duration::from_secs(30));

    // What a harness reads before its first tool call.
    assert!(server.get_info().instructions.is_some());
    assert!(server.get_tool("read_tree").is_some());
    // An eager bridge connects within milliseconds, and a connection made at
    // any point here would be waiting in the backlog for this accept.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let early = timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(
        early.is_err(),
        "the bridge connected before any tool call: {early:?}"
    );

    let app = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("first-call"))).await;
        (listener, app)
    });
    let result = server
        .read_tree()
        .await
        .expect("the first call waits for the app and answers with its tree");
    assert!(
        result_text(&result).contains("first-call"),
        "{}",
        result_text(&result)
    );
    drop(app.await.expect("fake app task"));
}

/// Issue #13 end to end. Two sessions share one app, which serves one bridge
/// at a time. The first holds it; the second's first call lands in the listen
/// backlog and says the app is held, and how it gets free. The first then
/// goes quiet, lets go after its window with no call from anyone, and the
/// second is accepted from the backlog, with no restart.
#[tokio::test]
async fn an_idle_bridge_hands_the_app_to_the_bridge_waiting_behind_it() {
    hand_over().await;
}

/// The same, on a runtime that runs the bridges' tasks and the calls on other
/// threads than the test's, as the binary's runtime may: the activity lock is
/// the only thing ordering a call against a release there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_bridge_hands_the_app_over_on_a_multi_thread_runtime() {
    hand_over().await;
}

async fn hand_over() {
    let (_dir, path) = test_socket_path("hand-over");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let holder = on_demand(path.clone(), Duration::from_millis(1_500));
    let waiting = on_demand(path, Duration::from_secs(30));

    let (released_tx, released_rx) = tokio::sync::oneshot::channel();
    let app = tokio::spawn(async move {
        let mut first = FakeApp::accept(&listener, Snapshot::new(1, demo_root("holder"))).await;
        first.wait_closed().await;
        let _ = released_tx.send(Instant::now());
        let second = FakeApp::accept(&listener, Snapshot::new(1, demo_root("waiting"))).await;
        (listener, second)
    });

    // Before the call, so the window, which starts when the call ends, is
    // all inside the measurement.
    let holder_called = Instant::now();
    let tree = holder.read_tree().await.expect("the holder gets the app");
    assert!(result_text(&tree).contains("holder"), "{tree:?}");

    let err = waiting
        .read_tree()
        .await
        .expect_err("the app is serving the holder");
    // The waiting bridge's own window is the one its answer names: a holder
    // started the same way lets go after the same.
    assert!(
        err.message
            .starts_with("no snapshot from the app yet: the app is held by another bridge. ")
            && err.message.contains(HELD_ON_30_S)
            && !err.message.contains("may have exited"),
        "{}",
        err.message
    );

    let released_at = timeout(WAIT, released_rx)
        .await
        .expect("the holder should let go after its window")
        .expect("the fake app task");
    let quiet = released_at.duration_since(holder_called);
    assert!(
        quiet >= Duration::from_millis(1_500),
        "the holder let go {quiet:?} after its last call began, inside its 1.5 s window"
    );

    // Accepted from the backlog now; the tree follows the accept by a moment.
    let tree = timeout(WAIT, async {
        loop {
            if let Ok(result) = waiting.read_tree().await {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the waiting bridge should be handed the app");
    assert!(result_text(&tree).contains("waiting"), "{tree:?}");
    drop(app.await.expect("fake app task"));
}

/// A bridge that let go reconnects on the next call, and that call answers
/// with the tree the app serves now, in one call.
#[tokio::test]
async fn a_call_after_a_release_reconnects_and_gets_a_fresh_tree() {
    call_after_release().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_after_a_release_reconnects_on_a_multi_thread_runtime() {
    call_after_release().await;
}

async fn call_after_release() {
    let (_dir, path) = test_socket_path("after-release");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, SHORT_IDLE);
    let (mut closed, _inputs) = serve_generations(listener, PROTOCOL_VERSION);

    let tree = server.read_tree().await.expect("the first connection");
    assert!(result_text(&tree).contains("gen-1"), "{tree:?}");
    assert_eq!(wait_released(&mut closed).await, 1);

    let tree = server
        .read_tree()
        .await
        .expect("the call after the release reconnects");
    assert!(result_text(&tree).contains("gen-2"), "{tree:?}");
}

/// A call cancelled partway (a harness that gave up on it) still ends, as far
/// as the idle window is concerned. Counted in and never out, it would keep
/// the bridge holding the app for the rest of the session.
#[tokio::test]
async fn a_cancelled_call_still_starts_the_idle_window() {
    cancelled_call().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_call_still_starts_the_idle_window_on_a_multi_thread_runtime() {
    cancelled_call().await;
}

async fn cancelled_call() {
    let (_dir, path) = test_socket_path("cancelled");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, SHORT_IDLE);
    // Accepted and silent, so the call waits on it until it is cancelled.
    let app = tokio::spawn(async move {
        let (stream, _addr) = timeout(WAIT, listener.accept())
            .await
            .expect("bridge should connect")
            .expect("accept");
        let mut app = FakeApp::from_stream(stream);
        app.wait_closed().await;
        listener
    });

    let cut = timeout(Duration::from_millis(100), server.read_tree()).await;
    assert!(cut.is_err(), "the call should still be waiting: {cut:?}");
    // `wait_closed` fails the task if the bridge never lets go.
    drop(
        app.await
            .expect("the bridge should let go after the window"),
    );
}

/// Calls that land on the end of the idle window, again and again, on a
/// runtime where the release and the call really run at once. Each call is
/// either served by the connection the manager kept or by a fresh one it made
/// for that call, never refused for a connection it let go of under the call,
/// and an input sent by such a call always reaches the app.
///
/// Some rounds must find the app let go: were the release never to run, every
/// call would be served by the kept connection, and the test would pass
/// without a single call racing a release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_on_the_edge_of_the_window_are_always_served() {
    let (_dir, path) = test_socket_path("edge-calls");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let window = Duration::from_millis(100);
    let server = on_demand(path, window);
    let (mut closed, mut inputs) = serve_generations(listener, PROTOCOL_VERSION);

    server.read_tree().await.expect("the first connection");
    let mut releases = 0;
    // Each round is a key call, which waits its half second for a change
    // that never comes, then a pause that lands the next call on the edge.
    for round in 0..8u64 {
        // Around the window's end: a little before, on either side of it,
        // and far enough after it that the release has had time to run on
        // a loaded host, which is what the count below relies on.
        let offset = Duration::from_millis([90, 97, 103, 150][(round % 4) as usize]);
        tokio::time::sleep(offset).await;
        server
            .key(Parameters(KeyParams {
                key: "x".to_string(),
                repeat: None,
            }))
            .await
            .unwrap_or_else(|err| panic!("round {round} after {offset:?}: {}", err.message));
        let input = timeout(WAIT, inputs.recv())
            .await
            .expect("the input should reach the app")
            .expect("the fake app is still serving");
        assert_eq!(input, AgentInput::key("x"), "round {round}");
        // A connection the manager let go of before this round's call is
        // closed before the app accepts the one the call made, so it is
        // counted by now.
        while closed.try_recv().is_ok() {
            releases += 1;
        }
    }
    assert!(
        releases > 0,
        "no round found the app let go, so no call raced a release"
    );
}

/// Letting go ends the connection, and an adapter discards whatever that
/// connection queued that it had not dequeued yet. So a bridge holds the app
/// past its window while an input it wrote awaits its ack, and lets go as
/// soon as the ack arrives.
#[tokio::test]
async fn an_input_awaiting_its_ack_holds_the_app_past_the_idle_window() {
    let (_dir, path) = test_socket_path("slow-ack");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, SHORT_IDLE);
    let app = tokio::spawn(async move {
        let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("slow-ack")))
            .await
            .never_acking();
        let (id, input) = app.recv_input().await;
        // Well past the window: the call ends half a second after the send,
        // and its window 300 ms after that. Failing here means the bridge let
        // go with the input unacknowledged.
        app.expect_no_input(Duration::from_millis(1_500)).await;
        app.ack(id, InputStatus::Delivered).await;
        let acked = Instant::now();
        app.wait_closed().await;
        (listener, input, acked.elapsed())
    });

    let result = server
        .key(Parameters(KeyParams {
            key: "x".to_string(),
            repeat: None,
        }))
        .await
        .expect("an unanswered input is not a failed call");
    assert!(
        result_text(&result).contains("neither acknowledged"),
        "{}",
        result_text(&result)
    );

    let (_listener, input, closed_after_ack) = app.await.expect("fake app task");
    assert_eq!(input, AgentInput::key("x"));
    assert!(
        closed_after_ack < Duration::from_secs(1),
        "the window had closed, so the ack should free the app at once, not after \
         {closed_after_ack:?}"
    );
}

/// The first call to an app that another bridge holds answers in that call,
/// and says so: it does not take a second call to diagnose a silence the
/// first one waited through. The input tools say the same.
#[tokio::test]
async fn the_first_call_to_a_held_app_says_it_is_held() {
    let (_dir, path) = test_socket_path("held-first-call");
    // Listening and never accepting: the app serving another bridge.
    let _listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, Duration::from_secs(30));

    let read_err = server.read_tree().await.expect_err("the app is held");
    let key_err = server
        .key(Parameters(KeyParams {
            key: "q".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("the app is held");
    for err in [&read_err, &key_err] {
        assert!(
            err.message
                .starts_with("no snapshot from the app yet: the app is held by another bridge. ")
                && err.message.contains("The socket path is right")
                && err.message.contains(HELD_ON_30_S)
                && !err.message.contains("may have exited"),
            "{}",
            err.message
        );
    }
}

/// The restart race: the app restarts, two bridges reconnect, and the other
/// one wins. This one waits in the new instance's backlog holding the
/// `Disconnected` record of the instance it served, and "it may have exited"
/// was its answer, about an app that was up and serving the winner.
#[tokio::test]
async fn the_bridge_that_loses_a_restart_race_says_the_app_is_held() {
    let (_dir, path) = test_socket_path("restart-race");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());
    let app = FakeApp::accept(&listener, Snapshot::new(3, demo_root("before-restart"))).await;
    wait_for_snapshot(&mut handle.state_rx, 3).await;

    // The restarted instance binds the path and serves another bridge, so it
    // never accepts this one.
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let _restarted = UnixListener::bind(&path).expect("rebind fake app socket");
    drop(app);
    wait_for_disconnect(&mut handle.state_rx).await;
    wait_for_link_open(&handle).await;

    let err = timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("the app is held");
            if err.message.contains("held by another bridge") {
                return err;
            }
            // Until the silence outlasts the grace, the connection is young,
            // and the answer says that much. It said the app may have exited
            // before 0.3.2, about an app that was up and serving the winner.
            assert!(
                err.message.starts_with(
                    "app 'fake-app' disconnected (last snapshot seq 3), and this bridge \
                     reconnected to the socket "
                ) && err.message.contains("worth one retry")
                    && !err.message.contains("may have exited"),
                "{}",
                err.message
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a reconnect nobody accepts should be diagnosed");
    assert!(
        err.message.starts_with(
            "app 'fake-app' disconnected (last snapshot seq 3), and the app is now held by \
             another bridge. The socket path is right"
        ) && err.message.contains("lets go of the app after 60 s")
            && !err.message.contains("may have exited"),
        "{}",
        err.message
    );
    let act_err = server
        .act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
        .await
        .expect_err("the app is held");
    assert!(
        act_err.message.contains("held by another bridge"),
        "{}",
        act_err.message
    );
}

/// The words a bridge that had the app gets from a restarted instance that has
/// greeted it and published nothing yet, here about the fake app's seq 4.
const GREETED_AFTER_RESTART: &str = "app 'fake-app' disconnected (last snapshot seq 4), and this \
bridge has reconnected: the app completed the taria handshake (protocol version 1) and has not \
published a tree on this connection yet. The socket path is right and the app is running, so \
nothing needs restarting; retry, and the first snapshot it publishes is served.";

/// An app restarts, and its new instance accepts the bridge's reconnect and
/// greets it before it has drawn. A bridge that never had the app is told the
/// handshake completed; this one, which had, was told the app may have
/// exited, while it was running and connected. Both tool kinds now say what
/// happened, and the first tree the new instance publishes is served.
#[tokio::test]
async fn a_restarted_app_that_greeted_and_has_not_drawn_is_not_reported_gone() {
    let (_dir, path) = test_socket_path("greeted-restart");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path.clone(), Duration::from_secs(30));
    let first = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(4, demo_root("first-life"))).await;
        (listener, app)
    });
    server.read_tree().await.expect("the first instance's tree");
    let (listener, app) = first.await.expect("fake app task");

    // The new instance is listening before the old one's connection ends, so
    // the bridge's reconnect reaches it, and it greets and draws nothing.
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let restarted = UnixListener::bind(&path).expect("rebind fake app socket");
    let (greeted_tx, greeted_rx) = tokio::sync::oneshot::channel();
    let second = tokio::spawn(async move {
        let (stream, _addr) = timeout(WAIT, restarted.accept())
            .await
            .expect("the bridge should reconnect")
            .expect("accept");
        let mut app = FakeApp::from_stream(stream);
        app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
            .await;
        let _ = greeted_tx.send(());
        (restarted, app)
    });
    drop(app);
    timeout(WAIT, greeted_rx)
        .await
        .expect("the restarted app should greet the bridge")
        .expect("the fake app task");

    let read_err = server
        .read_tree()
        .await
        .expect_err("a greeting is not a tree");
    let key_err = server
        .key(Parameters(KeyParams {
            key: "x".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("nothing to send a key against");
    for err in [&read_err, &key_err] {
        assert_eq!(err.message, GREETED_AFTER_RESTART);
    }

    let (_restarted, mut app) = second.await.expect("fake app task");
    app.send(&AppToBridge::Snapshot(Snapshot::new(
        1,
        demo_root("second-life"),
    )))
    .await;
    let tree = timeout(WAIT, async {
        loop {
            match server.read_tree().await {
                Ok(tree) => return tree,
                Err(err) => assert_eq!(err.message, GREETED_AFTER_RESTART),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first frame should be served");
    assert!(result_text(&tree).contains("second-life"), "{tree:?}");
}

/// A bridge that had the app reconnects into a restarted instance that
/// accepts it late. Every answer on the way names what the connection is
/// doing, in the order it does it: young, then held once the silence
/// outlasts the grace, then greeted once the app accepts it, then the tree.
/// None of them says the app may have exited: the connect succeeded, so
/// something is listening at the path.
#[tokio::test]
async fn a_reconnect_the_app_accepts_late_is_never_reported_gone() {
    let (_dir, path) = test_socket_path("late-accept");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let mut handle = bridge::spawn(path.clone());
    let server = TariaMcpServer::new(handle.clone());
    let app = FakeApp::accept(&listener, Snapshot::new(4, demo_root("first-life"))).await;
    wait_for_snapshot(&mut handle.state_rx, 4).await;

    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let restarted = UnixListener::bind(&path).expect("rebind fake app socket");
    drop(app);
    wait_for_disconnect(&mut handle.state_rx).await;
    wait_for_link_open(&handle).await;

    let opening = "app 'fake-app' disconnected (last snapshot seq 4), ";
    let mut seen = Vec::new();
    let mut ask = async |want: &str| {
        timeout(WAIT, async {
            loop {
                let err = server.read_tree().await.expect_err("no tree yet");
                assert!(
                    err.message.starts_with(opening) && !err.message.contains("may have exited"),
                    "{}",
                    err.message
                );
                let kind = if err.message.contains("worth one retry") {
                    "young"
                } else if err.message.contains("held by another bridge") {
                    "held"
                } else if err.message == GREETED_AFTER_RESTART {
                    "greeted"
                } else {
                    panic!("an answer of no known kind: {}", err.message);
                };
                if seen.last() != Some(&kind) {
                    seen.push(kind);
                }
                if kind == want {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never answered {want}; answered {seen:?}"));
    };
    ask("held").await;

    let (stream, _addr) = timeout(WAIT, restarted.accept())
        .await
        .expect("the reconnect is in the backlog")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .await;
    ask("greeted").await;
    drop(ask);
    // The first answer is young unless the call came a grace after the
    // reconnect, which only a badly stalled host does.
    assert!(
        seen == ["young", "held", "greeted"] || seen == ["held", "greeted"],
        "{seen:?}"
    );

    app.send(&AppToBridge::Snapshot(Snapshot::new(1, demo_root("late"))))
        .await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    let tree = server.read_tree().await.expect("the tree is served");
    assert!(result_text(&tree).contains("late"), "{tree:?}");
}

/// An app that leaves while the bridge holds it past its window for an ack
/// ends the hold, and the bridge does not reconnect: the window has closed,
/// so a reconnect would take the app again for a session that is not using
/// it, only to let go of it at once. The next call reconnects as ever.
#[tokio::test]
async fn an_app_that_leaves_during_the_ack_hold_is_not_reconnected_to() {
    let (_dir, path) = test_socket_path("leaves-in-hold");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, SHORT_IDLE);
    let app = tokio::spawn(async move {
        let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("leaving")))
            .await
            .never_acking();
        app.recv_input().await;
        // The call ends half a second after the send and its window 300 ms
        // after that, so this is well past the window and well inside the
        // 30 s hold.
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(app);
        let again = timeout(Duration::from_secs(1), listener.accept()).await;
        (listener, again.is_ok())
    });

    server
        .key(Parameters(KeyParams {
            key: "x".to_string(),
            repeat: None,
        }))
        .await
        .expect("an unanswered input is not a failed call");
    let (listener, reconnected) = app.await.expect("fake app task");
    assert!(
        !reconnected,
        "the bridge reconnected to an app it had no tool call for"
    );

    let app = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("next-call"))).await;
        (listener, app)
    });
    let tree = server.read_tree().await.expect("the next call reconnects");
    assert!(result_text(&tree).contains("next-call"), "{tree:?}");
    drop(app.await.expect("fake app task"));
}

/// A release is demoted like a disconnect, so when the app has really gone by
/// the next call, that call says so in the words every departure gets, byte
/// for byte, and at once: the connect that finds nothing listening answers it.
#[tokio::test]
async fn after_a_release_an_app_that_has_gone_is_reported_as_before() {
    let (_dir, path) = test_socket_path("gone-after-release");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path.clone(), SHORT_IDLE);
    let app = tokio::spawn(async move {
        let mut app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("leaving"))).await;
        app.wait_closed().await;
        listener
    });
    server.read_tree().await.expect("the app is there");
    let listener = app
        .await
        .expect("the bridge should let go after the window");

    // The app exits while the bridge is let go.
    drop(listener);
    std::fs::remove_file(&path).expect("remove the app's socket file");

    let err = server.read_tree().await.expect_err("the app is gone");
    assert_eq!(
        err.message,
        "app 'fake-app' disconnected (last snapshot seq 1); it may have exited. The bridge \
         reconnects automatically; retry once the app is back."
    );
}

/// A tree over the line limit is a reason that outlives the connection it
/// arrived on, because the app resends it to every bridge. Letting go is not
/// evidence of anything about the app, so the reason survives it exactly as it
/// survives a reconnect. Here the app greets the call after the release and
/// publishes nothing, so the reason is the only thing that can name the size.
#[tokio::test]
async fn the_oversized_reason_survives_a_release() {
    let (_dir, path) = test_socket_path("oversized-release");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, Duration::from_secs(1));
    let (released_tx, released_rx) = tokio::sync::oneshot::channel();
    let app = tokio::spawn(async move {
        let accept = || async {
            let (stream, _addr) = timeout(WAIT, listener.accept())
                .await
                .expect("bridge should connect")
                .expect("accept");
            FakeApp::from_stream(stream)
        };
        // The first call's connection: a tree over the limit.
        let mut first = accept().await;
        first
            .send(&AppToBridge::hello("big-app", PROTOCOL_VERSION))
            .await;
        first.send_oversized(1).await;
        // The reconnect after it, held silent until the bridge lets go.
        let mut second = accept().await;
        second.wait_closed().await;
        let _ = released_tx.send(());
        // The next call's connection: a greeting and no tree.
        let mut third = accept().await;
        third
            .send(&AppToBridge::hello("big-app", PROTOCOL_VERSION))
            .await;
        (first, third)
    });

    let err = server.read_tree().await.expect_err("no tree fits");
    assert!(err.message.contains(OVER_THE_LIMIT), "{}", err.message);
    timeout(WAIT, released_rx)
        .await
        .expect("the bridge should let go after the window")
        .expect("the fake app task");

    let err = server.read_tree().await.expect_err("still no tree");
    assert!(
        err.message.starts_with("no snapshot from the app yet")
            && err.message.contains(OVER_THE_LIMIT)
            && err.message.contains("The socket path is right"),
        "the release must not forget the size: {}",
        err.message
    );
    assert!(
        !err.message.contains("completed the taria handshake"),
        "{}",
        err.message
    );
    drop(app.await.expect("fake app task"));
}

/// A peer on another protocol version cannot parse this bridge's inputs. The
/// version is learned from each connection's `hello` and forgotten when the
/// connection ends, a release included, so the first call after a release has
/// to wait for the new `hello` before it decides, or it sends to a peer that
/// cannot read it.
#[tokio::test]
async fn a_protocol_mismatch_still_refuses_input_on_the_first_call_after_a_release() {
    let (_dir, path) = test_socket_path("mismatch-release");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, SHORT_IDLE);
    let other = PROTOCOL_VERSION + 1;
    let (mut closed, mut inputs) = serve_generations(listener, other);
    let act = || {
        server.act(Parameters(ActParams {
            node: "btn".to_string(),
            action: "activate".to_string(),
            value: None,
        }))
    };

    server.read_tree().await.expect("snapshots parse across it");
    let err = act().await.expect_err("a peer that cannot read input");
    assert!(err.message.contains(&format!("protocol version {other}")));
    assert_eq!(wait_released(&mut closed).await, 1);

    let err = act()
        .await
        .expect_err("the first call after a release refuses too");
    assert!(
        err.message.contains(&format!("protocol version {other}"))
            && err
                .message
                .contains(&format!("bridge speaks {PROTOCOL_VERSION}")),
        "{}",
        err.message
    );
    assert!(
        inputs.try_recv().is_err(),
        "nothing may reach a peer that cannot parse it"
    );
}

/// `--idle-release 0`: once connected, the bridge keeps the app for the rest
/// of the session, the way every bridge before 0.3.2 did. The connection
/// stays open for five idle windows of the other tests.
#[tokio::test]
async fn a_zero_idle_window_never_lets_go() {
    let (_dir, path) = test_socket_path("never-release");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, Duration::ZERO);
    let app = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(1, demo_root("kept"))).await;
        (listener, app)
    });
    server.read_tree().await.expect("the first call connects");
    let (_listener, mut app) = app.await.expect("fake app task");

    app.expect_no_input(SHORT_IDLE * 5).await;
    let tree = server.read_tree().await.expect("still connected");
    assert!(result_text(&tree).contains("kept"), "{tree:?}");
}

/// A bridge that found no app keeps retrying while its window is open, with a
/// backoff that grows to two seconds. A call that comes meanwhile does not sit
/// out the rest of the pause: it reconnects at once, so an app that came up
/// in between answers that call.
///
/// The app here accepts and drops every connection until the test lets it
/// serve, which the bridge backs off from exactly as from a failed connect,
/// and which, unlike a failed connect, the test can time. The serving connect
/// must come sooner after the last dropped one than the pause that followed
/// it, which a pause that ran out cannot do however a loaded host stretches
/// the schedule: the next attempt comes at least the whole pause after the
/// accept before it. So this cannot pass without the cut, and only a host
/// stalled for most of a second can fail it with the cut in place.
#[tokio::test]
async fn a_call_cuts_the_reconnect_backoff_short() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_dir, path) = test_socket_path("backoff-cut");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path, Duration::from_secs(30));
    let serve = Arc::new(AtomicBool::new(false));
    let (attempt_tx, mut attempts) = mpsc::unbounded_channel();
    let app = tokio::spawn({
        let serve = Arc::clone(&serve);
        async move {
            loop {
                let (stream, _addr) = listener.accept().await.expect("accept");
                let at = Instant::now();
                let serving = serve.load(Ordering::SeqCst);
                let _ = attempt_tx.send((at, serving));
                if serving {
                    let mut app = FakeApp::from_stream(stream);
                    app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
                        .await;
                    app.send(&AppToBridge::Snapshot(Snapshot::new(
                        1,
                        demo_root("came-up"),
                    )))
                    .await;
                    return (listener, app);
                }
            }
        }
    });

    server
        .read_tree()
        .await
        .expect_err("the app drops the connection");
    // Three dropped attempts, so the bridge is in the pause after the third:
    // 250 ms after the first, 500 ms after the second, 1 s after the third.
    let mut all = Vec::new();
    for _ in 0..3 {
        let attempt = timeout(WAIT, attempts.recv())
            .await
            .expect("the bridge should retry")
            .expect("the fake app task");
        all.push(attempt);
    }
    serve.store(true, Ordering::SeqCst);
    // The accept is reported before the bridge has read the drop, and a call
    // begun before that is answered by it, as any outcome after a call began
    // answers the call. A fifth of the pause lets it land first; a retry
    // covers a host slower than that, and cuts the pause just the same.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let tree = timeout(WAIT, async {
        loop {
            if let Ok(tree) = server.read_tree().await {
                return tree;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the call reconnects at once and finds the app");
    assert!(result_text(&tree).contains("came-up"), "{tree:?}");
    drop(app.await.expect("fake app task"));

    // Every attempt, in order: the ones read above, any a stalled test let
    // the schedule add, and last the one that was served.
    while let Ok(attempt) = attempts.try_recv() {
        all.push(attempt);
    }
    let Some(((served_at, true), dropped)) = all.split_last() else {
        panic!("the last attempt should be the served one: {all:?}");
    };
    let Some((last_dropped, false)) = dropped.last() else {
        panic!("only the last attempt is served: {all:?}");
    };
    let pause =
        (Duration::from_millis(250) * (1 << (dropped.len() - 1))).min(Duration::from_secs(2));
    let gap = served_at.duration_since(*last_dropped);
    assert!(
        gap < pause,
        "the serving connect came {gap:?} after the last dropped one, which the {pause:?} pause \
         after it could have run out to: the call did not cut it short"
    );
}

/// Issue #16. A bridge that had the app reconnects to a restarted instance
/// that greets it and then publishes a snapshot this bridge cannot parse (a
/// tree past what the JSON parser recurses into, or a taria whose snapshot
/// shape differs, after a change the adopter just made). The answer was the
/// greeted one, "retry, and the first snapshot it publishes is served", on
/// every call, about an app that had published and been thrown away. The
/// reason is quoted instead, as it is for a bridge that never had a tree.
#[tokio::test]
async fn a_restarted_app_whose_tree_cannot_be_read_is_named_not_retried() {
    let (_dir, path) = test_socket_path("rejected-restart");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path.clone(), Duration::from_secs(30));
    let first = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(4, demo_root("first-life"))).await;
        (listener, app)
    });
    server.read_tree().await.expect("the first instance's tree");
    let (listener, app) = first.await.expect("fake app task");

    // The new instance is listening before the old one's connection ends, so
    // the bridge's reconnect reaches it.
    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let restarted = UnixListener::bind(&path).expect("rebind fake app socket");
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
    let second = tokio::spawn(async move {
        let (stream, _addr) = timeout(WAIT, restarted.accept())
            .await
            .expect("the bridge should reconnect")
            .expect("accept");
        let mut app = FakeApp::from_stream(stream);
        app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
            .await;
        app.send_raw(r#"{"type":"snapshot","seq":"not a number","root":{}}"#)
            .await;
        let _ = sent_tx.send(());
        (restarted, app)
    });
    drop(app);
    timeout(WAIT, sent_rx)
        .await
        .expect("the restarted app should send its line")
        .expect("the fake app task");

    let opening = "app 'fake-app' disconnected (last snapshot seq 4), and this bridge has \
                   reconnected: the app sent lines on the new connection, and this bridge could \
                   not read the last one (invalid type: string \"not a number\", expected u64";
    // The manager reads the line on its own schedule; until it has, the
    // greeting is the answer.
    let err = timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("no tree can be read");
            if err.message.starts_with(opening) {
                return err;
            }
            assert_eq!(err.message, GREETED_AFTER_RESTART);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the unreadable line should be named");
    assert!(
        err.message.ends_with(
            "). The socket path is right, so the line is what is wrong. A snapshot more than 32 \
             levels deep exceeds what a JSON parser will recurse into and fails exactly like \
             this; so does an app built against a taria whose snapshot shape differs from this \
             bridge's. Fix the tree the app publishes, or match its taria dependency to this \
             bridge's; the bridge keeps reading and needs no restart."
        ),
        "{}",
        err.message
    );
    // Every call says so, the input tools included, until the app publishes
    // something readable.
    let again = server.read_tree().await.expect_err("still unreadable");
    let key_err = server
        .key(Parameters(KeyParams {
            key: "x".to_string(),
            repeat: None,
        }))
        .await
        .expect_err("nothing to send a key against");
    assert_eq!(again.message, err.message);
    assert_eq!(key_err.message, err.message);

    let (_restarted, mut app) = second.await.expect("fake app task");
    app.send(&AppToBridge::Snapshot(Snapshot::new(1, demo_root("fixed"))))
        .await;
    let tree = timeout(WAIT, async {
        loop {
            if let Ok(tree) = server.read_tree().await {
                return tree;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a readable snapshot is served as usual");
    assert!(result_text(&tree).contains("fixed"), "{tree:?}");
}

/// The other half of the fix for issue #16. A line the bridge could not read
/// is quoted with a connection open because it came from that connection:
/// the bridge forgets every reason at a connect. Here the app's first
/// instance sends a bad line partway through its session and then restarts,
/// and the instance that greets the reconnect has sent nothing wrong. Quoting
/// the old reason would blame it for its predecessor's line; it gets the
/// greeted answer, word for word.
#[tokio::test]
async fn a_line_an_earlier_connection_sent_is_not_held_against_the_next() {
    let (_dir, path) = test_socket_path("stale-rejected");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let server = on_demand(path.clone(), Duration::from_secs(30));
    let first = tokio::spawn(async move {
        let app = FakeApp::accept(&listener, Snapshot::new(3, demo_root("first-life"))).await;
        (listener, app)
    });
    server.read_tree().await.expect("the first instance's tree");
    let (listener, mut app) = first.await.expect("fake app task");
    // Mid-session, a line the bridge cannot read, then a tree it can. Lines
    // are read in order, so once the second tree is served the bad line has
    // been read and its reason recorded.
    app.send_raw(r#"{"type":"snapshot","seq":"x"}"#).await;
    app.send(&AppToBridge::Snapshot(Snapshot::new(
        4,
        demo_root("after-a-bad-line"),
    )))
    .await;
    timeout(WAIT, async {
        loop {
            let tree = server.read_tree().await.expect("a tree is being served");
            if result_text(&tree).contains("after-a-bad-line") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the tree after the bad line should be served");

    drop(listener);
    std::fs::remove_file(&path).expect("remove old socket file");
    let restarted = UnixListener::bind(&path).expect("rebind fake app socket");
    let (greeted_tx, greeted_rx) = tokio::sync::oneshot::channel();
    let second = tokio::spawn(async move {
        let (stream, _addr) = timeout(WAIT, restarted.accept())
            .await
            .expect("the bridge should reconnect")
            .expect("accept");
        let mut app = FakeApp::from_stream(stream);
        app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
            .await;
        let _ = greeted_tx.send(());
        (restarted, app)
    });
    drop(app);
    timeout(WAIT, greeted_rx)
        .await
        .expect("the restarted app should greet the bridge")
        .expect("the fake app task");

    for _ in 0..3 {
        let err = server
            .read_tree()
            .await
            .expect_err("a greeting is not a tree");
        assert_eq!(err.message, GREETED_AFTER_RESTART);
    }
    drop(second.await.expect("fake app task"));
}

/// The same forgetting, for a bridge that never had a tree: an app whose
/// line it could not read went away, and the next connection is greeted. Up
/// to 0.3.2 the old reason outranked that greeting for good, since nothing
/// but a parsed snapshot retired it; now the greeting is the answer, and the
/// reason was the answer only while it was the last thing that happened.
#[tokio::test]
async fn a_bridge_that_never_had_a_tree_forgets_the_last_connections_line() {
    let (_dir, listener, _handle, server) = setup("never-stale-rejected").await;
    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("bridge should connect")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .await;
    app.send_raw(r#"{"type":"snapshot","seq":"x"}"#).await;
    timeout(WAIT, async {
        loop {
            let err = server.read_tree().await.expect_err("nothing readable");
            if err.message.contains("could not read the last one") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the bad line should be named while it is the last thing that happened");

    // The app leaves, and the bridge's reconnect is greeted.
    drop(app);
    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("bridge should reconnect")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .await;
    let err = timeout(WAIT, async {
        loop {
            let err = server
                .read_tree()
                .await
                .expect_err("a greeting is not a tree");
            if err.message.contains("completed the taria handshake") {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the greeting should be the answer once the new connection has it");
    assert!(
        err.message
            .starts_with("no snapshot from the app yet, and the socket is not what is wrong")
            && !err.message.contains("could not read"),
        "{}",
        err.message
    );
    drop(app);
}

/// A bridge that let go of the app after its idle window comes back to find
/// another session holding it. Its answer opens with the departure every
/// disconnect gets, and agents read that as a fault and went looking for it;
/// so after the phrases callers match on, it says the departure was this
/// bridge letting go on purpose, and when to retry.
#[tokio::test]
async fn a_bridge_that_let_go_says_its_disconnect_was_on_purpose() {
    let (_dir, path) = test_socket_path("released-held");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let first = on_demand(path.clone(), SHORT_IDLE);
    let other = on_demand(path, Duration::from_secs(30));
    let (mut closed, _inputs) = serve_generations(listener, PROTOCOL_VERSION);

    let tree = first
        .read_tree()
        .await
        .expect("the first bridge gets the app");
    assert!(result_text(&tree).contains("gen-1"), "{tree:?}");
    assert_eq!(wait_released(&mut closed).await, 1);
    let tree = other.read_tree().await.expect("the other session takes it");
    assert!(result_text(&tree).contains("gen-2"), "{tree:?}");

    let err = first
        .read_tree()
        .await
        .expect_err("the other session holds the app");
    assert_eq!(
        err.message,
        "app 'fake-app' disconnected (last snapshot seq 1), and the app is now held by another \
         bridge. That disconnect was this bridge letting go on purpose after 0.3 s with no tool \
         calls, or up to 30 s later while an input awaited its ack, so another session could use \
         the app; it is not a fault. A holder started like this bridge lets go the same way, \
         and this bridge takes over as soon as it does, with no restart; retry in 0.3 s. If the \
         app stays held, another session is using it, or the holder is a taria-mcp older than \
         0.3.2, which holds the app until its session ends."
    );
}

/// A bridge that let go of the app, and then found nothing listening at the
/// path, no longer calls the disconnect its own: the app it let go of has
/// gone since. Before, a new instance held by another session got the
/// released answer, "it is not a fault", about a departure that by then
/// included the app's exit. Both "it may have exited" answers in between are
/// as they always were.
#[tokio::test]
async fn a_bridge_that_let_go_drops_the_claim_once_the_app_is_gone() {
    let (_dir, path) = test_socket_path("released-gone");
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let first = on_demand(path.clone(), SHORT_IDLE);

    let (tree, app) = tokio::join!(
        first.read_tree(),
        FakeApp::accept(&listener, Snapshot::new(1, demo_root("gen-1")))
    );
    assert!(
        result_text(&tree.expect("the bridge gets the app")).contains("gen-1"),
        "the first tree"
    );
    // The bridge lets go after its window, and the app sees the close.
    let mut app = app;
    app.wait_closed().await;

    // The app exits and takes its socket with it.
    drop(app);
    drop(listener);
    std::fs::remove_file(&path).expect("remove the socket file");
    for ask in 1..=2 {
        let err = first.read_tree().await.expect_err("nothing is listening");
        assert_eq!(
            err.message,
            "app 'fake-app' disconnected (last snapshot seq 1); it may have exited. The bridge \
             reconnects automatically; retry once the app is back.",
            "ask {ask}"
        );
    }

    // A new instance, taken by a bridge that never lets go.
    let listener = UnixListener::bind(&path).expect("bind the new instance's socket");
    let (_closed, _inputs) = serve_generations(listener, PROTOCOL_VERSION);
    let holder = on_demand(path, Duration::ZERO);
    let tree = holder.read_tree().await.expect("the holder takes the app");
    assert!(result_text(&tree).contains("gen-1"), "{tree:?}");

    let err = timeout(WAIT, async {
        loop {
            let err = first.read_tree().await.expect_err("the holder has the app");
            if !err.message.contains("worth one retry") {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a connection nobody accepts should be diagnosed");
    assert!(
        err.message.starts_with(
            "app 'fake-app' disconnected (last snapshot seq 1), and the app is now held by \
             another bridge. The socket path is right: this bridge has been connected for "
        ) && !err.message.contains("on purpose"),
        "{}",
        err.message
    );
}

/// Accept the bridge's connection and publish `initial` with no `hello`: a
/// hand-written peer that skips the handshake.
async fn accept_without_hello(listener: &UnixListener, initial: Snapshot) -> FakeApp {
    let (stream, _addr) = timeout(WAIT, listener.accept())
        .await
        .expect("bridge should connect")
        .expect("accept");
    let mut app = FakeApp::from_stream(stream);
    app.send(&AppToBridge::Snapshot(initial)).await;
    app
}

/// `initial` as an app on `protocol_version` would publish it.
fn on_version(protocol_version: u32, mut initial: Snapshot) -> Snapshot {
    initial.protocol_version = protocol_version;
    initial
}

/// The `act` every test below sends.
fn activate_btn() -> Parameters<ActParams> {
    Parameters(ActParams {
        node: "btn".to_string(),
        action: "activate".to_string(),
        value: None,
    })
}

/// A peer that never sends `hello` used to pass the version check, since
/// only the handshake set the version and none read as "no mismatch known",
/// so a peer on another version was sent input it cannot parse. Every
/// snapshot states its version, and until a `hello` arrives the first
/// snapshot's is the version the bridge goes by.
#[tokio::test]
async fn an_app_that_never_greets_is_held_to_the_version_its_snapshot_states() {
    let (_dir, listener, mut handle, server) = setup("no-hello-other").await;
    let other = PROTOCOL_VERSION + 1;
    let mut app = accept_without_hello(
        &listener,
        on_version(other, Snapshot::new(1, demo_root("no-hello"))),
    )
    .await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;

    let tree = server.read_tree().await.expect("the snapshot still parses");
    assert!(result_text(&tree).contains("no-hello"), "{tree:?}");
    let err = server
        .act(activate_btn())
        .await
        .expect_err("a peer on another version cannot read input");
    assert!(
        err.message.starts_with(&format!(
            "the app speaks taria protocol version {other}, this bridge speaks {PROTOCOL_VERSION}"
        )),
        "{}",
        err.message
    );
    app.expect_no_input(Duration::from_millis(300)).await;
}

/// The same peer on this bridge's version is served input as any other.
#[tokio::test]
async fn an_app_that_never_greets_on_this_version_is_sent_input() {
    let (_dir, listener, mut handle, server) = setup("no-hello-same").await;
    let app = accept_without_hello(&listener, Snapshot::new(1, demo_root("no-hello"))).await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    let echo = tokio::spawn(async move {
        let mut app = app;
        let (_id, input) = app.recv_input().await;
        assert!(matches!(input, AgentInput::Act { .. }), "{input:?}");
        app
    });
    let result = server.act(activate_btn()).await.expect("the act is sent");
    assert!(
        result_text(&result).contains("The app received this input"),
        "{}",
        result_text(&result)
    );
    drop(echo.await.expect("fake app task"));
}

/// A `hello` that comes after a snapshot is the peer's word on its version,
/// and replaces the one its snapshot stated, either way round.
#[tokio::test]
async fn a_hello_after_a_snapshot_replaces_the_version_it_stated() {
    let (_dir, listener, mut handle, server) = setup("late-hello").await;
    let other = PROTOCOL_VERSION + 1;
    let mut app = accept_without_hello(
        &listener,
        on_version(other, Snapshot::new(1, demo_root("late-hello"))),
    )
    .await;
    wait_for_snapshot(&mut handle.state_rx, 1).await;
    server
        .act(activate_btn())
        .await
        .expect_err("the snapshot said another version");

    app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .await;
    let mut protocol = handle.protocol_rx.clone();
    timeout(WAIT, protocol.wait_for(|v| *v == Some(PROTOCOL_VERSION)))
        .await
        .expect("the hello should be read")
        .expect("the bridge task is alive");
    let echo = tokio::spawn(async move {
        let (_id, input) = app.recv_input().await;
        assert!(matches!(input, AgentInput::Act { .. }), "{input:?}");
        app
    });
    server
        .act(activate_btn())
        .await
        .expect("the hello said this version");
    let mut app = echo.await.expect("fake app task");

    // And the other way round: a hello naming another version outranks a
    // snapshot that named this one.
    app.send(&AppToBridge::hello("fake-app", other)).await;
    timeout(WAIT, protocol.wait_for(|v| *v == Some(other)))
        .await
        .expect("the hello should be read")
        .expect("the bridge task is alive");
    let err = server
        .act(activate_btn())
        .await
        .expect_err("the hello said another version");
    assert!(
        err.message.contains(&format!("protocol version {other}")),
        "{}",
        err.message
    );
}
