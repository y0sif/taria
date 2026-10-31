//! Where an app's Unix socket lives, and whether the kernel will accept the
//! path.
//!
//! The app binds the socket and the bridge connects to it, so the two have to
//! derive the same default path from the same environment or they simply never
//! meet. This module is that derivation, kept here rather than copied into each
//! side.
//!
//! Path building only: nothing here touches the filesystem, so the core crate
//! stays free of I/O. Creating, vetting, and removing the socket belong to the
//! adapter.
//!
//! On Windows the endpoint will be a named pipe rather than a Unix socket, and
//! [`pipe_name`], [`check_pipe_name`] and [`resolve_pipe_name`] are what the
//! Windows transport will use to name it. They are as pure as the rest, so
//! they build and are tested on every platform.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Longest socket path the kernel will accept, in bytes.
///
/// `sockaddr_un.sun_path` is a fixed buffer that has to hold the path and its
/// terminating NUL, so the bound is that buffer minus one, and the buffer is
/// not the same size everywhere. A path between the two sizes binds on Linux
/// and is refused on macOS, by `std` itself before the kernel sees it, which
/// is not a hypothetical band: macOS `$TMPDIR` is around 49 bytes, so the
/// temp-dir fallback plus a long app label lands in it.
///
/// On Windows, where taria's endpoint is a named pipe, it is
/// [`MAX_PIPE_NAME_CHARS`] instead, counted in UTF-16 code units.
#[cfg(not(windows))]
pub const MAX_SOCKET_PATH_BYTES: usize = SUN_PATH_BYTES - 1;

/// Longest endpoint name the system will accept: on Windows, where taria's
/// endpoint is a named pipe, the pipe-name limit [`MAX_PIPE_NAME_CHARS`],
/// counted in UTF-16 code units rather than bytes.
///
/// Elsewhere it is the AF_UNIX `sun_path` buffer minus its NUL, in bytes.
#[cfg(windows)]
pub const MAX_SOCKET_PATH_BYTES: usize = MAX_PIPE_NAME_CHARS;

/// `sun_path` on the BSD-derived unixes, Apple's included: `char
/// sun_path[104]`, unchanged since 4.4BSD.
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
const SUN_PATH_BYTES: usize = 104;

/// `sun_path` on Linux and the other unixes taria builds for: 108 bytes.
/// Windows has no entry here: its endpoint is a named pipe, whose limit is
/// [`MAX_PIPE_NAME_CHARS`].
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
    windows
)))]
const SUN_PATH_BYTES: usize = 108;

/// Resolve the default socket path for `app_label`.
///
/// Precedence, highest first:
///
/// 1. `taria_sock` (`$TARIA_SOCK`) verbatim, when set and non-empty. An
///    explicit path is an override, so it is used exactly as given.
/// 2. `<xdg_runtime_dir>/taria/<app_label>.sock`, when `$XDG_RUNTIME_DIR` is
///    set and non-empty. This is the per-user runtime directory, already
///    private and already cleaned up at logout.
/// 3. `<temp_dir>/taria-<user>/<app_label>.sock`. The temp dir is shared, so
///    `user` namespaces it to keep one user's sockets out of another's reach.
///
/// The label becomes a file name here, so it has to be one:
/// [`InvalidAppLabel`] is returned for anything that is not a single plain
/// component. It is checked before the precedence above rather than inside
/// the two branches that use it, so a label is acceptable or not on its own
/// terms and cannot pass on a machine that happens to set `$TARIA_SOCK` and
/// fail on the next one. An app that wants a path this rejects passes the
/// path itself, which is what the override and the adapter's `bind_at` are
/// for.
///
/// The environment is passed in rather than read here: this keeps the function
/// pure, and lets both call sites test their resolution without mutating
/// process state.
pub fn resolve_path(
    taria_sock: Option<OsString>,
    xdg_runtime_dir: Option<OsString>,
    temp_dir: &Path,
    user: &str,
    app_label: &str,
) -> Result<PathBuf, InvalidAppLabel> {
    if !is_plain_file_name(app_label) {
        return Err(InvalidAppLabel {
            label: app_label.to_string(),
        });
    }
    if let Some(path) = taria_sock
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    if let Some(dir) = xdg_runtime_dir
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir)
            .join("taria")
            .join(format!("{app_label}.sock")));
    }
    Ok(temp_dir
        .join(format!("taria-{user}"))
        .join(format!("{app_label}.sock")))
}

/// Is `label` a single plain path component, safe to interpolate into a file
/// name?
///
/// The label is formatted into `<label>.sock`, not joined as a component, so
/// this is a string check and not a [`Path`] one: `Path` normalizes a
/// trailing separator away, and `"app/"` would pass such a check and then
/// build `.../app/.sock`, a different file in a different directory.
fn is_plain_file_name(label: &str) -> bool {
    !label.is_empty()
        && label != "."
        && label != ".."
        && !label.contains('/')
        && !label.contains('\0')
}

