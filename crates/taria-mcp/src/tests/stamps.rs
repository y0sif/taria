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
//!
//! Then every other way a stamped wait can end: a departure, a restart, a
//! drop, lost acks, silence, a line the bridge could not read, a burst
//! answered only in part, another call's input landing inside a burst, an
//! `Ignored` read behind the tree it refines, and a peer whose claim to stamp
//! is false.
//!
//! # Timing
//!
//! The bounds here are for slow, jittery CI runners, not for this machine. A
//! lower bound is a sleep the test itself took, which a runner can only
//! lengthen, so it holds anywhere. An upper bound is held to what proves the
//! point and no tighter: [`PROMPT`] for "answered early", which is half the
//! 3 s cap, so an answer inside it can still not have waited the cap out.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bridge::{self, AppSnapshot, BridgeHandle, BridgeState};
use crate::server::{ActParams, KeyParams, SETTLE_WAIT, Tools, UPDATE_WAIT, observe, report};
use crate::tree_format::TreeFormat;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{Action, AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

/// Generous bound for local socket round trips.
const WAIT: Duration = Duration::from_secs(5);

/// How long the slow handler in these tests takes: past the 500 ms an
/// unstamped peer is given, short of the 3 s a stamped one is, with a second
/// of room on the near side and a second and a half on the far one, so a
/// runner's jitter moves neither answer across its window.
const SLOW: Duration = Duration::from_millis(1_500);

/// What "at once" and "early" are held to: half the 3 s cap. A local round
/// trip takes milliseconds, so the rest is slack for a slow runner, and an
/// answer inside it still did not wait out the cap.
const PROMPT: Duration = Duration::from_millis(1_500);

/// How far apart a test sends two trees the bridge has to see one by one.
///
/// The bridge's watch channel keeps only the newest tree, so two trees back
/// to back can reach the observer as the second alone, which is the gap
/// `observe` documents. Nothing an app can see says the observer has read
/// the first, so the gap has to be time: the observer only needs to be
/// scheduled once inside it, which a loaded runner still does in a fraction
/// of this. The windows these trees land in are 3 s, so a wide gap costs
/// test time and nothing else.
const APART: Duration = Duration::from_millis(500);

/// How often [`frames`] republishes, like an app drawing every frame.
const FRAME: Duration = Duration::from_millis(200);

/// The answer to an input a stamped peer took and changed nothing for.
const UNCHANGED: &str = "The app received this input, and its tree did not change.";

/// The answer to an input a stamped peer took and published nothing after.
const NO_TREE_AFTER: &str = "The app received this input but published no tree after it within \
                             3000ms; it may still be working. Call read_tree to see its effect.";

/// The answer to an input a stamped peer neither acknowledged nor published
/// a tree after.
const SILENT_STAMPED: &str = "The app neither acknowledged this input nor published a tree after \
                              it within 3000ms; it may be stuck, busy or stopped. Call read_tree \
                              to check.";

/// The answer to a burst of three whose first two presses a stamped peer
/// acknowledged, and then nothing.
const PARTIAL_STAMPED: &str = "The app acknowledged 2 of the 3 inputs this call sent, then \
                               neither acknowledged the last nor published a tree after it \
                               within 3000ms; it may be stuck, busy or stopped. Call read_tree to \
                               check.";

/// The answer to an input the app dropped, from any peer.
const DROPPED: &str = "the app dropped this input because its input queue was full, so nothing \
                       was applied. Send fewer inputs, or wait for each call to return before \
                       sending the next; slower input gets through.";

/// The answers an unstamped peer gets, as 0.3.3 gave them.
const UNCHANGED_UNSTAMPED: &str = "The app received this input, and its tree did not change \
                                   within 500ms. Call read_tree if you expect a delayed effect.";
const SILENT_UNSTAMPED: &str = "The app neither acknowledged this input nor changed its tree \
                                within 500ms. It may not report acknowledgements, or it may \
                                not have reacted yet; call read_tree to re-check.";

/// What the bridge logs, once per connection, for a peer whose claim to
/// stamp is false; matched on this much of it.
const BROKEN_CLAIM: &str = "the app's hello said it stamps its snapshots, but";

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

/// `snapshot` as the bridge stores it, for the tests that drive the observer
/// directly.
fn stored(snapshot: Snapshot) -> AppSnapshot {
    let line = serde_json::to_string(&AppToBridge::Snapshot(snapshot.clone()))
        .expect("serialize a snapshot");
    AppSnapshot::from_line(&line, snapshot)
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
        let line = serde_json::to_string(msg).expect("serialize app message");
        self.send_line(&line).await;
    }

    /// Write `line` as it is, framing and all: for a line no
    /// [`AppToBridge`] serializes to.
    async fn send_line(&mut self, line: &str) {
        self.write
            .write_all(format!("{line}\n").as_bytes())
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

/// Wait until the bridge holds a tree whose root is labelled `marker`: for a
/// connection that replaced another, whose `seq` started over.
async fn wait_for_label(handle: &BridgeHandle, marker: &str) {
    let mut rx = handle.state_rx.clone();
    timeout(
        WAIT,
        rx.wait_for(|state| {
            matches!(state, BridgeState::Connected(s)
                if s.parsed.root.label.as_deref() == Some(marker))
        }),
    )
    .await
    .expect("the tree should arrive")
    .expect("the bridge task is alive");
}

/// Let every task that is ready run until it waits on something. These
/// tests run on the current-thread runtime, so this is an order and not a
/// race: a task spawned before it has run up to its first wait by the time
/// it returns.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// Take the next input, ack it `Delivered`, and publish `snapshot(seq,
/// marker, None)` every [`FRAME`] until told to stop: a peer that draws
/// every frame and never stamps, whatever its hello said.
fn frames(
    mut app: FakeApp,
    seq: u64,
    marker: &'static str,
) -> (JoinHandle<FakeApp>, oneshot::Sender<()>) {
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        loop {
            app.publish(snapshot(seq, marker, None)).await;
            tokio::select! {
                _ = &mut stop_rx => break,
                () = sleep(FRAME) => {}
            }
        }
        app
    });
    (task, stop_tx)
}

