//! The browser door — a loopback WebSocket, because a browser extension can
//! open nothing else: no socket, no process, and (in snap/Flatpak browsers) no
//! native-messaging host either.
//!
//! Any web page can reach 127.0.0.1 too, and any local process — including
//! another user's — can connect and send whatever headers it likes. So a
//! connection must pass two independent checks before the router hears a byte:
//!
//! 1. **Origin + Host** (at the HTTP upgrade). The browser, not the page, sets
//!    `Origin`; a page cannot forge it. Admitted: our Blink extension id (fixed
//!    by the manifest `key`), and any `moz-extension://` or
//!    `safari-web-extension://` origin — those ids are random per install, so
//!    there the proof below is what authenticates. `Host` must be this loopback
//!    port, which also refuses DNS rebinding.
//! 2. **Pairing proof** (`pair.rs`): mutual HMAC over this user's pairing key,
//!    router first. A process that sets any Origin it likes still cannot answer
//!    without the key, which is 0600 in this user's config.
//!
//! After both, the browser is one more node: its frames — the ZAP router
//! envelope, one per WebSocket binary message — are piped into `router::handle`
//! exactly as a UDS stream would be, except that a browser may only address the
//! router or answer a call (`originates`), and 60 s of silence closes it.

use std::io::{Error, ErrorKind, Result};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::WebSocketStream;

use crate::frame::{self, Frame};
use crate::pair::{self, Pairing};
use crate::router::{self, Registry};

/// Blink extension origins, each fixed by its manifest `key`. A brand adds its
/// id here when it ships an extension.
const BLINK: &[&str] = &[
    "chrome-extension://biingenefmanpecedoafkfajbnlgdmbl", // Hanzo AI
];

/// Per-install random origins; the pairing proof authenticates these.
const RANDOM: &[&str] = &["moz-extension://", "safari-web-extension://"];

/// How long a connection has to finish the upgrade and the proof.
const ADMIT: Duration = Duration::from_secs(5);

/// A paired browser that sends nothing for this long is gone. The extension
/// probes every 20 s, which is also what keeps an MV3 service worker alive.
const IDLE: Duration = Duration::from_secs(60);

pub async fn serve(listener: TcpListener, port: u16, registry: Registry) {
    loop {
        match listener.accept().await {
            Ok((tcp, _)) => {
                let registry = registry.clone();
                tokio::spawn(async move {
                    if let Err(e) = admit(tcp, port, registry).await {
                        tracing::info!("zapd: door refused: {e}");
                    }
                });
            }
            Err(e) => tracing::warn!("zapd: door accept: {e}"),
        }
    }
}

/// The Origin/Host check, as the upgrade callback.
pub fn check(
    origin: Option<&str>,
    host: Option<&str>,
    port: u16,
) -> std::result::Result<(), &'static str> {
    let host_ok =
        host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return Err("host");
    }
    match origin {
        Some(o)
            if BLINK.contains(&o)
                || RANDOM.iter().any(|p| o.starts_with(p) && o.len() > p.len()) =>
        {
            Ok(())
        }
        _ => Err("origin"),
    }
}

async fn admit(tcp: TcpStream, port: u16, registry: Registry) -> Result<()> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(frame::MAX_FRAME as usize + 4))
        .max_frame_size(Some(frame::MAX_FRAME as usize + 4));
    // The Err type is tungstenite's upgrade-callback contract, not ours to shrink.
    #[allow(clippy::result_large_err)]
    let callback =
        |req: &Request, resp: Response| -> std::result::Result<Response, ErrorResponse> {
            let header = |name| req.headers().get(name).and_then(|v| v.to_str().ok());
            match check(header("origin"), header("host"), port) {
                Ok(()) => Ok(resp),
                Err(why) => {
                    let mut refuse = ErrorResponse::new(Some(format!("zapd: {why} refused")));
                    *refuse.status_mut() = StatusCode::FORBIDDEN;
                    Err(refuse)
                }
            }
        };
    let mut ws = tokio::time::timeout(ADMIT, async {
        let mut ws = tokio_tungstenite::accept_hdr_async_with_config(tcp, callback, Some(config))
            .await
            .map_err(|e| Error::new(ErrorKind::PermissionDenied, e.to_string()))?;
        prove(&mut ws).await?;
        Ok::<_, Error>(ws)
    })
    .await
    .map_err(|_| Error::new(ErrorKind::TimedOut, "admission timed out"))??;

    // Paired. Pipe the socket into the router as one more node.
    let (ours, theirs) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        if let Err(e) = router::handle(theirs, registry).await {
            tracing::debug!("zapd: browser ended: {e}");
        }
    });
    let (mut rd, mut wr) = tokio::io::split(ours);
    let mut quiet = Box::pin(tokio::time::sleep(IDLE));
    loop {
        tokio::select! {
            m = ws.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    quiet.as_mut().reset(tokio::time::Instant::now() + IDLE);
                    let f = Frame::decode(&b)?;
                    if originates(&f) {
                        let no = Frame::new(frame::ERROR, "zapd", "", format!("forbidden:{}", f.to).into_bytes());
                        ws.send(Message::Binary(Bytes::from(no.encode()))).await.map_err(Error::other)?;
                        continue;
                    }
                    wr.write_all(&b).await?;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(_)) | Some(Err(_)) | None => return Ok(()),
            },
            f = Frame::read(&mut rd) => match f? {
                Some(f) => ws.send(Message::Binary(Bytes::from(f.encode()))).await.map_err(Error::other)?,
                None => return Ok(()),
            },
            _ = &mut quiet => return Err(Error::new(ErrorKind::TimedOut, "browser went quiet")),
        }
    }
}