/// An app label that cannot be turned into a socket file name.
///
/// Refused rather than interpolated because the adapter binds *and unlinks*
/// what this module resolves: `/etc/cron.d/evil` as a label makes
/// [`Path::join`] discard the whole resolution and keep the absolute path,
/// and `../../../tmp/pwn` walks out of the runtime directory. The label is
/// argv-sourced today, so this is not a privilege boundary; it is the
/// difference between a bad argument reported as one and a bad argument
/// deleting a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidAppLabel {
    label: String,
}

impl InvalidAppLabel {
    /// The label that was rejected.
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Display for InvalidAppLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "app label {:?} is not a file name; it must be a single path component, so no `/`, \
             and not `.`, `..` or empty. Pass the socket path itself to use a path of your own.",
            self.label,
        )
    }
}

impl Error for InvalidAppLabel {}

/// A socket path the kernel will refuse because it exceeds
/// [`MAX_SOCKET_PATH_BYTES`].
///
/// Worth its own error because the one `std` produces is
/// `InvalidInput: path must be shorter than SUN_LEN`, which names neither the
/// path, nor its length, nor the limit, nor a way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    path: PathBuf,
    len: usize,
}

impl SocketPathTooLong {
    /// The path that was rejected.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Length of the rejected path, in bytes (in UTF-16 code units on
    /// Windows, where the path is a pipe name).
    pub fn path_len(&self) -> usize {
        self.len
    }
}

impl fmt::Display for SocketPathTooLong {
    #[cfg(windows)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pipe name {} is {} characters, over the {MAX_SOCKET_PATH_BYTES} character limit for \
             named pipes on {}; set $TARIA_SOCK to a shorter name of the form \
             {LOCAL_PIPE_PREFIX}<name>",
            self.path.display(),
            self.len,
            std::env::consts::OS,
        )
    }

    #[cfg(not(windows))]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The limit is named with the platform it belongs to: it differs
        // across unixes, so a bare number sends someone comparing it against
        // the wrong `sun_path` when the same path works on their other
        // machine.
        write!(
            f,
            "socket path {} is {} bytes, over the {MAX_SOCKET_PATH_BYTES} byte limit for Unix \
             sockets on {}; set $TARIA_SOCK to a shorter path inside a directory only you can \
             reach, for example $XDG_RUNTIME_DIR/t.sock, or ~/.taria/t.sock where that variable \
             is unset. A shared directory such as /tmp will not work: the adapter binds only \
             under a directory you own that grants no group or other access, and creates a \
             missing one with mode 0700",
            self.path.display(),
            self.len,
            std::env::consts::OS,
        )
    }
}

impl Error for SocketPathTooLong {}

