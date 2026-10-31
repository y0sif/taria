//! The Windows transport: a named pipe, at the name the bridge was given, which
//! [`taria::socket::resolve_pipe_name`] derives the way the app-side adapter
//! does.
//!
//! The app's pipe has one instance, so a second client's open fails with
//! `ERROR_PIPE_BUSY` while the first is served. That is the only sign Windows
//! gives of an app held by another bridge: there is no listen backlog to wait
//! in, so a connection that opens is one the app's pipe took, and silence on
//! it is the app's own (see [`BACKLOG`]).
//!
//! Pipe names are machine-wide, so any process can create a pipe under the
//! app's name before the app does. Every connection is checked to be to a
//! pipe owned by this bridge's own user, and refused unread and unwritten
//! otherwise ([`Refusal::Foreign`]). The open itself asks for
//! `SECURITY_IDENTIFICATION`, tokio's default, so whatever serves the pipe can
//! learn who this bridge is and cannot act as it.
//!
//! Only the wide-character (`W`) Win32 functions are called, and names reach
//! them as UTF-16. The ANSI ones convert a name through the code page first,
//! and a best-fit conversion can turn a character
//! [`check_pipe_name`](taria::socket::check_pipe_name) passed into a `\`.

use std::error::Error;
use std::fmt;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

use super::Refusal;

/// A connection to the app: a named pipe's client end.
pub(crate) type Stream = NamedPipeClient;

/// No backlog: an open succeeds only into an instance of the pipe that is
/// free, and one serving another client refuses it as busy. A connection that
/// opens and stays silent is an app that took it and is not answering:
/// suspended, or stuck.
pub(crate) const BACKLOG: bool = false;

/// Open the app's pipe named by `path`, and check that it is owned by this
/// bridge's user before anything is read from it or written to it.
///
/// A name that is not a pipe on this machine is refused before it is opened:
/// a file path would open the file, and `\\host\pipe\...` goes over SMB. A
/// pipe owned by anyone else is closed again and refused with an error that
/// [`refusal`] reads as [`Refusal::Foreign`].
pub(crate) async fn connect(path: &Path) -> io::Result<Stream> {
    let name = pipe_name(path)?;
    let pipe = ClientOptions::new().open(name)?;
    let user = TokenUser::of_this_process()?;
    let owner = Owner::of(pipe.as_raw_handle())?;
    check_owner(owner.sid, user.sid(), name)?;
    Ok(pipe)
}

/// The pid of the process serving the pipe, from
/// `GetNamedPipeServerProcessId`, when Windows names one.
///
/// Only for telling one app from the next at the same name: what keeps an
/// impostor out is the owner check in [`connect`], not this.
pub(crate) fn peer_pid(stream: &Stream) -> Option<i32> {
    let mut pid = 0u32;
    // SAFETY: the handle is the client end `stream` keeps open for as long as
    // the borrow lasts, and `pid` is a valid place for the one `u32` written.
    let named = unsafe { GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid) };
    if named == 0 {
        return None;
    }
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// True when a failed open means no app is there: a name nobody serves,
/// `ERROR_FILE_NOT_FOUND`, which `std` reports as `NotFound`. A pipe whose
/// every instance is taken (`ERROR_PIPE_BUSY`) is held, not gone, and is
/// [`refusal`]'s.
pub(crate) fn nothing_listens(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::NotFound)
}

/// What a failed [`connect`] says about who turned it away: the app's pipe
/// busy with another client, or a pipe at its name that another account owns.
pub(crate) fn refusal(err: &io::Error) -> Option<Refusal> {
    if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
        return Some(Refusal::Busy);
    }
    err.get_ref()
        .is_some_and(|inner| inner.is::<NotOurPipe>())
        .then_some(Refusal::Foreign)
}

/// Refuse a path that is not a pipe on this machine, by the rules of
/// [`check_pipe_name`](taria::socket::check_pipe_name), so a server is never
/// built around a name [`connect`] would refuse on every attempt.
pub(crate) fn check_endpoint(path: &Path) -> io::Result<()> {
    pipe_name(path).map(drop)
}

