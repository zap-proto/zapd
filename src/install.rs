//! `zapd install-host` — write the native-messaging host manifests so the
//! browser can launch THIS binary (in host mode) when the extension calls
//! `connectNative("ai.<brand>.zap")`. One binary, registered as the host.
//!
//! A Web Store extension can't place files on disk, so this is the one-time
//! "install" step: `zapd install-host --brand hanzo`.

use std::io::Result;
use std::path::PathBuf;

/// Per-brand extension ids. The native host name is product-specific
/// (`ai.hanzo.zap`); the binary it points to is the neutral `zapd`.
fn ids(brand: &str) -> (&'static str, &'static str) {
    match brand {
        "hanzo" => ("biingenefmanpecedoafkfajbnlgdmbl", "hanzo-ai@hanzo.ai"),
        _ => ("", ""),
    }
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Chrome/Chromium-family native-messaging dirs on macOS + Linux. One MV3 build
/// plus one stable manifest `key` (id `biingenefmanpecedoafkfajbnlgdmbl`) serves
/// every Blink browser; we only need to drop the host manifest into each one's
/// dir. Browsers that aren't installed are skipped (parent-exists filter below).
fn chrome_dirs() -> Vec<PathBuf> {
    let h = home();
    #[cfg(target_os = "macos")]
    let bases = [
        "Library/Application Support/Google/Chrome",
        "Library/Application Support/Chromium",
        "Library/Application Support/BraveSoftware/Brave-Browser",
        "Library/Application Support/Microsoft Edge",
        "Library/Application Support/Arc/User Data",
        "Library/Application Support/Vivaldi",
        "Library/Application Support/com.operasoftware.Opera",
        "Library/Application Support/com.operasoftware.OperaGX",
    ];
    #[cfg(target_os = "linux")]
    let bases = [
        ".config/google-chrome",
        ".config/chromium",
        ".config/BraveSoftware/Brave-Browser",
        ".config/microsoft-edge",
        ".config/vivaldi",
        ".config/opera",
        // snap Chromium (Ubuntu) confines its config under ~/snap/<pkg>/common.
        // Its snap layout nests the profile under `chromium/`, so the hosts dir
        // is `.../common/chromium/NativeMessagingHosts` (the join below completes it).
        "snap/chromium/common/chromium",
        // Flatpak Chromium/Chrome keep their own per-app HOME under ~/.var/app.
        ".var/app/org.chromium.Chromium/config/chromium",
        ".var/app/com.google.Chrome/config/google-chrome",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let bases: [&str; 0] = [];
    bases
        .iter()
        .map(|b| h.join(b).join("NativeMessagingHosts"))
        .filter(|d| d.parent().map(|p| p.exists()).unwrap_or(false))
        .collect()
}

/// Firefox native-messaging dirs. Return ALL candidates UNFILTERED and let the
/// caller decide which to write — a machine may run standard, snap, and Flatpak
/// Firefox at once, and each confines its profile to a different HOME. The snap
/// path in particular is the Ubuntu default, where `~/.mozilla` never exists —
/// omitting it (or filtering it out here on the immediate parent) is why
/// `install-host` used to write 0 manifests there.
fn firefox_dirs() -> Vec<PathBuf> {
    let h = home();
    #[cfg(target_os = "macos")]
    let bases = ["Library/Application Support/Mozilla/NativeMessagingHosts"];
    #[cfg(not(target_os = "macos"))]
    let bases = [
        ".mozilla/native-messaging-hosts",
        "snap/firefox/common/.mozilla/native-messaging-hosts",
        ".var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts",
    ];
    bases.iter().map(|b| h.join(b)).collect()
}

pub fn run(brand: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let (chrome_id, firefox_id) = ids(brand);
    let name = format!("ai.{brand}.zap");
    let desc = format!("{brand} ZAP native host — relays browser stdio to zapd");

    let mut wrote = 0;
    if !chrome_id.is_empty() {
        let body = serde_json::json!({
            "name": name, "description": desc, "path": exe, "type": "stdio",
            "allowed_origins": [format!("chrome-extension://{chrome_id}/")],
        });
        let bytes = serde_json::to_vec_pretty(&body)?;
        for dir in chrome_dirs() {
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{name}.json"));
            std::fs::write(&path, &bytes)?;
            println!("wrote {}", path.display());
            wrote += 1;
        }
    }
    if !firefox_id.is_empty() {
        let body = serde_json::json!({
            "name": name, "description": desc, "path": exe, "type": "stdio",
            "allowed_extensions": [firefox_id],
        });
        let bytes = serde_json::to_vec_pretty(&body)?;
        // A snap Firefox creates `~/snap/firefox` but not the nested
        // `common/.mozilla/native-messaging-hosts` chain, so gate on the snap
        // ROOT existing (`~/snap/firefox`) rather than the immediate parent —
        // otherwise the snap manifest is silently skipped on a fresh profile.
        // Non-snap trees (standard `~/.mozilla`, Flatpak `~/.var/app/...`) still
        // gate on their immediate parent existing.
        for dir in firefox_dirs() {
            let install = if dir.starts_with(home().join("snap")) {
                home().join("snap/firefox").exists()
            } else {
                dir.parent().map(|p| p.exists()).unwrap_or(false)
            };
            if install {
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{name}.json"));
                std::fs::write(&path, &bytes)?;
                println!("wrote {}", path.display());
                wrote += 1;
            }
        }
    }
    println!("zapd install-host: {wrote} manifest(s) → {}", exe.display());
    Ok(())
}