/// Check `path` against [`MAX_SOCKET_PATH_BYTES`] before binding or connecting.
///
/// Length is measured with `OsStr::len`, which on unix is the byte length of
/// the path, the same count the kernel applies. On Windows the path is a pipe
/// name, and it is measured the way Windows counts one, in UTF-16 code units.
pub fn check_path_len(path: &Path) -> Result<(), SocketPathTooLong> {
    let len = endpoint_len(path);
    if len > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

/// The length [`check_path_len`] holds against the limit: bytes on unix.
#[cfg(not(windows))]
fn endpoint_len(path: &Path) -> usize {
    path.as_os_str().len()
}

/// The length [`check_path_len`] holds against the limit: UTF-16 code units
/// on Windows, which is what the pipe-name limit counts.
#[cfg(windows)]
fn endpoint_len(path: &Path) -> usize {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().count()
}

/// The prefix of every named pipe on this machine: `\\.\pipe\`.
///
/// Part of what the Windows transport will use. A pipe name is a path into
/// the pipe namespace of some machine, and this one is the local machine's.
/// `\\host\pipe\name` is the same pipe on another machine, reached over SMB,
/// so it is not merely another spelling: see [`check_pipe_name`].
pub const LOCAL_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Longest name a Windows named pipe may have, [`LOCAL_PIPE_PREFIX`]
/// included, in UTF-16 code units (Windows characters).
///
/// Part of what the Windows transport will use. Windows documents the whole
/// pipe name string as at most 256 characters, so the bound is on the name
/// as a whole, not on the part after the prefix.
pub const MAX_PIPE_NAME_CHARS: usize = 256;

/// Build the default pipe name for `app_label`, run by the user whose SID
/// string is `user`: `\\.\pipe\taria-<user>-<label>`.
///
/// What the Windows transport names its pipe by, in place of
/// [`resolve_path`]'s socket paths, through [`resolve_pipe_name`]. Pipes
/// share one namespace per machine rather than living in a per-user
/// directory, so the user's SID is in the name to keep one user's pipes apart
/// from another's.
///
/// The name only keeps them apart, and the safety does not come from it: any
/// process can create a pipe under any name first. It comes from four things
/// the transports do with the name. The adapter creates the pipe with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so it never joins a pipe something else
/// created first, and with `PIPE_REJECT_REMOTE_CLIENTS`, so no other machine
/// can connect, and gives it a DACL that lets only its own user in; the
/// bridge checks that the pipe it opened is owned by its own user before it
/// reads or writes anything.
///
/// Windows trims trailing dots and spaces from a name as it opens it, so a
/// label `app.` names the same pipe as `app`. Both peers pass the name to
/// Windows, which normalises it alike for each, so they still meet.
///
/// `user` is a SID string, as `ConvertSidToStringSidW` writes one
/// (`S-1-5-21-...`), and is supplied by the caller, which keeps this function
/// pure. Anything else is refused with [`InvalidPipeName::User`]: it is
/// interpolated into the name, so a value that is not a SID could carry a
/// separator into it.
///
/// The label is held to the rule [`resolve_path`] holds it to, and also may
/// not contain `\`, which on Windows is a path separator: anything else is
/// [`InvalidPipeName::Label`]. A name over [`MAX_PIPE_NAME_CHARS`] is
/// [`InvalidPipeName::TooLong`].
pub fn pipe_name(app_label: &str, user: &str) -> Result<String, InvalidPipeName> {
    if !is_pipe_label(app_label) {
        return Err(InvalidPipeName::Label(app_label.to_string()));
    }
    if !is_sid_string(user) {
        return Err(InvalidPipeName::User(user.to_string()));
    }
    let name = format!("{LOCAL_PIPE_PREFIX}taria-{user}-{app_label}");
    check_pipe_name_len(&name)?;
    Ok(name)
}

/// Check a pipe name given whole, such as `$TARIA_SOCK`, before binding or
/// connecting to it.
///
/// Part of what the Windows transport will use, for the override and for an
/// explicit name passed to the adapter or the bridge. The name is honoured
/// only if it starts with [`LOCAL_PIPE_PREFIX`], in any letter case, since
/// Windows matches the prefix without regard to case (`\\.\PIPE\name` is the
/// same pipe as `\\.\pipe\name`): anything else is
/// [`InvalidPipeName::NotLocal`], `\\host\pipe\name` included, because
/// opening a pipe on another machine goes over SMB, which hands that machine
/// the user's credentials and puts the agent's input on the network.
///
/// The pipe's own name, after the prefix, must be one name. A `\` or `/` in
/// it, a NUL, or nothing but dots and spaces is [`InvalidPipeName::Name`]:
/// Windows normalizes a `\\.\` path before opening it, folding `/` into `\`,
/// resolving `.` and `..` and trimming trailing dots and spaces, so
/// `\\.\pipe\..\UNC\host\share` would leave the pipe namespace for SMB after
/// passing a prefix check, and a NUL would end the name early. A name over
/// [`MAX_PIPE_NAME_CHARS`] is [`InvalidPipeName::TooLong`].
pub fn check_pipe_name(name: &str) -> Result<(), InvalidPipeName> {
    // The prefix is ASCII, so a name whose first bytes split a character
    // cannot start with it, and `get` says so by returning `None`.
    let prefix_len = LOCAL_PIPE_PREFIX.len();
    let Some(own) = name
        .get(..prefix_len)
        .filter(|prefix| prefix.eq_ignore_ascii_case(LOCAL_PIPE_PREFIX))
        .map(|_| &name[prefix_len..])
    else {
        return Err(InvalidPipeName::NotLocal(name.to_string()));
    };
    let separated = own.contains(['\\', '/', '\0']);
    if separated || own.trim_end_matches(['.', ' ']).is_empty() {
        return Err(InvalidPipeName::Name(name.to_string()));
    }
    check_pipe_name_len(name)
}

/// Resolve the pipe name for `app_label`, the Windows counterpart of
/// [`resolve_path`] and part of what the Windows transport will use.
///
/// Precedence, highest first:
///
/// 1. `taria_sock` (`$TARIA_SOCK`), when set and non-empty, if it passes
///    [`check_pipe_name`]. An override the check refuses is an error, not a
///    reason to fall back: the user named an endpoint, and serving or
///    connecting somewhere else would leave the two peers apart with nothing
///    said. One that is not valid Unicode cannot name a pipe either, and is
///    [`InvalidPipeName::NotLocal`].
/// 2. [`pipe_name`] for `app_label` and `user`.
///
/// The label is checked before the precedence above, as [`resolve_path`]
/// checks it, so a label is acceptable or not on its own terms whatever the
/// environment says.
pub fn resolve_pipe_name(
    taria_sock: Option<OsString>,
    user: &str,
    app_label: &str,
) -> Result<String, InvalidPipeName> {
    if !is_pipe_label(app_label) {
        return Err(InvalidPipeName::Label(app_label.to_string()));
    }
    if let Some(name) = taria_sock
        && !name.is_empty()
    {
        let Some(name) = name.to_str() else {
            return Err(InvalidPipeName::NotLocal(
                name.to_string_lossy().into_owned(),
            ));
        };
        check_pipe_name(name)?;
        return Ok(name.to_string());
    }
    pipe_name(app_label, user)
}

/// Is `label` fit to end a pipe name? A plain file name, as for a socket,
/// and no `\` either, which Windows reads as a separator.
fn is_pipe_label(label: &str) -> bool {
    is_plain_file_name(label) && !label.contains('\\')
}

/// Is `user` a SID string: `S-` and then fields of ASCII letters and digits,
/// separated by single dashes?
///
/// Loose on the digits, since an identifier authority of 2^32 or more is
/// written in hex, and strict on everything that could act as a separator.
fn is_sid_string(user: &str) -> bool {
    user.strip_prefix("S-").is_some_and(|fields| {
        fields.split('-').all(|field| {
            !field.is_empty() && field.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    })
}

/// Refuse a pipe name over [`MAX_PIPE_NAME_CHARS`] UTF-16 code units.
fn check_pipe_name_len(name: &str) -> Result<(), InvalidPipeName> {
    let len = name.encode_utf16().count();
    if len > MAX_PIPE_NAME_CHARS {
        return Err(InvalidPipeName::TooLong {
            name: name.to_string(),
            len,
        });
    }
    Ok(())
}

/// A pipe name that cannot be built or used, from [`pipe_name`],
/// [`check_pipe_name`] or [`resolve_pipe_name`].
///
/// Part of what the Windows transport will use. Each variant carries the
/// value refused, so the message can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidPipeName {
    /// The app label is not a single plain name: it has a `/` or `\`, is
    /// `.`, `..` or empty, or holds a NUL.
    Label(String),
    /// The user is not a SID string.
    User(String),
    /// The name does not start with [`LOCAL_PIPE_PREFIX`], so it is not a
    /// pipe on this machine.
    NotLocal(String),
    /// The part after [`LOCAL_PIPE_PREFIX`] is not one name.
    Name(String),
    /// The name is over [`MAX_PIPE_NAME_CHARS`].
    TooLong {
        /// The name refused.
        name: String,
        /// Its length in UTF-16 code units.
        len: usize,
    },
}

impl fmt::Display for InvalidPipeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Label(label) => write!(
                f,
                "app label {label:?} cannot end a pipe name; it must be a single name, so no `/` \
                 or `\\`, and not `.`, `..` or empty. Set $TARIA_SOCK to a pipe name of your own, \
                 {LOCAL_PIPE_PREFIX}<name>, to use one."
            ),
            Self::User(user) => write!(
                f,
                "{user:?} is not a SID string (S-1-...), so it cannot keep this user's pipes apart \
                 from another's"
            ),
            Self::NotLocal(name) => write!(
                f,
                "`{name}` does not start with {LOCAL_PIPE_PREFIX}, so it is not taken for a pipe \
                 on this machine; $TARIA_SOCK must name one that way. A pipe on another machine \
                 (\\\\host\\pipe\\...) is refused, because opening it goes over SMB"
            ),
            Self::Name(name) => write!(
                f,
                "`{name}` names no single pipe: after {LOCAL_PIPE_PREFIX} it needs one name, with \
                 no `\\` or `/`, and not empty or only dots and spaces"
            ),
            Self::TooLong { name, len } => write!(
                f,
                "pipe name `{name}` is {len} characters, over the {MAX_PIPE_NAME_CHARS} character \
                 limit for named pipes; use a shorter app label, or set $TARIA_SOCK to a shorter \
                 name of the form {LOCAL_PIPE_PREFIX}<name>"
            ),
        }
    }
}

