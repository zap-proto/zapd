//! The router — registry, route, presence, and nothing else.
//!
//! Brand-neutral and dumb-and-strong: every ZAP node of one user login (the
//! browser, agents, the dev CLI, the IDE, the desktop app) is connected here,
//! through the UDS or the browser door, and this module relays opaque frames
//! between them:
//!   * **registry** — who is connected (`id → connection`, role, brand, caps),
//!   * **route**    — relay an opaque frame from A to B by its `to` field,
//!   * **presence** — broadcast peer connected/disconnected.
//!
//! It never parses a payload, never speaks a schema, never holds a lease. The
//! identity it owns is the id: a `hello` names a kind and a name, the router
//! stamps its own host into it (`id.rs`), hands the result back in `welcome`,
//! and stamps it onto every frame that connection sends — a node can never
//! speak as another.
//!
//! Transport-free: `handle` takes any byte stream. The UDS hands it the socket;
//! the door hands it one end of an in-memory pipe after the browser proves it
//! is paired. Which process runs this is decided by `elect.rs`.

use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::frame::{self, Frame, ProviderEntry};
use crate::id;

/// A connected node. `tx` feeds its per-connection writer pump.
struct Peer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    role: u8,
    brand: String,
    caps: Vec<String>,
    /// Per-connection generation token. On disconnect a peer removes its own
    /// registry entry only if the entry is still its own — so a faster reconnect
    /// that already replaced it (same id) is never clobbered.
    token: u64,
    /// Fired when a newer connection takes this id: the old one is closed, so
    /// two connections never speak as one node.
    evict: oneshot::Sender<()>,
}

#[derive(Clone)]
pub struct Registry {
    peers: Arc<Mutex<HashMap<String, Peer>>>,
    host: Arc<str>,
}

impl Registry {
    pub fn new(host: &str) -> Registry {
        Registry {
            peers: Arc::default(),
            host: host.into(),
        }
    }
}

/// Monotonic per-connection token source (see `Peer::token`).
static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Serve one node: require `hello`, register, then relay frames until it goes.
pub async fn handle<S>(stream: S, registry: Registry) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);

    // 1) Require HELLO — the node proposes `<kind>/<name>`, role, brand, caps.
    let hello = match Frame::read(&mut rd).await? {
        Some(f) if f.typ == frame::HELLO => f,
        Some(_) => return Err(Error::new(ErrorKind::InvalidData, "expected HELLO")),
        None => return Ok(()),
    };
    let Some(id) = id::stamp(&hello.from, &registry.host) else {
        let why = format!("bad_id:{}", hello.from);
        wr.write_all(&Frame::new(frame::ERROR, "zapd", "", why.clone().into_bytes()).encode())
            .await?;
        return Err(Error::new(ErrorKind::InvalidData, why));
    };
    let (role, brand, caps) = frame::decode_hello(&hello.payload)?;

    // 2) Register, last-writer-wins. A reconnecting node with the same id takes
    //    over rather than being rejected as a duplicate — otherwise a stale
    //    entry wedges the id and the live node can never register. The old
    //    connection is closed.
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (evict, mut evicted_rx) = oneshot::channel();
    let token = NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let evicted = registry.peers.lock().unwrap().insert(
        id.clone(),
        Peer {
            tx: tx.clone(),
            role,
            brand: brand.clone(),
            caps,
            token,
            evict,
        },
    );
    if let Some(old) = evicted {
        let _ = old.evict.send(());
        tracing::info!("zapd: {id} reconnected — replaced stale peer");
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
    broadcast(
        &registry,
        &id,
        frame::PEER_CONNECTED,
        frame::encode_peer(&id),
    );

    // 3) Relay until EOF, error, or eviction.
    let result = tokio::select! {
        r = route_loop(&mut rd, &registry, &id) => r,
        _ = &mut evicted_rx => Ok(()),
    };

    // 4) Presence: remove + announce departure — but only if this connection is
    //    still the registered one.
    let was_current = {
        let mut reg = registry.peers.lock().unwrap();
        if reg.get(&id).map(|p| p.token) == Some(token) {
            reg.remove(&id);
            true
        } else {
            false
        }
    };
    if was_current {
        broadcast(
            &registry,
            &id,
            frame::PEER_DISCONNECTED,
            frame::encode_peer(&id),
        );
        tracing::info!("zapd: {id} offline");
    }
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
                    let entries = list(registry, &filter);
                    let reply = Frame::new(
                        frame::PROVIDERS,
                        "zapd",
                        id,
                        frame::encode_providers(&entries),
                    );
                    send_to(registry, id, reply.encode());
                }
                frame::HELLO => { /* already greeted */ }
                _ => {
                    let err = Frame::new(frame::ERROR, "zapd", id, b"unknown_control".to_vec());
                    send_to(registry, id, err.encode());
                }
            }
        } else {
            // Forward verbatim, stamping the registered sender id so a node can
            // never spoof `from`. Payload stays opaque.
            let dest = f.to.clone();
            f.from = id.to_string();
            if !send_to(registry, &dest, f.encode()) {
                let err = Frame::new(
                    frame::ERROR,
                    "zapd",
                    id,
                    format!("no_route:{dest}").into_bytes(),
                );
                send_to(registry, id, err.encode());
            }
        }
    }
    Ok(())
}

/// Every node on this router, each with its role. A caller that wants only
/// providers filters on the role; the registry answers what it holds.
fn list(registry: &Registry, brand_filter: &str) -> Vec<ProviderEntry> {
    let reg = registry.peers.lock().unwrap();
    reg.iter()
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
    let reg = registry.peers.lock().unwrap();
    reg.get(id)
        .map(|p| p.tx.send(bytes).is_ok())
        .unwrap_or(false)
}

/// Presence fan-out to every peer except the subject.
fn broadcast(registry: &Registry, except: &str, typ: u8, payload: Vec<u8>) {
    let reg = registry.peers.lock().unwrap();
    for (pid, p) in reg.iter() {
        if pid == except {
            continue;
        }
        let f = Frame::new(typ, "zapd", pid, payload.clone());
        let _ = p.tx.send(f.encode());
    }
}
