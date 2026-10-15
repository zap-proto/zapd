//! Windows: the door is a named pipe, `\\.\pipe\zapd-<user SID>-<runtime dir>`,
//! made with a security descriptor that admits this user and SYSTEM alone. The
//! election is `LockFileEx` on the same lock file, released by the kernel when
//! the holder's process exits; the winner writes its pid beside the lock, and a
//! node checks the pipe's server process against it, as a Unix node checks the
//! socket's peer against the lock holder. Directories are given a protected DACL
//! for the same two principals, the counterpart of `0700`, and a file is this
//! user's when its owner is this process's user (or the owner its token gives
//! new objects, Administrators for an elevated one).

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io::{Error, ErrorKind, Result, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::time::Duration;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_BUSY, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetTokenInformation, TokenOwner, TokenUser, ACL, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::OVERLAPPED;

/// A local node's connection, as the router accepts it.
pub(crate) type Served = NamedPipeServer;
/// A local node's connection, as the node dialled it.
pub(crate) type Dialed = NamedPipeClient;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// A SID as its string form, the memory it came in left to its owner.
///
/// # Safety
/// `sid` must point at a valid SID.
unsafe fn sid_string(sid: PSID) -> Result<String> {
    let mut s = null_mut();
    if ConvertSidToStringSidW(sid, &mut s) == 0 {
        return Err(Error::last_os_error());
    }
    let len = (0..).take_while(|&i| *s.add(i) != 0).count();
    let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
    LocalFree(s.cast());
    Ok(out)
}

/// One SID of this process's token: its user, or the owner it gives new objects.
fn token_sid(class: TOKEN_INFORMATION_CLASS) -> Result<String> {
    // SAFETY: the token handle is closed on every path; the buffer is sized by
    // the first call, word-aligned for the pointer the structure holds, and read
    // only as the structure `class` names.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, class, null_mut(), 0, &mut len);
        let mut buf = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) != 0;
        let err = Error::last_os_error();
        CloseHandle(token);
        if !ok {
            return Err(err);
        }
        let sid = if class == TokenUser {
            (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid
        } else {
            (*(buf.as_ptr() as *const TOKEN_OWNER)).Owner
        };
        sid_string(sid)
    }
}

/// The SIDs this process's own files and directories are owned by.
fn mine() -> Result<Vec<String>> {
    let user = token_sid(TokenUser)?;
    let owner = token_sid(TokenOwner)?;
    Ok(if owner == user { vec![user] } else { vec![user, owner] })
}

/// A security descriptor from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    /// Full access for this user (and the owner an elevated token gives new
    /// objects) and SYSTEM, nobody else, and no inherited entries. `inherit`
    /// passes the entries to everything created inside a directory.
    fn private(inherit: bool) -> Result<Descriptor> {
        let flags = if inherit { "OICI" } else { "" };
        let mut sddl = String::from("D:P");
        for sid in mine()?.iter().map(String::as_str).chain(["SY"]) {
            sddl.push_str(&format!("(A;{flags};GA;;;{sid})"));
        }
        let text = wide(OsStr::new(&sddl));
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `text` is NUL-terminated; on success `sd` is LocalAlloc'd and freed on drop.
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(text.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
            return Err(Error::last_os_error());
        }
        Ok(Descriptor(sd))
    }

    fn dacl(&self) -> Result<*mut ACL> {
        let (mut present, mut defaulted, mut dacl) = (0, 0, null_mut());
        // SAFETY: `self.0` is a valid security descriptor for as long as `self` lives.
        if unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted) } == 0 || present == 0 {
            return Err(Error::last_os_error());
        }
        Ok(dacl)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0.cast()) };
    }
}

// SAFETY: the descriptor is immutable once made and only read by the kernel.
unsafe impl Send for Descriptor {}
unsafe impl Sync for Descriptor {}

/// The SID that owns `p`.
fn owner(p: &Path) -> Result<String> {
    let name = wide(p.as_os_str());
    let (mut sid, mut sd): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
    // SAFETY: `name` is NUL-terminated; `sid` points into `sd`, which is freed after it is read.
    unsafe {
        let rc = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut sid,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != ERROR_SUCCESS {
            return Err(Error::from_raw_os_error(rc as i32));
        }
        let s = sid_string(sid);
        LocalFree(sd.cast());
        s
    }
}

