//! Pairing — the one secret a browser must prove it holds before the door
//! admits it, and that the router must prove back before the browser trusts it.
//!
//! **The pairing code** is what a human pastes into the extension, once:
//! `ws://127.0.0.1:<port>/#<token>`. The port is this user's door; the token,
//! in the fragment, is 32 random bytes as 64 lowercase hex characters. A
//! WebSocket client never sends a fragment, so the code names where to connect
//! and what to prove without the token crossing the wire.
//!
//! **The file** `<state>/zap/pair` holds the code and a newline —
//! `$XDG_STATE_HOME` (default `~/.local/state`) on Linux, `~/Library/Application
//! Support` on macOS. State, not config: the code pairs this machine's router
//! with this machine's browsers and means nothing anywhere else, and a config
//! directory is what people commit and sync as dotfiles. It is `0600` in a
//! `0700` directory the user owns; before every read both are `lstat`ed, and a
//! symlink, a foreign owner or any group or other permission bit refuses the
//! door (the UDS still serves) rather than trusting a token someone else could
//! read or plant. A missing file is minted — a port nothing holds at that
//! moment ([`free_port`]), a token from the OS CSPRNG — and linked into place,
//! so a racing reader never sees half a file; deleting it re-mints both. Every
//! door connection reads it afresh. The port in the file is the one the router
//! binds; if something else takes it later, `zapd pair --reset` picks another.
//!
//! **The proof** is HMAC-SHA256 under the token, fresh nonces from both sides,
//! router first:
//!
//! ```text
//! browser → router   AUTH  nc                        (32 random bytes)
//! router  → browser  AUTH  ns ‖ HMAC(k, "zap router" ‖ nc ‖ ns)
//! browser → router   AUTH  HMAC(k, "zap client" ‖ nc ‖ ns)
//! ```
//!
//! The router proves first so a browser never obeys a squatter on its port; the
//! labels differ so neither proof can be reflected as the other.

use std::io::{Error, ErrorKind, Result, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const NONCE: usize = 32;
pub const PROOF: usize = 32;
pub const ROUTER: &[u8] = b"zap router";
pub const CLIENT: &[u8] = b"zap client";

/// This user's door and token.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pairing {
    pub port: u16,
    pub key: [u8; 32],
}

impl Pairing {
    /// The code a human pastes into the extension.
    pub fn code(&self) -> String {
        format!("ws://127.0.0.1:{}/#{}", self.port, hex(&self.key))
    }

    /// Parse a code. Loopback only: a code that points anywhere else is not a
    /// code a router minted.
    pub fn parse(code: &str) -> Result<Pairing> {
        let bad = || Error::new(ErrorKind::InvalidData, "zapd: not a pairing code");
        let rest = code
            .trim()
            .strip_prefix("ws://127.0.0.1:")
            .ok_or_else(bad)?;
        let (port, key) = rest.split_once("/#").ok_or_else(bad)?;
        let port: u16 = port.parse().map_err(|_| bad())?;
        if port == 0 {
            return Err(bad());
        }
        Ok(Pairing {
            port,
            key: unhex(key).ok_or_else(bad)?,
        })
    }

    /// HMAC(k, label ‖ nc ‖ ns).
    pub fn proof(&self, label: &[u8], nc: &[u8], ns: &[u8]) -> [u8; PROOF] {
        self.mac(label, nc, ns).finalize().into_bytes().into()
    }

    /// Constant-time check of a peer's proof.
    pub fn verify(&self, label: &[u8], nc: &[u8], ns: &[u8], proof: &[u8]) -> bool {
        self.mac(label, nc, ns).verify_slice(proof).is_ok()
    }

    fn mac(&self, label: &[u8], nc: &[u8], ns: &[u8]) -> Hmac<Sha256> {
        let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key).expect("any key length");
        m.update(label);
        m.update(nc);
        m.update(ns);
        m
    }
}

/// The door ports a Blink extension tries by itself, with no pairing code, in
/// this order: 9998 (ZAP over WebSocket, beside the services' native ZAP on
/// 9999), then 21000-21007 for when 9998 is taken. The first free one is what
/// a new pairing takes.
pub const WELL_KNOWN: &[u16] = &[9998, 21000, 21001, 21002, 21003, 21004, 21005, 21006, 21007];

/// A door port for a new pairing: the first free well-known port, else one in
/// 20000–29999 — below every OS's ephemeral range — that nothing on this
/// machine holds now. It is written into the pairing, which is what the router
/// binds from then on.
pub fn free_port() -> Result<u16> {
    for &p in WELL_KNOWN {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Ok(p);
        }
    }
    for _ in 0..100 {
        let r = random()?;
        let p = 20000 + (u16::from_le_bytes([r[0], r[1]]) % 10000);
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Ok(p);
        }
    }
    Err(Error::new(
        ErrorKind::AddrInUse,
        "zapd: no free door port in 20000-29999",
    ))
}

