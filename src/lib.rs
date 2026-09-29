//! zapd — the ZAP router, as a library every ZAP process embeds.
//!
//! There is no daemon. A process that speaks ZAP calls [`embed`] and
//! [`Node::join`]: the first makes it a candidate for this user's router (the
//! kernel lock in `elect` picks exactly one), the second gives it a seat on
//! whichever process won. When the router's process exits, another candidate
//! takes over and every node reconnects on its own.
//!
//! Modules: `frame` (the envelope), `id` (node ids), `router` (registry +
//! route + presence),
//! `elect` (the lock and the doors), `door` (the browser's WebSocket), `pair`
//! (the browser's key), `node` (a process's own seat). HIP-1334 is the design.

pub mod frame;
pub mod id;
pub mod pair;

mod door;
mod elect;
mod node;
mod router;

pub use elect::{embed, runtime_dir, socket_path};
pub use frame::{Descriptor, Entry};
pub use id::host;
pub use node::Node;

/// The one runtime the router and every node of this process run on. Owned
/// here so a host needs no async runtime and never shares ours by accident.
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zapd-io")
            .enable_all()
            .build()
            .expect("zapd runtime")
    })
}

/// Run a node call to completion from synchronous code (a Python thread, a CLI).
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    runtime().block_on(f)
}
