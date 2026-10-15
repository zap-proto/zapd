//! Unix: the door is `<runtime>/zapd.sock`, 0600 in a 0700 directory; the
//! election is an fcntl(2) record lock, which also names its holder.

use std::fs::File;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

/// A local node's connection, as the router accepts it.
pub(crate) type Served = UnixStream;
/// A local node's connection, as the node dialled it.
pub(crate) type Dialed = UnixStream;

/// The door local nodes connect to.
pub(crate) fn address(dir: &Path) -> PathBuf {
    dir.join("zapd.sock")
}

/// Make `dir` (and its parents) with no group or other access.
pub(crate) fn make_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// `dir` made and kept private: refused if it is a symlink or another user's;
/// tightened to `0700` if this user made it looser.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    make_dir(dir)?;
    let m = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid(2) cannot fail.
    let me = unsafe { libc::getuid() };
    if !m.file_type().is_dir() || m.uid() != me {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!("zapd: {} must be a directory owned by uid {me}", dir.display()),
        ));
    }
    if m.mode() & 0o077 != 0 {
        tracing::warn!("zapd: {} was open to others; made it 0700", dir.display());
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Refuse a path anyone but this user could read, write or have planted.
pub(crate) fn private(p: &Path, dir: bool) -> Result<()> {
    let m = std::fs::symlink_metadata(p)?;
    // SAFETY: getuid(2) cannot fail.
    let me = unsafe { libc::getuid() };
    let kind_ok = if dir {
        m.file_type().is_dir()
    } else {
        m.file_type().is_file()
    };
    if !kind_ok || m.uid() != me || m.mode() & 0o077 != 0 {
        return Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "zapd: {} must be a {} owned by uid {me} with no group or other access",
                p.display(),
                if dir { "directory" } else { "file" }
            ),
        ));
    }
    Ok(())
}

/// A new file only this user can read or write.
pub(crate) fn create_private(p: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)
}

/// The election's lock file, opened once per process and never closed: POSIX
/// drops all of a process's record locks on a file when ANY descriptor of it
/// closes.
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Take the whole-file write lock, waiting for it, or drop it.
///
/// A record lock, not flock(2): record locks belong to the process and are not
/// inherited across fork(2), so a child forked without exec never holds the
/// router's lock after the router is gone.
pub(crate) fn lock(f: &File, take: bool) -> Result<()> {
    let cmd = if take { libc::F_SETLKW } else { libc::F_SETLK };
    // SAFETY: a zeroed flock struct is valid; we set the fields fcntl reads.
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = if take { libc::F_WRLCK } else { libc::F_UNLCK } as _;
    l.l_whence = libc::SEEK_SET as _;
    loop {
        // SAFETY: fcntl on a valid fd with a valid flock struct.
        if unsafe { libc::fcntl(f.as_raw_fd(), cmd, &l) } == 0 {
            return Ok(());
        }
        let e = Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// The router's pid on Unix needs no record: the lock names its holder.
pub(crate) fn announce(_dir: &Path) -> Result<()> {
    Ok(())
}

/// The pid holding the lock, asked of the lock itself.
pub(crate) fn holder(f: &File, _dir: &Path) -> Option<u32> {
    // SAFETY: a zeroed flock struct is valid; F_GETLK fills it in.
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = libc::F_WRLCK as _;
    l.l_whence = libc::SEEK_SET as _;
    // SAFETY: fcntl(F_GETLK) on a valid fd with a valid flock struct.
    let ok = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut l) } == 0;
    let unlocked: libc::c_short = libc::F_UNLCK as _;
    (ok && l.l_type != unlocked).then_some(l.l_pid as u32)
}

/// The router's door, bound in `dir`.
pub(crate) struct Listener(UnixListener);

impl Listener {
    /// Bind the socket. The caller holds the lock, so any socket already at the
    /// path is stale by definition: it is unlinked and rebound.
    pub(crate) fn bind(dir: &Path) -> Result<Listener> {
        let path = address(dir);
        let _ = std::fs::remove_file(&path);
        let uds = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Listener(uds))
    }

    pub(crate) async fn accept(&mut self) -> Result<Served> {
        Ok(self.0.accept().await?.0)
    }
}

/// Connect to the door in `dir`, with the pid of the process serving it.
pub(crate) async fn dial(dir: &Path) -> Result<(Dialed, Option<u32>)> {
    let s = UnixStream::connect(address(dir)).await?;
    let pid = s.peer_cred()?.pid().map(|p| p as u32);
    Ok((s, pid))
}

/// This machine's hostname.
pub(crate) fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length; gethostname NUL-terminates on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if ok {
        String::from_utf8_lossy(buf.split(|b| *b == 0).next().unwrap_or(&[])).into_owned()
    } else {
        String::new()
    }
}
