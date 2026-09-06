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

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

/// Longest socket path the kernel will accept, in bytes.
///
/// `sockaddr_un.sun_path` is 108 bytes including the terminating NUL, leaving
/// 107 for the path itself.
pub const MAX_SOCKET_PATH_BYTES: usize = 107;

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
/// The environment is passed in rather than read here: this keeps the function
/// pure, and lets both call sites test their resolution without mutating
/// process state.
pub fn resolve_path(
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

    /// Length of the rejected path, in bytes.
    pub fn path_len(&self) -> usize {
        self.len
    }
}

impl fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "socket path {} is {} bytes, over the {MAX_SOCKET_PATH_BYTES} byte limit for Unix \
             sockets; set $TARIA_SOCK to a shorter path, for example /tmp/taria.sock",
            self.path.display(),
            self.len,
        )
    }
}

impl Error for SocketPathTooLong {}

/// Check `path` against [`MAX_SOCKET_PATH_BYTES`] before binding or connecting.
///
/// Length is measured with `OsStr::len`, which on unix is the byte length of
/// the path, the same count the kernel applies. Unix domain sockets exist only
/// on unix, so no other measure is needed.
pub fn check_path_len(path: &Path) -> Result<(), SocketPathTooLong> {
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taria_sock_env_wins() {
        let path = resolve_path(
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
        let path = resolve_path(
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
        let path = resolve_path(
            None,
            Some("/run/user/1000".into()),
            Path::new("/tmp"),
            "1000",
            "demo",
        );
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
            let path = resolve_path(sock.clone(), xdg.clone(), Path::new("/tmp"), "1000", "demo");
            assert_eq!(
                path,
                PathBuf::from("/tmp/taria-1000/demo.sock"),
                "sock: {sock:?}, xdg: {xdg:?}"
            );
        }
    }

    #[test]
    fn path_length_boundary_is_inclusive() {
        let at_limit = PathBuf::from("/".repeat(MAX_SOCKET_PATH_BYTES));
        assert_eq!(at_limit.as_os_str().len(), 107);
        assert_eq!(check_path_len(&at_limit), Ok(()));

        let over_limit = PathBuf::from("/".repeat(MAX_SOCKET_PATH_BYTES + 1));
        assert_eq!(over_limit.as_os_str().len(), 108);
        let err = check_path_len(&over_limit).unwrap_err();
        assert_eq!(err.path_len(), 108);
        assert_eq!(err.path(), over_limit);
    }

    #[test]
    fn too_long_error_names_path_length_and_limit() {
        let path = PathBuf::from(format!("/tmp/{}.sock", "x".repeat(120)));
        let err = check_path_len(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("xxxx"), "message: {message}");
        assert!(message.contains("130 bytes"), "message: {message}");
        assert!(message.contains("107 byte"), "message: {message}");
        assert!(message.contains("TARIA_SOCK"), "message: {message}");
    }
}