/// A browser is driven and never drives: it renders hostile pages, so a frame
/// from it may address the router (`to` empty) or answer a call (RESPONSE),
/// and nothing else. A browser that tries to call another node — an agent, a
/// dev session — is refused.
fn originates(f: &Frame) -> bool {
    !f.to.is_empty() && f.typ != frame::RESPONSE
}

/// The pairing proof, router side. The key is re-read per connection, so a
/// reset takes effect at once.
async fn prove(ws: &mut WebSocketStream<TcpStream>) -> Result<()> {
    let refused = || Error::new(ErrorKind::PermissionDenied, "pairing proof failed");
    let key: Pairing = pair::load()?;
    let nc = auth(ws).await?;
    if nc.len() != pair::NONCE {
        return Err(refused());
    }
    let ns = pair::nonce()?;
    let mut hello = ns.to_vec();
    hello.extend_from_slice(&key.proof(pair::ROUTER, &nc, &ns));
    ws.send(Message::Binary(Bytes::from(
        Frame::new(frame::AUTH, "", "", hello).encode(),
    )))
    .await
    .map_err(Error::other)?;
    let proof = auth(ws).await?;
    if !key.verify(pair::CLIENT, &nc, &ns, &proof) {
        return Err(refused());
    }
    Ok(())
}

/// Read one AUTH frame's payload; anything else ends admission.
async fn auth(ws: &mut WebSocketStream<TcpStream>) -> Result<Vec<u8>> {
    match ws.next().await {
        Some(Ok(Message::Binary(b))) => match Frame::decode(&b)? {
            f if f.typ == frame::AUTH => Ok(f.payload),
            _ => Err(Error::new(ErrorKind::PermissionDenied, "expected AUTH")),
        },
        _ => Err(Error::new(ErrorKind::PermissionDenied, "expected AUTH")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_our_extensions_only() {
        let port = 21000;
        let host = Some("127.0.0.1:21000");
        assert!(check(
            Some("chrome-extension://biingenefmanpecedoafkfajbnlgdmbl"),
            host,
            port
        )
        .is_ok());
        assert!(check(
            Some("moz-extension://6c3a0b3e-1111-2222-3333-444455556666"),
            host,
            port
        )
        .is_ok());
        assert!(check(
            Some("safari-web-extension://ABCD"),
            Some("localhost:21000"),
            port
        )
        .is_ok());
        for bad in [
            None,
            Some("https://evil.example"),
            Some("null"),
            Some("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("moz-extension://"),
            Some("http://127.0.0.1:21000"),
        ] {
            assert_eq!(check(bad, host, port), Err("origin"), "{bad:?}");
        }
    }

    #[test]
    fn host_must_be_this_loopback_port() {
        let o = Some("chrome-extension://biingenefmanpecedoafkfajbnlgdmbl");
        for bad in [
            None,
            Some("evil.example:21000"),
            Some("127.0.0.1:21001"),
            Some("127.0.0.1"),
        ] {
            assert_eq!(check(o, bad, 21000), Err("host"), "{bad:?}");
        }
    }

    #[test]
    fn browsers_answer_and_never_call() {
        assert!(!originates(&Frame::new(
            frame::PROVIDERS_LIST,
            "",
            "",
            vec![]
        )));
        assert!(!originates(&Frame::new(
            frame::RESPONSE,
            "",
            "agent/dgx/hanzo-1",
            vec![]
        )));
        assert!(originates(&Frame::new(
            frame::ROUTE,
            "",
            "dev/dgx/42",
            vec![]
        )));
        assert!(originates(&Frame::new(
            frame::EVENT,
            "",
            "agent/dgx/hanzo-1",
            vec![]
        )));
    }
}
