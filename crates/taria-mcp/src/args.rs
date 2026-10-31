//! Command-line argument parsing and socket-path resolution.
//!
//! The bridge needs exactly one way to find the app's socket: an explicit
//! `--socket <path>`, or `--app <label>` which derives the same default path
//! the app-side adapter (`taria-ratatui`) binds to. On Windows the endpoint is
//! a named pipe, and both name one: `--socket` takes a pipe name, and `--app`
//! derives the one the adapter serves, from this user's SID.

use std::env;
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use taria_mcp::{DEFAULT_IDLE_RELEASE, TreeFormat};

/// The longest `--idle-release` takes, in seconds: a day.
///
/// A bridge meant to hold the app longer than that while idle is one meant
/// never to let go, which `0` says. Without a bound, a value past what the
/// window's deadline can reach was taken, and silently meant "never".
pub const MAX_IDLE_RELEASE_SECS: u64 = 86_400;

/// Help text printed for `--help` and appended to argument errors.
#[cfg(not(windows))]
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
                                3. <temp dir>/taria-<user>/<label>.sock
                                   (<user> is the uid where available,
                                   else $USER, else $LOGNAME, else default)
  taria-mcp --help            Show this help

Exactly one of --socket or --app is required.

Options:
  --tree-format <json|outline>
                              How read_tree and the input tools show the
                              tree: json (the default) passes on the app's
                              own snapshot line; outline renders the same
                              snapshot as one indented line per node, in
                              fewer characters. It names each action as act
                              takes it, without other keys in its object.
  --idle-release <SECS>       Let go of the app after SECS seconds
                              (0 to 86400) with no tool call
                              (default 60, 0 never lets go).

Environment:
  TARIA_LOG    Log filter (tracing env-filter syntax), default `info`.
               Logs go to stderr; stdout is the MCP channel.";

/// Help text printed for `--help` and appended to argument errors: on
/// Windows, where the app's endpoint is a named pipe, the same help with the
/// pipe's naming in place of the socket's.
#[cfg(windows)]
pub const HELP: &str = r"taria-mcp - MCP bridge for taria-enabled TUI apps

Speaks MCP over stdio to an agent harness and connects to a running TUI
app's taria named pipe, exposing read_tree / act / type_text / key tools.

