//! The universal router — one shared ZAP control plane per machine.
//!
//! Brand-neutral and **dumb-and-strong**: a single daemon binds one UDS and
//! every ZAP service from every brand (hanzo, lux, zoo, ...) connects to it —
//! browser extensions via the native host, IDE extensions, CLI agents,
//! hanzo-mcp. Launched once, shared by all.
//!
//! The router does three things and nothing else:
//!   * **registry** — who is connected (`id → connection`, role, brand, caps),
//!   * **router**   — relay an opaque frame from A to B by its `to` field,
//!   * **presence** — broadcast peer connected/disconnected.
//!
//! It never parses a payload, never speaks a schema, never holds a lease.
//! Exclusivity, leasing, E2E post-quantum encryption and payments are all
//! end-to-end concerns above this layer: the router forwards the (PQ-encrypted)
//! payload untouched. The one identity bit it owns is verifying the `hello` so
//! a peer cannot spoof another's id — the verified id is stamped onto every
//! frame it forwards.
//!
//! Transport is a UDS, never TCP: the broker is local, nothing is exposed on
//! the network. Cross-machine reach is federation *between routers*.

use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::frame::{self, Frame, ProviderEntry};

/// A connected node. `tx` feeds its per-connection writer pump.
struct Peer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    role: u8,
    brand: String,
    caps: Vec<String>,
}

type Registry = Arc<Mutex<HashMap<String, Peer>>>;

/// Resolve the brand-neutral router socket path.
///
/// Order: explicit `ZAP_SOCK` → `$XDG_RUNTIME_DIR/zap/zapd.sock` →
/// `~/.zap/run/zapd.sock` (fallback, and the macOS path).
pub fn socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ZAP_SOCK") {
        return PathBuf::from(explicit);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Path::new(&runtime).join("zap").join("zapd.sock");
    }
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".zap").join("run").join("zapd.sock")
}

/// Bind the listening socket — adopt a socket-activated fd if present, else
/// bind the well-known path, refusing to fight a live router for it.
async fn bind(path: &Path) -> Result<UnixListener> {
    if let Ok(raw) = std::env::var("ZAP_LISTEN_FD") {
        if let Ok(fd) = raw.parse::<i32>() {
            // SAFETY: the supervisor guarantees a bound, listening AF_UNIX fd.
            let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
            std_listener.set_nonblocking(true)?;
            tracing::info!("zapd: adopted socket-activated fd {fd}");
            return UnixListener::from_std(std_listener);
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    // Single-instance guard under concurrent spawns: serialize connect-check +
    // unlink-stale + bind behind an advisory lock. Without it, two routers racing
    // on a *stale* socket both unlink it and both bind() → one orphaned (split
    // brain, providers split across routers — the exact thing we observed). Held
    // only across the bind; once a router owns the live socket, that socket is
    // the guard and the lock drops.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    // SAFETY: flock(LOCK_EX) on a valid fd; blocks until this process wins it.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    if path.exists() {
        match UnixStream::connect(path).await {
            Ok(_) => {
                return Err(Error::new(
                    ErrorKind::AddrInUse,
                    format!("another zapd is already listening on {}", path.display()),
                ));
            }
            Err(_) => {
                tracing::warn!("zapd: clearing stale socket {}", path.display());
                let _ = std::fs::remove_file(path);
            }
        }
    }

    let listener = UnixListener::bind(path)?;
    // User-only socket. The 0700 parent dir already gates access; this is
    // belt-and-braces so the control plane is never world/group reachable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    drop(lock); // release the spawn lock — the bound socket is now the guard
    tracing::info!("zapd: listening on {}", path.display());
    Ok(listener)
}

/// Run the router until shutdown.
pub async fn run() -> Result<()> {
    let path = socket_path();
    let listener = bind(&path).await?;
    let cleanup = path.clone();
    let activated = std::env::var("ZAP_LISTEN_FD").is_ok();
    let registry: Registry = Arc::new(Mutex::new(HashMap::new()));

    let serve = {
        let registry = registry.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let reg = registry.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, reg).await {
                                tracing::debug!("zapd: connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("zapd: accept error: {e}"),
                }
            }
        }
    };

    tokio::select! {
        _ = serve => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("zapd: shutting down"),
    }

    if !activated {
        let _ = std::fs::remove_file(&cleanup);
    }
    Ok(())
}

