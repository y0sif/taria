//! The Windows transport: a named pipe, at the name the bridge was given, which
//! [`taria::socket::resolve_pipe_name`] derives the way the app-side adapter
//! does.
//!
//! The app's pipe has one instance, so a second client's open fails with
//! `ERROR_PIPE_BUSY` while the first is served. That is the only sign Windows
//! gives of an app held by another bridge: there is no listen backlog to wait
//! in, so a connection that opens is one the app's pipe took, and silence on
//! it is the app's own (see [`BACKLOG`]). The instance is also busy for a
//! moment between two sessions, while the adapter retires the last one and
//! listens again, so a busy open is retried for [`BUSY_RETRY`] before it
//! counts as held.
//!
//! Pipe names are machine-wide, so any process can create a pipe under the
//! app's name before the app does. Every connection is checked to be to a
//! pipe owned by this bridge's own user, and refused otherwise, with nothing
//! written to it and nothing it sent parsed ([`Refusal::Foreign`]); so is an
//! open the pipe refuses as access denied, which the app's own pipe never
//! answers its user. The open itself asks for `SECURITY_IDENTIFICATION`,
//! tokio's default, so whatever serves the pipe can learn who this bridge is
//! and cannot act as it.
//!
//! Only the wide-character (`W`) Win32 functions are called, and names reach
//! them as UTF-16. The ANSI ones convert a name through the code page first,
//! and a best-fit conversion can turn a character
//! [`check_pipe_name`](taria::socket::check_pipe_name) passed into a `\`.

use std::error::Error;
use std::fmt;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::time::Instant;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_SUCCESS, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, IsValidSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

use super::Refusal;
use super::windows_sid::{TokenUser, sid_string};

/// A connection to the app: a named pipe's client end.
pub(crate) type Stream = NamedPipeClient;

/// No backlog: an open succeeds only into an instance of the pipe that is
/// free, and one serving another client refuses it as busy. A connection that
/// opens and stays silent is an app that took it and is not answering:
/// suspended, or stuck.
pub(crate) const BACKLOG: bool = false;

/// What the answers call the app's endpoint: a pipe.
pub(crate) const ENDPOINT: &str = "pipe";

/// What the answers call what the bridge was given to find the app by: a
/// pipe name, where Unix has a socket path.
pub(crate) const ENDPOINT_ADDRESS: &str = "pipe name";

/// How long an open the pipe refuses as busy is retried before the refusal
/// stands, as tokio's docs advise for a client.
///
/// Between two sessions the app's one instance is busy for a moment: the
/// bridge that left has closed its end, and the adapter has yet to retire its
/// session, disconnect it and listen again, which takes a poll of its
/// accept loop (50 ms) and the session's threads ending. Refused at once, a
/// bridge arriving in that moment was told the app is held by another bridge,
/// about an app that was free. A pipe still busy at the end of this is served
/// to another bridge.
pub(crate) const BUSY_RETRY: Duration = Duration::from_millis(250);

/// How long a busy open naps before the next try.
const BUSY_NAP: Duration = Duration::from_millis(10);

/// Open the app's pipe named by `path`, and check that it is owned by this
/// bridge's user before anything is written to it or anything it sent is
/// parsed.
///
/// Registering the pipe with the runtime starts a read into the runtime's
/// own buffer at once, so bytes the pipe sent may have been read by the time
/// of the check. Nothing reads them out of that buffer before it passes, and
/// a pipe that fails it is dropped with them.
///
/// A name that is not a pipe on this machine is refused before it is opened:
/// a file path would open the file, and `\\host\pipe\...` goes over SMB. A
/// pipe owned by anyone else is closed again, and a pipe that refuses the
/// open as access denied is not opened at all; both fail with an error that
/// [`refusal`] reads as [`Refusal::Foreign`]. A busy pipe is retried for
/// [`BUSY_RETRY`] first.
pub(crate) async fn connect(path: &Path) -> io::Result<Stream> {
    let name = pipe_name(path)?;
    let pipe = open(name).await?;
    let user = TokenUser::of_this_process()?;
    let owner = Owner::of(pipe.as_raw_handle())?;
    check_owner(owner.sid, user.sid(), name)?;
    Ok(pipe)
}

/// Open the pipe `name`, retrying for [`BUSY_RETRY`] while it is busy, and
/// with an open refused as access denied turned into the error [`refusal`]
/// reads as [`Refusal::Foreign`].
async fn open(name: &str) -> io::Result<NamedPipeClient> {
    let until = Instant::now() + BUSY_RETRY;
    loop {
        match ClientOptions::new().open(name) {
            Err(err) if is_busy(&err) && Instant::now() < until => {}
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                return Err(denied(name));
            }
            result => return result,
        }
        tokio::time::sleep(BUSY_NAP).await;
    }
}

/// True when an open failed because every instance of the pipe is taken.
fn is_busy(err: &io::Error) -> bool {
    err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
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
/// busy with another client, or a pipe at its name that is not the app's,
/// because another account owns it or it refused this user access.
///
/// Access denied is read as foreign whether [`connect`] wrapped it or not:
/// the adapter's pipe grants its user all access, so a pipe at the name that
/// denies this user is not one the adapter made for it.
pub(crate) fn refusal(err: &io::Error) -> Option<Refusal> {
    if is_busy(err) {
        return Some(Refusal::Busy);
    }
    if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        return Some(Refusal::Foreign);
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
        NotOurPipe::Owned {
            name: name.to_string(),
            owner: sid_string(owner).unwrap_or_else(unreadable),
            user: sid_string(user).unwrap_or_else(unreadable),
        },
    ))
}

/// The error an open of `name` refused as access denied is reported as.
fn denied(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        NotOurPipe::Denied {
            name: name.to_string(),
        },
    )
}

