//! Integration tests for the taria transport side of `TariaLayer`: handshake,
//! snapshot streaming, agent input, per-input acks, and reconnects, all over a
//! real Unix domain socket.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp, InputId, InputStatus};
use taria::{AgentInput, Node, PROTOCOL_VERSION, Role};
use taria_ratatui::TariaLayer;

const TIMEOUT: Duration = Duration::from_secs(5);

/// A [`TariaLayer`] bound in its own temp dir. Dropping the guard drops the
/// layer first (which unlinks the socket file) and then removes the dir
/// itself - also when the test panics - so runs leave nothing in /tmp.
struct TestLayer {
    layer: TariaLayer,
    _dir: tempfile::TempDir,
}

impl Deref for TestLayer {
    type Target = TariaLayer;

    fn deref(&self) -> &TariaLayer {
        &self.layer
    }
}

impl DerefMut for TestLayer {
    fn deref_mut(&mut self) -> &mut TariaLayer {
        &mut self.layer
    }
}

/// Bind a layer on a throwaway socket path cleaned up on drop.
fn bind_layer(label: &str) -> TestLayer {
    let dir = tempfile::Builder::new()
        .prefix("taria-it-")
        // The layer vets the socket dir: it must be private (0700).
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .expect("create test socket dir");
    let layer = TariaLayer::bind_at(label, dir.path().join(format!("{label}.sock"))).unwrap();
    TestLayer { layer, _dir: dir }
}

/// A test stand-in for the bridge: a line-framed client on the layer's socket.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(layer: &TariaLayer) -> Client {
        let stream = UnixStream::connect(layer.socket_path()).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        let writer = stream.try_clone().unwrap();
        Client {
            reader: BufReader::new(stream),
            writer,
        }
    }

    fn read_message(&mut self) -> AppToBridge {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).unwrap();
        assert!(n > 0, "server closed the connection unexpectedly");
        serde_json::from_str(&line).unwrap()
    }

    fn write_line(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
    }

    fn send(&mut self, msg: &BridgeToApp) {
        let line = serde_json::to_string(msg).unwrap();
        self.write_line(&line);
    }

    /// Send one input under `id`; every ack answering it carries that id.
    fn send_input(&mut self, id: InputId, input: AgentInput) {
        self.send(&BridgeToApp::Input { id, input });
    }
}

/// Write one input straight to a stream, for tests that hand the read half to
/// another thread and so cannot use [`Client`].
fn write_input(stream: &mut UnixStream, id: InputId, input: AgentInput) {
    let mut line = serde_json::to_string(&BridgeToApp::Input { id, input }).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
}

fn key(name: &str) -> AgentInput {
    AgentInput::Key { key: name.into() }
}

fn ack(id: InputId, status: InputStatus) -> AppToBridge {
    AppToBridge::Ack { id, status }
}

/// Poll `try_recv` until an input arrives or the timeout passes.
fn poll_try_recv(layer: &TariaLayer) -> Option<AgentInput> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if let Some(input) = layer.try_recv() {
            return Some(input);
        }
        thread::sleep(Duration::from_millis(5));
    }
    None
}

/// [`poll_try_recv`], keeping the id the app has to ack.
fn poll_try_recv_with_id(layer: &TariaLayer) -> Option<(InputId, AgentInput)> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if let Some(pair) = layer.try_recv_with_id() {
            return Some(pair);
        }
        thread::sleep(Duration::from_millis(5));
    }
    None
}

fn publish_single_node(layer: &mut TariaLayer, id: &str) {
    let mut rec = layer.frame();
    rec.push(Node::new(id, Role::Text));
    rec.publish();
}

