//! `zapd` — the operator's view of this user's router. It never serves: the
//! router runs inside the processes that speak ZAP (see the library).
//!
//!   zapd pair            print the pairing code the browser extension needs
//!   zapd pair --reset    mint a new key; every paired browser pairs again
//!   zapd ls              list the nodes on this machine's router

use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = match args.as_slice() {
        ["pair"] => zapd::pair::load().map(|p| println!("{}", p.code())),
        ["pair", "--reset"] => zapd::pair::reset().map(|p| println!("{}", p.code())),
        ["ls"] => ls(),
        ["--version" | "-V"] => {
            println!("zapd {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("usage: zapd pair [--reset] | zapd ls");
            return ExitCode::from(2);
        }
    };
    match out {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zapd: {e}");
            ExitCode::FAILURE
        }
    }
}

fn ls() -> std::io::Result<()> {
    let me = format!("cli/{}", std::process::id());
    let node = zapd::Node::join(&me, zapd::frame::ROLE_CONSUMER, "", &[]);
    let mut nodes = zapd::block_on(node.nodes(Duration::from_secs(2)))?;
    let me = node.id();
    nodes.retain(|n| n.id != me);
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for n in nodes {
        let role = match n.role {
            zapd::frame::ROLE_PROVIDER => "provider",
            zapd::frame::ROLE_CONSUMER => "consumer",
            zapd::frame::ROLE_ROUTER => "router",
            _ => "?",
        };
        println!("{}\t{role}\t{}\t{}", n.id, n.brand, n.caps.join(","));
    }
    Ok(())
}
