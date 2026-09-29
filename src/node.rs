//! A node — this process's own seat on the router.
//!
//! `Node::join` connects to the UDS, says `hello` as `<kind>/<name>`, learns
//! its full id from `welcome`, and stays registered: when the
//! router's process exits and another takes over, the connection drops and the
//! node reconnects (50 ms doubling to 1 s) and registers again under the same
//! id. Hosts never write a reconnect loop of their own.
//!
//! Calls are one at a time per node, matched by the responder's `from`:
//! correlation lives in the payload's schema, not the envelope, so the node
//! does not interleave two calls it could not tell apart.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};

use crate::frame::{self, Frame, ProviderEntry};

type Reply = oneshot::Sender<Result<Frame>>;

struct Inner {
    /// Proposed as `<kind>/<name>`; the router's full id once `welcome` names it.
    id: Mutex<String>,
    /// The live connection's writer; `None` while (re)connecting.
    tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    up: watch::Sender<bool>,
    /// The one outstanding call: who must answer, and where the answer goes.
    waiting: Mutex<Option<(String, Reply)>>,
    turn: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct Node(Arc<Inner>);

impl Node {
    /// Register `id` on this user's router and keep it registered.
    pub fn join(id: &str, role: u8, brand: &str, caps: &[String]) -> Node {
        let (up, _) = watch::channel(false);
        let node = Node(Arc::new(Inner {
            id: Mutex::new(id.to_string()),
            tx: Mutex::new(None),
            up,
            waiting: Mutex::new(None),
            turn: tokio::sync::Mutex::new(()),
        }));
        let hello =
            Frame::new(frame::HELLO, id, "", frame::encode_hello(role, brand, caps)).encode();
        let inner = node.0.clone();
        crate::runtime().spawn(async move {
            let mut backoff = Duration::from_millis(50);
            loop {
                if let Ok(s) = UnixStream::connect(crate::socket_path()).await {
                    backoff = Duration::from_millis(50);
                    if let Err(e) = session(s, &hello, &inner).await {
                        tracing::debug!("zapd: node {} dropped: {e}", inner.id.lock().unwrap());
                    }
                    inner.tx.lock().unwrap().take();
                    inner.up.send_replace(false);
                    if let Some((_, reply)) = inner.waiting.lock().unwrap().take() {
                        let _ = reply.send(Err(Error::new(
                            ErrorKind::ConnectionReset,
                            "router changed",
                        )));
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
        });
        node
    }

    /// This node's id: `<kind>/<host>/<name>` once registered.
    pub fn id(&self) -> String {
        self.0.id.lock().unwrap().clone()
    }

    /// Route `payload` to `to` and return its RESPONSE payload.
    pub async fn call(&self, to: &str, payload: Vec<u8>, timeout: Duration) -> Result<Vec<u8>> {
        tokio::time::timeout(timeout, async {
            let f = self
                .ask(Frame::new(frame::ROUTE, "", to, payload), to)
                .await?;
            Ok(f.payload)
        })
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, format!("zapd: {to} did not answer")))?
    }

    /// Every node on this router.
    pub async fn nodes(&self, timeout: Duration) -> Result<Vec<ProviderEntry>> {
        tokio::time::timeout(timeout, async {
            let f = self
                .ask(
                    Frame::new(frame::PROVIDERS_LIST, "", "", Vec::new()),
                    "zapd",
                )
                .await?;
            frame::decode_providers(&f.payload)
        })
        .await
        .map_err(|_| Error::new(ErrorKind::TimedOut, "zapd: no router on this machine"))?
    }

    async fn ask(&self, f: Frame, answerer: &str) -> Result<Frame> {
        let _turn = self.0.turn.lock().await;
        let mut up = self.0.up.subscribe();
        up.wait_for(|u| *u).await.map_err(Error::other)?;
        let (reply, answer) = oneshot::channel();
        *self.0.waiting.lock().unwrap() = Some((answerer.to_string(), reply));
        let sent = self
            .0
            .tx
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|tx| tx.send(f.encode()).is_ok());
        if !sent {
            self.0.waiting.lock().unwrap().take();
            return Err(Error::new(ErrorKind::ConnectionReset, "router changed"));
        }
        answer
            .await
            .map_err(|_| Error::new(ErrorKind::ConnectionReset, "router changed"))?
    }
}

/// One connection's life: HELLO, WELCOME, then answers until it drops.
async fn session(s: UnixStream, hello: &[u8], inner: &Inner) -> Result<()> {
    let (mut rd, mut wr) = s.into_split();
    wr.write_all(hello).await?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(b) = rx.recv().await {
            if wr.write_all(&b).await.is_err() {
                break;
            }
        }
    });
    let result = async {
        while let Some(f) = Frame::read(&mut rd).await? {
            match f.typ {
                frame::WELCOME => {
                    *inner.id.lock().unwrap() = f.to.clone();
                    *inner.tx.lock().unwrap() = Some(tx.clone());
                    inner.up.send_replace(true);
                }
                frame::PROVIDERS => settle(inner, "zapd", Ok(f)),
                frame::RESPONSE => {
                    let from = f.from.clone();
                    settle(inner, &from, Ok(f));
                }
                frame::ERROR => {
                    let why = String::from_utf8_lossy(&f.payload).into_owned();
                    match why.strip_prefix("no_route:") {
                        Some(dest) => settle(
                            inner,
                            dest,
                            Err(Error::new(ErrorKind::NotFound, why.clone())),
                        ),
                        None => tracing::warn!("zapd: node {}: {why}", inner.id.lock().unwrap()),
                    }
                }
                _ => {} // presence and inbound calls: phase 1 nodes only call out
            }
        }
        Ok(())
    }
    .await;
    writer.abort();
    result
}

/// Hand an answer to the outstanding call if it came from the one it asked.
fn settle(inner: &Inner, from: &str, answer: Result<Frame>) {
    let mut waiting = inner.waiting.lock().unwrap();
    if waiting.as_ref().is_some_and(|(who, _)| who == from) {
        let (_, reply) = waiting.take().unwrap();
        let _ = reply.send(answer);
    }
}