#[test]
fn hello_then_snapshots_stream_to_client() {
    let mut layer = bind_layer("stream");
    let mut client = Client::connect(&layer);

    // First message is always the handshake.
    let hello = client.read_message();
    assert_eq!(
        hello,
        AppToBridge::Hello {
            app_label: "stream".into(),
            protocol_version: PROTOCOL_VERSION,
        }
    );

    // A publish after connect streams to the client.
    publish_single_node(&mut layer, "first");
    let AppToBridge::Snapshot(snapshot) = client.read_message() else {
        panic!("expected a snapshot after publish");
    };
    assert_eq!(snapshot.seq, 1);
    assert_eq!(snapshot.root.children[0].id.0, "first");

    // And so does the next one.
    publish_single_node(&mut layer, "second");
    let AppToBridge::Snapshot(snapshot) = client.read_message() else {
        panic!("expected a second snapshot");
    };
    assert_eq!(snapshot.seq, 2);
    assert_eq!(snapshot.root.children[0].id.0, "second");
}

#[test]
fn latest_snapshot_is_replayed_on_connect() {
    let mut layer = bind_layer("replay");
    // Published before any client exists.
    publish_single_node(&mut layer, "pre");

    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));
    let AppToBridge::Snapshot(snapshot) = client.read_message() else {
        panic!("expected the latest snapshot right after hello");
    };
    assert_eq!(snapshot.root.children[0].id.0, "pre");
}

#[test]
fn inputs_reach_the_app_acked_delivered_and_malformed_lines_are_ignored() {
    let layer = bind_layer("input");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    assert_eq!(layer.try_recv(), None);

    // Garbage must be skipped without killing the connection.
    client.write_line("not json at all");
    client.write_line(r#"{"type":"unknown"}"#);

    let sent = key("q");
    client.send_input(1, sent.clone());
    assert_eq!(poll_try_recv(&layer), Some(sent));
    // Nothing was written before the app dequeued it: delivered means the
    // event loop took it, so the ack is the first thing the client sees.
    assert_eq!(client.read_message(), ack(1, InputStatus::Delivered));

    let act = AgentInput::Act {
        node: taria::NodeId("btn".into()),
        action: taria::Action::Activate,
        value: None,
    };
    client.send_input(2, act.clone());
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(act));
    assert_eq!(client.read_message(), ack(2, InputStatus::Delivered));
}

/// A timeout the clock cannot turn into a deadline must not abort the app:
/// `Instant::now() + Duration::MAX` panics, and a panic here takes down the
/// app that embedded the layer. Such a timeout asks to wait for as long as
/// the queue can deliver an input, so that is what it gets.
#[test]
fn recv_timeout_survives_a_timeout_no_clock_can_hold() {
    let layer = bind_layer("hugetimeout");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    // Queued before the wait starts, so the wait ends on the input rather
    // than on a deadline that does not exist.
    client.send_input(1, key("j"));
    assert_eq!(layer.recv_timeout(Duration::MAX), Some(key("j")));
    assert_eq!(client.read_message(), ack(1, InputStatus::Delivered));
}

#[test]
fn ack_can_be_refined_to_ignored() {
    let layer = bind_layer("ackignored");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    client.send_input(9, key("q"));
    let (id, input) = poll_try_recv_with_id(&layer).expect("input should reach the app");
    assert_eq!((id, input), (9, key("q")));
    assert_eq!(client.read_message(), ack(9, InputStatus::Delivered));

    // The app looked at it and deliberately did nothing. Last ack wins, so
    // an agent waiting on an effect can stop waiting.
    layer.ack(id, InputStatus::Ignored);
    assert_eq!(client.read_message(), ack(9, InputStatus::Ignored));
}

#[test]
fn ack_precedes_the_snapshot_published_after_it() {
    let mut layer = bind_layer("ackorder");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    client.send_input(3, key("j"));
    assert_eq!(poll_try_recv(&layer), Some(key("j")));
    // The app reacts to the input by publishing a new tree.
    publish_single_node(&mut layer, "moved");

    // Order matters: an agent seeing the snapshot first cannot tell whether
    // it already reflects the input.
    assert_eq!(client.read_message(), ack(3, InputStatus::Delivered));
    let AppToBridge::Snapshot(snapshot) = client.read_message() else {
        panic!("expected the snapshot after the ack");
    };
    assert_eq!(snapshot.root.children[0].id.0, "moved");
}

