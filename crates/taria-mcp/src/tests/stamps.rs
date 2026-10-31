//! Stamped snapshots (issue #17): a fake taria app on one side, the bridge and
//! the tool methods on the other, playing the orders of acks and trees a slow
//! app produces.
//!
//! A peer that says `stamps` in its hello marks every tree with the newest
//! input it had dequeued, and the bridge credits a tree to an input only once
//! the stamp reaches it. These tests hold the three things that buys: a slow
//! handler's tree reaches its own input's answer, an input that changes
//! nothing is answered at once, and a tree that lands late is never credited
//! to the input after it. A peer that does not say it stamps is answered as
//! every peer was before, word for word.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::bridge::{self, BridgeHandle, BridgeState};
use crate::server::{ActParams, KeyParams, Tools};
use crate::tree_format::TreeFormat;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

/// Generous bound for local socket round trips.
const WAIT: Duration = Duration::from_secs(5);

/// How long the slow handler in these tests takes: past the 500 ms an
/// unstamped peer is given, short of the 3 s a stamped one is.
const SLOW: Duration = Duration::from_millis(800);

/// The answer to an input a stamped peer took and changed nothing for.
const UNCHANGED: &str = "The app received this input, and its tree did not change.";

/// The answer to an input a stamped peer took and published nothing after.
const NO_TREE_AFTER: &str = "The app received this input but published no tree after it within \
                             3000ms; it may still be working. Call read_tree to see its effect.";

/// The answers an unstamped peer gets, as 0.3.3 gave them.
const UNCHANGED_UNSTAMPED: &str = "The app received this input, and its tree did not change \
                                   within 500ms. Call read_tree if you expect a delayed effect.";
const SILENT_UNSTAMPED: &str = "The app neither acknowledged this input nor changed its tree \
                                within 500ms. It may not report acknowledgements, or it may \
                                not have reacted yet; call read_tree to re-check.";

/// A socket path for `name` in its own self-cleaning directory. Keep the
/// returned guard alive for the duration of the test.
fn test_socket_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("taria-mcp-stamps-")
        .tempdir()
        .expect("create test socket dir");
    let path = dir.path().join(format!("{name}.sock"));
    (dir, path)
}

/// An `app` root whose label marks which state it is in, with one button.
fn tree(marker: &str) -> Node {
    Node::new("app", Role::App).label(marker).child(
        Node::new("btn", Role::Button)
            .label("Go")
            .action(Action::Activate),
    )
}

/// A snapshot of [`tree`], stamped at `stamp` when there is one.
fn snapshot(seq: u64, marker: &str, stamp: Option<InputId>) -> Snapshot {
    let snapshot = Snapshot::new(seq, tree(marker));
    match stamp {
        Some(id) => snapshot.after(id),
        None => snapshot,
    }
}

/// The app half of one accepted connection. It answers nothing on its own:
/// each test plays its own order of acks and trees.
struct FakeApp {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl FakeApp {
    /// Accept the bridge, say hello (stamping or not), and send `initial`.
    async fn accept(listener: &UnixListener, stamps: bool, initial: Snapshot) -> Self {
        let (stream, _addr) = timeout(WAIT, listener.accept())
            .await
            .expect("bridge should connect")
            .expect("accept");
        let (read, write) = stream.into_split();
        let mut app = Self {
            lines: BufReader::new(read).lines(),
            write,
        };
        app.send(&AppToBridge::hello("fake-app", PROTOCOL_VERSION).stamps(stamps))
            .await;
        app.send(&AppToBridge::Snapshot(initial)).await;
        app
    }

    async fn send(&mut self, msg: &AppToBridge) {
        let mut line = serde_json::to_string(msg).expect("serialize app message");
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .expect("write to bridge");
    }

    async fn ack(&mut self, id: InputId, status: InputStatus) {
        self.send(&AppToBridge::ack(id, status)).await;
    }

    async fn publish(&mut self, snapshot: Snapshot) {
        self.send(&AppToBridge::Snapshot(snapshot)).await;
    }