impl Error for InvalidPipeName {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution for a label the tests are not testing.
    fn resolved(
        taria_sock: Option<OsString>,
        xdg_runtime_dir: Option<OsString>,
        label: &str,
    ) -> PathBuf {
        resolve_path(
            taria_sock,
            xdg_runtime_dir,
            Path::new("/tmp"),
            "1000",
            label,
        )
        .expect("a plain label resolves")
    }

    #[test]
    fn taria_sock_env_wins() {
        let path = resolved(
            Some("/custom/app.sock".into()),
            Some("/run/user/1000".into()),
            "demo",
        );
        assert_eq!(path, PathBuf::from("/custom/app.sock"));
    }

    #[test]
    fn empty_taria_sock_is_ignored() {
        let path = resolved(Some("".into()), Some("/run/user/1000".into()), "demo");
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn xdg_runtime_dir_is_second_choice() {
        let path = resolved(None, Some("/run/user/1000".into()), "demo");
        assert_eq!(path, PathBuf::from("/run/user/1000/taria/demo.sock"));
    }

    #[test]
    fn temp_dir_is_the_fallback() {
        let cases = [
            (None, None),
            (None, Some(OsString::from(""))),
            (Some(OsString::from("")), Some(OsString::from(""))),
        ];
        for (sock, xdg) in cases {
            let path = resolved(sock.clone(), xdg.clone(), "demo");
            assert_eq!(
                path,
                PathBuf::from("/tmp/taria-1000/demo.sock"),
                "sock: {sock:?}, xdg: {xdg:?}"
            );
        }
    }

    /// The two shapes that make a label more than a file name. Both used to
    /// resolve to a path the adapter would bind and, on the way, unlink:
    /// `Path::join` keeps an absolute label and discards everything resolved
    /// before it, and a relative one walks up out of the runtime directory.
    #[test]
    fn a_label_that_is_not_a_file_name_is_refused() {
        for label in [
            "/etc/cron.d/evil",
            "../../../tmp/pwn",
            "sub/dir",
            "app/",
            ".",
            "..",
            "",
            "nul\0byte",
        ] {
            let err = resolve_path(
                None,
                Some("/run/user/1000".into()),
                Path::new("/tmp"),
                "1000",
                label,
            )
            .unwrap_err();
            assert_eq!(err.label(), label);
        }
    }

    /// The override is not a way around the check: a label is acceptable or
    /// not on its own, so the same argument cannot bind here and be refused
    /// on a machine that sets the variable differently.
    #[test]
    fn a_bad_label_is_refused_even_when_the_override_would_hide_it() {
        assert!(
            resolve_path(
                Some("/custom/app.sock".into()),
                None,
                Path::new("/tmp"),
                "1000",
                "../pwn",
            )
            .is_err()
        );
    }

    /// A plain label may still hold anything a file name may hold; the check
    /// is about components, not about characters someone finds surprising.
    #[test]
    fn plain_labels_with_awkward_characters_still_resolve() {
        for label in ["my app", "app.v2", "..app", "app..", "-", "app:1"] {
            let path = resolved(None, Some("/run/user/1000".into()), label);
            assert_eq!(
                path,
                PathBuf::from(format!("/run/user/1000/taria/{label}.sock")),
                "label: {label}"
            );
        }
    }

    #[test]
    fn the_error_names_the_label_and_what_a_label_may_be() {
        let err = resolve_path(None, None, Path::new("/tmp"), "1000", "../pwn").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("../pwn"), "message: {message}");
        assert!(
            message.contains("single path component"),
            "message: {message}"
        );
    }