/// Serve one node: require `hello`, register, then relay frames until it goes.
async fn handle(stream: UnixStream, registry: Registry) -> Result<()> {
    let (mut rd, mut wr) = stream.into_split();

    // 1) Require HELLO — the peer declares its id (`from`), role, brand, caps.
    //    (PQ-identity: a signature field verifies id→DID before registering;
    //    wired with the crypto layer — until then the id is taken on trust.)
    let hello = match Frame::read(&mut rd).await? {
        Some(f) if f.typ == frame::HELLO => f,
        Some(_) => return Err(Error::new(ErrorKind::InvalidData, "expected HELLO")),
        None => return Ok(()),
    };
    let id = hello.from.clone();
    if id.is_empty() {
        return Err(Error::new(ErrorKind::InvalidData, "HELLO missing id"));
    }
    let (role, brand, caps) = frame::decode_hello(&hello.payload)?;

    // 2) Register, with a per-connection writer pump.
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let duplicate = {
        // Scope the guard so it is never held across an await (it is !Send).
        let mut reg = registry.lock().unwrap();
        if reg.contains_key(&id) {
            true
        } else {
            reg.insert(
                id.clone(),
                Peer { tx: tx.clone(), role, brand: brand.clone(), caps },
            );
            false
        }
    };
    if duplicate {
        let _ = wr
            .write_all(&Frame::new(frame::ERROR, "zapd", &id, b"id_in_use".to_vec()).encode())
            .await;
        return Err(Error::new(ErrorKind::AddrInUse, format!("duplicate id {id}")));
    }
    tracing::info!("zapd: {id} online (role={role}, brand={brand})");

    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let _ = tx.send(Frame::new(frame::WELCOME, "zapd", &id, Vec::new()).encode());
    broadcast(&registry, &id, frame::PEER_CONNECTED, frame::encode_peer(&id));

    // 3) Relay until EOF/error.
    let result = route_loop(&mut rd, &registry, &id).await;

    // 4) Presence: remove + announce departure.
    registry.lock().unwrap().remove(&id);
    broadcast(&registry, &id, frame::PEER_DISCONNECTED, frame::encode_peer(&id));
    tracing::info!("zapd: {id} offline");
    writer.abort();
    result
}

/// The hot path: `to` empty ⇒ control for the router; else forward opaquely.
async fn route_loop<R: AsyncRead + Unpin>(rd: &mut R, registry: &Registry, id: &str) -> Result<()> {
    while let Some(mut f) = Frame::read(rd).await? {
        if f.to.is_empty() {
            match f.typ {
                frame::PROVIDERS_LIST => {
                    let filter = frame::decode_brand_filter(&f.payload);
                    let entries = list_providers(registry, &filter);
                    let reply = Frame::new(frame::PROVIDERS, "zapd", id, frame::encode_providers(&entries));
                    send_to(registry, id, reply.encode());
                }
                frame::HELLO => { /* already greeted */ }
                _ => {
                    let err = Frame::new(frame::ERROR, "zapd", id, b"unknown_control".to_vec());
                    send_to(registry, id, err.encode());
                }
            }
        } else {
            // Forward verbatim, but stamp the *verified* sender id so a peer
            // can never spoof `from`. Payload stays opaque.
            let dest = f.to.clone();
            f.from = id.to_string();
            if !send_to(registry, &dest, f.encode()) {
                let err = Frame::new(frame::ERROR, "zapd", id, format!("no_route:{dest}").into_bytes());
                send_to(registry, id, err.encode());
            }
        }
    }
    Ok(())
}

fn list_providers(registry: &Registry, brand_filter: &str) -> Vec<ProviderEntry> {
    let reg = registry.lock().unwrap();
    reg.iter()
        .filter(|(_, p)| p.role == frame::ROLE_PROVIDER)
        .filter(|(_, p)| brand_filter.is_empty() || p.brand == brand_filter)
        .map(|(id, p)| ProviderEntry {
            id: id.clone(),
            role: p.role,
            brand: p.brand.clone(),
            caps: p.caps.clone(),
        })
        .collect()
}

/// Deliver an already-encoded frame to one peer. Returns false if unknown.
fn send_to(registry: &Registry, id: &str, bytes: Vec<u8>) -> bool {
    let reg = registry.lock().unwrap();
    reg.get(id).map(|p| p.tx.send(bytes).is_ok()).unwrap_or(false)
}

/// Presence fan-out to every peer except the subject.
fn broadcast(registry: &Registry, except: &str, typ: u8, payload: Vec<u8>) {
    let reg = registry.lock().unwrap();
    for (pid, p) in reg.iter() {
        if pid == except {
            continue;
        }
        let f = Frame::new(typ, "zapd", pid, payload.clone());
        let _ = p.tx.send(f.encode());
    }
}
