//! `zapd` native-messaging host mode — pure Rust, folded into the one binary.
//!
//! Browsers spawn this over stdio (native messaging: uint32-LE length + UTF-8
//! JSON — the single platform-forced JSON inch). It CONNECTS to an existing
//! `zapd` router and relays 1:1 between the browser's JSON frames and the binary
//! ZAP envelope on the router socket. The same `zapd` binary is the router (no
//! args) and the host — but the host never *starts* the router: that is the MCP
//! client's job, so the browser can't spin up services. No router ⇒ exit.
//!
//! Fail-fast: if either side drops, the process exits so the extension's
//! reconnect gets a clean host once a consumer is present.

use std::io::{ErrorKind, Result};

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::frame::Frame;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().split('.').next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local".into())
}

/// The host is the machine's local agent: stamp the device hostname into a
/// browser provider id the sandboxed extension can't know.
fn stamp_device(typ: u8, from: &str, host: &str) -> String {
    if typ == crate::frame::HELLO
        && from.starts_with("browser:")
        && from.ends_with("/default")
        && from.matches('/').count() == 1
    {
        return format!("{}/{}/default", &from[..from.len() - "/default".len()], host);
    }
    from.to_string()
}

/// Connect to an already-running router. The host NEVER spawns it: the router's
/// lifecycle belongs to the MCP client, not the browser — a browser-launched
/// process must not start background services. No router ⇒ Err, the host exits
/// cleanly, and the extension backs off until a consumer brings zapd up.
async fn connect_router() -> Result<UnixStream> {
    UnixStream::connect(crate::broker::socket_path()).await
}

/// Read one native-messaging frame from stdin: u32-LE length + JSON.
async fn nm_read<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Option<serde_json::Value>> {
    let mut lenb = [0u8; 4];
    match r.read_exact(&mut lenb).await {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(lenb) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf).ok())
}

/// Write one native-messaging frame to stdout: u32-LE length + JSON.
async fn nm_write<W: AsyncWriteExt + Unpin>(w: &mut W, v: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(v).unwrap_or_default();
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

pub async fn run() -> Result<()> {
    let zsock = connect_router().await?;
    let (mut zr, mut zw) = zsock.into_split();
    let host = hostname();

    // browser stdin -> zapd
    let up = async move {
        let mut stdin = tokio::io::stdin();
        loop {
            let msg = match nm_read(&mut stdin).await {
                Ok(Some(m)) => m,
                _ => break, // extension closed / bad frame
            };
            let typ = msg.get("t").and_then(|v| v.as_u64()).unwrap_or(0) as u8;
            let flags = msg.get("flags").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("");
            let to = msg.get("to").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let payload = msg
                .get("payload")
                .and_then(|v| v.as_str())
                .and_then(|s| B64.decode(s).ok())
                .unwrap_or_default();
            let mut f = Frame::new(typ, stamp_device(typ, from, &host), to, payload);
            f.flags = flags;
            if zw.write_all(&f.encode()).await.is_err() {
                break;
            }
        }
    };

    // zapd -> browser stdout
    let down = async move {
        let mut stdout = tokio::io::stdout();
        loop {
            let f = match Frame::read(&mut zr).await {
                Ok(Some(f)) => f,
                _ => break, // zapd dropped
            };
            let v = serde_json::json!({
                "t": f.typ, "flags": f.flags, "from": f.from, "to": f.to,
                "payload": B64.encode(&f.payload),
            });
            if nm_write(&mut stdout, &v).await.is_err() {
                break;
            }
        }
    };

    // Fail-fast: whichever side ends first, exit so the extension respawns us.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    std::process::exit(0);
}
