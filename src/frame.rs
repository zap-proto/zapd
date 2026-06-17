//! The ZAP router envelope — compact binary, schema-agnostic. Not capnp, not
//! JSON. zapd parses **only** this envelope; it never parses a `.zap` payload
//! schema (browser, payments, identity, PQ channel) — those ride inside
//! `payload` as opaque bytes and are an end-to-end concern between peers.
//!
//! Envelope (little-endian):
//!   u32 len            bytes that follow
//!   u8  type
//!   u16 flags
//!   u16 from_len
//!   u16 to_len
//!   u32 payload_len
//!   bytes from         source id   (router stamps the verified id)
//!   bytes to           destination (empty ⇒ the frame is for zapd)
//!   bytes payload      opaque
//!
//! Routing rule: `to` empty ⇒ for zapd (hello / providers.list); `to` set ⇒
//! forward opaquely. Request/response correlation lives in the payload's `.zap`
//! schema, not here — the router does not correlate.
//!
//! The HELLO / PROVIDERS bodies below are zapd's *own* control protocol (the
//! `to`-empty frames), not application payloads — the router owns them.

use std::io::{Error, ErrorKind, Result};

use tokio::io::{AsyncRead, AsyncReadExt};

// Envelope types the router acts on.
pub const HELLO: u8 = 1;
pub const WELCOME: u8 = 2;
pub const PROVIDERS_LIST: u8 = 3;
pub const PROVIDERS: u8 = 4;
pub const PEER_CONNECTED: u8 = 5;
pub const PEER_DISCONNECTED: u8 = 6;
pub const ERROR: u8 = 7;
// Pass-through types the router forwards but never interprets.
pub const ROUTE: u8 = 16;
pub const RESPONSE: u8 = 17;
pub const EVENT: u8 = 18;

// Roles.
pub const ROLE_PROVIDER: u8 = 1;
pub const ROLE_CONSUMER: u8 = 2;
pub const ROLE_ROUTER: u8 = 3;

const HEADER: usize = 1 + 2 + 2 + 2 + 4; // type + flags + from_len + to_len + payload_len
const MAX_FRAME: u32 = 64 * 1024 * 1024;

/// A parsed ZAP router envelope. `payload` is opaque to the router.
#[derive(Debug, Clone)]
pub struct Frame {
    pub typ: u8,
    pub flags: u16,
    pub from: String,
    pub to: String,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(typ: u8, from: impl Into<String>, to: impl Into<String>, payload: Vec<u8>) -> Self {
        Self { typ, flags: 0, from: from.into(), to: to.into(), payload }
    }

    /// Serialize to the wire.
    pub fn encode(&self) -> Vec<u8> {
        let from = self.from.as_bytes();
        let to = self.to.as_bytes();
        let body = HEADER + from.len() + to.len() + self.payload.len();
        let mut b = Vec::with_capacity(4 + body);
        b.extend_from_slice(&(body as u32).to_le_bytes());
        b.push(self.typ);
        b.extend_from_slice(&self.flags.to_le_bytes());
        b.extend_from_slice(&(from.len() as u16).to_le_bytes());
        b.extend_from_slice(&(to.len() as u16).to_le_bytes());
        b.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        b.extend_from_slice(from);
        b.extend_from_slice(to);
        b.extend_from_slice(&self.payload);
        b
    }

    /// Read one frame. `Ok(None)` on a clean EOF between frames.
    pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
        let mut lenb = [0u8; 4];
        match r.read_exact(&mut lenb).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(lenb);
        if len > MAX_FRAME || (len as usize) < HEADER {
            return Err(Error::new(ErrorKind::InvalidData, "zapd: bad frame length"));
        }
        let mut buf = vec![0u8; len as usize];
        r.read_exact(&mut buf).await?;

        let mut c = Cursor::new(&buf);
        let typ = c.u8()?;
        let flags = c.u16()?;
        let from_len = c.u16()? as usize;
        let to_len = c.u16()? as usize;
        let pay_len = c.u32()? as usize;
        let from = c.string(from_len)?;
        let to = c.string(to_len)?;
        let payload = c.take(pay_len)?.to_vec();
        Ok(Some(Frame { typ, flags, from, to, payload }))
    }
}