    /// The next input the bridge writes, unanswered.
    async fn next_input(&mut self) -> (InputId, AgentInput) {
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
        (id, input)
    }
}

/// Listener, bridge and tool layer for one test; hold the guard as `_dir`.
async fn setup(
    name: &str,
    format: TreeFormat,
) -> (tempfile::TempDir, UnixListener, BridgeHandle, Tools) {
    let (dir, path) = test_socket_path(name);
    let listener = UnixListener::bind(&path).expect("bind fake app socket");
    let handle = bridge::spawn(path);
    let server = Tools::with_tree_format(handle.clone(), format);
    (dir, listener, handle, server)
}

/// Wait until the bridge holds a tree of at least `min_seq`, and says
/// whether the peer stamps as `stamps`.
async fn wait_ready(handle: &BridgeHandle, min_seq: u64, stamps: bool) {
    let mut rx = handle.state_rx.clone();
    timeout(
        WAIT,
        rx.wait_for(|state| matches!(state, BridgeState::Connected(s) if s.parsed.seq >= min_seq)),
    )
    .await
    .expect("the tree should arrive")
    .expect("the bridge task is alive");
    let mut stamps_rx: watch::Receiver<bool> = handle.stamps_rx.clone();
    timeout(WAIT, stamps_rx.wait_for(|flag| *flag == stamps))
        .await
        .expect("the hello should be read")
        .expect("the bridge task is alive");
}

fn activate() -> Parameters<ActParams> {
    Parameters(ActParams {
        node: "btn".to_string(),
        action: "activate".to_string(),
        value: None,
    })
}

/// The text of a successful tool result's single content block.
fn text(result: &CallToolResult) -> &str {
    match result.content.first() {
        Some(ContentBlock::Text(text)) => &text.text,
        other => panic!("expected text content, got {other:?}"),
    }
}

/// The label of the root in a JSON tree answer.
fn root_label(answer: &str) -> String {
    let tree: serde_json::Value = serde_json::from_str(answer)
        .unwrap_or_else(|err| panic!("expected a JSON tree, got {answer:?}: {err}"));
    tree["root"]["label"]
        .as_str()
        .expect("the root has a label")
        .to_string()
}

/// The repro of issue #17. A handler slower than 500 ms used to read as
/// "did not change", with its tree credited to the next input. From a peer
/// that stamps, the tree comes back in the act's own answer, however late
/// within the window, and the same app without the stamp still gets the
/// answer it always got.
#[tokio::test]
async fn a_slow_stamped_tree_is_credited_to_its_own_act() {
    for stamps in [true, false] {
        let (_dir, listener, handle, server) =
            setup(&format!("slow-{stamps}"), TreeFormat::Json).await;
        let mut app = FakeApp::accept(&listener, stamps, snapshot(1, "before", None)).await;
        wait_ready(&handle, 1, stamps).await;

        let slow = tokio::spawn(async move {
            let (id, _) = app.next_input().await;
            app.ack(id, InputStatus::Delivered).await;
            sleep(SLOW).await;
            app.publish(snapshot(2, "after", stamps.then_some(id)))
                .await;
            app
        });
        let started = Instant::now();
        let result = server.act(activate()).await.expect("act succeeds");
        let took = started.elapsed();
        let _app = slow.await.expect("the fake app");

        if stamps {
            assert_eq!(root_label(text(&result)), "after", "{}", text(&result));
            assert!(
                took >= SLOW && took < Duration::from_secs(3),
                "answered after {took:?}"
            );
        } else {
            assert_eq!(text(&result), UNCHANGED_UNSTAMPED);
            assert!(
                took < SLOW,
                "an unstamped peer keeps the 500 ms window: {took:?}"
            );
        }
    }
}

/// The other half of the fix: a longer window must not make every no-op pay
/// it. A stamped tree that did not change answers at once, here within a
/// frame of the ack, far under the 500 ms an unstamped peer waits out.
#[tokio::test]
async fn an_unchanged_stamped_tree_is_answered_at_once() {
    let (_dir, listener, handle, server) = setup("unchanged", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        // The adapter's resend: the same tree under the same seq, stamped.
        app.publish(snapshot(1, "static", Some(id))).await;
        app
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let _app = echo.await.expect("the fake app");

    assert_eq!(text(&result), UNCHANGED);
    assert!(
        took < Duration::from_millis(500),
        "an unchanged stamped tree must not wait out a window: {took:?}"
    );
}

/// An app that takes the input and publishes nothing after it gets the full
/// window and then the answer that says so, not "did not change": nothing
/// stamped says it did not. Then the tree that input caused lands while the
/// next one is in flight, and becomes that input's baseline instead of its
/// answer. Credited, it would be issue #17 over again: the next input
/// changed nothing, and the agent would be told it did.
///
/// The unstamped run plays the same app and gets the misattribution 0.3.3
/// gave, which is what the stamp exists to remove.
#[tokio::test]
async fn a_late_tree_rebases_instead_of_being_credited_to_the_next_input() {
    for stamps in [true, false] {
        let (_dir, listener, handle, server) =
            setup(&format!("late-{stamps}"), TreeFormat::Json).await;
        let mut app = FakeApp::accept(&listener, stamps, snapshot(1, "before", None)).await;
        wait_ready(&handle, 1, stamps).await;

        // First input: taken, and nothing published within the window.
        let first = tokio::spawn(async move {
            let (id, _) = app.next_input().await;
            app.ack(id, InputStatus::Delivered).await;
            (app, id)
        });
        let started = Instant::now();
        let result = server.act(activate()).await.expect("act succeeds");
        let took = started.elapsed();
        let (mut app, first_id) = first.await.expect("the fake app");
        if stamps {
            assert_eq!(text(&result), NO_TREE_AFTER);
            assert!(
                took >= Duration::from_secs(3) && took < Duration::from_secs(4),
                "the cap is the one deadline: {took:?}"
            );
        } else {
            assert_eq!(text(&result), UNCHANGED_UNSTAMPED);
        }

        // Second input: the first one's tree lands in its window, then the
        // second is taken and changes nothing.
        let second = tokio::spawn(async move {
            let (id, _) = app.next_input().await;
            app.publish(snapshot(2, "after-first", stamps.then_some(first_id)))
                .await;
            // Apart, so the watch shows both trees rather than only the
            // newest: back to back is the gap the observer documents.
            sleep(Duration::from_millis(100)).await;
            app.ack(id, InputStatus::Delivered).await;
            app.publish(snapshot(2, "after-first", stamps.then_some(id)))
                .await;
            app
        });
        let result = server.act(activate()).await.expect("act succeeds");
        let _app = second.await.expect("the fake app");
        if stamps {
            assert_eq!(
                text(&result),
                UNCHANGED,
                "the late tree is the first input's, not the second's"
            );
        } else {
            assert_eq!(
                root_label(text(&result)),
                "after-first",
                "an unstamped peer cannot tell, as before"
            );
        }
    }
}

/// A burst is answered on its last press. A tree stamped partway through is
/// the burst's effect so far, so it neither answers nor moves the baseline:
/// here the first press moves the tree and the rest change nothing, and the
/// answer is still the moved tree, because the burst moved it. And a drop in
/// the middle is still reported as a partial burst, stamps or not.
#[tokio::test]
async fn a_burst_waits_for_its_last_stamp_and_reports_a_partial_drop() {
    let (_dir, listener, handle, server) = setup("burst", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "row-0", None)).await;
    wait_ready(&handle, 1, true).await;

    let presses = tokio::spawn(async move {
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(app.next_input().await.0);
        }
        app.ack(ids[0], InputStatus::Delivered).await;
        app.publish(snapshot(2, "row-1", Some(ids[0]))).await;
        sleep(Duration::from_millis(100)).await;
        app.ack(ids[1], InputStatus::Delivered).await;
        app.publish(snapshot(2, "row-1", Some(ids[1]))).await;
        // The last press is slow, past the 500 ms an unstamped peer is given.
        sleep(Duration::from_millis(600)).await;
        app.ack(ids[2], InputStatus::Delivered).await;
        app.publish(snapshot(2, "row-1", Some(ids[2]))).await;
        (app, ids)
    });
    let started = Instant::now();
    let result = server
        .key(Parameters(KeyParams {
            key: "down".to_string(),
            repeat: Some(3),
        }))
        .await
        .expect("key succeeds");
    let took = started.elapsed();
    let (mut app, ids) = presses.await.expect("the fake app");
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
    assert_eq!(root_label(text(&result)), "row-1", "{}", text(&result));
    assert!(
        took >= Duration::from_millis(700),
        "answered early: {took:?}"
    );