#[test]
fn drain_hands_over_every_queued_input_and_acks_each() {
    let layer = bind_layer("drain");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    for id in 0..3 {
        client.send_input(id, key(&format!("k{id}")));
    }

    // The inputs cross a socket, so drain until all three have landed.
    let mut got = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    while got.len() < 3 && Instant::now() < deadline {
        layer.drain_with_ids(|id, input| got.push((id, input)));
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        got,
        vec![(0, key("k0")), (1, key("k1")), (2, key("k2"))],
        "drain must hand over every queued input, in order"
    );

    for id in 0..3 {
        assert_eq!(client.read_message(), ack(id, InputStatus::Delivered));
    }
}

#[test]
fn client_can_reconnect_after_disconnect() {
    let mut layer = bind_layer("reconnect");
    publish_single_node(&mut layer, "n");

    let mut first = Client::connect(&layer);
    assert!(matches!(first.read_message(), AppToBridge::Hello { .. }));
    assert!(matches!(first.read_message(), AppToBridge::Snapshot(_)));
    drop(first);

    // The listener must return to accepting after losing its client.
    let mut second = Client::connect(&layer);
    assert!(matches!(second.read_message(), AppToBridge::Hello { .. }));
    assert!(matches!(second.read_message(), AppToBridge::Snapshot(_)));

    // The fresh connection is fully functional in both directions.
    second.send_input(1, key("esc"));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(key("esc")));
    assert_eq!(second.read_message(), ack(1, InputStatus::Delivered));
}

#[test]
fn oversized_line_disconnects_the_client() {
    let layer = bind_layer("linecap");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    // Stream a few MiB with no newline; the layer must cut the connection
    // (capped at 1 MiB per line) rather than buffer it all.
    client.writer.set_write_timeout(Some(TIMEOUT)).unwrap();
    let chunk = [b'a'; 64 * 1024];
    let mut disconnected_while_writing = false;
    for _ in 0..48 {
        // 48 * 64 KiB = 3 MiB
        if client.writer.write_all(&chunk).is_err() {
            disconnected_while_writing = true;
            break;
        }
    }
    if !disconnected_while_writing {
        // The server may still be draining; its close must reach us as EOF
        // (or a reset), never as a timeout with the connection still open.
        let mut line = String::new();
        match client.reader.read_line(&mut line) {
            Ok(0) => {}
            Ok(n) => panic!("expected disconnect, read {n} bytes"),
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::TimedOut =>
            {
                panic!("server kept the connection open after an oversized line")
            }
            Err(_) => {} // connection reset: also a disconnect
        }
    }

    // The listener recovers: a fresh client is served again.
    let mut second = Client::connect(&layer);
    assert!(matches!(second.read_message(), AppToBridge::Hello { .. }));
}

