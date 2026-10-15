//! The election on Windows, end to end with real processes: candidates elect
//! exactly one router over `LockFileEx`, nodes reach it through its named pipe,
//! a killed router hands over, and a pipe the lock holder does not serve is
//! never spoken to.
//!
//! Each candidate is this test binary re-run as the ignored `candidate` test,
//! with its own `XDG_RUNTIME_DIR` so nothing here touches the router of the user
//! running the tests.

#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zapd::frame;

/// A router candidate: embed, join as a node, live until stdin closes.
#[test]
#[ignore = "run by the election tests as a child process"]
fn candidate() {
    zapd::embed();
    let _node = zapd::Node::join(&format!("agent/cand-{}", std::process::id()), frame::ROLE_CONSUMER, "", &[]);
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

struct Home {
    run: PathBuf,
    kids: Vec<Child>,
}

impl Home {
    fn new(name: &str) -> Home {
        let run = std::env::temp_dir().join(format!("zapd-win-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run);
        std::fs::create_dir_all(&run).unwrap();
        // This process's own nodes and the candidates share one runtime directory.
        std::env::set_var("XDG_RUNTIME_DIR", &run);
        Home { run, kids: Vec::new() }
    }

    fn spawn(&mut self) -> u32 {
        let kid = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "candidate", "--nocapture"])
            .env("XDG_RUNTIME_DIR", &self.run)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = kid.id();
        self.kids.push(kid);
        pid
    }

    /// The pid the elected router recorded.
    fn router(&self) -> Option<u32> {
        std::fs::read_to_string(self.run.join("zap").join("zapd.pid")).ok()?.trim().parse().ok()
    }

    fn kill(&mut self, pid: u32) {
        let kid = self.kids.iter_mut().find(|k| k.id() == pid).unwrap();
        kid.kill().unwrap();
        kid.wait().unwrap();
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        for k in &mut self.kids {
            let _ = k.kill();
            let _ = k.wait();
        }
    }
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(t) = f() {
            return t;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn cand(pid: u32) -> String {
    format!("agent/{}/cand-{pid}", zapd::host())
}

/// The ids this router lists, as one node sees them.
fn listed(node: &zapd::Node) -> Option<Vec<String>> {
    let mut ids: Vec<String> = zapd::block_on(node.nodes(Duration::from_secs(2))).ok()?.into_iter().map(|e| e.id).collect();
    ids.sort();
    Some(ids)
}

/// One test, two homes in turn: the runtime directory this process's own nodes
/// use is an environment variable, so the two cannot run side by side.
#[test]
fn the_election_on_windows() {
    router_moves_and_everyone_follows();
    a_pipe_the_lock_holder_does_not_serve_is_never_spoken_to();
}

fn router_moves_and_everyone_follows() {
    let mut home = Home::new("moves");
    let a = home.spawn();
    let b = home.spawn();
    let first = until("a router", || home.router().filter(|p| *p == a || *p == b));
    let node = zapd::Node::join("cli/watcher", frame::ROLE_CONSUMER, "", &[]);
    let both = until("both candidates listed", || listed(&node).filter(|ids| ids.contains(&cand(a)) && ids.contains(&cand(b))));
    assert!(both.contains(&node.id()), "{both:?}");
    eprintln!("router {first} lists {both:?}");

    // Kill the router: the other candidate takes the lock and every node,
    // this one included, registers with it again.
    home.kill(first);
    let second = if first == a { b } else { a };
    until("the other candidate elected", || home.router().filter(|p| *p == second));
    let after = until("the survivor listed again", || listed(&node).filter(|ids| ids.contains(&cand(second)) && !ids.contains(&cand(first))));
    assert!(after.contains(&node.id()), "{after:?}");
    eprintln!("router {second} lists {after:?}");
}

fn a_pipe_the_lock_holder_does_not_serve_is_never_spoken_to() {
    let home = Home::new("squat");
    // The record names a router that is not this process (a router that has
    // since exited, say), and this process serves the router's pipe name.
    std::fs::create_dir_all(home.run.join("zap")).unwrap();
    let other = std::process::id() ^ 4;
    std::fs::write(home.run.join("zap").join("zapd.pid"), other.to_string()).unwrap();
    let squat = zapd::block_on(async {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(zapd::socket_path())
    })
    .unwrap();
    // A node dials it, finds its server is not the recorded router, and refuses.
    let node = zapd::Node::join("cli/careful", frame::ROLE_CONSUMER, "", &[]);
    assert!(zapd::block_on(node.nodes(Duration::from_secs(2))).is_err(), "a node spoke to a squatter");
    drop(squat);
}