/// The bridge works here, so there is nothing to refuse.
pub(crate) fn ensure_supported() -> io::Result<()> {
    Ok(())
}

/// The SID string of the user this process runs as (`S-1-5-21-...`), read
/// from its token: what [`taria::socket::resolve_pipe_name`] names this
/// user's pipes by, so `taria-mcp --app <label>` finds the pipe the app-side
/// adapter serves for the same user.
///
/// Windows only.
///
/// # Errors
///
/// When the process token cannot be opened or read, or its SID cannot be
/// written as a string. None of these happens to a process running normally.
pub fn current_user_sid() -> io::Result<String> {
    sid_string(TokenUser::of_this_process()?.sid())
}

/// `path` as the pipe name it has to be, or the reason it is not one.
fn pipe_name(path: &Path) -> io::Result<&str> {
    let name = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not valid Unicode, so it cannot name a pipe",
                path.display()
            ),
        )
    })?;
    taria::socket::check_pipe_name(name)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    Ok(name)
}

/// Refuse the pipe `name` unless its `owner` is `user`.
///
/// The app-side adapter creates its pipe owned by the user who runs it, with
/// a DACL that grants that user alone. A pipe owned by anyone else, even a
/// group the user belongs to, was created by something else.
fn check_owner(owner: PSID, user: PSID, name: &str) -> io::Result<()> {
    // SAFETY: both are valid SIDs for the length of this call: `owner` lives
    // in a security descriptor its caller holds, `user` in a token buffer.
    if unsafe { EqualSid(owner, user) } != 0 {
        return Ok(());
    }
    let unreadable = |_| "a SID that cannot be written out".to_string();
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        NotOurPipe {
            name: name.to_string(),
            owner: sid_string(owner).unwrap_or_else(unreadable),
            user: sid_string(user).unwrap_or_else(unreadable),
        },
    ))
}

/// A pipe at the app's name that another account owns, refused unused.
#[derive(Debug)]
struct NotOurPipe {
    name: String,
    owner: String,
    user: String,
}

impl fmt::Display for NotOurPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the pipe {} is owned by {}, not by this user ({}), so it is not the app's: pipe names \
             are machine-wide, and any process can create one under the app's name first. This \
             bridge closed it without reading or writing anything",
            self.name, self.owner, self.user
        )
    }
}

impl Error for NotOurPipe {}

/// The `TOKEN_USER` of this process's token, which holds the user's SID.
struct TokenUser {
    /// The structure and the SID it points at, as `GetTokenInformation`
    /// wrote them: in `u64`s, so the structure is aligned.
    buf: Vec<u64>,
}