#[test]
fn input_flood_is_bounded_acked_dropped_and_never_blocks_the_socket_thread() {
    const FLOOD: u64 = 2000;
    const QUEUE: u64 = 256;

    let mut layer = bind_layer("inputflood");
    assert_eq!(layer.dropped_inputs(), 0, "no drops before any flood");

    // Read on a second thread for the whole flood: every dropped input is
    // acked, and a client that only wrote would let those acks back up until
    // the layer's write timeout killed the connection.
    let stream = UnixStream::connect(layer.socket_path()).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let collector = thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut messages = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // the layer closed the connection
                Ok(_) => messages.push(serde_json::from_str::<AppToBridge>(&line).unwrap()),
                Err(err) => panic!("client read failed: {err}"),
            }
        }
        messages
    });

    // Far more inputs than the queue holds, with the app not draining.
    for id in 0..FLOOD {
        write_input(&mut writer, id, key(&format!("k{id}")));
    }

    // The socket thread must stay live mid-flood: a publish still streams.
    publish_single_node(&mut layer, "alive");

    // Half-close the write side: the reader consumes the whole flood, sees
    // EOF, and tears the connection down, which the collector observes as
    // EOF. This is the barrier proving the flood was fully processed without
    // blocking.
    writer.shutdown(Shutdown::Write).unwrap();
    let messages = collector.join().expect("collector thread");

    // Only the queue capacity was retained; the overflow was dropped instead
    // of buffered. None of it reaches the app either: the connection that
    // sent it is gone, so every queued input is stale by now.
    let mut received = 0;
    while layer.try_recv().is_some() {
        received += 1;
    }
    assert_eq!(
        received, 0,
        "inputs queued on a dead connection must never be handed to the app"
    );
    assert_eq!(
        layer.stale_inputs(),
        QUEUE,
        "the queue held {QUEUE} inputs, and each discard must be counted once"
    );
    assert_eq!(
        layer.dropped_inputs(),
        FLOOD - QUEUE,
        "each dropped input must be counted exactly once"
    );

    assert!(
        matches!(messages.first(), Some(AppToBridge::Hello { .. })),
        "the handshake is always first"
    );
    assert!(
        messages.iter().any(|msg| matches!(
            msg,
            AppToBridge::Snapshot(snapshot) if snapshot.root.children[0].id.0 == "alive"
        )),
        "a publish mid-flood must still reach the client"
    );

    // The drops reach the agent, which used to see nothing at all. Not
    // necessarily all of them: the connection ends the moment the reader
    // hits EOF, and acks still queued then die with it.
    let dropped: Vec<InputId> = messages
        .iter()
        .filter_map(|msg| match msg {
            AppToBridge::Ack {
                id,
                status: InputStatus::Dropped,
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert!(!dropped.is_empty(), "dropped inputs must be acked dropped");
    assert!(
        dropped.len() as u64 <= FLOOD - QUEUE,
        "more drops acked ({}) than dropped",
        dropped.len()
    );
    assert!(
        dropped.iter().all(|id| *id < FLOOD),
        "a dropped ack must name an input the client actually sent"
    );
    // Nothing was acked delivered: the app drained only after the flood, by
    // which point the connection was gone.
    let other_acks = messages
        .iter()
        .filter(
            |msg| matches!(msg, AppToBridge::Ack { status, .. } if *status != InputStatus::Dropped),
        )
        .count();
    assert_eq!(other_acks, 0, "the only acks during a flood are drops");

    // And the layer still serves fresh clients and inputs afterwards.
    let mut second = Client::connect(&layer);
    assert!(matches!(second.read_message(), AppToBridge::Hello { .. }));
    assert!(matches!(second.read_message(), AppToBridge::Snapshot(_)));
    second.send_input(0, key("after"));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(key("after")));
    // A stale ack from the dead connection must not surface here: ids start
    // over with each client.
    assert_eq!(second.read_message(), ack(0, InputStatus::Delivered));

    // Accepted inputs never bump either counter.
    assert_eq!(layer.dropped_inputs(), FLOOD - QUEUE);
    assert_eq!(layer.stale_inputs(), QUEUE);
}

/// The mirror image of the bridge dropping inputs it queued while no app was
/// connected: an input that arrived on a connection which then died targets a
/// bridge session that no longer exists. Applying it would let, say, a `key q`
/// sent just before the bridge restarted quit the app on the next frame, with
/// nobody left to be told.
#[test]
fn inputs_from_a_dead_connection_are_discarded_and_the_next_ones_are_not() {
    let layer = bind_layer("staleinput");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    client.send_input(1, key("q"));
    // Half-close: the reader consumes the input, then sees EOF and tears the
    // connection down. Reading our side to EOF is the barrier proving the
    // layer is done with this connection, input included.
    client.writer.shutdown(Shutdown::Write).unwrap();
    let mut line = String::new();
    loop {
        line.clear();
        match client.reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => panic!("client read failed: {err}"),
        }
    }

    // Discarding must not cut a wait short: an app pacing its event loop on
    // `recv_timeout` would otherwise spin through the rest of its budget the
    // moment a bridge disconnects.
    const WAIT: Duration = Duration::from_millis(150);
    let started = Instant::now();
    assert_eq!(
        layer.recv_timeout(WAIT),
        None,
        "an input from a dead connection must never reach the app"
    );
    let waited = started.elapsed();
    assert!(
        waited >= WAIT,
        "recv_timeout returned after {waited:?}, short of its {WAIT:?} timeout"
    );
    assert_eq!(layer.stale_inputs(), 1, "the discard must be counted");

    // Nothing was acked for it either: the peer that would read the ack is
    // the one that is gone. A fresh session then works normally, and its
    // inputs are live.
    let mut second = Client::connect(&layer);
    assert!(matches!(second.read_message(), AppToBridge::Hello { .. }));
    second.send_input(1, key("j"));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(key("j")));
    assert_eq!(second.read_message(), ack(1, InputStatus::Delivered));
    assert_eq!(layer.stale_inputs(), 1, "a live input is not a stale one");
}

