//! MCP bridge binary: stdio MCP server on one side, the app's taria Unix
//! socket on the other. Run with `--socket <path>` or `--app <label>`, and
//! optionally `--tree-format <json|outline>` and `--idle-release <SECS>`.
//!
//! The socket is connected on demand: nothing until the first tool call, and
//! the app is let go of again once no tool call has run for the idle window
//! (60 s by default), so a session that has gone quiet does not lock every
//! other session out of the app. See
//! [`TariaMcpServer::on_demand`](taria_mcp::TariaMcpServer::on_demand).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use rmcp::ServiceExt;
use taria_mcp::{TariaMcpServer, TreeFormat};

mod args;

fn main() -> ExitCode {
    let cli = match args::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("taria-mcp: {msg}\n\n{}", args::HELP);
            return ExitCode::from(2);
        }
    };
    let (socket, tree_format, idle_release) = match cli {
        args::Cli::Help => {
            println!("{}", args::HELP);
            return ExitCode::SUCCESS;
        }
        args::Cli::Run {
            socket,
            tree_format,
            idle_release,
        } => (socket, tree_format, idle_release),
    };

    init_tracing();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("taria-mcp: failed to start tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(socket, tree_format, idle_release)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("taria-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve MCP over stdio until the harness closes the session.
async fn run(
    socket: PathBuf,
    tree_format: TreeFormat,
    idle_release: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        socket = %socket.display(),
        tree_format = tree_format.name(),
        idle_release_s = idle_release.as_secs(),
        version = env!("CARGO_PKG_VERSION"),
        taria_protocol = taria::PROTOCOL_VERSION,
        "taria-mcp starting; connecting to the app on the first tool call"
    );
    let service = TariaMcpServer::on_demand(socket, tree_format, idle_release)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Logs go to stderr only: stdout carries the MCP protocol. Filter via
/// `TARIA_LOG` (tracing env-filter syntax), default `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("TARIA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}