fn activate() -> Parameters<ActParams> {
    Parameters(ActParams {
        node: "btn".to_string(),
        action: "activate".to_string(),
        value: None,
    })
}

fn down(repeat: u32) -> Parameters<KeyParams> {
    Parameters(KeyParams {
        key: "down".to_string(),
        repeat: Some(repeat),
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

/// What the bridge logs at warn and above, from the moment
/// [`install`](Self::install) is called on the test's thread. These tests
/// run the bridge and the tools on the current-thread runtime, so that
/// thread is where everything they log is logged.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("the log lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Log {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let log = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || log.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// How many times the broken-claim warning has been logged.
    fn broken_claims(&self) -> usize {
        let bytes = self.0.lock().expect("the log lock");
        String::from_utf8_lossy(&bytes)
            .matches(BROKEN_CLAIM)
            .count()
    }
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
            // The tree in the answer is the proof it beat the cap: at the
            // cap the answer would have been the no-tree note.
            assert_eq!(root_label(text(&result)), "after", "{}", text(&result));
            assert!(took >= SLOW, "answered before the tree: {took:?}");
        } else {
            // The note, not the tree, already says the window closed before
            // the tree came; the bound says so in time as well.
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
/// frame of the ack.
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
    // A round trip here takes milliseconds; under half the cap still proves
    // the answer came on the tree and not at the cap, on any runner.
    assert!(
        took < PROMPT,
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
            // A second window would end at 6 s; the slack is the runner's.
            assert!(
                took >= SETTLE_WAIT && took < SETTLE_WAIT + PROMPT,
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
            sleep(APART).await;
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
    let result = server.key(down(3)).await.expect("key succeeds");
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
        .key(down(3))
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
    // Under half the cap: it ended on the tree, well before 3 s.
    assert!(took < PROMPT, "{took:?}");

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
    // At least the 500 ms after the ack, and short of the cap by as much
    // again: the cap would be 3 s, so 2.5 s still tells the two apart and
    // leaves two seconds of slack.
    assert!(
        took >= UPDATE_WAIT - Duration::from_millis(50) && took < SETTLE_WAIT - UPDATE_WAIT,
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
    // The 500 ms window and not the 3 s one, with the difference as slack.
    assert!(
        took >= UPDATE_WAIT - Duration::from_millis(50) && took < SETTLE_WAIT - UPDATE_WAIT,
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
/// they were claimed, which holds only if a send claims its id once it holds
/// the send lock. Shown without a race: while the lock is held, a send that
/// is waiting on it has claimed nothing, so the id claimed under the lock is
/// the lower one, and both reach the app in that order. A send that claimed
/// before taking the lock, as the bridge once did, claims the lower id while
/// it waits, and goes out second.
#[tokio::test]
async fn a_send_claims_its_id_only_once_it_holds_the_send_lock() {
    let (_dir, listener, handle, _server) = setup("claim", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let order = handle.send_order.lock().await;
    let waiting = {
        let handle = handle.clone();
        tokio::spawn(async move {
            handle
                .send(AgentInput::key("j"), WAIT)
                .await
                .expect("the send goes out")
        })
    };
    // The waiting send has run up to the lock.
    settle().await;
    // Claimed and queued under the lock, the way a send that got it first
    // claims and queues.
    let first = handle.next_input_id();
    handle
        .input_tx
        .send((first, AgentInput::key("k")))
        .await
        .expect("the queue has room");
    drop(order);
    let second = waiting.await.expect("the waiting send");

    let wire = [app.next_input().await.0, app.next_input().await.0];
    assert_eq!(wire, [first, second], "the order they went out in");
    assert!(
        first < second,
        "the send waiting on the lock claimed {second:?} before the one holding it claimed \
         {first:?}"
    );
}

/// The same property under load, from eight senders on four threads: every
/// id claimed goes out once, in ascending order.
///
/// Tolerant by construction. A sound bridge passes on every schedule, so
/// this can fail only on a real reordering; what the scheduler decides is
/// only whether a broken bridge happens to be caught on a given run, which
/// is why the test above exists.
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

/// An app that goes away mid-wait is answered half a second after it went,
/// not at the 3 s cap: time for a restarted app's tree and no more. An
/// acknowledged input is answered as the quit it may have been, an
/// unacknowledged one as a failure, both in the words every peer gets.
#[tokio::test]
async fn a_departure_ends_a_stamped_wait_half_a_second_after_it() {
    for acked in [true, false] {
        let (_dir, listener, handle, server) =
            setup(&format!("gone-{acked}"), TreeFormat::Json).await;
        let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
        wait_ready(&handle, 1, true).await;
        // No restart: a reconnect finds nothing listening.
        drop(listener);

        let quit = tokio::spawn(async move {
            let (id, _) = app.next_input().await;
            if acked {
                app.ack(id, InputStatus::Delivered).await;
            }
            sleep(Duration::from_millis(200)).await;
            drop(app);
            Instant::now()
        });
        let result = server.act(activate()).await;
        let answered = Instant::now();
        let closed = quit.await.expect("the fake app");
        if acked {
            let result = result.expect("an acknowledged input that ended the app is no failure");
            assert!(
                text(&result).starts_with("The app acknowledged this input and then disconnected:"),
                "{}",
                text(&result)
            );
        } else {
            let err = result.expect_err("an input the app never acknowledged is a failure");
            assert!(
                err.message
                    .starts_with("the app disconnected before acknowledging this input:"),
                "{}",
                err.message
            );
        }
        // At least the half second it gives a restart, and short of the
        // 2.8 s the cap would have left by a margin a slow runner keeps.
        let after = answered.saturating_duration_since(closed);
        assert!(
            after >= UPDATE_WAIT - Duration::from_millis(50) && after < PROMPT,
            "answered {after:?} after the app went (acked: {acked})"
        );
    }
}

/// An app that goes away and comes back inside the window answers with its
/// fresh instance's tree, unstamped as every connection's first tree is,
/// and as soon as it arrives.
#[tokio::test]
async fn a_restart_mid_wait_answers_with_the_new_tree() {
    let (_dir, listener, handle, server) = setup("restart", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(5, "before", None)).await;
    wait_ready(&handle, 5, true).await;

    let restart = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        drop(app);
        let app = FakeApp::accept(&listener, true, snapshot(1, "restarted", None)).await;
        (app, listener)
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let (_app, _listener) = restart.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "restarted", "{}", text(&result));
    assert!(took < PROMPT, "{took:?}");
}

/// Acks the observer fell behind on could have been this burst's own drops,
/// so a stamped wait reports them as lost too, even with a credited tree in
/// hand. Driven on the observer directly: the bridge's channel holds 512
/// acks, and no socket fills it faster than the observer reads on every
/// runner, so a fake app cannot make it lag on demand.
#[tokio::test]
async fn acks_lost_to_lag_are_reported_on_a_stamped_wait_too() {
    let pre = stored(snapshot(1, "static", None));
    let ids = [1, 2, 3, 4].map(InputId::new);
    let (ack_tx, acks) = broadcast::channel(2);
    let (state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
    for id in ids {
        let _ = ack_tx.send((id, InputStatus::Delivered));
    }
    // Credited, so the wait ends here rather than at the cap.
    state_tx.send_replace(BridgeState::Connected(stored(snapshot(
        1,
        "static",
        Some(ids[3]),
    ))));

    let started = Instant::now();
    let seen = observe(acks, rx, &pre, &ids, true, false).await;
    assert!(
        started.elapsed() < PROMPT,
        "the credited tree ends the wait"
    );
    let err = report(seen, pre, ids.len(), TreeFormat::Json)
        .expect_err("an incomplete observation is not a success");
    assert!(
        err.message.starts_with(
            "the bridge lost 2 of the app's acknowledgements while this call was in flight"
        ),
        "{}",
        err.message
    );
}

/// A dropped input is final at once, from a peer that stamps as from any.
#[tokio::test]
async fn a_dropped_input_ends_a_stamped_wait_at_once() {
    let (_dir, listener, handle, server) = setup("dropped", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let drop_it = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Dropped).await;
        app
    });
    let started = Instant::now();
    let err = server
        .act(activate())
        .await
        .expect_err("a dropped input is a failure");
    let took = started.elapsed();
    let _app = drop_it.await.expect("the fake app");
    assert_eq!(err.message, DROPPED);
    assert!(took < PROMPT, "{took:?}");
}

/// A peer that stamps acks every input it dequeues, so its silence is never
/// an app that does not report acknowledgements: the answer says what it
/// can be, and names the window it got, the whole 3 s.
#[tokio::test]
async fn a_silent_stamped_peer_is_answered_in_its_own_words() {
    let (_dir, listener, handle, server) = setup("silent", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let quiet = tokio::spawn(async move {
        app.next_input().await;
        app
    });
    let started = Instant::now();
    let result = server
        .act(activate())
        .await
        .expect("silence is not a failed call");
    let took = started.elapsed();
    let _app = quiet.await.expect("the fake app");
    assert_eq!(text(&result), SILENT_STAMPED);
    assert!(took >= SETTLE_WAIT, "{took:?}");
}

/// A line the bridge could not read may have been the input's ack, and the
/// silence answer from a peer that stamps says so, quoting the reason.
#[tokio::test]
async fn a_rejected_line_is_quoted_in_a_stamped_silence() {
    let (_dir, listener, handle, server) = setup("rejected", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "static", None)).await;
    wait_ready(&handle, 1, true).await;

    let unreadable = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        // An ack whose status this build has never heard of fails as a line.
        app.send_line(&format!(
            r#"{{"type":"ack","id":{},"status":"deferred"}}"#,
            id.get()
        ))
        .await;
        app
    });
    let result = server
        .act(activate())
        .await
        .expect("silence is not a failed call");
    let _app = unreadable.await.expect("the fake app");
    let err = handle
        .rejected_rx
        .borrow()
        .clone()
        .expect("the bridge kept why it threw the line away");
    let expected = format!(
        "The app neither acknowledged this input nor published a tree after it within 3000ms, \
         but it did send a line this bridge could not read ({err}). That line may have been \
         this input's acknowledgement: an acknowledgement whose status this bridge does not \
         know fails as a whole and takes the input id with it, so it arrives here as silence. \
         If it was, matching the app's taria dependency to this bridge's makes it readable; \
         call read_tree for the current tree either way."
    );
    assert_eq!(text(&result), expected);
}

/// A burst whose first presses were acknowledged and whose last was not is
/// not an app that said nothing: at the cap, the answer says how many it
/// took.
#[tokio::test]
async fn a_partly_acknowledged_burst_says_how_many_were_at_the_cap() {
    let (_dir, listener, handle, server) = setup("partial", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "row-0", None)).await;
    wait_ready(&handle, 1, true).await;

    let presses = tokio::spawn(async move {
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(app.next_input().await.0);
        }
        app.ack(ids[0], InputStatus::Delivered).await;
        app.ack(ids[1], InputStatus::Delivered).await;
        // The burst so far, which answers nothing.
        app.publish(snapshot(2, "row-2", Some(ids[1]))).await;
        app
    });
    let started = Instant::now();
    let result = server
        .key(down(3))
        .await
        .expect("silence is not a failed call");
    let took = started.elapsed();
    let _app = presses.await.expect("the fake app");
    assert_eq!(text(&result), PARTIAL_STAMPED);
    assert!(took >= SETTLE_WAIT, "{took:?}");
}

/// A tree stamped below a burst was published before the app had any of it,
/// however late it lands: here after the burst's first press was taken. It
/// is the earlier act's effect, so it becomes the burst's baseline, and the
/// burst, which changed nothing, is answered "did not change".
#[tokio::test]
async fn a_tree_stamped_below_a_burst_rebases_it_mid_burst() {
    let (_dir, listener, handle, server) = setup("below-burst", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
    wait_ready(&handle, 1, true).await;

    // An earlier act, taken and answered unchanged.
    let earlier = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(1, "before", Some(id))).await;
        (app, id)
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let (mut app, earlier_id) = earlier.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED);

    let presses = tokio::spawn(async move {
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(app.next_input().await.0);
        }
        app.ack(ids[0], InputStatus::Delivered).await;
        // Published before the press was dequeued, written after its ack.
        app.publish(snapshot(2, "late", Some(earlier_id))).await;
        sleep(APART).await;
        app.ack(ids[1], InputStatus::Delivered).await;
        app.ack(ids[2], InputStatus::Delivered).await;
        app.publish(snapshot(2, "late", Some(ids[2]))).await;
        app
    });
    let result = server.key(down(3)).await.expect("key succeeds");
    let _app = presses.await.expect("the fake app");
    assert_eq!(
        text(&result),
        UNCHANGED,
        "the late tree is the earlier act's, not the burst's"
    );
}

