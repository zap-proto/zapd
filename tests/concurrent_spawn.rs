//! Regression: a stale `zapd.sock` plus N concurrent router spawns must
//! converge to EXACTLY ONE router — the flock single-instance guard.
//!
//! Before the lock, racers all saw the stale socket, all unlinked it, and all
//! bound → split brain: providers register with router A, consumers query
//! router B, `providers.list` returns 0. The lock serializes the
//! connect-check + unlink + bind so exactly one wins and the losers exit
//! `AddrInUse` cleanly.

use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

#[test]
fn one_router_under_concurrent_spawn_with_stale_socket() {
    let bin = env!("CARGO_BIN_EXE_zapd");
    let dir = std::env::temp_dir().join(format!("zapd-race-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("zapd.sock");

    // Plant a STALE socket: a regular file at the path. connect() fails on it,
    // so every racer thinks it is stale and wants to unlink+bind — the exact
    // condition that produced two live routers before the lock.
    std::fs::write(&sock, b"stale").unwrap();

    // Spawn N routers at once, all pointed at the same socket.
    let n = 50;
    let mut kids = Vec::with_capacity(n);
    for _ in 0..n {
        kids.push(
            Command::new(bin)
                .env("ZAP_SOCK", &sock)
                .env("ZAP_LOG", "error")
                .spawn()
                .expect("spawn zapd"),
        );
    }

    // Let them race the lock + bind, and let the losers exit.
    std::thread::sleep(Duration::from_secs(3));

    // Observe BEFORE cleanup so a failing assert never leaks the children.
    let socket_is_live = UnixStream::connect(&sock).is_ok();
    let mut alive = 0;
    for k in &mut kids {
        if k.try_wait().expect("try_wait").is_none() {
            alive += 1; // still running → the one router
        }
    }

    // Cleanup: kill the survivor, reap everyone, remove temp.
    for k in &mut kids {
        let _ = k.kill();
        let _ = k.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(socket_is_live, "the socket must be a live listener after the race");
    assert_eq!(alive, 1, "exactly one router must survive the race, got {alive}");
}
