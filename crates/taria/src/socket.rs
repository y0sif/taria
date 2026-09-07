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
/// `sockaddr_un.sun_path` is a fixed buffer that has to hold the path and its
/// terminating NUL, so the bound is that buffer minus one, and the buffer is
/// not the same size everywhere. A path between the two sizes binds on Linux
/// and is refused on macOS, by `std` itself before the kernel sees it, which
/// is not a hypothetical band: macOS `$TMPDIR` is around 49 bytes, so the
/// temp-dir fallback plus a long app label lands in it.
pub const MAX_SOCKET_PATH_BYTES: usize = SUN_PATH_BYTES - 1;

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
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
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

    /// Length of the rejected path, in bytes.
    pub fn path_len(&self) -> usize {
        self.len
    }
}

impl fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The limit is named with the platform it belongs to: it differs
        // across unixes, so a bare number sends someone comparing it against
        // the wrong `sun_path` when the same path works on their other
        // machine.
        write!(
            f,
            "socket path {} is {} bytes, over the {MAX_SOCKET_PATH_BYTES} byte limit for Unix \
             sockets on {}; set $TARIA_SOCK to a shorter path, for example /tmp/taria.sock",
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
            target_os = "dragonfly"
        )))]
        assert_eq!(
            MAX_SOCKET_PATH_BYTES, 107,
            "this platform declares sun_path[108]"
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
}