/// Another call's input can go out between a burst's presses, and a tree
/// stamped at it is the burst's effect so far, plus the other call's: it
/// answers that call, and neither answers the burst nor becomes its
/// baseline. Here the burst changes nothing after it, and is still answered
/// with the changed tree, because the tree changed while it ran.
///
/// The order is made, not raced for: both calls queue on the send lock while
/// the test holds it, and the lock is fair, so the burst's first press goes
/// out, then the act, then the rest of the burst.
#[tokio::test]
async fn another_calls_input_inside_a_burst_is_neither_its_answer_nor_its_baseline() {
    let (_dir, listener, handle, server) = setup("interleaved", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
    wait_ready(&handle, 1, true).await;

    let order = handle.send_order.lock().await;
    let burst = {
        let server = server.clone();
        tokio::spawn(async move { server.key(down(3)).await })
    };
    settle().await;
    let act = {
        let server = server.clone();
        tokio::spawn(async move { server.act(activate()).await })
    };
    settle().await;
    drop(order);

    let mut wire = Vec::new();
    let mut presses = Vec::new();
    let mut acted = None;
    for _ in 0..4 {
        let (id, input) = app.next_input().await;
        wire.push(id);
        match input {
            AgentInput::Act { .. } => acted = Some(id),
            _ => presses.push(id),
        }
    }
    let acted = acted.expect("the act went out");
    assert!(
        presses.len() == 3 && presses[0] < acted && acted < presses[1],
        "the act should go out between the burst's first press and the rest: {wire:?}"
    );

    app.ack(presses[0], InputStatus::Delivered).await;
    app.publish(snapshot(2, "row-1", Some(presses[0]))).await;
    app.ack(acted, InputStatus::Delivered).await;
    app.publish(snapshot(3, "acted", Some(acted))).await;
    app.ack(presses[1], InputStatus::Delivered).await;
    app.ack(presses[2], InputStatus::Delivered).await;
    app.publish(snapshot(3, "acted", Some(presses[2]))).await;

    let act = act.await.expect("the act's task").expect("act succeeds");
    assert_eq!(root_label(text(&act)), "acted", "{}", text(&act));
    let burst = burst
        .await
        .expect("the burst's task")
        .expect("key succeeds");
    assert_eq!(
        root_label(text(&burst)),
        "acted",
        "a tree stamped inside the burst is not its baseline: {}",
        text(&burst)
    );
}

/// The reference adapter writes the acks queued before each snapshot, so a
/// tree the app published just before the connection's first dequeue can go
/// out after that dequeue's ack, without a stamp. That is an honest peer,
/// and its tree is the baseline: the stamped tree after it is unchanged, so
/// the input is answered "did not change", and nothing is logged. Read as a
/// broken claim, the tree would have been credited to the input as a change
/// it did not make.
#[tokio::test]
async fn an_honest_unstamped_tree_after_the_first_ack_is_not_a_broken_claim() {
    let log = Log::default();
    let _log = log.install();
    let (_dir, listener, handle, server) = setup("honest", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
    wait_ready(&handle, 1, true).await;

    let first = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        // Apart from the ack, so the bridge cannot mistake it for a tree
        // that came before: this is the case it has to let through.
        sleep(APART).await;
        app.publish(snapshot(2, "drawn-before", None)).await;
        sleep(APART).await;
        app.publish(snapshot(2, "drawn-before", Some(id))).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let _app = first.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED);
    assert_eq!(
        log.broken_claims(),
        0,
        "an honest peer was logged as broken"
    );
}

/// A hello that says `stamps` from an app that never stamps. Its changed
/// tree used to become the baseline, and every input was answered at the
/// cap with "published no tree after it", trees arriving all the while.
/// Now the first call that can tell is answered by the rules for a peer
/// that does not stamp, credited the changed tree, and the log says once
/// that the claim is false; the rest of the connection is answered as a
/// peer that does not stamp, 500 ms window and 0.3.3's texts. A new
/// connection starts with a clean record, and is logged again if it lies
/// again.
#[tokio::test]
async fn a_claim_to_stamp_that_never_holds_is_read_as_no_stamps_and_logged_once() {
    let log = Log::default();
    let _log = log.install();
    let (_dir, listener, handle, server) = setup("never-stamps", TreeFormat::Json).await;
    let app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
    wait_ready(&handle, 1, true).await;

    // The connection's first input: the app's first tree after the ack may
    // be one it drew before it, so the claim breaks on the second.
    let (drawing, stop) = frames(app, 2, "moved");
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let _ = stop.send(());
    let mut app = drawing.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "moved", "{}", text(&result));
    assert!(took < SETTLE_WAIT, "{took:?}");
    assert_eq!(log.broken_claims(), 1);

    // The rest of the connection: a peer that does not stamp.
    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(2, "moved", None)).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let mut app = echo.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED_UNSTAMPED);
    let echo = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(3, "moved-again", None)).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let app = echo.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "moved-again");
    assert_eq!(log.broken_claims(), 1, "once per connection");

    // A new connection, the same lie.
    drop(app);
    let app = FakeApp::accept(&listener, true, snapshot(1, "fresh", None)).await;
    wait_for_label(&handle, "fresh").await;
    let (drawing, stop) = frames(app, 2, "moved");
    let result = server.act(activate()).await.expect("act succeeds");
    let _ = stop.send(());
    let _app = drawing.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "moved", "{}", text(&result));
    assert_eq!(
        log.broken_claims(),
        2,
        "and once more on the next connection"
    );
}