    let presses = tokio::spawn(async move {
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(app.next_input().await.0);
        }
        app.ack(ids[0], InputStatus::Delivered).await;
        app.ack(ids[1], InputStatus::Dropped).await;
        app.ack(ids[2], InputStatus::Delivered).await;
        app.publish(snapshot(3, "row-2", Some(ids[2]))).await;
        app
    });
    let err = server
        .key(Parameters(KeyParams {
            key: "down".to_string(),
            repeat: Some(3),
        }))
        .await
        .expect_err("a burst with a drop in it is a partial failure");
    let _app = presses.await.expect("the fake app");
    assert!(
        err.message
            .starts_with("the app dropped 1 of the 3 inputs this call sent"),
        "{}",
        err.message
    );
}

/// `Ignored` still ends the wait early. With the stamped tree behind it, at
/// that tree; with none coming, 500 ms after the ack, as an unstamped peer's
/// window would have, rather than the 3 s a delivered input waits.
#[tokio::test]
async fn an_ignored_ack_ends_the_wait_early() {
    let (_dir, listener, handle, server) = setup("ignored", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let refuse = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.ack(id, InputStatus::Ignored).await;
        app.publish(snapshot(1, "static", Some(id))).await;
        app
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let mut app = refuse.await.expect("the fake app");
    assert!(
        text(&result).starts_with("The app received this input and deliberately did nothing"),
        "{}",
        text(&result)
    );
    assert_eq!(
        root_label(text(&result).split_once('\n').expect("a tree").1),
        "static"
    );
    assert!(took < Duration::from_millis(500), "{took:?}");

    let refuse = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.ack(id, InputStatus::Ignored).await;
        app
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let _app = refuse.await.expect("the fake app");
    assert!(
        text(&result).starts_with("The app received this input and deliberately did nothing"),
        "{}",
        text(&result)
    );
    assert!(
        took >= Duration::from_millis(450) && took < Duration::from_millis(1_500),
        "an ignore with no tree after it waits 500 ms, not the 3 s cap: {took:?}"
    );
}

/// A peer whose hello does not say it stamps is answered byte for byte as
/// 0.3.3 answered it: the 500 ms window, its two texts, and a tree that
/// differs anywhere but in a stamp it never said it sends.
#[tokio::test]
async fn an_unstamped_peer_gets_the_answers_it_always_did() {
    let (_dir, listener, handle, server) = setup("unstamped", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, false, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, false).await;

    // Acked, nothing published.
    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let mut app = echo.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED_UNSTAMPED);
    assert!(
        took >= Duration::from_millis(450) && took < Duration::from_millis(1_500),
        "{took:?}"
    );

    // A stamp it never declared does not make the same tree a change.
    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(1, "static", Some(id))).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let mut app = echo.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED_UNSTAMPED);

    // Silence.
    let quiet = tokio::spawn(async move {
        app.next_input().await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let mut app = quiet.await.expect("the fake app");
    assert_eq!(text(&result), SILENT_UNSTAMPED);

    // And a tree that changed is the answer, as it always was.
    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(2, "moved", None)).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let _app = echo.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "moved");
}

