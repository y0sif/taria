//! MCP bridge binary: stdio MCP server on one side, the app's taria Unix
//! socket on the other. Run with `--socket <path>` or `--app <label>`, and
//! optionally `--tree-format <json|outline>`.

use std::path::PathBuf;
use std::process::ExitCode;

use rmcp::ServiceExt;
use taria_mcp::server::TariaMcpServer;
use taria_mcp::tree_format::TreeFormat;
use taria_mcp::{args, bridge};

fn main() -> ExitCode {
    let cli = match args::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("taria-mcp: {msg}\n\n{}", args::HELP);
            return ExitCode::from(2);
        }
    };
    let (socket, tree_format) = match cli {
        args::Cli::Help => {
            println!("{}", args::HELP);
            return ExitCode::SUCCESS;
        }
        args::Cli::Run {
            socket,
            tree_format,
            ..
        } => (socket, tree_format),
        // `Cli` is `#[non_exhaustive]` for the library's callers. This binary
        // ships with the parser, so the arm is unreachable; a variant added
        // without handling it here is a usage error rather than a panic.
        other => {
            eprintln!("taria-mcp: unsupported command line: {other:?}");
            return ExitCode::from(2);
        }
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
    match runtime.block_on(run(socket, tree_format)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("taria-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve MCP over stdio until the harness closes the session.
async fn run(socket: PathBuf, tree_format: TreeFormat) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        socket = %socket.display(),
        tree_format = tree_format.name(),
        version = env!("CARGO_PKG_VERSION"),
        taria_protocol = taria::PROTOCOL_VERSION,
        "taria-mcp starting"
    );
    let handle = bridge::spawn(socket);
    let service = TariaMcpServer::with_tree_format(handle, tree_format)
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
