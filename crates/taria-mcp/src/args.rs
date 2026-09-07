//! Command-line argument parsing and socket-path resolution.
//!
//! The bridge needs exactly one way to find the app's socket: an explicit
//! `--socket <path>`, or `--app <label>` which derives the same default path
//! the app-side adapter (`taria-ratatui`) binds to.

use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

/// Help text printed for `--help` and appended to argument errors.
pub const HELP: &str = "\
taria-mcp - MCP bridge for taria-enabled TUI apps

Speaks MCP over stdio to an agent harness and connects to a running TUI
app's taria Unix socket, exposing read_tree / act / type_text / key tools.

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
    let socket = match (socket, app) {
        (Some(_), Some(_)) => return Err("pass either --socket or --app, not both".to_string()),
        (Some(path), None) => PathBuf::from(path),
        // A label is a file name, not a path: `--app /etc/cron.d/evil` used
        // to resolve to `/etc/cron.d/evil.sock`, which the app-side adapter
        // would go on to unlink. Refused while the user is still reading
        // argument errors, and `--socket` remains the way to name a path.
        (None, Some(label)) => resolve_socket_path(&label).map_err(|err| err.to_string())?,
        (None, None) => {
            return Err("one of --socket <path> or --app <label> is required".to_string());
        }
    };
    // Check the length here rather than at connect time: the kernel's own
    // refusal names neither the path nor the limit, and it would surface from
    // inside the reconnect loop, where it reads as "the app is not running".
    taria::socket::check_path_len(&socket).map_err(|err| err.to_string())?;
    Ok(Cli::Run { socket })
}

/// Resolve the default socket path for `app_label` from the environment.
///
/// The derivation itself lives in [`taria::socket::resolve_path`], shared with
/// the app-side adapter: the two have to agree on the path or they never meet,
/// refusal of a label that is not a plain file name included.
pub fn resolve_socket_path(app_label: &str) -> Result<PathBuf, taria::socket::InvalidAppLabel> {
    taria::socket::resolve_path(
        env::var_os("TARIA_SOCK"),
        env::var_os("XDG_RUNTIME_DIR"),
        &env::temp_dir(),
        &user_identity(),
        app_label,
    )
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
                || Ok(socket.clone()) == resolve_socket_path("demo"),
            "socket: {socket:?}"
        );
    }

    /// A label is interpolated into a file name, so one carrying a path is
    /// refused here rather than resolved into a socket path the app-side
    /// adapter would bind and unlink.
    #[test]
    fn app_labels_that_are_not_file_names_are_rejected() {
        for label in ["/etc/cron.d/evil", "../../../tmp/pwn", "sub/dir", ".."] {
            let err = parse_strs(&["--app", label]).unwrap_err();
            assert!(err.contains(label), "err: {err}");
            assert!(
                err.contains("single path component"),
                "error should say what a label may be: {err}"
            );
        }
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

    /// A path the kernel would refuse must be caught while the user is still
    /// looking at argument errors, not later as a failed connect.
    #[test]
    fn over_long_socket_paths_are_rejected_with_the_limit() {
        let long = format!(
            "/tmp/{}.sock",
            "x".repeat(taria::socket::MAX_SOCKET_PATH_BYTES)
        );
        let err = parse_strs(&["--socket", &long]).unwrap_err();
        assert!(err.contains("over the"), "err: {err}");
        assert!(
            err.contains(&taria::socket::MAX_SOCKET_PATH_BYTES.to_string()),
            "error should name the limit: {err}"
        );
        assert!(
            err.contains("TARIA_SOCK"),
            "error should say how out: {err}"
        );
    }
}