/// Crediting compares ids, so the ids have to reach the app in the order
/// they were claimed. Sends racing on a multi-thread runtime, which claimed
/// the id and queued the input as two steps, could put a higher id on the
/// wire first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_keep_the_ids_in_order_on_the_wire() {
    const SENDS: usize = 200;
    let (_dir, listener, handle, _server) = setup("order", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let reader = tokio::spawn(async move {
        let mut ids = Vec::with_capacity(SENDS);
        while ids.len() < SENDS {
            ids.push(app.next_input().await.0);
        }
        ids
    });
    let senders: Vec<_> = (0..8)
        .map(|_| {
            let handle = handle.clone();
            tokio::spawn(async move {
                let mut sent = Vec::new();
                for _ in 0..SENDS / 8 {
                    let id = handle
                        .send(AgentInput::key("j"), WAIT)
                        .await
                        .expect("the send goes out");
                    sent.push(id);
                    tokio::task::yield_now().await;
                }
                sent
            })
        })
        .collect();
    let mut claimed = Vec::new();
    for sender in senders {
        claimed.extend(sender.await.expect("a sender"));
    }
    let wire = reader.await.expect("the fake app");
    assert_eq!(wire.len(), SENDS);
    assert!(
        wire.windows(2).all(|pair| pair[0] < pair[1]),
        "ids went out of order: {wire:?}"
    );
    claimed.sort();
    assert_eq!(claimed, wire, "every id claimed went out, once");
}

/// The stamp is the bridge's, like the `type` key: it reaches the agent in
/// neither format, from `read_tree` or from an input's answer.
#[tokio::test]
async fn the_stamp_is_stripped_from_every_tree_the_agent_reads() {
    for format in [TreeFormat::Json, TreeFormat::Outline] {
        let (_dir, listener, handle, server) =
            setup(&format!("strip-{}", format.name()), format).await;
        let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
        wait_ready(&handle, 1, true).await;

        let echo = tokio::spawn(async move {
            let (id, _) = app.next_input().await;
            app.ack(id, InputStatus::Delivered).await;
            app.publish(snapshot(2, "after", Some(id))).await;
            app
        });
        let answer = server.act(activate()).await.expect("act succeeds");
        let _app = echo.await.expect("the fake app");
        let read = server.read_tree().await.expect("a tree");

        for result in [&answer, &read] {
            let text = text(result);
            assert!(text.contains("after"), "{text}");
            assert!(!text.contains("after_input"), "{format:?}: {text}");
            match format {
                TreeFormat::Json => {
                    let tree: serde_json::Value = serde_json::from_str(text).expect("JSON");
                    assert_eq!(
                        tree.as_object()
                            .map(|tree| tree.keys().cloned().collect::<Vec<_>>()),
                        Some(vec![
                            "protocol_version".to_string(),
                            "root".to_string(),
                            "seq".to_string()
                        ]),
                        "{text}"
                    );
                }
                TreeFormat::Outline => {
                    assert!(text.starts_with("protocol_version=1 seq=2\n"), "{text}");
                }
            }
        }
    }
}
