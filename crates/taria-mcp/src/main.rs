//! MCP bridge binary: stdio MCP server on one side, the app's taria Unix
//! socket on the other. Run with `--socket <path>` or `--app <label>`.

use std::path::PathBuf;
use std::process::ExitCode;

use rmcp::ServiceExt;
use taria_mcp::server::TariaMcpServer;
use taria_mcp::{args, bridge};

fn main() -> ExitCode {
    let cli = match args::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("taria-mcp: {msg}\n\n{}", args::HELP);
            return ExitCode::from(2);
        }
    };
    let socket = match cli {
        args::Cli::Help => {
            println!("{}", args::HELP);
            return ExitCode::SUCCESS;
        }
        args::Cli::Run { socket } => socket,
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
    match runtime.block_on(run(socket)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("taria-mcp: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Serve MCP over stdio until the harness closes the session.
async fn run(socket: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!(
        socket = %socket.display(),
        version = env!("CARGO_PKG_VERSION"),
        taria_protocol = taria::PROTOCOL_VERSION,
        "taria-mcp starting"
    );
    let handle = bridge::spawn(socket);
    let service = TariaMcpServer::new(handle)
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
