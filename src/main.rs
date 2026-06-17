//! zapd — the ZAP universal router.
//!
//! One brand-neutral daemon per machine. Every ZAP service — browser
//! extensions (via the native host), IDE extensions, CLI agents, hanzo-mcp —
//! connects to the one shared Unix socket and is multiplexed here. Brands ship
//! thin white-label wrappers (`@hanzo/zapd`, `@lux/zapd`, `@zoo/zapd`) over
//! this same binary; the router itself is neutral and schema-agnostic.

mod broker;
// frame.rs defines the full ZAP router envelope surface (all types/roles +
// client-side body codecs); the router itself uses a subset.
#[allow(dead_code)]
mod frame;

use clap::Parser;
use std::io::Result;

#[derive(Parser)]
#[command(name = "zapd", version, about = "ZAP universal router — the one shared local broker")]
struct Cli {
    /// Override the socket path (default: $ZAP_SOCK, else
    /// $XDG_RUNTIME_DIR/zap/zapd.sock, else ~/.zap/run/zapd.sock).
    #[arg(long, env = "ZAP_SOCK")]
    sock: Option<String>,

    /// Log filter (e.g. info, zapd=debug).
    #[arg(long, default_value = "info", env = "ZAP_LOG")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(sock) = cli.sock {
        // Normalise into the env the broker reads, so `--sock` and `ZAP_SOCK`
        // are exactly one mechanism.
        std::env::set_var("ZAP_SOCK", sock);
    }
    tracing_subscriber::fmt().with_env_filter(cli.log).init();

    tracing::info!("zapd {} starting", env!("CARGO_PKG_VERSION"));
    broker::run().await
}