impl TokenUser {
    fn of_this_process() -> io::Result<Self> {
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess` is a pseudo-handle that needs no
        // closing, and `raw` is a valid place for the handle written.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so `raw` is a token handle this process
        // now owns, and nothing else closes it.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut len = 0u32;
        // Asked with no buffer, for the size: this fails with
        // `ERROR_INSUFFICIENT_BUFFER` and sets `len`, or fails for a reason
        // of its own and leaves it at 0.
        // SAFETY: a null buffer of length 0 is how the size is asked for.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &mut len,
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        // SAFETY: `buf` holds at least `len` bytes, the size just reported.
        let read = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buf.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the process token's user record is shorter than one",
            ));
        }
        Ok(Self { buf })
    }

    /// The user's SID, valid for as long as `self` is.
    fn sid(&self) -> PSID {
        // SAFETY: `buf` holds a whole `TOKEN_USER` (checked when it was
        // read), aligned to 8 bytes, which covers the structure's alignment.
        unsafe { (*self.buf.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// The owner of a kernel object, with the security descriptor it lives in,
/// freed on drop.
struct Owner {
    sid: PSID,
    descriptor: PSECURITY_DESCRIPTOR,
}

impl Owner {
    fn of(handle: HANDLE) -> io::Result<Self> {
        let mut sid: PSID = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `handle` is open for the length of the call, and every
        // pointer is either null, for what is not asked for, or a valid place
        // for what is written.
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut sid,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        // Built before anything can return, so the descriptor is freed
        // whatever follows.
        let owner = Self { sid, descriptor };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // SAFETY: a non-null owner points into the descriptor just returned.
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the pipe's security descriptor names no owner",
            ));
        }
        Ok(owner)
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: `GetSecurityInfo` allocated the descriptor for the
            // caller to free with `LocalFree`, and nothing else frees it.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

/// `sid` written out the standard way, `S-1-...`.
fn sid_string(sid: PSID) -> io::Result<String> {
    let mut wide: PWSTR = ptr::null_mut();
    // SAFETY: `sid` is a valid SID, per every caller, and `wide` a valid
    // place for the string's address.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call succeeded, so `wide` is a NUL-terminated UTF-16 string
    // allocated for the caller, read here up to its NUL and freed once, after.
    let text = unsafe {
        let len = (0..).take_while(|&at| *wide.add(at) != 0).count();
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        LocalFree(wide.cast());
        text
    };
    Ok(text)
}

/// Pipes for the tests to connect to: created the way the app-side adapter
/// creates its own, or owned by somebody else.
#[cfg(test)]
pub(crate) mod testing {
    use std::io;
    use std::mem::size_of;
    use std::ptr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    /// A pipe name no other test, and no other run, uses.
    pub(crate) fn unique_pipe_name(tag: &str) -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            r"\\.\pipe\taria-mcp-test-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The app's pipe as the adapter makes it: byte mode, one instance, the
    /// first of its name, local clients only, owned by this user, and open
    /// to this user alone.
    pub(crate) fn app_pipe(name: &str) -> io::Result<NamedPipeServer> {
        let user = super::current_user_sid()?;
        pipe_with_sddl(name, &format!("O:{user}D:P(A;;GA;;;{user})"))
    }

    /// A pipe like [`app_pipe`] that names `owner` (an SDDL SID, such as
    /// `BA` for the Administrators group) as its owner instead.
    pub(crate) fn pipe_owned_by(name: &str, owner: &str) -> io::Result<NamedPipeServer> {
        let user = super::current_user_sid()?;
        pipe_with_sddl(name, &format!("O:{owner}D:P(A;;GA;;;{user})"))
    }

    fn pipe_with_sddl(name: &str, sddl: &str) -> io::Result<NamedPipeServer> {
        let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated, and `descriptor` a valid place for
        // the address written.
        let parsed = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if parsed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` is a whole SECURITY_ATTRIBUTES whose
        // descriptor outlives the call.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .max_instances(1)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        };
        // SAFETY: allocated by the conversion above for the caller to free,
        // and freed once, after its last use.
        unsafe { LocalFree(descriptor) };
        server
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{app_pipe, pipe_owned_by, unique_pipe_name};
    use super::*;
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    /// A SID from its string, freed on drop.
    struct Sid(PSID);

    impl Sid {
        fn parse(text: &str) -> Self {
            let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
            let mut sid: PSID = ptr::null_mut();
            // SAFETY: NUL-terminated input and a valid place for the output.
            let parsed = unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut sid) };
            assert_ne!(parsed, 0, "{text} parses: {}", io::Error::last_os_error());
            Self(sid)
        }
    }

    impl Drop for Sid {
        fn drop(&mut self) {
            // SAFETY: allocated by the conversion for the caller to free.
            unsafe { LocalFree(self.0) };
        }
    }

    /// The SID read from the token is one `pipe_name` takes, and the name it
    /// builds is one `connect` accepts, so `--app` reaches the adapter's pipe.
    #[test]
    fn this_users_sid_names_a_pipe_connect_accepts() {
        let sid = current_user_sid().expect("the token names a user");
        assert!(sid.starts_with("S-1-"), "{sid}");
        let name = taria::socket::pipe_name("demo", &sid).expect("a SID string");
        assert_eq!(name, format!(r"\\.\pipe\taria-{sid}-demo"));
        check_endpoint(Path::new(&name)).expect("a local pipe name");
    }

    /// Anything that is not one pipe on this machine is refused before it is
    /// opened: a file path would open the file, and a remote pipe goes over
    /// SMB. The prefix's case is Windows' to ignore, and so it is here.
    #[tokio::test]
    async fn only_a_local_pipe_name_is_opened() {
        for name in [
            r"C:\Windows\win.ini",
            r"\\host\pipe\taria",
            r"\\.\pipe\..\UNC\host\share",
            "/run/user/1000/taria/demo.sock",
        ] {
            let err = connect(Path::new(name))
                .await
                .expect_err("not a local pipe");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}: {err}");
            assert!(err.to_string().contains(name), "{name}: {err}");
            assert!(!nothing_listens(&err) && refusal(&err).is_none(), "{name}");
            assert!(check_endpoint(Path::new(name)).is_err(), "{name}");
        }
        check_endpoint(Path::new(r"\\.\PIPE\taria")).expect("any case of the prefix");
    }

    /// A name nobody serves is `NotFound`, the one failure that says the app
    /// has gone, and is no refusal.
    #[tokio::test]
    async fn a_pipe_nobody_serves_is_nothing_listening() {
        let name = unique_pipe_name("nobody");
        let err = connect(Path::new(&name))
            .await
            .expect_err("nothing serves it");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(nothing_listens(&err));
        assert_eq!(refusal(&err), None);
    }

    /// The app's pipe, made the way the adapter makes it, is connected to,
    /// and names this process as its server; while that connection holds its
    /// one instance, the next open is refused as busy, which is held and not
    /// gone.
    #[tokio::test]
    async fn the_apps_pipe_is_opened_and_its_one_instance_is_then_busy() {
        let name = unique_pipe_name("busy");
        let server = app_pipe(&name).expect("create the app's pipe");
        let first = connect(Path::new(&name)).await.expect("the app's own pipe");
        server.connect().await.expect("the app takes the bridge");
        // SAFETY: no arguments, no preconditions.
        let me = unsafe { GetCurrentProcessId() };
        assert_eq!(
            peer_pid(&first),
            i32::try_from(me).ok(),
            "the server is this process"
        );

        let err = connect(Path::new(&name)).await.expect_err("one instance");
        assert_eq!(err.raw_os_error(), Some(ERROR_PIPE_BUSY as i32), "{err}");
        assert_eq!(refusal(&err), Some(Refusal::Busy));
        assert!(!nothing_listens(&err), "a busy pipe is held, not gone");
        drop(first);
    }

    /// A pipe at the app's name that someone else owns is refused, and the
    /// refusal is read as one, with the name, the owner and the user in the
    /// message.
    #[tokio::test]
    async fn a_pipe_another_account_owns_is_refused() {
        let name = unique_pipe_name("foreign");
        // Wine lets a process name the Administrators group as an owner; a
        // Windows process that is not elevated may not, and then the check
        // itself is what is tested, below.
        match pipe_owned_by(&name, "BA") {
            Err(err) => eprintln!("no pipe another account owns here ({err}); the check alone"),
            Ok(server) => {
                let err = connect(Path::new(&name))
                    .await
                    .expect_err("not this user's");
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
                assert_eq!(refusal(&err), Some(Refusal::Foreign));
                assert!(!nothing_listens(&err));
                let message = err.to_string();
                assert!(
                    message.contains(&name) && message.contains("S-1-5-32-544"),
                    "{message}"
                );
                drop(server);
            }
        }

        let user = current_user_sid().expect("the token names a user");
        let mine = Sid::parse(&user);
        let admins = Sid::parse("S-1-5-32-544");
        check_owner(mine.0, mine.0, r"\\.\pipe\x").expect("the user's own pipe");
        let err = check_owner(admins.0, mine.0, r"\\.\pipe\x").expect_err("another owner");
        assert_eq!(refusal(&err), Some(Refusal::Foreign));
        assert_eq!(
            err.to_string(),
            format!(
                "the pipe \\\\.\\pipe\\x is owned by S-1-5-32-544, not by this user ({user}), so it \
                 is not the app's: pipe names are machine-wide, and any process can create one \
                 under the app's name first. This bridge closed it without reading or writing \
                 anything"
            )
        );
    }
}
