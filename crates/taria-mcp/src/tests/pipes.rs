//! The bridge against a fake app on a Windows named pipe: the pipe made the
//! way the adapter makes the app's, connected to by the manager through the
//! transport, and the answers the tool layer gives for each thing a pipe can
//! do to a connect.
//!
//! Windows only. Under Wine these run against Wine's pipes, which is a strong
//! hint and not Windows.

use std::path::PathBuf;
use std::time::Duration;

use rmcp::model::CallToolResult;
use taria::wire::{AppToBridge, BridgeToApp, InputStatus};
use taria::{AgentInput, Node, PROTOCOL_VERSION, Role, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::TariaMcpServer;
use crate::bridge::{self, BridgeState, LinkState};
use crate::server::Tools;
use crate::transport::testing::{app_pipe, pipe_owned_by, unique_pipe_name};

/// Far longer than anything here takes on a healthy machine.
const STEP: Duration = Duration::from_secs(10);

/// The tool layer of a server built the way the binary builds it.
fn on_demand(name: &str) -> Tools {
    TariaMcpServer::builder(PathBuf::from(name))
        .on_demand()
        .expect("a tokio test runs inside a runtime, and the name is a pipe's")
        .tools
}

fn result_text(result: &CallToolResult) -> &str {
    result.content[0]
        .as_text()
        .map(|text| text.text.as_str())
        .expect("a text result")
}

/// Write one message on the app's end, as a line.
async fn send(pipe: &mut NamedPipeServer, msg: &AppToBridge) {
    let mut line = serde_json::to_string(msg).expect("a message serializes");
    line.push('\n');
    pipe.write_all(line.as_bytes()).await.expect("write a line");
}

/// Take the bridge on `pipe` and greet it with a hello and a first tree.
async fn greet(pipe: &mut NamedPipeServer, seq: u64) {
    timeout(STEP, pipe.connect())
        .await
        .expect("a bridge connects")
        .expect("the pipe takes it");
    send(pipe, &AppToBridge::hello("demo", PROTOCOL_VERSION)).await;
    let root = Node::new("app", Role::App).label("pipe demo");
    send(pipe, &AppToBridge::Snapshot(Snapshot::new(seq, root))).await;
}

async fn wait_connected(state: &mut watch::Receiver<BridgeState>) {
    timeout(
        STEP,
        state.wait_for(|state| matches!(state, BridgeState::Connected(_))),
    )
    .await
    .expect("a tree within the step")
    .expect("the manager is alive");
}

/// The whole exchange over the app's pipe: the manager connects through the
/// transport, owner check included, reads the hello and the tree through
/// `serve_connection`, writes an input as a line, and republishes the app's
/// ack for it.
#[tokio::test]
async fn a_hello_a_tree_and_an_input_cross_the_apps_pipe() {
    let name = unique_pipe_name("exchange");
    let mut pipe = app_pipe(&name).expect("create the app's pipe");
    let handle = bridge::spawn(PathBuf::from(&name));
    greet(&mut pipe, 3).await;
    let mut state = handle.state_rx.clone();
    wait_connected(&mut state).await;
    let BridgeState::Connected(tree) = state.borrow().clone() else {
        unreachable!("waited for it");
    };
    assert_eq!(tree.parsed.seq, 3);
    assert!(tree.tree_json.contains("pipe demo"), "{}", tree.tree_json);
    assert_eq!(*handle.protocol_rx.borrow(), Some(PROTOCOL_VERSION));
    assert!(matches!(*handle.link_rx.borrow(), LinkState::Open { .. }));

    let mut acks = handle.subscribe_acks();
    let id = handle
        .send(AgentInput::key("j"), STEP)
        .await
        .expect("queued for the app");
    let (read, mut write) = tokio::io::split(pipe);
    let mut lines = BufReader::new(read).lines();
    let line = timeout(STEP, lines.next_line())
        .await
        .expect("an input within the step")
        .expect("read the pipe")
        .expect("a line, not the end");
    match serde_json::from_str(&line).expect("a bridge message") {
        BridgeToApp::Input { id: got, .. } => assert_eq!(got, id),
        other => panic!("expected an input, got {other:?}"),
    }
    let mut ack = serde_json::to_string(&AppToBridge::ack(id, InputStatus::Delivered))
        .expect("an ack serializes");
    ack.push('\n');
    write.write_all(ack.as_bytes()).await.expect("send the ack");
    let got = timeout(STEP, acks.recv())
        .await
        .expect("an ack within the step")
        .expect("the ack channel is open");
    assert_eq!(got, (id, InputStatus::Delivered));
}

/// A name nobody serves is nothing listening: before any tree, the answer
/// that the path and the app are the questions; after an app that served
/// one exits, which takes its pipe's name with it, the departure answer as
/// on every platform, "it may have exited".
#[tokio::test]
async fn an_app_that_exits_is_nothing_listening_and_may_have_exited() {
    let name = unique_pipe_name("exits");
    let tools = on_demand(&name);
    let never = tools.read_tree().await.expect_err("nothing serves it");
    assert_eq!(
        never.message,
        "no snapshot from the app yet, and this bridge holds no connection to the pipe: is the \
         taria-enabled app running, and is the pipe name correct? The bridge reconnects \
         automatically; retry once the app is up."
    );

    let mut pipe = app_pipe(&name).expect("create the app's pipe");
    let greeted = async {
        greet(&mut pipe, 1).await;
        pipe
    };
    let (tree, pipe) = tokio::join!(
        async {
            for _ in 0..40 {
                if let Ok(tree) = tools.read_tree().await {
                    return tree;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("no tree from the app's pipe");
        },
        greeted
    );
    assert!(result_text(&tree).contains("pipe demo"));

    // The app exits: its pipe goes, and with it the name.
    drop(pipe);
    let mut gone = None;
    for _ in 0..40 {
        match tools.read_tree().await {
            Err(err) if err.message.contains("disconnected") => {
                gone = Some(err.message);
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert_eq!(
        gone.as_deref(),
        Some(
            "app 'demo' disconnected (last snapshot seq 1); it may have exited. The bridge \
             reconnects automatically; retry once the app is back."
        )
    );
}

/// The app's one instance taken by another bridge refuses this one as busy,
/// which is the held answer; once the holder lets go and the app takes the
/// next bridge, this one gets in on a later attempt, with no restart.
#[tokio::test]
async fn a_busy_pipe_is_held_and_the_bridge_gets_in_once_it_is_free() {
    let name = unique_pipe_name("held");
    let pipe = app_pipe(&name).expect("create the app's pipe");
    let holder = ClientOptions::new().open(&name).expect("another bridge");
    pipe.connect()
        .await
        .expect("the app takes the other bridge");

    let tools = on_demand(&name);
    let held = tools.read_tree().await.expect_err("held by the other");
    assert!(
        held.message.starts_with(
            "no snapshot from the app yet: the app is held by another bridge. The pipe name is \
             right: the app's pipe refused this bridge as busy"
        ),
        "{}",
        held.message
    );

    // The holder lets go, and the app makes its pipe again for the next
    // bridge, the way an adapter does between bridges.
    drop(holder);
    drop(pipe);
    // The old instance closes once the runtime has seen its pending read
    // cancelled, so the name is busy for a moment after the drop.
    let mut remade = None;
    for _ in 0..100 {
        match app_pipe(&name) {
            Ok(pipe) => {
                remade = Some(pipe);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut pipe = remade.expect("make the pipe again");
    let (tree, ()) = tokio::join!(
        async {
            for _ in 0..40 {
                if let Ok(tree) = tools.read_tree().await {
                    return tree;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("never got in");
        },
        greet(&mut pipe, 2)
    );
    assert!(result_text(&tree).contains("pipe demo"));
}

/// A connection the app's pipe took, with nothing ever said on it, is the
/// app suspended or stuck, not another bridge: on a pipe, that would have
/// been refused as busy.
#[tokio::test]
async fn a_silent_pipe_is_answered_as_an_app_that_is_not_answering() {
    let name = unique_pipe_name("silent");
    let _pipe = app_pipe(&name).expect("create the app's pipe");
    let tools = on_demand(&name);
    // The first call waits out the grace on the connection it opened.
    let silent = tools.read_tree().await.expect_err("nothing said");
    let silent = if silent.message.contains("worth one retry") {
        tokio::time::sleep(Duration::from_millis(600)).await;
        tools.read_tree().await.expect_err("still nothing said")
    } else {
        silent
    };
    assert!(
        silent.message.starts_with(
            "no snapshot from the app yet: the app accepted this bridge's connection "
        ) && silent.message.contains("it may be suspended or stuck")
            && !silent.message.contains("held by another bridge"),
        "{}",
        silent.message
    );
}

/// A pipe at the app's name that another account owns is refused: the
/// bridge closes it having written nothing, and says it is not the app.
/// Only where the test can make such a pipe, which Wine allows; a Windows
/// process that is not elevated may not name the Administrators group as
/// an owner, and the check is tested on its own in the transport.
#[tokio::test]
async fn a_pipe_another_account_owns_is_closed_unwritten_and_named() {
    let name = unique_pipe_name("foreign");
    let mut pipe = match pipe_owned_by(&name, "BA") {
        Ok(pipe) => pipe,
        Err(err) => {
            eprintln!("no pipe another account owns here ({err}); nothing to refuse");
            return;
        }
    };
    let tools = on_demand(&name);
    let refused = tools.read_tree().await.expect_err("not the app");
    assert!(
        refused.message.starts_with(
            "no snapshot from the app yet: the pipe at this name is owned by another account \
             than this bridge's user, or refused this user access"
        ),
        "{}",
        refused.message
    );
    // The bridge opened it and closed it, and sent nothing in between.
    timeout(STEP, pipe.connect())
        .await
        .expect("the bridge's open")
        .expect("the pipe took it");
    let mut sent = Vec::new();
    let read = timeout(STEP, pipe.read_to_end(&mut sent)).await;
    assert!(
        matches!(read, Ok(Ok(0)) | Ok(Err(_))),
        "the bridge wrote to it: {read:?} {sent:?}"
    );
    assert!(sent.is_empty());
}

/// The pid at the other end of the pipe is the process serving it, here
/// this test's own, which is what tells one app from the next at a name.
#[tokio::test]
async fn the_peer_pid_is_the_process_serving_the_pipe() {
    let name = unique_pipe_name("pid");
    let mut pipe = app_pipe(&name).expect("create the app's pipe");
    let (handle, demand) = bridge::spawn_on_demand(PathBuf::from(&name), Duration::from_secs(30));
    let call = demand.begin();
    greet(&mut pipe, 1).await;
    let mut state = handle.state_rx.clone();
    wait_connected(&mut state).await;
    assert_eq!(demand.peer_pid(), i32::try_from(std::process::id()).ok());
    drop(call);
}
