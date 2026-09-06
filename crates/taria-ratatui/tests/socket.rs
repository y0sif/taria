//! Integration tests for the taria transport side of `TariaLayer`: handshake,
//! snapshot streaming, agent input, and reconnects, all over a real Unix
//! domain socket.

use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::ops::{Deref, DerefMut};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp};
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
fn inputs_reach_the_app_and_malformed_lines_are_ignored() {
    let layer = bind_layer("input");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    assert_eq!(layer.try_recv(), None);

    // Garbage must be skipped without killing the connection.
    client.write_line("not json at all");
    client.write_line(r#"{"type":"unknown"}"#);

    let sent = AgentInput::Key { key: "q".into() };
    client.send(&BridgeToApp::Input(sent.clone()));
    assert_eq!(poll_try_recv(&layer), Some(sent));

    let act = AgentInput::Act {
        node: taria::NodeId("btn".into()),
        action: taria::Action::Activate,
        value: None,
    };
    client.send(&BridgeToApp::Input(act.clone()));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(act));
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
    let sent = AgentInput::Key { key: "esc".into() };
    second.send(&BridgeToApp::Input(sent.clone()));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(sent));
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
fn input_flood_is_bounded_and_does_not_block_the_socket_thread() {
    let mut layer = bind_layer("inputflood");
    let mut client = Client::connect(&layer);
    assert!(matches!(client.read_message(), AppToBridge::Hello { .. }));

    // Far more inputs than the queue holds, with the app not draining.
    for i in 0..2000 {
        client.send(&BridgeToApp::Input(AgentInput::Key {
            key: format!("k{i}"),
        }));
    }

    // The socket thread must stay live mid-flood: a publish still streams.
    publish_single_node(&mut layer, "alive");
    let AppToBridge::Snapshot(snapshot) = client.read_message() else {
        panic!("expected a snapshot during the input flood");
    };
    assert_eq!(snapshot.root.children[0].id.0, "alive");

    // Half-close the write side: the reader consumes the whole flood, sees
    // EOF, and tears the connection down, which we observe as EOF here. This
    // is the barrier proving the flood was fully processed without blocking.
    client.writer.shutdown(Shutdown::Write).unwrap();
    let mut line = String::new();
    loop {
        line.clear();
        match client.reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(err) => panic!("expected EOF after write shutdown, got {err}"),
        }
    }

    // Only the queue capacity (256) was retained; the overflow was dropped
    // instead of buffered.
    let mut received = 0;
    while layer.try_recv().is_some() {
        received += 1;
    }
    assert_eq!(received, 256, "input queue should be bounded at 256");

    // And the layer still serves fresh clients and inputs afterwards.
    let mut second = Client::connect(&layer);
    assert!(matches!(second.read_message(), AppToBridge::Hello { .. }));
    assert!(matches!(second.read_message(), AppToBridge::Snapshot(_)));
    let sent = AgentInput::Key {
        key: "after".into(),
    };
    second.send(&BridgeToApp::Input(sent.clone()));
    assert_eq!(layer.recv_timeout(TIMEOUT), Some(sent));
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