/// A pipe at the app's name that is not the app's, refused unused.
#[derive(Debug)]
enum NotOurPipe {
    /// Opened, and owned by another account than this user: closed again.
    Owned {
        name: String,
        owner: String,
        user: String,
    },
    /// Refused as access denied at the open, so never opened.
    Denied { name: String },
}

impl fmt::Display for NotOurPipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owned { name, owner, user } => write!(
                f,
                "the pipe {name} is owned by {owner}, not by this user ({user}), so it is not \
                 the app's: pipe names are machine-wide, and any process can create one under \
                 the app's name first. This bridge closed it without parsing anything it sent or \
                 writing anything to it"
            ),
            Self::Denied { name } => write!(
                f,
                "the pipe {name} refused this user access, which the app's own pipe never does, \
                 so it is not the app's: pipe names are machine-wide, and any process can create \
                 one under the app's name first. This bridge never opened it, so nothing was read \
                 from it or written to it"
            ),
        }
    }
}

impl Error for NotOurPipe {}

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

    use super::super::windows_sid::current_user_sid;

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
        let user = current_user_sid()?;
        pipe_with_sddl(name, &format!("O:{user}D:P(A;;GA;;;{user})"))
    }

    /// A pipe like [`app_pipe`] that names `owner` (an SDDL SID, such as
    /// `BA` for the Administrators group) as its owner instead.
    pub(crate) fn pipe_owned_by(name: &str, owner: &str) -> io::Result<NamedPipeServer> {
        let user = current_user_sid()?;
        pipe_with_sddl(name, &format!("O:{owner}D:P(A;;GA;;;{user})"))
    }

    /// A pipe like [`app_pipe`] whose DACL has no entries, so it lets
    /// nobody open it to read or write, its owner included: what a pipe at
    /// the app's name that refuses this user looks like.
    pub(crate) fn pipe_denying_this_user(name: &str) -> io::Result<NamedPipeServer> {
        let user = current_user_sid()?;
        pipe_with_sddl(name, &format!("O:{user}D:P"))
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
    use super::super::windows_sid::current_user_sid;
    use super::testing::{app_pipe, pipe_denying_this_user, pipe_owned_by, unique_pipe_name};
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

        let tried = Instant::now();
        let err = connect(Path::new(&name)).await.expect_err("one instance");
        assert!(
            tried.elapsed() >= BUSY_RETRY,
            "refused as busy after {:?}, inside the retry",
            tried.elapsed()
        );
        assert_eq!(err.raw_os_error(), Some(ERROR_PIPE_BUSY as i32), "{err}");
        assert_eq!(refusal(&err), Some(Refusal::Busy));
        assert!(!nothing_listens(&err), "a busy pipe is held, not gone");
        drop(first);
    }

    /// Between two sessions the one instance is busy for a moment: the last
    /// bridge has gone and the app has yet to disconnect it and listen again.
    /// An open in that moment waits it out and gets in, rather than reading
    /// a free app as held.
    #[tokio::test]
    async fn a_pipe_busy_between_two_sessions_is_waited_for() {
        let name = unique_pipe_name("between");
        let server = app_pipe(&name).expect("create the app's pipe");
        let last = connect(Path::new(&name)).await.expect("the last bridge");
        server.connect().await.expect("the app takes it");
        drop(last);
        // The last bridge has gone, and the app has not listened again yet.
        let err = ClientOptions::new()
            .open(&name)
            .expect_err("busy until the app listens again");
        assert!(is_busy(&err), "{err}");

        let app = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            server.disconnect().expect("retire the last session");
            server
                .connect()
                .await
                .expect("the app takes the next bridge");
            server
        });
        let tried = Instant::now();
        let next = connect(Path::new(&name))
            .await
            .expect("the open waits out the moment between sessions");
        assert!(
            tried.elapsed() < BUSY_RETRY + Duration::from_millis(200),
            "took {:?}",
            tried.elapsed()
        );
        let _server = app.await.expect("the app task");
        drop(next);
    }

    /// An open the pipe refuses as access denied is a pipe that is not the
    /// app's, since the adapter's pipe lets its own user in: refused as
    /// foreign, never as an app that is not running. Wine may let the open
    /// through a DACL that Windows enforces, and then the mapping alone is
    /// what is tested.
    #[tokio::test]
    async fn a_pipe_that_refuses_this_user_is_foreign() {
        let name = unique_pipe_name("denied");
        let server = pipe_denying_this_user(&name).expect("create the pipe");
        match connect(Path::new(&name)).await {
            Ok(_) => eprintln!("this system let the open through an empty DACL; the mapping alone"),
            Err(err) => {
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
                assert_eq!(refusal(&err), Some(Refusal::Foreign), "{err}");
                assert!(!nothing_listens(&err));
                assert!(
                    err.to_string()
                        .starts_with(&format!("the pipe {name} refused this user access")),
                    "{err}"
                );
            }
        }
        drop(server);

        let raw = io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
        assert_eq!(refusal(&raw), Some(Refusal::Foreign));
        assert!(!nothing_listens(&raw));
        let wrapped = denied(r"\\.\pipe\x");
        assert_eq!(refusal(&wrapped), Some(Refusal::Foreign));
        assert_eq!(
            wrapped.to_string(),
            "the pipe \\\\.\\pipe\\x refused this user access, which the app's own pipe \
             never does, so it is not the app's: pipe names are machine-wide, and any process \
             can create one under the app's name first. This bridge never opened it, so nothing \
             was read from it or written to it"
        );
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
                 under the app's name first. This bridge closed it without parsing anything it \
                 sent or writing anything to it"
            )
        );
    }
}
