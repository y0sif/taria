//! Who the pipe belongs to: the current user's SID, read from the process
//! token, and the security descriptor the pipe is created with, which makes
//! that user its owner and the only one it lets in.
//!
//! Every call here is a plain Win32 call on memory this module owns, and
//! every buffer the system hands back is freed once, by a guard.

use std::io;
use std::mem;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HLOCAL, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAce, CopySid, GetLengthSid,
    GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, IsValidSid, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::Handle;

/// The rights the pipe grants its user, and nobody else: read and write,
/// which is what a bridge opens it with. The system maps them to the pipe's
/// specific rights as it stores the descriptor.
pub(super) const PIPE_RIGHTS: u32 = GENERIC_READ | GENERIC_WRITE;

/// `SECURITY_DESCRIPTOR_REVISION`, which lives in a `windows-sys` feature
/// this crate does not otherwise need. It has been 1 since Windows NT.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

/// A SID, copied into memory of its own.
///
/// Kept in `u32`s because a SID's sub-authorities are `u32`s and the API
/// expects it aligned for them.
pub(super) struct Sid {
    words: Vec<u32>,
}

impl Sid {
    /// The user this process runs as, from its token. The process token, not
    /// a thread's impersonation token: the pipe belongs to whoever runs the
    /// app.
    pub(super) fn current_user() -> io::Result<Self> {
        let token = process_token()?;
        let mut needed = 0u32;
        // SAFETY: a size query: no buffer and a length of 0, so the call
        // writes nothing but `needed`, a local.
        let sized =
            unsafe { GetTokenInformation(token.raw(), TokenUser, ptr::null_mut(), 0, &mut needed) };
        if sized == 0 {
            // SAFETY: reads this thread's last error, set by the call above.
            let err = unsafe { GetLastError() };
            if err != ERROR_INSUFFICIENT_BUFFER {
                return Err(os_error(err));
            }
        }
        let len = needed as usize;
        if len < mem::size_of::<TOKEN_USER>() {
            return Err(io::Error::other(
                "the process token reported no room for its user",
            ));
        }
        // `u64`s, so the TOKEN_USER at its start, which holds a pointer, is
        // aligned for one.
        let mut info = vec![0u64; len.div_ceil(mem::size_of::<u64>())];
        // SAFETY: `info` holds at least `needed` bytes, the length passed, and
        // outlives the call; the call writes a TOKEN_USER and the SID it
        // points to into it.
        let filled = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                info.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if filled == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call above filled `info` with a TOKEN_USER at its
        // start, aligned for it (see `info`), and at least that long (checked
        // above). Its SID points into `info`, which is still alive here.
        let sid = unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` points into `info`, filled by the system.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::other(
                "the process token holds no valid user SID",
            ));
        }
        Self::copy_of(sid)
    }

    /// Copy the SID at `sid`, which must be valid, into memory of its own.
    pub(super) fn copy_of(sid: PSID) -> io::Result<Self> {
        // SAFETY: `sid` is a valid SID, which is all GetLengthSid reads.
        let len = unsafe { GetLengthSid(sid) };
        let mut words = vec![0u32; (len as usize).div_ceil(mem::size_of::<u32>())];
        // SAFETY: `words` holds at least `len` bytes, the length passed, and
        // `sid` is valid; the call writes only into `words`.
        if unsafe { CopySid(len, words.as_mut_ptr().cast(), sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { words })
    }

    /// The SID as the API takes it. The calls this is passed to read it and
    /// never write it, which is why a pointer from a shared borrow is enough.
    pub(super) fn as_psid(&self) -> PSID {
        self.words.as_ptr().cast_mut().cast()
    }

    /// The SID's string form, `S-1-5-21-...`, as `ConvertSidToStringSidW`
    /// writes it.
    pub(super) fn to_string_sid(&self) -> io::Result<String> {
        let mut wide = ptr::null_mut();
        // SAFETY: reads this SID, which is valid, and writes a pointer to a
        // string the system allocates into `wide`, a local.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut wide) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Freed exactly once, by the guard, whatever happens below.
        let string = LocalMemory(wide.cast());
        // SAFETY: on success `wide` is a NUL-terminated UTF-16 string, so
        // every unit up to the NUL is readable, and it stays alive until the
        // guard frees it after this function is done with it.
        let units = unsafe {
            let mut len = 0;
            while *wide.add(len) != 0 {
                len += 1;
            }
            std::slice::from_raw_parts(wide, len)
        };
        let sid = String::from_utf16(units)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a SID string is not UTF-16"));
        drop(string);
        sid
    }
}

/// The security descriptor a taria pipe is created with: owned by its user,
/// with a DACL that grants that user [`PIPE_RIGHTS`] and nobody anything.
///
/// The owner is set outright rather than left to the default, because an
/// elevated token defaults the owner of what it creates to Administrators.
/// The DACL is marked protected, so nothing can be inherited into it. No
/// Everyone, no anonymous logon, no Administrators or SYSTEM: an ACL with
/// one entry.
///
/// The descriptor is in absolute form and points at the owner's SID and at
/// the ACL, so it borrows the [`Sid`] and owns the ACL, and lives at least as
/// long as the call that creates the pipe with it.
pub(super) struct PipeSecurity<'sid> {
    /// Boxed so the pointer [`attributes`](Self::attributes) hands out stays
    /// put however this value moves.
    descriptor: Box<SECURITY_DESCRIPTOR>,
    /// The DACL the descriptor points at, in `u32`s because an ACL has to be
    /// aligned for them.
    _acl: Vec<u32>,
    _owner: &'sid Sid,
}

impl<'sid> PipeSecurity<'sid> {
    /// A descriptor that makes `owner` the pipe's owner and only user.
    pub(super) fn owned_by(owner: &'sid Sid) -> io::Result<Self> {
        let sid = owner.as_psid();
        // SAFETY: `sid` is a valid SID, owned by `owner`.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        // An ACL header and one access-allowed ACE, whose `SidStart` is the
        // first word of the SID it carries, so it counts once.
        let acl_len = (mem::size_of::<ACL>() + mem::size_of::<ACCESS_ALLOWED_ACE>()
            - mem::size_of::<u32>()
            + sid_len)
            .next_multiple_of(mem::size_of::<u32>());
        let acl_len_u32 = u32::try_from(acl_len)
            .map_err(|_| io::Error::other("a one-entry ACL does not fit its own length field"))?;
        let mut acl = vec![0u32; acl_len / mem::size_of::<u32>()];
        let acl_ptr: *mut ACL = acl.as_mut_ptr().cast();
        // SAFETY: `acl` is `acl_len` bytes, aligned for an ACL, and owned
        // here; the call writes an empty ACL header into it.
        if unsafe { InitializeAcl(acl_ptr, acl_len_u32, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `acl_ptr` is the ACL initialized above, sized for exactly
        // this one entry, and `sid` is valid; the call copies the SID in.
        if unsafe { AddAccessAllowedAce(acl_ptr, ACL_REVISION, PIPE_RIGHTS, sid) } == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: an all-zero SECURITY_DESCRIPTOR is plain data (integers and
        // null pointers), and it is initialized properly right below.
        let mut descriptor: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { mem::zeroed() });
        let sd = (&raw mut *descriptor).cast();
        // SAFETY: `sd` points at a SECURITY_DESCRIPTOR this function owns.
        if unsafe { InitializeSecurityDescriptor(sd, SECURITY_DESCRIPTOR_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is initialized; it stores the pointer to
        // `sid`, which `owner` keeps alive for as long as `Self` borrows it.
        if unsafe { SetSecurityDescriptorOwner(sd, sid, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: as above; it stores the pointer to `acl`, whose heap buffer
        // moves into `Self` and so lives as long as the descriptor does.
        if unsafe { SetSecurityDescriptorDacl(sd, 1, acl_ptr, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sets one control bit on the initialized descriptor.
        if unsafe { SetSecurityDescriptorControl(sd, SE_DACL_PROTECTED, SE_DACL_PROTECTED) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            _acl: acl,
            _owner: owner,
        })
    }

    /// Attributes that create an object with this descriptor, and a handle
    /// no child process inherits. Valid while `self` is.
    pub(super) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&raw const *self.descriptor).cast_mut().cast(),
            bInheritHandle: 0,
        }
    }
}

/// Memory the system allocated for a result and the caller frees with
/// `LocalFree`, freed exactly once, when this is dropped.
pub(super) struct LocalMemory(pub(super) HLOCAL);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a call that hands its result to
            // the caller to free with LocalFree, and only this guard frees it.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// This process's token, opened to query.
fn process_token() -> io::Result<Handle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
    // closing, and the call writes the token's handle into `token`, a local
    // that the `Handle` below then owns.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(token))
}

/// A Win32 error code as the `io::Error` it is.
fn os_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}
