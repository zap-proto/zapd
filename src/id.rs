//! Node ids: `<kind>/<host>/<name>`.
//!
//! * `kind` — one of [`KINDS`].
//! * `host` — the router's machine: the first label of its hostname,
//!   lowercased, anything outside `[a-z0-9-]` replaced by `-`, at most 16
//!   characters. The router stamps it at `hello`; a node never chooses it.
//! * `name` — `[a-z0-9][a-z0-9-]{0,15}`, chosen by the node, stable across its
//!   reconnects and distinct among nodes of one kind on one host.
//!
//! A node says `hello` as `<kind>/<name>` (or with any host, which is
//! overwritten) and learns its full id from `welcome`.

pub const KINDS: &[&str] = &[
    "agent", "browser", "cli", "desktop", "dev", "engine", "gpu", "host", "ide", "mcp", "router",
    "sandbox",
];

/// This machine's host label.
pub fn host() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length; gethostname NUL-terminates on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let raw = if ok {
        String::from_utf8_lossy(buf.split(|b| *b == 0).next().unwrap_or(&[])).into_owned()
    } else {
        String::new()
    };
    label(&raw)
}

fn label(hostname: &str) -> String {
    let first = hostname.split('.').next().unwrap_or("");
    let l: String = first
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(16)
        .collect();
    if l.is_empty() {
        "local".into()
    } else {
        l
    }
}

fn name_ok(n: &str) -> bool {
    let b = n.as_bytes();
    (1..=16).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The id a `hello` from `proposed` registers as on `host`, or `None` if it
/// does not name a kind and a well-formed name.
pub fn stamp(proposed: &str, host: &str) -> Option<String> {
    let parts: Vec<&str> = proposed.split('/').collect();
    let (kind, name) = match parts.as_slice() {
        [kind, name] | [kind, _, name] => (*kind, *name),
        _ => return None,
    };
    (KINDS.contains(&kind) && name_ok(name)).then(|| format!("{kind}/{host}/{name}"))
}

/// The kind of an id.
pub fn kind(id: &str) -> &str {
    id.split('/').next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_the_router_host() {
        assert_eq!(
            stamp("browser/chromium-3fa2", "dgx").as_deref(),
            Some("browser/dgx/chromium-3fa2")
        );
        // A reconnect with its old full id, or a claimed foreign host, is re-stamped.
        assert_eq!(
            stamp("browser/dgx/chromium-3fa2", "dgx").as_deref(),
            Some("browser/dgx/chromium-3fa2")
        );
        assert_eq!(
            stamp("agent/spark/hanzo-42", "dgx").as_deref(),
            Some("agent/dgx/hanzo-42")
        );
    }

    #[test]
    fn refuses_what_is_not_an_id() {
        for bad in [
            "",
            "browser",
            "toaster/x",
            "browser/",
            "browser/-x",
            "browser/UPPER",
            "browser/a_b",
            "browser/seventeen-chars-xx",
            "a/b/c/d",
            "browser:chrome/default",
        ] {
            assert_eq!(stamp(bad, "dgx"), None, "{bad}");
        }
    }

    #[test]
    fn host_labels() {
        assert_eq!(label("DGX.local"), "dgx");
        assert_eq!(label("my_box.lan"), "my-box");
        assert_eq!(label("a-very-long-hostname-indeed"), "a-very-long-host");
        assert_eq!(label(""), "local");
    }
}
