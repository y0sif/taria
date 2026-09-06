//! Integration tests for the taria transport side of `TariaLayer`: handshake,
//! snapshot streaming, agent input, and reconnects, all over a real Unix
//! domain socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use taria::wire::{AppToBridge, BridgeToApp};
use taria::{AgentInput, Node, PROTOCOL_VERSION, Role};
use taria_ratatui::TariaLayer;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Bind a layer on a unique throwaway socket path.
fn bind_layer(label: &str) -> TariaLayer {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir: PathBuf = std::env::temp_dir().join(format!("taria-it-{}", std::process::id()));
    TariaLayer::bind_at(label, dir.join(format!("{label}-{n}.sock"))).unwrap()
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
fn drop_removes_the_socket_file() {
    let layer = bind_layer("cleanup");
    let path = layer.socket_path().to_path_buf();
    assert!(path.exists());
    drop(layer);
    assert!(!path.exists(), "socket file should be removed on drop");
}
