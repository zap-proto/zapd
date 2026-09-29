//! Election — which of this user's processes is the router.
//!
//! Every process that speaks ZAP embeds the router and runs `embed()`. Each
//! blocks on one lock file; the kernel grants it to exactly one process, and
//! that process binds the doors and serves until it exits. When it does — clean
//! exit, crash or SIGKILL alike — the kernel releases the lock and wakes exactly
//! one waiter, which binds the same doors. No daemon, no consensus, no polling:
//! the lock is the election and process exit is the resignation.
//!
//! The lock is an fcntl(2) record lock, not flock(2): record locks belong to the
//! process and are not inherited across fork(2). A child forked without exec
//! (Python multiprocessing, say) therefore never holds the router's lock after
//! the router is gone and blocks nobody's takeover.
//!
//! Doors, both owned by the lock holder:
//!   * `<runtime>/zapd.sock` — the UDS, 0600 in a 0700 directory; every local
//!     node connects here. Holding the lock, any socket already at that path is
//!     stale by definition, so it is unlinked and rebound.
//!   * `127.0.0.1:<port>` — the browser door (`door.rs`), port from this user's
//!     pairing (`pair.rs`). If something else holds the port the router keeps
//!     serving the UDS and retries the port.

use std::fs::File;
use std::io::{Error, Result};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::net::{TcpListener, UnixListener};

use crate::router::{self, Registry};
use crate::{door, pair};

/// `$XDG_RUNTIME_DIR/zap`, else `~/.zap/run` (macOS has no runtime dir).
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(r) => PathBuf::from(r).join("zap"),
        None => directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zap")
            .join("run"),
    }
}

/// The UDS every local node connects to.
pub fn socket_path() -> PathBuf {
    runtime_dir().join("zapd.sock")
}

/// Join the election in the background and return at once. The first call in
/// a process starts one thread with its own runtime, so a host needs no async
/// runtime of its own and no knowledge of ours; later calls are no-ops.
pub fn embed() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("zapd".into())
            .spawn(|| {
                let rt = crate::runtime();
                loop {
                    if let Err(e) = lock(libc::F_SETLKW, libc::F_WRLCK) {
                        tracing::error!("zapd: cannot take the router lock: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    ELECTED.store(true, Ordering::SeqCst);
                    tracing::info!("zapd: elected router (pid {})", std::process::id());
                    if let Err(e) = rt.block_on(serve()) {
                        tracing::error!("zapd: router failed: {e}");
                    }
                    // Resign so another process can try; then stand again.
                    ELECTED.store(false, Ordering::SeqCst);
                    let _ = lock(libc::F_SETLK, libc::F_UNLCK);
                    std::thread::sleep(Duration::from_secs(1));
                }
            })
            .expect("spawn the zapd thread");
    });
}

/// This process holds the router lock.
static ELECTED: AtomicBool = AtomicBool::new(false);

/// The pid of this user's router, or `None` if nobody holds the lock. A node
/// talks only to the process this names (it checks the socket's peer), so a
/// process that binds the socket path without holding the lock — an older
/// daemon left running, say — is never mistaken for the router.
pub fn holder() -> Option<u32> {
    if ELECTED.load(Ordering::SeqCst) {
        return Some(std::process::id());
    }
    // SAFETY: a zeroed flock struct is valid; F_GETLK fills it in.
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = libc::F_WRLCK as _;
    l.l_whence = libc::SEEK_SET as _;
    let fd = lock_fd().ok()?;
    // SAFETY: fcntl(F_GETLK) on a valid fd with a valid flock struct.
    let ok = unsafe { libc::fcntl(fd, libc::F_GETLK, &mut l) } == 0;
    (ok && l.l_type != libc::F_UNLCK as _).then_some(l.l_pid as u32)
}

/// One open descriptor on `<runtime>/zapd.lock` per process, never closed.
/// POSIX drops all of a process's record locks on a file when ANY descriptor
/// of it closes, so opening the file afresh to ask who holds it would release
/// the lock this very process holds.
fn lock_fd() -> Result<std::os::unix::io::RawFd> {
    static FILES: Mutex<Vec<(PathBuf, File)>> = Mutex::new(Vec::new());
    let dir = runtime_dir();
    let path = dir.join("zapd.lock");
    let mut files = FILES.lock().unwrap();
    if let Some((_, f)) = files.iter().find(|(p, _)| *p == path) {
        return Ok(f.as_raw_fd());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    let fd = f.as_raw_fd();
    files.push((path, f));
    Ok(fd)
}

/// Take (`F_SETLKW`, blocking) or drop (`F_UNLCK`) the whole-file write lock.
fn lock(cmd: libc::c_int, kind: libc::c_int) -> Result<()> {
    let fd = lock_fd()?;
    // SAFETY: a zeroed flock struct is valid; we set the fields fcntl reads.
    let mut l: libc::flock = unsafe { std::mem::zeroed() };
    l.l_type = kind as _;
    l.l_whence = libc::SEEK_SET as _;
    loop {
        // SAFETY: fcntl on a valid fd with a valid flock struct.
        if unsafe { libc::fcntl(fd, cmd, &l) } == 0 {
            return Ok(());
        }
        let e = Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Bind the doors and serve until the process exits. Returns only if the UDS
/// cannot be bound, which resigns the lock.
async fn serve() -> Result<()> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path); // stale: we hold the lock
    let uds = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("zapd: listening on {}", path.display());

    let registry = Registry::new(&crate::host());
    tokio::spawn(browser_door(registry.clone()));
    loop {
        match uds.accept().await {
            Ok((stream, _)) => {
                let registry = registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = router::handle(stream, registry).await {
                        tracing::debug!("zapd: connection ended: {e}");
                    }
                });
            }
            Err(e) => tracing::warn!("zapd: accept: {e}"),
        }
    }
}

/// Bind this user's door port and serve browsers on it, retrying while the port
/// is held elsewhere: by another program, or for the moment it takes a killed
/// router's process to close its listener after the kernel freed its lock.
async fn browser_door(registry: Registry) {
    let mut wait = Duration::from_millis(50);
    let mut warned = false;
    loop {
        let port = match pair::load() {
            Ok(p) => p.port,
            Err(e) => {
                tracing::error!("zapd: no pairing at {}: {e}", pair::path().display());
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => {
                tracing::info!("zapd: browser door on 127.0.0.1:{port}");
                door::serve(listener, port, registry).await;
                return;
            }
            Err(e) => {
                if !warned && wait >= Duration::from_secs(1) {
                    tracing::warn!(
                        "zapd: browser door 127.0.0.1:{port} unavailable ({e}); retrying"
                    );
                    warned = true;
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(2));
            }
        }
    }
}
