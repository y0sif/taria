//! This user's SID, read from the process token: what the bridge checks a
//! pipe's owner against, and what [`taria::socket::resolve_pipe_name`] names
//! this user's pipes by.
//!
//! Compiled twice, into the library (as part of the Windows transport) and
//! into the binary (by `#[path]`, for `--app`), so the binary can name the
//! adapter's pipe without the library making the SID a public item of its
//! API.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetTokenInformation, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser as TOKEN_USER_CLASS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::PWSTR;

/// The SID string of the user this process runs as (`S-1-5-21-...`), read
/// from its token: what [`taria::socket::resolve_pipe_name`] names this
/// user's pipes by, so `taria-mcp --app <label>` finds the pipe the app-side
/// adapter serves for the same user.
///
/// # Errors
///
/// When the process token cannot be opened or read, or its SID cannot be
/// written as a string. None of these happens to a process running normally.
// The library itself reads the SID raw, for the owner check; only its tests
// want the string, and the binary does.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn current_user_sid() -> io::Result<String> {
    sid_string(TokenUser::of_this_process()?.sid())
}

/// The `TOKEN_USER` of this process's token, which holds the user's SID.
pub(crate) struct TokenUser {
    /// The structure and the SID it points at, as `GetTokenInformation`
    /// wrote them: in `u64`s, so the structure is aligned.
    buf: Vec<u64>,
}

impl TokenUser {
    pub(crate) fn of_this_process() -> io::Result<Self> {
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
                TOKEN_USER_CLASS,
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
                TOKEN_USER_CLASS,
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
    pub(crate) fn sid(&self) -> PSID {
        // SAFETY: `buf` holds a whole `TOKEN_USER` (checked when it was
        // read), aligned to 8 bytes, which covers the structure's alignment.
        unsafe { (*self.buf.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

/// `sid` written out the standard way, `S-1-...`.
pub(crate) fn sid_string(sid: PSID) -> io::Result<String> {
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