    #[test]
    fn path_length_boundary_is_inclusive() {
        let at_limit = PathBuf::from("/".repeat(MAX_SOCKET_PATH_BYTES));
        assert_eq!(at_limit.as_os_str().len(), MAX_SOCKET_PATH_BYTES);
        assert_eq!(check_path_len(&at_limit), Ok(()));

        let over_limit = PathBuf::from("/".repeat(MAX_SOCKET_PATH_BYTES + 1));
        let err = check_path_len(&over_limit).unwrap_err();
        assert_eq!(err.path_len(), MAX_SOCKET_PATH_BYTES + 1);
        assert_eq!(err.path(), over_limit);
    }

    /// The limit is the platform's, not Linux's everywhere. Both bounds are
    /// spelled out because the wrong one is silent: it passes the check and
    /// then fails inside `bind`, with the kernel error this module exists to
    /// replace.
    #[test]
    fn the_limit_is_the_platforms_sun_path_minus_the_nul() {
        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        assert_eq!(
            MAX_SOCKET_PATH_BYTES, 103,
            "this platform declares sun_path[104]"
        );
        #[cfg(not(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            windows
        )))]
        assert_eq!(
            MAX_SOCKET_PATH_BYTES, 107,
            "this platform declares sun_path[108]"
        );
        #[cfg(windows)]
        assert_eq!(
            MAX_SOCKET_PATH_BYTES, MAX_PIPE_NAME_CHARS,
            "the endpoint here is a named pipe, not a Unix socket"
        );
    }

    /// The band between the two `sun_path` sizes: every length in it binds on
    /// Linux and is refused on macOS, and it is exactly where the macOS temp
    /// dir plus a long app label lands.
    #[test]
    fn the_band_between_the_two_sun_path_sizes_follows_the_platform() {
        for len in 104..=107 {
            let path = PathBuf::from("/".repeat(len));
            let checked = check_path_len(&path);
            if MAX_SOCKET_PATH_BYTES == 103 {
                assert!(checked.is_err(), "{len} bytes does not fit sun_path[104]");
            } else {
                assert_eq!(checked, Ok(()), "{len} bytes fits sun_path[108]");
            }
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn too_long_error_names_path_length_limit_and_platform() {
        let path = PathBuf::from(format!("/tmp/{}.sock", "x".repeat(120)));
        let err = check_path_len(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("xxxx"), "message: {message}");
        assert!(message.contains("130 bytes"), "message: {message}");
        assert!(
            message.contains(&format!("{MAX_SOCKET_PATH_BYTES} byte")),
            "message: {message}"
        );
        assert!(
            message.contains(std::env::consts::OS),
            "the limit is per platform, so the message must name this one: {message}"
        );
        assert!(message.contains("TARIA_SOCK"), "message: {message}");
    }

    /// On Windows the limit is the pipe name's, counted the way Windows
    /// counts it, and the message says it is about a pipe.
    #[cfg(windows)]
    #[test]
    fn too_long_error_on_windows_names_the_pipe_limit() {
        let name = PathBuf::from(format!("{LOCAL_PIPE_PREFIX}{}", "x".repeat(300)));
        let err = check_path_len(&name).unwrap_err();
        assert_eq!(err.path_len(), 309);
        let message = err.to_string();
        assert!(message.contains("309 characters"), "message: {message}");
        assert!(message.contains("256 character"), "message: {message}");
        assert!(message.contains(LOCAL_PIPE_PREFIX), "message: {message}");
    }

    /// A SID string as Windows writes one for a local account.
    const SID: &str = "S-1-5-21-3623811015-3361044348-30300820-1013";

    #[test]
    fn a_pipe_is_named_for_the_user_and_the_label() {
        let name = pipe_name("demo", SID).expect("a plain label and a SID name a pipe");
        assert_eq!(
            name,
            r"\\.\pipe\taria-S-1-5-21-3623811015-3361044348-30300820-1013-demo"
        );
        assert_eq!(
            check_pipe_name(&name),
            Ok(()),
            "the default name is one the override check accepts"
        );
    }

    /// The label rule for a socket file, plus `\`, which Windows reads as a
    /// separator, so `a\b` would name a pipe inside a pipe namespace path.
    #[test]
    fn a_pipe_label_must_be_one_name_with_no_backslash() {
        for label in [
            r"a\b",
            r"\",
            r"..\..\x",
            r"\\host\pipe\x",
            "/etc/cron.d/evil",
            "sub/dir",
            ".",
            "..",
            "",
            "nul\0byte",
        ] {
            assert_eq!(
                pipe_name(label, SID),
                Err(InvalidPipeName::Label(label.to_string())),
                "label: {label:?}"
            );
            // Refused before the override is looked at, as `resolve_path`
            // refuses one.
            assert_eq!(
                resolve_pipe_name(Some(r"\\.\pipe\custom".into()), SID, label),
                Err(InvalidPipeName::Label(label.to_string())),
                "label: {label:?}"
            );
        }
        // The socket-path rule itself is untouched: `\` is an ordinary
        // character in a Unix file name.
        #[cfg(unix)]
        assert!(resolve_path(None, None, Path::new("/tmp"), "1000", r"a\b").is_ok());
    }

    #[test]
    fn plain_labels_with_awkward_characters_still_name_a_pipe() {
        for label in ["my app", "app.v2", "..app", "app..", "-", "app:1"] {
            assert_eq!(
                pipe_name(label, SID),
                Ok(format!(r"\\.\pipe\taria-{SID}-{label}")),
                "label: {label:?}"
            );
        }
    }

    /// The user is interpolated into the name, so only a SID string is
    /// taken, and nothing in one can act as a separator.
    #[test]
    fn the_user_must_be_a_sid_string() {
        for user in [
            "S-1-5-18",
            SID,
            // An identifier authority past 32 bits is written in hex.
            "S-1-0x123456789ABC-1",
        ] {
            assert!(pipe_name("demo", user).is_ok(), "user: {user:?}");
        }
        for user in [
            "",
            "S-",
            "S--",
            "S-1-",
            "S-1--5",
            "s-1-5-18",
            "1000",
            "alice",
            r"S-1-5\x",
            "S-1-5/x",
            "S-1-5-18 ",
            "S-1-5-..",
        ] {
            assert_eq!(
                pipe_name("demo", user),
                Err(InvalidPipeName::User(user.to_string())),
                "user: {user:?}"
            );
        }
    }

    /// Only a pipe on this machine is honoured. Every other spelling is
    /// refused rather than normalized into one, `\\host\pipe\` first among
    /// them, because opening a remote pipe goes over SMB.
    #[test]
    fn an_override_must_name_a_pipe_on_this_machine() {
        for name in [
            r"\\host\pipe\taria",
            r"\\192.168.0.7\pipe\taria",
            r"\\?\pipe\taria",
            "//./pipe/taria",
            r"\\.\pipetaria",
            r"\\.\pipe",
            r"C:\Users\me\taria.sock",
            "/run/user/1000/taria/demo.sock",
            "",
        ] {
            assert_eq!(
                check_pipe_name(name),
                Err(InvalidPipeName::NotLocal(name.to_string())),
                "name: {name:?}"
            );
        }
    }

    /// Windows matches the prefix in any letter case, so `\\.\PIPE\taria` is
    /// the local pipe `\\.\pipe\taria` and is honoured as one, rather than
    /// refused as a pipe on another machine, which it is not. The rest of the
    /// name is held to the same rules whatever the prefix's case, and a
    /// prefix that only looks alike in some other script is still refused.
    #[test]
    fn the_local_prefix_is_matched_in_any_letter_case() {
        for name in [r"\\.\PIPE\taria", r"\\.\Pipe\taria", r"\\.\pIpE\taria"] {
            assert_eq!(check_pipe_name(name), Ok(()), "name: {name:?}");
            assert_eq!(
                resolve_pipe_name(Some(name.into()), SID, "demo"),
                Ok(name.to_string()),
                "an override is used as given: {name:?}"
            );
        }
        for name in [r"\\.\PIPE\a\b", r"\\.\PIPE\..", r"\\.\PIPE\"] {
            assert_eq!(
                check_pipe_name(name),
                Err(InvalidPipeName::Name(name.to_string())),
                "name: {name:?}"
            );
        }
        let long = format!(r"\\.\PIPE\{}", "x".repeat(MAX_PIPE_NAME_CHARS));
        assert!(matches!(
            check_pipe_name(&long),
            Err(InvalidPipeName::TooLong { .. })
        ));
        // Not the prefix in another case: a Cyrillic `р` is not `p`, an `é`
        // is not `e`, and `\\?\` is another prefix altogether. A character
        // that straddles the prefix's length is refused too, not sliced
        // through.
        for name in [
            "\\\\.\\\u{440}ipe\\taria",
            "\\\\.\\pip\u{e9}\\taria",
            r"\\?\PIPE\taria",
            "\\\\.\\pipe\u{212A}taria",
        ] {
            assert_eq!(
                check_pipe_name(name),
                Err(InvalidPipeName::NotLocal(name.to_string())),
                "name: {name:?}"
            );
        }
    }

    /// The part after the prefix has to be a single name: Windows normalizes
    /// a `\\.\` path before opening it, so a separator or a dot segment
    /// there could walk out of the pipe namespace after the prefix check.
    #[test]
    fn an_override_must_name_one_pipe() {
        for name in [
            r"\\.\pipe\",
            r"\\.\pipe\..\UNC\host\share",
            r"\\.\pipe\..\..\C:\Windows\win.ini",
            r"\\.\pipe\a\b",
            r"\\.\pipe\a/b",
            r"\\.\pipe\.",
            r"\\.\pipe\..",
            r"\\.\pipe\ . ",
            "\\\\.\\pipe\\a\0b",
        ] {
            assert_eq!(
                check_pipe_name(name),
                Err(InvalidPipeName::Name(name.to_string())),
                "name: {name:?}"
            );
        }
        for name in [
            r"\\.\pipe\taria",
            r"\\.\pipe\my pipe",
            r"\\.\pipe\..taria",
            r"\\.\pipe\taria.",
            r"\\.\pipe\taria:1",
        ] {
            assert_eq!(check_pipe_name(name), Ok(()), "name: {name:?}");
        }
    }

    /// 256 characters for the whole name, prefix included, counted in UTF-16
    /// code units: a character outside the BMP is two of them.
    #[test]
    fn the_pipe_name_limit_is_the_whole_name_in_utf16_units() {
        let prefix = LOCAL_PIPE_PREFIX.encode_utf16().count();
        let at_limit = format!(
            "{LOCAL_PIPE_PREFIX}{}",
            "x".repeat(MAX_PIPE_NAME_CHARS - prefix)
        );
        assert_eq!(check_pipe_name(&at_limit), Ok(()));
        let over = format!("{at_limit}x");
        assert_eq!(
            check_pipe_name(&over),
            Err(InvalidPipeName::TooLong {
                name: over.clone(),
                len: MAX_PIPE_NAME_CHARS + 1
            })
        );

        // One unit short of the limit, then a character that takes two.
        let short = format!(
            "{LOCAL_PIPE_PREFIX}{}",
            "x".repeat(MAX_PIPE_NAME_CHARS - prefix - 1)
        );
        assert_eq!(check_pipe_name(&format!("{short}\u{e9}")), Ok(()));
        assert!(matches!(
            check_pipe_name(&format!("{short}\u{1F600}")),
            Err(InvalidPipeName::TooLong { len, .. }) if len == MAX_PIPE_NAME_CHARS + 1
        ));

        // The default name is held to the same limit, so a long label is
        // refused rather than handed to Windows to truncate or reject.
        let fixed = format!("{LOCAL_PIPE_PREFIX}taria-{SID}-")
            .encode_utf16()
            .count();
        let longest = "l".repeat(MAX_PIPE_NAME_CHARS - fixed);
        assert!(pipe_name(&longest, SID).is_ok());
        assert!(matches!(
            pipe_name(&format!("{longest}l"), SID),
            Err(InvalidPipeName::TooLong { len, .. }) if len == MAX_PIPE_NAME_CHARS + 1
        ));
    }

    #[test]
    fn a_valid_override_wins_and_an_empty_one_is_ignored() {
        assert_eq!(
            resolve_pipe_name(Some(r"\\.\pipe\custom".into()), SID, "demo"),
            Ok(r"\\.\pipe\custom".to_string())
        );
        for unset in [None, Some(OsString::new())] {
            assert_eq!(
                resolve_pipe_name(unset.clone(), SID, "demo"),
                pipe_name("demo", SID),
                "override: {unset:?}"
            );
        }
        // The user only names the default; an override needs no SID.
        assert!(resolve_pipe_name(Some(r"\\.\pipe\custom".into()), "", "demo").is_ok());
        assert_eq!(
            resolve_pipe_name(None, "", "demo"),
            Err(InvalidPipeName::User(String::new()))
        );
    }

    /// An override the check refuses is an error, never a reason to serve or
    /// connect at the default name instead, where the other peer is not.
    #[test]
    fn a_refused_override_is_an_error_not_a_fallback() {
        assert_eq!(
            resolve_pipe_name(Some(r"\\host\pipe\custom".into()), SID, "demo"),
            Err(InvalidPipeName::NotLocal(r"\\host\pipe\custom".to_string()))
        );
        assert_eq!(
            resolve_pipe_name(Some(r"\\.\pipe\a\b".into()), SID, "demo"),
            Err(InvalidPipeName::Name(r"\\.\pipe\a\b".to_string()))
        );
    }

    /// An override that is not valid Unicode cannot be a pipe name.
    #[cfg(unix)]
    #[test]
    fn an_override_that_is_not_unicode_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(b"\\\\.\\pipe\\\xff".to_vec());
        assert!(matches!(
            resolve_pipe_name(Some(name), SID, "demo"),
            Err(InvalidPipeName::NotLocal(_))
        ));
    }

    #[test]
    fn pipe_errors_name_the_value_and_the_rule() {
        let message = check_pipe_name(r"\\host\pipe\taria")
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(r"`\\host\pipe\taria`"),
            "message: {message}"
        );
        assert!(message.contains(LOCAL_PIPE_PREFIX), "message: {message}");
        assert!(message.contains("SMB"), "message: {message}");

        let message = check_pipe_name(r"\\.\pipe\a\b").unwrap_err().to_string();
        assert!(message.contains(r"`\\.\pipe\a\b`"), "message: {message}");
        assert!(message.contains("one name"), "message: {message}");

        let message = pipe_name(r"a\b", SID).unwrap_err().to_string();
        assert!(message.contains(r#""a\\b""#), "message: {message}");
        assert!(message.contains(r"`\`"), "message: {message}");
        assert!(message.contains("TARIA_SOCK"), "message: {message}");

        let message = pipe_name("demo", "alice").unwrap_err().to_string();
        assert!(message.contains("alice"), "message: {message}");
        assert!(message.contains("SID"), "message: {message}");

        let long = format!("{LOCAL_PIPE_PREFIX}{}", "x".repeat(300));
        let message = check_pipe_name(&long).unwrap_err().to_string();
        assert!(message.contains("309 characters"), "message: {message}");
        assert!(
            message.contains(&format!("{MAX_PIPE_NAME_CHARS} character")),
            "message: {message}"
        );
        assert!(message.contains("TARIA_SOCK"), "message: {message}");
    }
}