/// `<state>/zap/pair`.
pub fn path() -> PathBuf {
    let base = directories::BaseDirs::new()
        .map(|d| d.state_dir().unwrap_or(d.data_local_dir()).to_path_buf());
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("zap")
        .join("pair")
}

/// This user's pairing, minting it on first use.
pub fn load() -> Result<Pairing> {
    let p = path();
    let dir = p.parent().expect("pair path has a parent");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    private(dir, true)?;
    match read(&p) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Losing a mint race is fine: the winner's code is the one. Any
            // other failure shows up as the read below failing.
            let _ = publish(
                &p,
                &Pairing {
                    port: free_port()?,
                    key: random()?,
                },
                false,
            );
            read(&p)
        }
        r => r,
    }
}

/// Mint a new token on a new free port. Every paired browser must pair again;
/// the door re-reads the file on every connection, so the old token stops
/// working at once, and the next router elected binds the new port.
pub fn reset() -> Result<Pairing> {
    load()?;
    publish(
        &path(),
        &Pairing {
            port: free_port()?,
            key: random()?,
        },
        true,
    )?;
    load()
}

fn read(p: &Path) -> Result<Pairing> {
    private(p, false)?;
    Pairing::parse(&std::fs::read_to_string(p)?)
}

/// Refuse a path anyone but this user could read, write or have planted.
fn private(p: &Path, dir: bool) -> Result<()> {
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

fn random() -> Result<[u8; 32]> {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).map_err(|e| Error::other(format!("zapd: no randomness: {e}")))?;
    Ok(k)
}

pub fn nonce() -> Result<[u8; NONCE]> {
    random()
}

/// Write a code to a private temp file, then move it into place: `link` when it
/// must not exist yet (a racing reader never sees half a file, and a racing
/// minter loses), `rename` when it replaces.
fn publish(p: &Path, pairing: &Pairing, replace: bool) -> Result<()> {
    let dir = p.parent().expect("pair path has a parent");
    let tmp = dir.join(format!(".pair.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(format!("{}\n", pairing.code()).as_bytes())?;
    f.sync_all()?;
    let moved = if replace {
        std::fs::rename(&tmp, p)
    } else {
        std::fs::hard_link(&tmp, p)
    };
    let _ = std::fs::remove_file(&tmp);
    moved
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, k) in k.iter_mut().enumerate() {
        *k = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing() -> Pairing {
        Pairing {
            port: 21000,
            key: [9; 32],
        }
    }

    #[test]
    fn code_roundtrips() {
        let p = pairing();
        assert_eq!(
            p.code(),
            format!("ws://127.0.0.1:21000/#{}", "09".repeat(32))
        );
        assert_eq!(Pairing::parse(&p.code()).unwrap(), p);
        assert_eq!(Pairing::parse(&format!("  {}\n", p.code())).unwrap(), p);
    }

    #[test]
    fn code_is_loopback_only() {
        let key = "09".repeat(32);
        for bad in [
            format!("ws://evil.example:21000/#{key}"),
            format!("ws://localhost:21000/#{key}"),
            format!("wss://127.0.0.1:21000/#{key}"),
            format!("ws://127.0.0.1:0/#{key}"),
            format!("ws://127.0.0.1:21000/#{}", "0A".repeat(32)),
            "ws://127.0.0.1:21000/#short".to_string(),
            "ws://127.0.0.1:21000/".to_string(),
        ] {
            assert!(Pairing::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proofs_are_labelled() {
        let p = pairing();
        let (nc, ns) = ([1u8; 32], [2u8; 32]);
        let r = p.proof(ROUTER, &nc, &ns);
        assert!(p.verify(ROUTER, &nc, &ns, &r));
        // A router proof is not a client proof: no reflection.
        assert!(!p.verify(CLIENT, &nc, &ns, &r));
        // Another key cannot answer.
        let other = Pairing {
            port: 21000,
            key: [8; 32],
        };
        assert!(!other.verify(ROUTER, &nc, &ns, &r));
    }

    #[test]
    fn a_new_door_port_is_free_and_below_ephemeral() {
        let p = free_port().unwrap();
        assert!((20000..30000).contains(&p));
        assert!(std::net::TcpListener::bind(("127.0.0.1", p)).is_ok());
    }

    #[test]
    fn refuses_a_readable_or_planted_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("zapd-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tok = dir.join("pair");
        let first = Pairing {
            port: 21000,
            key: [3; 32],
        };
        publish(&tok, &first, false).unwrap();
        assert_eq!(read(&tok).unwrap(), first);
        // A second mint loses to the first.
        assert!(publish(
            &tok,
            &Pairing {
                port: 21000,
                key: [4; 32]
            },
            false
        )
        .is_err());
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(read(&tok).unwrap_err().kind(), ErrorKind::PermissionDenied);
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&tok, &link).unwrap();
        assert_eq!(read(&link).unwrap_err().kind(), ErrorKind::PermissionDenied);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            private(&dir, true).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