Usage:
  taria-mcp --socket <pipe>   Connect to an explicit pipe, \\.\pipe\<name>
  taria-mcp --app <label>     Derive the pipe name for <label> the same
                              way the app-side adapter does:
                                1. $TARIA_SOCK, if set and non-empty
                                   (a pipe name, \\.\pipe\<name>)
                                2. \\.\pipe\taria-<SID>-<label>
                                   (<SID> is this user's, S-1-5-21-...)
  taria-mcp --help            Show this help

Exactly one of --socket or --app is required.

Options:
  --tree-format <json|outline>
                              How read_tree and the input tools show the
                              tree: json (the default) passes on the app's
                              own snapshot line; outline renders the same
                              snapshot as one indented line per node, in
                              fewer characters. It names each action as act
                              takes it, without other keys in its object.
  --idle-release <SECS>       Let go of the app after SECS seconds
                              (0 to 86400) with no tool call
                              (default 60, 0 never lets go).

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
        /// How the tree is shown to the agent (`--tree-format`).
        tree_format: TreeFormat,
        /// How long the bridge keeps the app with no tool calls before it
        /// lets go (`--idle-release`, whole seconds up to
        /// [`MAX_IDLE_RELEASE_SECS`]). [`Duration::ZERO`]
        /// never lets go. See
        /// [`TariaMcpServerBuilder::idle_release`](taria_mcp::TariaMcpServerBuilder::idle_release).
        idle_release: Duration,
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
    let mut tree_format = TreeFormat::default();
    let mut idle_release = DEFAULT_IDLE_RELEASE;
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
            "--tree-format" => {
                let name = iter.next().ok_or_else(|| {
                    format!("--tree-format requires a value: {}", TreeFormat::NAMES)
                })?;
                tree_format = TreeFormat::from_name(&name).ok_or_else(|| {
                    format!(
                        "unknown --tree-format `{name}`; expected {}",
                        TreeFormat::NAMES
                    )
                })?;
            }
            "--idle-release" => {
                let secs = iter.next().ok_or_else(|| {
                    format!(
                        "--idle-release requires whole seconds from 0 to {MAX_IDLE_RELEASE_SECS} \
                         (0 never lets go)"
                    )
                })?;
                idle_release = parse_idle_release(&secs)?;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let socket = match (socket, app) {
        (Some(_), Some(_)) => return Err("pass either --socket or --app, not both".to_string()),
        (Some(path), None) => {
            // A pipe name, and one on this machine: anything else would be
            // opened as a file, or as a pipe on another machine, over SMB.
            #[cfg(windows)]
            taria::socket::check_pipe_name(&path)
                .map_err(|err| format!("--socket takes a pipe name on Windows: {err}"))?;
            PathBuf::from(path)
        }
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
    Ok(Cli::Run {
        socket,
        tree_format,
        idle_release,
    })
}

/// The value of `--idle-release`: whole seconds, written as ASCII digits and
/// nothing else, from 0 to [`MAX_IDLE_RELEASE_SECS`].
///
/// Digits only, because `u64::from_str` also takes a leading `+`, so `+5` and
/// `+0` were windows. Leading zeros are accepted: `0005` is still five whole
/// seconds, and refusing it would buy nothing.
fn parse_idle_release(secs: &str) -> Result<Duration, String> {
    let refused = || {
        format!(
            "--idle-release takes whole seconds from 0 to {MAX_IDLE_RELEASE_SECS} (0 never lets \
             go), not `{secs}`"
        )
    };
    if secs.is_empty() || !secs.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(refused());
    }
    match secs.parse::<u64>() {
        Ok(secs) if secs <= MAX_IDLE_RELEASE_SECS => Ok(Duration::from_secs(secs)),
        _ => Err(refused()),
    }
}

/// Resolve the default socket path for `app_label` from the environment.
///
/// The derivation itself lives in [`taria::socket::resolve_path`], shared with
/// the app-side adapter: the two have to agree on the path or they never meet,
/// refusal of a label that is not a plain file name included.
#[cfg(unix)]
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
#[cfg(unix)]
fn user_identity() -> String {
    use std::os::unix::fs::MetadataExt;

    if let Ok(meta) = std::fs::metadata("/proc/self") {
        return meta.uid().to_string();
    }
    env::var("USER")
        .or_else(|_| env::var("LOGNAME"))
        .unwrap_or_else(|_| "default".to_string())
}

/// Resolve the default pipe name for `app_label` from the environment and
/// this user's SID, the Windows counterpart of the socket path.
///
/// The derivation itself lives in [`taria::socket::resolve_pipe_name`],
/// shared with the app-side adapter, and the SID is read from the process
/// token, as the adapter reads its own: the two have to agree on the name or
/// they never meet. A `$TARIA_SOCK` that is not a pipe on this machine is
/// refused, not fallen back from.
#[cfg(windows)]
pub fn resolve_socket_path(app_label: &str) -> Result<PathBuf, String> {
    resolve_pipe(
        env::var_os("TARIA_SOCK"),
        crate::windows_sid::current_user_sid,
        app_label,
    )
}

/// [`resolve_socket_path`] with the environment and the SID passed in, so
/// the tests need not set either. The SID is read only when it names the
/// pipe: an override needs none, and a token that cannot be read is no
/// reason to refuse one.
#[cfg(windows)]
fn resolve_pipe(
    taria_sock: Option<OsString>,
    user_sid: impl FnOnce() -> io::Result<String>,
    app_label: &str,
) -> Result<PathBuf, String> {
    let overridden = taria_sock.as_ref().is_some_and(|name| !name.is_empty());
    let user = if overridden {
        String::new()
    } else {
        user_sid().map_err(|err| {
            format!(
                "cannot read this user's SID from the process token, and the app's pipe is named \
                 by it: {err}; pass --socket \\\\.\\pipe\\<name> instead"
            )
        })?
    };
    taria::socket::resolve_pipe_name(taria_sock, &user, app_label)
        .map(PathBuf::from)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Cli, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    /// An endpoint `--socket` takes on this platform, for the cases that are
    /// about other flags: a socket path, or on Windows a pipe name.
    const SOCK: &str = if cfg!(windows) {
        r"\\.\pipe\x"
    } else {
        "/x.sock"
    };

    #[test]
    fn explicit_socket_is_used_verbatim() {
        let explicit = if cfg!(windows) {
            r"\\.\pipe\taria-demo"
        } else {
            "/run/user/1000/taria/demo.sock"
        };
        let cli = parse_strs(&["--socket", explicit]).unwrap();
        assert_eq!(
            cli,
            Cli::Run {
                socket: PathBuf::from(explicit),
                tree_format: TreeFormat::Json,
                idle_release: DEFAULT_IDLE_RELEASE,
            }
        );
    }

    #[test]
    fn app_label_resolves_a_path_ending_in_label_sock() {
        let cli = parse_strs(&["--app", "demo"]).unwrap();
        let Cli::Run { socket, .. } = cli else {
            panic!("expected Run");
        };
        let ending = if cfg!(windows) { "-demo" } else { "demo.sock" };
        assert!(
            socket.to_string_lossy().ends_with(ending)
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
            let rule = if cfg!(windows) {
                "single name"
            } else {
                "single path component"
            };
            assert!(
                err.contains(rule),
                "error should say what a label may be: {err}"
            );
        }
    }

    #[test]
    fn help_flag_wins() {
        assert_eq!(parse_strs(&["--help"]).unwrap(), Cli::Help);
        assert_eq!(parse_strs(&["-h"]).unwrap(), Cli::Help);
        assert_eq!(
            parse_strs(&["--socket", SOCK, "--help"]).unwrap(),
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
        let err = parse_strs(&["--socket", SOCK, "--app", "demo"]).unwrap_err();
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

    /// The default is the JSON every bridge before the flag served, and the
    /// flag works on either side of the socket selector.
    #[test]
    fn tree_format_defaults_to_json_and_takes_outline() {
        let format = |args: &[&str]| match parse_strs(args) {
            Ok(Cli::Run { tree_format, .. }) => tree_format,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(format(&["--socket", SOCK]), TreeFormat::Json);
        assert_eq!(
            format(&["--socket", SOCK, "--tree-format", "json"]),
            TreeFormat::Json
        );
        assert_eq!(
            format(&["--tree-format", "outline", "--socket", SOCK]),
            TreeFormat::Outline
        );
        assert_eq!(
            format(&["--app", "demo", "--tree-format", "outline"]),
            TreeFormat::Outline
        );
    }

    #[test]
    fn tree_format_without_a_known_value_is_an_error() {
        let err = parse_strs(&["--socket", SOCK, "--tree-format"]).unwrap_err();
        assert!(
            err.contains("--tree-format") && err.contains("json or outline"),
            "a missing value names the flag and its values: {err}"
        );

        for value in ["xml", "JSON", ""] {
            let err = parse_strs(&["--socket", SOCK, "--tree-format", value]).unwrap_err();
            assert!(
                err.contains(&format!("`{value}`")) && err.contains("json or outline"),
                "an unknown value names itself and the accepted ones: {err}"
            );
        }

        // A value is required even when the next word looks like a flag: the
        // flag consumes it, and the word is then refused as a format.
        let err = parse_strs(&["--tree-format", "--socket", SOCK]).unwrap_err();
        assert!(err.contains("`--socket`"), "err: {err}");
    }

    #[test]
    fn help_names_the_tree_format_flag() {
        assert!(HELP.contains("--tree-format <json|outline>"), "{HELP}");
    }

    #[test]
    fn help_names_the_idle_release_flag() {
        assert!(HELP.contains("--idle-release <SECS>"), "{HELP}");
        assert!(
            HELP.contains("0 to 86400")
                && HELP.contains("default 60")
                && HELP.contains("0 never lets go"),
            "the help must say the range, the default and what 0 means: {HELP}"
        );
    }

    /// The default is the minute a held bridge's answer promises, `0` is the
    /// one value that never lets go, and the flag works on either side of the
    /// socket selector.
    #[test]
    fn idle_release_defaults_to_a_minute_and_takes_whole_seconds() {
        let idle = |args: &[&str]| match parse_strs(args) {
            Ok(Cli::Run { idle_release, .. }) => idle_release,
            other => panic!("expected Run, got {other:?}"),
        };
        assert_eq!(DEFAULT_IDLE_RELEASE, Duration::from_secs(60));
        assert_eq!(idle(&["--socket", SOCK]), DEFAULT_IDLE_RELEASE);
        assert_eq!(
            idle(&["--socket", SOCK, "--idle-release", "0"]),
            Duration::ZERO
        );
        assert_eq!(
            idle(&["--idle-release", "300", "--app", "demo"]),
            Duration::from_secs(300)
        );
        // The top of the range, and leading zeros, which are still whole
        // seconds.
        assert_eq!(
            idle(&["--socket", SOCK, "--idle-release", "86400"]),
            Duration::from_secs(MAX_IDLE_RELEASE_SECS)
        );
        assert_eq!(
            idle(&["--socket", SOCK, "--idle-release", "0005"]),
            Duration::from_secs(5)
        );
        assert_eq!(
            idle(&["--socket", SOCK, "--idle-release", "00"]),
            Duration::ZERO
        );
    }

    #[test]
    fn idle_release_without_whole_seconds_is_an_error() {
        let err = parse_strs(&["--socket", SOCK, "--idle-release"]).unwrap_err();
        assert!(
            err.contains("--idle-release") && err.contains("whole seconds from 0 to 86400"),
            "a missing value names the flag and the range: {err}"
        );
        // `+5` and `+0` are what `u64::from_str` takes and this used to; the
        // last two are past the range, the second of them past a `u64` too,
        // and 2^64 - 1 below it used to be taken as a window too far off to
        // reach, which meant "never".
        for value in [
            "",
            "-1",
            "1.5",
            "60s",
            "abc",
            "99999999999999999999999",
            "+5",
            "+0",
            " 5",
            "5 ",
            "5s",
            "-0",
            "1e3",
            "0x10",
            "\u{0665}",
            "86401",
            "18446744073709551615",
            "18446744073709551616",
        ] {
            let err = parse_strs(&["--socket", SOCK, "--idle-release", value]).unwrap_err();
            assert!(
                err.contains("--idle-release")
                    && err.contains(&format!("`{value}`"))
                    && err.contains("whole seconds from 0 to 86400"),
                "a bad value names the flag, the range and itself: {err}"
            );
        }
    }

    /// A path the kernel would refuse must be caught while the user is still
    /// looking at argument errors, not later as a failed connect.
    #[test]
    fn over_long_socket_paths_are_rejected_with_the_limit() {
        let filler = "x".repeat(taria::socket::MAX_SOCKET_PATH_BYTES);
        let long = if cfg!(windows) {
            format!(r"\\.\pipe\{filler}")
        } else {
            format!("/tmp/{filler}.sock")
        };
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
    /// On Windows `--socket` takes a pipe on this machine and nothing else:
    /// a file path would be opened as the file, and `\\host\pipe\...` goes
    /// over SMB. The prefix may be in any case, as Windows takes it.
    #[cfg(windows)]
    #[test]
    fn on_windows_socket_takes_only_a_pipe_on_this_machine() {
        for name in [
            r"C:\Users\me\taria.sock",
            r"\\host\pipe\taria",
            r"\\.\pipe\a\b",
            "/x.sock",
        ] {
            let err = parse_strs(&["--socket", name]).unwrap_err();
            assert!(
                err.starts_with("--socket takes a pipe name on Windows: ") && err.contains(name),
                "{err}"
            );
        }
        assert!(parse_strs(&["--socket", r"\\.\PIPE\taria"]).is_ok());
    }

    /// On Windows a label names this user's pipe by the SID in the process
    /// token, as the adapter names it; `$TARIA_SOCK` overrides it, needs no
    /// SID, and is refused rather than fallen back from when it is not a pipe
    /// on this machine.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_label_names_this_users_pipe() {
        let sid = || Ok("S-1-5-21-1-2-3-1000".to_string());
        let no_sid = || Err(io::Error::other("no token"));
        let default = PathBuf::from(r"\\.\pipe\taria-S-1-5-21-1-2-3-1000-demo");
        assert_eq!(resolve_pipe(None, sid, "demo"), Ok(default.clone()));
        assert_eq!(resolve_pipe(Some("".into()), sid, "demo"), Ok(default));
        assert_eq!(
            resolve_pipe(Some(r"\\.\pipe\custom".into()), no_sid, "demo"),
            Ok(PathBuf::from(r"\\.\pipe\custom"))
        );
        let err = resolve_pipe(Some(r"\\host\pipe\x".into()), sid, "demo").unwrap_err();
        assert!(
            err.contains(r"\\host\pipe\x") && err.contains("TARIA_SOCK"),
            "{err}"
        );
        let err = resolve_pipe(None, no_sid, "demo").unwrap_err();
        assert!(
            err.contains("SID")
                && err.contains("no token")
                && err.contains(r"--socket \\.\pipe\<name>"),
            "{err}"
        );
        let err = resolve_pipe(None, sid, r"a\b").unwrap_err();
        assert!(err.contains("cannot end a pipe name"), "{err}");

        let user = crate::windows_sid::current_user_sid().expect("the token names a user");
        assert_eq!(
            resolve_pipe(None, crate::windows_sid::current_user_sid, "demo"),
            Ok(PathBuf::from(format!(r"\\.\pipe\taria-{user}-demo")))
        );
    }
}
