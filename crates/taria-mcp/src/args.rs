//! Command-line argument parsing and socket-path resolution.
//!
//! The bridge needs exactly one way to find the app's socket: an explicit
//! `--socket <path>`, or `--app <label>` which derives the same default path
//! the app-side adapter (`taria-ratatui`) binds to.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Help text printed for `--help` and appended to argument errors.
pub const HELP: &str = "\
taria-mcp - MCP bridge for taria-enabled TUI apps

Speaks MCP over stdio to an agent harness and connects to a running TUI
app's taria Unix socket, exposing read_tree / act / key tools.

Usage:
  taria-mcp --socket <path>   Connect to an explicit Unix socket path
  taria-mcp --app <label>     Derive the socket path for <label> the same
                              way the app-side adapter does:
                                1. $TARIA_SOCK, if set and non-empty
                                2. $XDG_RUNTIME_DIR/taria/<label>.sock
                                3. <temp dir>/taria-<uid>/<label>.sock
  taria-mcp --help            Show this help

Exactly one of --socket or --app is required.

Environment:
  TARIA_LOG    Log filter (tracing env-filter syntax), default `info`.
               Logs go to stderr; stdout is the MCP channel.";

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// Run the bridge against this socket path.
    Run {
        /// Path of the app's taria Unix socket.
        socket: PathBuf,
    },
    /// `--help` was requested.
    Help,
}

/// Parse the process arguments (without the binary name).
///
/// Returns a human-readable error message on invalid usage.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Cli, String> {
    let mut socket: Option<String> = None;
    let mut app: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(Cli::Help),
            "--socket" => {
                socket = Some(
                    iter.next()
                        .ok_or_else(|| "--socket requires a path argument".to_string())?,
                );
            }
            "--app" => {
                app = Some(
                    iter.next()
                        .ok_or_else(|| "--app requires a label argument".to_string())?,
                );
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    match (socket, app) {
        (Some(_), Some(_)) => Err("pass either --socket or --app, not both".to_string()),
        (Some(path), None) => Ok(Cli::Run {
            socket: PathBuf::from(path),
        }),
        (None, Some(label)) => Ok(Cli::Run {
            socket: resolve_socket_path(&label),
        }),
        (None, None) => Err("one of --socket <path> or --app <label> is required".to_string()),
    }
}

/// Resolve the default socket path for `app_label` from the environment,
/// mirroring the resolution in `taria-ratatui`'s `TariaLayer::bind`.
pub fn resolve_socket_path(app_label: &str) -> PathBuf {
    resolve_socket_path_from(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &user_identity(),
        app_label,
    )
}

/// Pure resolution logic, split out so it can be tested without touching the
/// process environment. Must stay in lockstep with the adapter's resolution.
fn resolve_socket_path_from(
    taria_sock: Option<OsString>,
    xdg_runtime_dir: Option<OsString>,
    temp_dir: &Path,
    user: &str,
    app_label: &str,
) -> PathBuf {
    if let Some(path) = taria_sock
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    if let Some(dir) = xdg_runtime_dir
        && !dir.is_empty()
    {
        return PathBuf::from(dir)
            .join("taria")
            .join(format!("{app_label}.sock"));
    }
    temp_dir
        .join(format!("taria-{user}"))
        .join(format!("{app_label}.sock"))
}

/// Uid where available (via `/proc/self` on Linux), else `$USER`/`$LOGNAME`,
/// else a fixed fallback. Only used to namespace the temp-dir fallback path.
fn user_identity() -> String {
    if let Ok(meta) = fs::metadata("/proc/self") {
        return meta.uid().to_string();
    }
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Cli, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn explicit_socket_is_used_verbatim() {
        let cli = parse_strs(&["--socket", "/run/user/1000/taria/demo.sock"]).unwrap();
        assert_eq!(
            cli,
            Cli::Run {
                socket: PathBuf::from("/run/user/1000/taria/demo.sock"),
            }
        );
    }

    #[test]
    fn app_label_resolves_a_path_ending_in_label_sock() {
        let cli = parse_strs(&["--app", "demo"]).unwrap();
        let Cli::Run { socket } = cli else {
            panic!("expected Run");
        };
        assert!(
            socket.to_string_lossy().ends_with("demo.sock")
                || socket == resolve_socket_path("demo"),
            "socket: {socket:?}"
        );
    }

    #[test]
    fn help_flag_wins() {
        assert_eq!(parse_strs(&["--help"]).unwrap(), Cli::Help);
        assert_eq!(parse_strs(&["-h"]).unwrap(), Cli::Help);
        assert_eq!(
            parse_strs(&["--socket", "/x.sock", "--help"]).unwrap(),
            Cli::Help
        );
    }

    #[test]
    fn missing_selector_is_an_error() {
        let err = parse_strs(&[]).unwrap_err();
        assert!(err.contains("--socket"), "err: {err}");
        assert!(err.contains("--app"), "err: {err}");
    }

    #[test]
    fn both_selectors_is_an_error() {
        let err = parse_strs(&["--socket", "/x.sock", "--app", "demo"]).unwrap_err();
        assert!(err.contains("not both"), "err: {err}");
    }

    #[test]
    fn missing_flag_values_are_errors() {
        assert!(parse_strs(&["--socket"]).unwrap_err().contains("--socket"));
        assert!(parse_strs(&["--app"]).unwrap_err().contains("--app"));
    }

    #[test]
    fn unknown_arguments_are_errors() {
        let err = parse_strs(&["--frobnicate"]).unwrap_err();
        assert!(err.contains("--frobnicate"), "err: {err}");
    }

    #[test]
    fn taria_sock_env_wins() {
        let path = resolve_socket_path_from(
            Some("/custom/app.sock".into()),
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/custom/app.sock"));
    }

    #[test]
    fn empty_taria_sock_is_ignored() {
        let path = resolve_socket_path_from(
            Some("".into()),
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn xdg_runtime_dir_is_second_choice() {
        let path = resolve_socket_path_from(
            None,
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn temp_dir_is_last_resort() {
        let path = resolve_socket_path_from(None, None, Path::new("/tmp"), "1000", "demo");
        assert_eq!(path, PathBuf::from("/tmp/taria-1000/demo.sock"));
    }

    #[test]
    fn empty_xdg_falls_through_to_temp_dir() {
        let path =
            resolve_socket_path_from(None, Some("".into()), Path::new("/tmp"), "alice", "demo");
        assert_eq!(path, PathBuf::from("/tmp/taria-alice/demo.sock"));
    }
}