/// A peer that stamped and then stopped: once a call on the connection has
/// seen a tree after an ack, an app that stamps sends no tree without one,
/// so the first unstamped tree after the next ack breaks the claim at once.
/// Here it is an app that publishes only on change, which sends that one
/// tree and no other, and the call is answered with it rather than at the
/// cap.
#[tokio::test]
async fn a_peer_that_stops_stamping_is_caught_on_its_first_unstamped_tree() {
    let log = Log::default();
    let _log = log.install();
    let (_dir, listener, handle, server) = setup("stops-stamping", TreeFormat::Json).await;
    let mut app = FakeApp::accept(&listener, true, snapshot(1, "before", None)).await;
    wait_ready(&handle, 1, true).await;

    let honest = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        app.publish(snapshot(1, "before", Some(id))).await;
        app
    });
    let result = server.act(activate()).await.expect("act succeeds");
    let mut app = honest.await.expect("the fake app");
    assert_eq!(text(&result), UNCHANGED);

    let lapsed = tokio::spawn(async move {
        let (id, _) = app.next_input().await;
        app.ack(id, InputStatus::Delivered).await;
        sleep(APART).await;
        app.publish(snapshot(2, "moved", None)).await;
        app
    });
    let started = Instant::now();
    let result = server.act(activate()).await.expect("act succeeds");
    let took = started.elapsed();
    let _app = lapsed.await.expect("the fake app");
    assert_eq!(root_label(text(&result)), "moved", "{}", text(&result));
    assert!(took < SETTLE_WAIT, "{took:?}");
    assert_eq!(log.broken_claims(), 1);
}

