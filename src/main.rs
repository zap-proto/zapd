//! zapd — the ZAP universal router (and, in the same binary, the browser
//! native-messaging host). ONE artifact, decomplected into modules
//! (frame / broker / host / install). No Cap'n Proto — the wire is the ZAP
//! envelope.
//!
//! Modes:
//!   zapd                  → router daemon (registry + route + presence)
//!   zapd host             → native-messaging host (browser stdio ⇄ router)
//!   zapd install-host     → write native-host manifests so the browser can
//!                           launch this binary on connectNative()
//! When a browser launches it (passing the extension origin) it auto-enters
//! host mode.

mod broker;
mod host;
mod install;
// frame.rs is the full ZAP envelope surface; each mode uses a subset.
#[allow(dead_code)]
mod frame;

use clap::{Parser, Subcommand};
use std::io::Result;

#[derive(Parser)]
#[command(name = "zapd", version, about = "ZAP universal router — the one shared local broker")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Override the socket path (router mode).
    #[arg(long, env = "ZAP_SOCK")]
    sock: Option<String>,

    /// Log filter (router mode).
    #[arg(long, default_value = "info", env = "ZAP_LOG")]
    log: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Native-messaging host mode (browsers launch this; usually auto-detected).
    Host,
    /// Write native-messaging host manifests for a brand (e.g. hanzo).
    InstallHost {
        #[arg(long, default_value = "hanzo")]
        brand: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // A browser launched us as its native host — it passes the extension origin
    // as an arg. Detect BEFORE clap (clap would reject it), and never log to
    // stdout here (stdout IS the native-messaging channel).
    if std::env::args()
        .skip(1)
        .any(|a| a.starts_with("chrome-extension://") || a.starts_with("moz-extension://"))
    {
        return host::run().await;
    }

    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Host) => host::run().await,
        Some(Cmd::InstallHost { brand }) => install::run(&brand),
        None => {
            if let Some(sock) = cli.sock {
                std::env::set_var("ZAP_SOCK", sock);
            }
            tracing_subscriber::fmt().with_env_filter(cli.log).init();
            tracing::info!("zapd {} starting", env!("CARGO_PKG_VERSION"));
            broker::run().await
        }
    }
}