/// Bounds-checked little-endian reader.
pub struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.p + n > self.b.len() {
            return Err(Error::new(ErrorKind::InvalidData, "zapd: truncated frame"));
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self, n: usize) -> Result<String> {
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }

    /// u16-length-prefixed string (for control bodies).
    pub fn str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        self.string(n)
    }
}

// ── zapd control bodies (HELLO / PROVIDERS) — the router's own protocol ────

pub fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u16).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

/// HELLO body: role(u8) + brand(str) + caps(u16 count + str…).
pub fn encode_hello(role: u8, brand: &str, caps: &[String]) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(role);
    put_str(&mut b, brand);
    b.extend_from_slice(&(caps.len() as u16).to_le_bytes());
    for c in caps {
        put_str(&mut b, c);
    }
    b
}

/// Parse a HELLO body → (role, brand, caps).
pub fn decode_hello(payload: &[u8]) -> Result<(u8, String, Vec<String>)> {
    let mut c = Cursor::new(payload);
    let role = c.u8()?;
    let brand = c.str()?;
    let n = c.u16()? as usize;
    let mut caps = Vec::with_capacity(n);
    for _ in 0..n {
        caps.push(c.str()?);
    }
    Ok((role, brand, caps))
}

pub struct ProviderEntry {
    pub id: String,
    pub role: u8,
    pub brand: String,
    pub caps: Vec<String>,
}

/// PROVIDERS body: u16 count + per entry (id, role, brand, caps).
pub fn encode_providers(entries: &[ProviderEntry]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in entries {
        put_str(&mut b, &e.id);
        b.push(e.role);
        put_str(&mut b, &e.brand);
        b.extend_from_slice(&(e.caps.len() as u16).to_le_bytes());
        for c in &e.caps {
            put_str(&mut b, c);
        }
    }
    b
}

/// PROVIDERS_LIST body: optional brand filter (empty = all).
pub fn decode_brand_filter(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    Cursor::new(payload).str().unwrap_or_default()
}

/// PEER_* body: a single peer id.
pub fn encode_peer(id: &str) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, id);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip() {
        let f = Frame::new(ROUTE, "consumer:mcp/1", "browser:chrome/dbc/default", b"opaque-\x00\x01\x02".to_vec());
        let bytes = f.encode();
        let mut c = Cursor::new(&bytes[4..]); // skip the u32 frame len
        assert_eq!(c.u8().unwrap(), ROUTE);
        assert_eq!(c.u16().unwrap(), 0); // flags
        let fl = c.u16().unwrap() as usize;
        let tl = c.u16().unwrap() as usize;
        let pl = c.u32().unwrap() as usize;
        assert_eq!(c.take(fl).unwrap(), b"consumer:mcp/1");
        assert_eq!(c.take(tl).unwrap(), b"browser:chrome/dbc/default");
        assert_eq!(c.take(pl).unwrap(), b"opaque-\x00\x01\x02");
    }

    #[test]
    fn hello_roundtrip() {
        let body = encode_hello(ROLE_PROVIDER, "hanzo", &["browser.tabs".into(), "browser.navigate".into()]);
        let (role, brand, caps) = decode_hello(&body).unwrap();
        assert_eq!(role, ROLE_PROVIDER);
        assert_eq!(brand, "hanzo");
        assert_eq!(caps, vec!["browser.tabs".to_string(), "browser.navigate".to_string()]);
    }

    #[test]
    fn providers_roundtrip() {
        let entries = vec![ProviderEntry {
            id: "browser:chrome/dbc/default".into(),
            role: ROLE_PROVIDER,
            brand: "hanzo".into(),
            caps: vec!["tabs".into()],
        }];
        let body = encode_providers(&entries);
        let mut c = Cursor::new(&body);
        assert_eq!(c.u16().unwrap(), 1); // count
        assert_eq!(c.str().unwrap(), "browser:chrome/dbc/default");
    }

    #[test]
    fn cursor_rejects_truncation() {
        let mut c = Cursor::new(&[0u8, 1]);
        assert!(c.u32().is_err());
    }

    #[test]
    fn brand_filter_empty_is_all() {
        assert_eq!(decode_brand_filter(&[]), "");
    }
}