/// Set `dir`'s DACL to this user's and SYSTEM's alone, inherited by what is made in it.
fn protect(dir: &Path) -> Result<()> {
    let d = Descriptor::private(true)?;
    let name = wide(dir.as_os_str());
    // SAFETY: `name` is NUL-terminated and the DACL lives in `d` for the call.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            d.dacl()?,
            null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

fn refuse(p: &Path, what: &str) -> Error {
    Error::new(
        ErrorKind::PermissionDenied,
        format!("zapd: {} must be a {what} this user owns, and no link", p.display()),
    )
}

/// The door local nodes connect to: one pipe per user and runtime directory.
pub(crate) fn address(dir: &Path) -> PathBuf {
    // FNV-1a over the directory, so two runtime directories are two routers, as
    // two socket paths are on Unix.
    let mut h: u64 = 0xcbf29ce484222325;
    for unit in dir.as_os_str().encode_wide() {
        for b in unit.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    let user = token_sid(TokenUser).unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!(r"\\.\pipe\zapd-{user}-{h:016x}"))
}

/// Make `dir` (and its parents), with only this user and SYSTEM able to open it.
pub(crate) fn make_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    protect(dir)
}

/// `dir` made and kept private: refused if it is a link or another user's; its
/// DACL set to this user's and SYSTEM's alone every time.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    private(dir, true)?;
    protect(dir)
}

/// Refuse a path that is a link, the wrong kind, or another user's.
pub(crate) fn private(p: &Path, dir: bool) -> Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    let kind_ok = if dir { m.file_type().is_dir() } else { m.file_type().is_file() };
    if !kind_ok || m.file_type().is_symlink() || !mine()?.contains(&owner(p)?) {
        return Err(refuse(p, if dir { "directory" } else { "file" }));
    }
    Ok(())
}

/// A new file in a private directory, which gives it that directory's DACL.
pub(crate) fn create_private(p: &Path) -> Result<File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(p)
}

/// The election's lock file, opened once per process and never closed.
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(refuse(path, "file"));
    }
    std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)
}

/// Take the whole-file exclusive lock, waiting for it, or drop it. The kernel
/// drops it too when the holder's process exits, however it exits.
pub(crate) fn lock(f: &File, take: bool) -> Result<()> {
    let handle = f.as_raw_handle() as HANDLE;
    // SAFETY: a zeroed OVERLAPPED is valid for a synchronous handle; the range is the whole file.
    let ok = unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        if take {
            LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, u32::MAX, u32::MAX, &mut ov)
        } else {
            UnlockFileEx(handle, 0, u32::MAX, u32::MAX, &mut ov)
        }
    };
    if ok == 0 {
        return Err(Error::last_os_error());
    }
    Ok(())
}

/// Record this process as the router: `LockFileEx` does not name its holder, so
/// the winner writes its pid beside the lock, in the private runtime directory.
pub(crate) fn announce(dir: &Path) -> Result<()> {
    let tmp = dir.join(format!(".zapd.pid.{}", std::process::id()));
    let mut f = create_private(&tmp)?;
    f.write_all(std::process::id().to_string().as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, dir.join("zapd.pid"))
}

/// The router's pid, as the elected process recorded it.
pub(crate) fn holder(_f: &File, dir: &Path) -> Option<u32> {
    std::fs::read_to_string(dir.join("zapd.pid")).ok()?.trim().parse().ok()
}

/// The router's door, bound in `dir`: one pipe instance waiting at a time.
pub(crate) struct Listener {
    name: PathBuf,
    sd: Descriptor,
    next: NamedPipeServer,
}

impl Listener {
    fn instance(name: &Path, sd: &Descriptor, first: bool) -> Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        // SAFETY: `sa` and the descriptor it points at outlive the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(name, (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>())
        }
    }

    /// Create the pipe. The first instance must be new: a pipe of this name that
    /// another process made is refused, not shared.
    pub(crate) fn bind(dir: &Path) -> Result<Listener> {
        let name = address(dir);
        let sd = Descriptor::private(false)?;
        let next = Self::instance(&name, &sd, true)?;
        Ok(Listener { name, sd, next })
    }

    pub(crate) async fn accept(&mut self) -> Result<Served> {
        self.next.connect().await?;
        let fresh = Self::instance(&self.name, &self.sd, false)?;
        Ok(std::mem::replace(&mut self.next, fresh))
    }
}

/// Connect to the door in `dir`, with the pid of the process serving it.
pub(crate) async fn dial(dir: &Path) -> Result<(Dialed, Option<u32>)> {
    let name = address(dir);
    let client = loop {
        match ClientOptions::new().open(&name) {
            Ok(c) => break c,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => return Err(e),
        }
    };
    let mut pid = 0u32;
    // SAFETY: the handle is the open client end of a named pipe.
    let ok = unsafe { GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut pid) } != 0;
    Ok((client, ok.then_some(pid)))
}

/// This machine's name.
pub(crate) fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}