/// A tree can be read ahead of acks that came before it on the wire: the
/// observer polls the ack channel and then the tree channel, and the socket
/// task can publish both in between. Here the credited tree is read first,
/// and the `Delivered` and the `Ignored` that refines it are queued back to
/// back behind it. Returning on the `Delivered`, with the credited tree in
/// hand, would answer "did not change" to an input the app said in time it
/// ignored; the queued `Ignored` is read first, and wins. Driven on the
/// observer directly, since no socket places a tree between two polls on
/// demand.
#[tokio::test]
async fn an_ignored_queued_behind_the_credited_tree_still_wins() {
    let pre = stored(snapshot(1, "static", None));
    let id = InputId::new(1);
    let (ack_tx, acks) = broadcast::channel(8);
    let (state_tx, rx) = watch::channel(BridgeState::Connected(pre.clone()));
    let baseline = pre.clone();
    let observer =
        tokio::spawn(async move { observe(acks, rx, &baseline, &[id], true, false).await });
    // The credited tree, read before any ack: nothing to answer with yet.
    state_tx.send_replace(BridgeState::Connected(stored(snapshot(
        1,
        "static",
        Some(id),
    ))));
    sleep(APART).await;
    // Then both acks at once, as the socket task can publish them.
    let _ = ack_tx.send((id, InputStatus::Delivered));
    let _ = ack_tx.send((id, InputStatus::Ignored));
    let seen = timeout(WAIT, observer)
        .await
        .expect("the observer answers")
        .expect("the observer task");
    let result = report(seen, pre, 1, TreeFormat::Json).expect("an ignored input is answered");
    assert!(
        text(&result).starts_with("The app received this input and deliberately did nothing"),
        "{}",
        text(&result)
    );
    assert_eq!(
        root_label(text(&result).split_once('\n').expect("a tree").1),
        "static"
    );
}
