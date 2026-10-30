//! A bridge that let go of an app, after the app was replaced by a new
//! process at the same path, no longer calls its own release the reason it is
//! locked out.
//!
//! The apps here are separate processes, because the bridge tells one app
//! from another by the pid the kernel names for the process listening at the
//! socket, and every listener a test opens in its own process has the test's
//! pid. Each is this test binary run again with [`APP_PROCESS_ENV`] set, which
//! turns [`fake_app_process`], an ignored test that otherwise does nothing,
//! into the app.
//! Linux only: the kernel names the listener's pid there (`SO_PEERCRED`).
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use crate::TariaMcpServer;
use crate::server::Tools;
use taria::wire::AppToBridge;
use taria::{Node, PROTOCOL_VERSION, Role, Snapshot};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

/// Set to a socket path, the variable that makes [`fake_app_process`] serve
/// at it.
const APP_PROCESS_ENV: &str = "TARIA_MCP_TEST_APP_PROCESS";

/// What the app process prints once it listens, and each time a bridge
/// closes its connection.
const LISTENING: &str = "fake app: listening";
const CLOSED: &str = "fake app: connection closed";

/// Not a test of its own: run by [`AppProcess::start`] with
/// [`APP_PROCESS_ENV`] set, it is an app that serves one bridge at a time, the
/// way the adapter does, until it is killed or its parent goes away. Each
/// bridge gets `hello` and one snapshot whose root names this process's pid,
/// and is held until it closes the connection. Without the variable it
/// returns at once.
#[test]
#[ignore = "helper: the restart test runs it as a child process"]
fn fake_app_process() {
    let Some(path) = std::env::var_os(APP_PROCESS_ENV) else {
        return;
    };
    // The parent holds this process's stdin open and never writes to it, so
    // end of file means the parent is gone, even SIGKILLed, with no drop to
    // kill this one; without this it would wait for a bridge forever.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    let path = PathBuf::from(path);
    // A killed predecessor leaves its socket file behind.
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind the app socket");
    println!("{LISTENING}");
    let hello = serde_json::to_string(&AppToBridge::hello("fake-app", PROTOCOL_VERSION))
        .expect("a hello serializes");
    let snapshot = serde_json::to_string(&AppToBridge::Snapshot(Snapshot::new(
        1,
        Node::new("app", Role::App).label(format!("pid {}", std::process::id())),
    )))
    .expect("a snapshot serializes");
    for stream in listener.incoming() {
        let mut stream = stream.expect("accept a bridge");
        writeln!(stream, "{hello}\n{snapshot}").expect("greet the bridge");
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        println!("{CLOSED}");
    }
}

/// An app process at a socket path, killed when dropped.
struct AppProcess {
    child: Child,
    pid: u32,
    /// The write end of the process's stdin, its lifeline: the process exits
    /// once this closes, which the kernel does when this process dies.
    _lifeline: ChildStdin,
    /// The lines the process prints, read on a thread of their own so a
    /// wait for one never blocks the runtime the bridges run on.
    lines: mpsc::UnboundedReceiver<String>,
}

impl AppProcess {
    /// Start one at `path`, and wait until it listens.
    async fn start(path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([
                "--exact",
                "tests::released_restart::fake_app_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(APP_PROCESS_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the app process");
        let lifeline = child.stdin.take().expect("the app's stdin is piped");
        let stdout = child.stdout.take().expect("the app's stdout is piped");
        let (tx, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let pid = child.id();
        let mut app = Self {
            child,
            pid,
            _lifeline: lifeline,
            lines,
        };
        app.wait_for(LISTENING).await;
        app
    }

    /// Wait for the process to print `marker`.
    async fn wait_for(&mut self, marker: &str) {
        timeout(WAIT, async {
            while let Some(line) = self.lines.recv().await {
                if line.contains(marker) {
                    return;
                }
            }
            panic!("the app process exited before printing {marker:?}");
        })
        .await
        .unwrap_or_else(|_| panic!("the app process never printed {marker:?}"));
    }
}

impl Drop for AppProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn result_text(result: &rmcp::model::CallToolResult) -> &str {
    result.content[0]
        .as_text()
        .map(|text| text.text.as_str())
        .expect("a text result")
}

/// The tool layer of a server built the way the binary builds it, with
/// `idle_release` as its window.
fn on_demand(path: PathBuf, idle_release: Duration) -> Tools {
    TariaMcpServer::builder(path)
        .idle_release(idle_release)
        .on_demand()
        .expect("a tokio test runs inside a runtime")
        .tools
}

/// A bridge lets go of the app after its window; the app is killed and a new
/// process takes the path while the bridge makes no call; another session's
/// bridge takes the new instance. The first bridge's next call lands in the
/// new instance's backlog, the silence of a held app, and found nothing to
/// fail on along the way, so it went on saying its disconnect was its own
/// release and "not a fault", about an app that has since been replaced.
/// The listener's pid is not the one it let go of, and the answer is the
/// plain held one.
#[tokio::test]
async fn a_release_of_an_app_since_replaced_is_not_called_the_reason() {
    let dir = tempfile::Builder::new()
        .prefix("taria-mcp-test-")
        .tempdir()
        .expect("create test socket dir");
    let path = dir.path().join("replaced.sock");

    let mut first_app = AppProcess::start(&path).await;
    let first = on_demand(path.clone(), Duration::from_millis(300));
    let tree = first.read_tree().await.expect("the bridge gets the app");
    let first_pid = format!("pid {}", first_app.pid);
    assert!(result_text(&tree).contains(&first_pid), "{tree:?}");
    first_app.wait_for(CLOSED).await;
    drop(first_app);

    let second_app = AppProcess::start(&path).await;
    let other = on_demand(path, Duration::from_secs(30));
    let tree = other
        .read_tree()
        .await
        .expect("another session takes the new instance");
    assert!(
        result_text(&tree).contains(&format!("pid {}", second_app.pid)),
        "{tree:?}"
    );

    let err = timeout(WAIT, async {
        loop {
            let err = first
                .read_tree()
                .await
                .expect_err("the other session holds the app");
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
