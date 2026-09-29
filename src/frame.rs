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
//! The same bytes ride every transport: a byte stream on the UDS, and one
//! WebSocket binary message per frame on the browser door (length prefix
//! included, so one codec serves both).
//!
//! The HELLO / PROVIDERS / AUTH bodies below are zapd's *own* control protocol
//! (the `to`-empty frames), not application payloads — the router owns them.

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
/// Pairing proof on the browser door (see `pair.rs`); never seen on the UDS.
pub const AUTH: u8 = 8;
// Pass-through types the router forwards but never interprets.
pub const ROUTE: u8 = 16;
pub const RESPONSE: u8 = 17;
pub const EVENT: u8 = 18;

// Roles.
pub const ROLE_PROVIDER: u8 = 1;
pub const ROLE_CONSUMER: u8 = 2;
pub const ROLE_ROUTER: u8 = 3;

const HEADER: usize = 1 + 2 + 2 + 2 + 4; // type + flags + from_len + to_len + payload_len
pub const MAX_FRAME: u32 = 64 * 1024 * 1024;

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
        Self {
            typ,
            flags: 0,
            from: from.into(),
            to: to.into(),
            payload,
        }
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
        Self::body(&buf).map(Some)
    }

    /// Decode exactly one whole frame, length prefix included — a WebSocket
    /// message. Anything short, long or trailing is refused.
    pub fn decode(b: &[u8]) -> Result<Frame> {
        let len = b
            .get(..4)
            .map(|l| u32::from_le_bytes(l.try_into().unwrap()) as usize);
        match len {
            Some(n) if n + 4 == b.len() && n >= HEADER && n <= MAX_FRAME as usize => {
                Self::body(&b[4..])
            }
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "zapd: not one whole frame",
            )),
        }
    }

    fn body(buf: &[u8]) -> Result<Frame> {
        let mut c = Cursor::new(buf);
        let typ = c.u8()?;
        let flags = c.u16()?;
        let from_len = c.u16()? as usize;
        let to_len = c.u16()? as usize;
        let pay_len = c.u32()? as usize;
        let from = c.string(from_len)?;
        let to = c.string(to_len)?;
        let payload = c.take(pay_len)?.to_vec();
        if c.p != buf.len() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "zapd: frame length disagrees with its fields",
            ));
        }
        Ok(Frame {
            typ,
            flags,
            from,
            to,
            payload,
        })
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

    /// Refuse trailing bytes: a control body is exactly its fields.
    pub fn end(&self) -> Result<()> {
        if self.p == self.b.len() {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::InvalidData,
                "zapd: trailing bytes in control body",
            ))
        }
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

/// What a node says about itself in HELLO, and what PROVIDERS returns for it:
/// role(u8) + brand(str) + caps(u16 count + str…) + attrs(u16 count + (key
/// str, value str)…). `caps` names what it serves; `attrs` carries the rest of
/// its description (resources, `leasable`) as strings, so the router can match
/// a query against them without a schema.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Descriptor {
    pub role: u8,
    pub brand: String,
    pub caps: Vec<String>,
    pub attrs: Vec<(String, String)>,
}

impl Descriptor {
    fn put(&self, b: &mut Vec<u8>) {
        b.push(self.role);
        put_str(b, &self.brand);
        b.extend_from_slice(&(self.caps.len() as u16).to_le_bytes());
        for c in &self.caps {
            put_str(b, c);
        }
        b.extend_from_slice(&(self.attrs.len() as u16).to_le_bytes());
        for (k, v) in &self.attrs {
            put_str(b, k);
            put_str(b, v);
        }
    }

    fn take(c: &mut Cursor) -> Result<Descriptor> {
        let role = c.u8()?;
        let brand = c.str()?;
        let caps = (0..c.u16()?).map(|_| c.str()).collect::<Result<_>>()?;
        let attrs = (0..c.u16()?)
            .map(|_| Ok((c.str()?, c.str()?)))
            .collect::<Result<_>>()?;
        Ok(Descriptor {
            role,
            brand,
            caps,
            attrs,
        })
    }
}

pub fn encode_hello(d: &Descriptor) -> Vec<u8> {
    let mut b = Vec::new();
    d.put(&mut b);
    b
}

pub fn decode_hello(payload: &[u8]) -> Result<Descriptor> {
    let mut c = Cursor::new(payload);
    let d = Descriptor::take(&mut c)?;
    c.end()?;
    Ok(d)
}

/// One node in a PROVIDERS reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub desc: Descriptor,
}

/// PROVIDERS body: u16 count + per entry (id str + descriptor).
pub fn encode_providers(entries: &[Entry]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for e in entries {
        put_str(&mut b, &e.id);
        e.desc.put(&mut b);
    }
    b
}

pub fn decode_providers(payload: &[u8]) -> Result<Vec<Entry>> {
    let mut c = Cursor::new(payload);
    let out = (0..c.u16()?)
        .map(|_| {
            Ok(Entry {
                id: c.str()?,
                desc: Descriptor::take(&mut c)?,
            })
        })
        .collect::<Result<_>>()?;
    c.end()?;
    Ok(out)
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
        let f = Frame::new(
            ROUTE,
            "consumer:mcp/1",
            "browser:chrome/dbc/default",
            b"opaque-\x00\x01\x02".to_vec(),
        );
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
        let d = Descriptor {
            role: ROLE_PROVIDER,
            brand: "hanzo".into(),
            caps: vec!["browser.tabs".into(), "browser.navigate".into()],
            attrs: vec![("leasable".into(), "false".into())],
        };
        assert_eq!(decode_hello(&encode_hello(&d)).unwrap(), d);
        let mut long = encode_hello(&d);
        long.push(0);
        assert!(decode_hello(&long).is_err(), "trailing bytes are refused");
    }

    #[test]
    fn providers_roundtrip() {
        let entries = vec![
            Entry {
                id: "browser/dgx/chromium-3fa2".into(),
                desc: Descriptor {
                    role: ROLE_PROVIDER,
                    brand: "hanzo".into(),
                    caps: vec!["tabs".into()],
                    attrs: vec![],
                },
            },
            Entry {
                id: "engine/spark/qwen3".into(),
                desc: Descriptor {
                    role: ROLE_PROVIDER,
                    brand: "".into(),
                    caps: vec![],
                    attrs: vec![("model".into(), "qwen3-32b".into())],
                },
            },
        ];
        assert_eq!(
            decode_providers(&encode_providers(&entries)).unwrap(),
            entries
        );
    }

    #[test]
    fn cursor_rejects_truncation() {
        let mut c = Cursor::new(&[0u8, 1]);
        assert!(c.u32().is_err());
    }

    #[test]
    fn decode_takes_exactly_one_frame() {
        let bytes = Frame::new(AUTH, "", "", vec![7; 32]).encode();
        let f = Frame::decode(&bytes).unwrap();
        assert_eq!((f.typ, f.payload.len()), (AUTH, 32));
        assert!(Frame::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut two = bytes.clone();
        two.extend_from_slice(&bytes);
        assert!(Frame::decode(&two).is_err());
    }

    #[test]
    fn brand_filter_empty_is_all() {
        assert_eq!(decode_brand_filter(&[]), "");
    }
}