/// One bridge at a time, which is the design and not an accident: the accept
/// loop serves a client to completion before returning to `accept`, so a
/// second one waits in the listen backlog.
///
/// Worth pinning because two agents attached at once is the likeliest way a
/// user meets it, and from the second bridge's side it looks like a socket
/// that connected and then said nothing. What must hold is that the wait is
/// only a wait: the second client is served in full, handshake included, the
/// moment the first goes away.
#[test]
fn a_second_client_waits_until_the_first_is_gone() {
    let mut layer = bind_layer("oneatatime");
    let mut first = Client::connect(&layer);
    assert!(matches!(first.read_message(), AppToBridge::Hello { .. }));

    // The kernel completes this connection into the backlog, so the client
    // has a socket either way; whether it is being served is what differs.
    let mut second = Client::connect(&layer);
    second
        .reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))
        .unwrap();

    // The first client is live throughout: it gets the publish, and the
    // second gets nothing, not even the handshake that opens every session.
    publish_single_node(&mut layer, "while-first");
    let AppToBridge::Snapshot(snapshot) = first.read_message() else {
        panic!("the served client must still receive publishes");
    };
    assert_eq!(snapshot.root.children[0].id.0, "while-first");

    let mut line = String::new();
    let err = second
        .reader
        .read_line(&mut line)
        .expect_err("a waiting client must receive nothing at all");
    assert!(
        matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
        "expected the read to time out, got {err:?} with {line:?}"
    );

    // The first client goes away, which is the only thing the second was
    // waiting for.
    drop(first);
    second
        .reader
        .get_ref()
        .set_read_timeout(Some(TIMEOUT))
        .unwrap();
    assert_eq!(
        second.read_message(),
        AppToBridge::Hello {
            app_label: "oneatatime".into(),
            protocol_version: PROTOCOL_VERSION,
        },
        "the waiting client must be served in full once its turn comes"
    );
    let AppToBridge::Snapshot(snapshot) = second.read_message() else {
        panic!("the newly served client must get the latest snapshot");
    };
    assert_eq!(
        snapshot.root.children[0].id.0, "while-first",
        "the tree published while it waited is the one it starts from"
    );

    // And it is a full session, not a leftover: its input reaches the app and
    // is acked on the connection it arrived on.
    second.send_input(1, key("j"));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(key("j")));
    assert_eq!(second.read_message(), ack(1, InputStatus::Delivered));
}

#[test]
fn drop_removes_the_socket_file() {
    let bound = bind_layer("cleanup");
    let path = bound.socket_path().to_path_buf();
    assert!(path.exists());
    // Split the guard so only the layer is dropped here: the socket file
    // removal being asserted must come from the layer, not the temp dir.
    let TestLayer { layer, _dir } = bound;
    drop(layer);
    assert!(!path.exists(), "socket file should be removed on drop");
}
