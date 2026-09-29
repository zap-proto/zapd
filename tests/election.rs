//! End to end, with real processes: candidates that embed the router elect
//! exactly one, a browser pairs through the door and is routed to, and when the
//! router's process is killed another candidate takes over and every node —
//! the browser included — comes back.
//!
//! Each candidate is this test binary re-run as the ignored `candidate` test,
//! with its own `XDG_RUNTIME_DIR` / `XDG_STATE_HOME` so nothing here touches
//! the real router of the user running the tests.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UnixStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::WebSocketStream;
use zapd::frame::{self, Frame};
use zapd::pair::{self, Pairing};

const CHROME: &str = "chrome-extension://biingenefmanpecedoafkfajbnlgdmbl";

/// A router candidate: embed, join as a node, live until stdin closes.
#[test]
#[ignore = "run by the election tests as a child process"]
fn candidate() {
    zapd::embed();
    let _node = zapd::Node::join(
        &format!("agent/cand-{}", std::process::id()),
        frame::ROLE_CONSUMER,
        "",
        &[],
    );
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
}

/// `<kind>/<this host>/<name>` — what the router stamps.
fn at(kind: &str, name: &str) -> String {
    format!("{kind}{}/{name}", zapd::host())
}

fn cand(pid: u32) -> String {
    at("agent/", &format!("cand-{pid}"))
}

fn me_id() -> String {
    at("cli/", "me")
}

fn desc(role: u8, brand: &str, caps: &[String]) -> frame::Descriptor {
    frame::Descriptor {
        role,
        brand: brand.into(),
        caps: caps.to_vec(),
        attrs: vec![],
    }
}

/// A node with no candidacy: joins, asks for the registry once, reports.
#[test]
#[ignore = "run by the election tests as a child process"]
fn bystander() {
    let me = zapd::Node::join("cli/bystander", frame::ROLE_CONSUMER, "", &[]);
    match zapd::block_on(me.nodes(Duration::from_secs(2))) {
        Ok(_) => println!("joined"),
        Err(_) => println!("refused"),
    }
}

/// `F_UNLCK` as `flock::l_type` holds it on every OS (it is a short on some).
const UNLOCKED: libc::c_short = libc::F_UNLCK as _;

struct Home {
    dir: PathBuf,
    pairing: Pairing,
}

impl Home {
    fn new(name: &str) -> Home {
        let dir = std::env::temp_dir().join(format!("zapd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).unwrap();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("state/zap"))
            .unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let pairing = Pairing { port, key: [7; 32] };
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dir.join("state/zap/pair"))
            .unwrap()
            .write_all(pairing.code().as_bytes())
            .unwrap();
        Home { dir, pairing }
    }

    fn sock(&self) -> PathBuf {
        self.dir.join("run/zap/zapd.sock")
    }

    fn spawn(&self) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "candidate", "--ignored", "--quiet"])
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("HOME", &self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// The pid holding the router lock, straight from the kernel (F_GETLK).
    fn router(&self) -> Option<i32> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join("run/zap/zapd.lock"))
            .ok()?;
        let mut l: libc::flock = unsafe { std::mem::zeroed() };
        l.l_type = libc::F_WRLCK as _;
        l.l_whence = libc::SEEK_SET as _;
        (unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETLK, &mut l) } == 0 && l.l_type != UNLOCKED)
            .then_some(l.l_pid)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn until<T, F: std::future::Future<Output = Option<T>>>(
    what: &str,
    mut probe: impl FnMut() -> F,
) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = probe().await {
            return v;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A raw UDS node, speaking the envelope by hand.
struct Uds(UnixStream);

impl Uds {
    async fn join(sock: &Path, id: &str, role: u8) -> Option<Uds> {
        let mut s = UnixStream::connect(sock).await.ok()?;
        s.write_all(
            &Frame::new(
                frame::HELLO,
                id,
                "",
                frame::encode_hello(&desc(role, "hanzo", &[])),
            )
            .encode(),
        )
        .await
        .ok()?;
        let mut u = Uds(s);
        u.until(frame::WELCOME).await.ok()?;
        Some(u)
    }

    async fn send(&mut self, f: Frame) {
        self.0.write_all(&f.encode()).await.unwrap();
    }

    async fn until(&mut self, typ: u8) -> std::io::Result<Frame> {
        loop {
            let f = tokio::time::timeout(Duration::from_secs(5), Frame::read(&mut self.0))
                .await??
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            if f.typ == typ {
                return Ok(f);
            }
        }
    }

    /// Wait until the router lists exactly `want`.
    async fn listed(&mut self, want: &[String]) {
        let mut want = want.to_vec();
        want.sort();
        let start = Instant::now();
        loop {
            let ids = self.ids().await;
            if ids == want {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "listed {ids:?}, want {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn ids(&mut self) -> Vec<String> {
        self.send(Frame::new(frame::PROVIDERS_LIST, "", "", vec![]))
            .await;
        let f = self.until(frame::PROVIDERS).await.unwrap();
        let mut ids: Vec<String> = frame::decode_providers(&f.payload)
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        ids.sort();
        ids
    }
}

type Ws = WebSocketStream<TcpStream>;

async fn ws_open(port: u16, origin: &str) -> Result<Ws, String> {
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|e| e.to_string())?;
    let mut req = format!("ws://127.0.0.1:{port}/")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert("origin", origin.parse().unwrap());
    tokio_tungstenite::client_async(req, tcp)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn ws_send(ws: &mut Ws, f: Frame) {
    ws.send(Message::Binary(Bytes::from(f.encode())))
        .await
        .unwrap();
}

async fn ws_next(ws: &mut Ws) -> Option<Frame> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .ok()??
        {
            Ok(Message::Binary(b)) => return Some(Frame::decode(&b).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// What the extension does: prove the pairing (checking the router's proof
/// first), then register as a browser. Returns the id the router assigned.
/// What the extension does: prove the pairing, checking the router's proof
/// first. Returns the socket, ready for HELLO.
async fn paired(port: u16, key: &Pairing) -> Option<Ws> {
    let mut ws = ws_open(port, CHROME).await.ok()?;
    let nc = pair::nonce().unwrap();
    ws_send(&mut ws, Frame::new(frame::AUTH, "", "", nc.to_vec())).await;
    let reply = ws_next(&mut ws).await?;
    let (ns, proof) = reply.payload.split_at(pair::NONCE);
    assert!(
        key.verify(pair::ROUTER, &nc, ns, proof),
        "router failed its proof"
    );
    ws_send(
        &mut ws,
        Frame::new(
            frame::AUTH,
            "",
            "",
            key.proof(pair::CLIENT, &nc, ns).to_vec(),
        ),
    )
    .await;
    Some(ws)
}

/// Say HELLO as `id` on a paired socket; the router's first answer.
async fn hello_as(ws: &mut Ws, id: &str) -> Option<Frame> {
    let caps = ["browser.navigate".to_string()];
    ws_send(
        ws,
        Frame::new(
            frame::HELLO,
            id,
            "",
            frame::encode_hello(&desc(frame::ROLE_PROVIDER, "hanzo", &caps)),
        ),
    )
    .await;
    ws_next(ws).await
}

/// A paired browser, registered. Returns the id the router assigned.
async fn browser(port: u16, key: &Pairing) -> Option<(Ws, String)> {
    let mut ws = paired(port, key).await?;
    let welcome = hello_as(&mut ws, "browser/test-3fa2").await?;
    assert_eq!(welcome.typ, frame::WELCOME);
    Some((ws, welcome.to))
}

#[tokio::test(flavor = "multi_thread")]
async fn router_moves_and_everyone_follows() {
    let home = Home::new("move");
    let sock = home.sock();
    let mut a = home.spawn();
    let first = until("the first router", || async { home.router() }).await;
    assert_eq!(first, a.id() as i32);
    let mut b = home.spawn();

    // The browser pairs through the door and registers under a stamped id.
    let (mut ws, id) = until("the door", || browser(home.pairing.port, &home.pairing)).await;
    assert_eq!(id, at("browser/", "test-3fa2"));

    // A local node routes to it; the router stamps `from`, the payload is opaque.
    let mut me = until("the socket", || {
        Uds::join(&sock, "cli/me", frame::ROLE_CONSUMER)
    })
    .await;
    me.listed(&[id.clone(), me_id(), cand(a.id()), cand(b.id())])
        .await;
    let opaque = b"\x00\x01 navigate (opaque to the router)".to_vec();
    me.send(Frame::new(frame::ROUTE, "cli/spoofed", &id, opaque.clone()))
        .await;
    let got = loop {
        let f = ws_next(&mut ws).await.unwrap();
        if f.typ != frame::PEER_CONNECTED && f.typ != frame::PEER_DISCONNECTED {
            break f;
        }
    };
    assert_eq!(
        (got.typ, got.from.as_str(), &got.payload),
        (frame::ROUTE, me_id().as_str(), &opaque)
    );
    ws_send(
        &mut ws,
        Frame::new(frame::RESPONSE, &id, me_id(), b"ok".to_vec()),
    )
    .await;
    let back = me.until(frame::RESPONSE).await.unwrap();
    assert_eq!(
        (back.from.as_str(), back.payload.as_slice()),
        (id.as_str(), &b"ok"[..])
    );

    // Kill the router's process outright. The other candidate takes the lock,
    // rebinds both doors, and the browser and the local node come back.
    let (dead, mut alive) = if first == a.id() as i32 {
        (&mut a, b)
    } else {
        (&mut b, a)
    };
    let dead_id = dead.id();
    dead.kill().unwrap();
    dead.wait().unwrap();
    let second = until("a new router", || async {
        home.router().filter(|p| *p != first)
    })
    .await;
    assert_eq!(second, alive.id() as i32);
    while let Some(f) = ws_next(&mut ws).await {
        assert!(
            f.typ == frame::PEER_CONNECTED || f.typ == frame::PEER_DISCONNECTED,
            "the browser must see the old door close"
        );
    }

    let (_ws, id2) = until("the door again", || {
        browser(home.pairing.port, &home.pairing)
    })
    .await;
    assert_eq!(id2, id);
    let mut me = until("the socket again", || {
        Uds::join(&sock, "cli/me", frame::ROLE_CONSUMER)
    })
    .await;
    me.listed(&[id, me_id(), cand(alive.id())]).await;
    assert!(!me.ids().await.contains(&cand(dead_id)));

    alive.kill().unwrap();
    alive.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn many_candidates_one_router() {
    let home = Home::new("race");
    // A stale socket file at the path: every candidate would once have unlinked
    // it and bound its own. Only the lock holder may.
    std::fs::create_dir_all(home.sock().parent().unwrap()).unwrap();
    std::fs::write(home.sock(), b"stale").unwrap();
    let mut kids: Vec<Child> = (0..20).map(|_| home.spawn()).collect();
    let router = until("a router", || async { home.router() }).await;
    assert!(kids.iter().any(|k| k.id() as i32 == router));
    let sock = home.sock();
    let mut me = until("the socket", || {
        Uds::join(&sock, "cli/me", frame::ROLE_CONSUMER)
    })
    .await;
    let mut want: Vec<String> = kids.iter().map(|k| cand(k.id())).collect();
    want.push(me_id());
    me.listed(&want).await;
    assert_eq!(
        home.router(),
        Some(router),
        "the router never changed hands"
    );
    for k in &mut kids {
        k.kill().unwrap();
        k.wait().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn door_refuses_strangers() {
    let home = Home::new("door");
    let mut a = home.spawn();
    let port = home.pairing.port;
    until("the door", || async {
        TcpStream::connect(("127.0.0.1", port)).await.ok()
    })
    .await;

    // A web page: the browser sets its Origin, and the page cannot change it.
    let page = ws_open(port, "https://evil.example").await.unwrap_err();
    assert!(page.contains("403"), "{page}");
    // Another extension.
    assert!(
        ws_open(port, "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await
            .unwrap_err()
            .contains("403")
    );

    // A process that forges our Origin but lacks the key: the router's proof
    // does not verify under a wrong key, and a guessed client proof is refused.
    let wrong = Pairing { port, key: [8; 32] };
    let mut ws = ws_open(port, CHROME).await.unwrap();
    let nc = pair::nonce().unwrap();
    ws_send(&mut ws, Frame::new(frame::AUTH, "", "", nc.to_vec())).await;
    let reply = ws_next(&mut ws).await.unwrap();
    let (ns, proof) = reply.payload.split_at(pair::NONCE);
    assert!(!wrong.verify(pair::ROUTER, &nc, ns, proof));
    ws_send(
        &mut ws,
        Frame::new(
            frame::AUTH,
            "",
            "",
            wrong.proof(pair::CLIENT, &nc, ns).to_vec(),
        ),
    )
    .await;
    ws_send(
        &mut ws,
        Frame::new(
            frame::HELLO,
            "browser/evil",
            "",
            frame::encode_hello(&desc(frame::ROLE_PROVIDER, "hanzo", &[])),
        ),
    )
    .await;
    assert!(
        ws_next(&mut ws).await.is_none(),
        "an unpaired client is closed, never welcomed"
    );

    // Skipping the proof entirely: HELLO first.
    let mut ws = ws_open(port, CHROME).await.unwrap();
    ws_send(
        &mut ws,
        Frame::new(
            frame::HELLO,
            "browser/evil",
            "",
            frame::encode_hello(&desc(frame::ROLE_PROVIDER, "hanzo", &[])),
        ),
    )
    .await;
    assert!(ws_next(&mut ws).await.is_none());

    // The key holder gets in — and may answer, but never call another node.
    let (mut ws, _) = browser(port, &home.pairing).await.unwrap();
    ws_send(
        &mut ws,
        Frame::new(frame::ROUTE, "", me_id(), b"run rm -rf".to_vec()),
    )
    .await;
    let no = loop {
        let f = ws_next(&mut ws).await.unwrap();
        if f.typ == frame::ERROR {
            break f;
        }
    };
    assert_eq!(no.payload, format!("forbidden:{}", me_id()).into_bytes());

    // A hello that is not an id is refused on the UDS too.
    let mut s = UnixStream::connect(home.sock()).await.unwrap();
    s.write_all(
        &Frame::new(
            frame::HELLO,
            "root/../../x",
            "",
            frame::encode_hello(&desc(frame::ROLE_CONSUMER, "", &[])),
        )
        .encode(),
    )
    .await
    .unwrap();
    let f = Frame::read(&mut s).await.unwrap().unwrap();
    assert_eq!(
        (f.typ, f.payload),
        (frame::ERROR, b"bad_id:root/../../x".to_vec())
    );
    assert!(Frame::read(&mut s).await.unwrap().is_none());
    a.kill().unwrap();
    a.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_socket_the_lock_holder_does_not_serve_is_never_spoken_to() {
    // Something binds the socket path without holding the router lock — an
    // older daemon left running, say. A node must not say HELLO to it.
    let home = Home::new("rogue");
    std::fs::create_dir_all(home.sock().parent().unwrap()).unwrap();
    let rogue = tokio::net::UnixListener::bind(home.sock()).unwrap();
    let heard = tokio::spawn(async move {
        let mut bytes = 0;
        while let Ok(Ok((mut c, _))) =
            tokio::time::timeout(Duration::from_secs(4), rogue.accept()).await
        {
            let mut buf = Vec::new();
            bytes += tokio::io::AsyncReadExt::read_to_end(&mut c, &mut buf)
                .await
                .unwrap_or(0);
        }
        bytes
    });
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "bystander",
            "--ignored",
            "--nocapture",
            "--quiet",
        ])
        .env("XDG_RUNTIME_DIR", home.dir.join("run"))
        .env("XDG_STATE_HOME", home.dir.join("state"))
        .env("HOME", &home.dir)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("refused"), "{said}");
    assert_eq!(heard.await.unwrap(), 0, "the rogue socket heard a frame");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_door_admits_browsers_only_and_they_take_no_id() {
    let home = Home::new("kinds");
    let mut a = home.spawn();
    let port = home.pairing.port;
    let sock = home.sock();
    let mut me = until("the socket", || {
        Uds::join(&sock, "cli/me", frame::ROLE_CONSUMER)
    })
    .await;

    // A door context that says it is an engine is refused and closed: it can
    // never become a node other nodes send their requests to.
    let mut w = paired(port, &home.pairing).await.unwrap();
    let no = hello_as(&mut w, "engine/qwen3").await.unwrap();
    assert_eq!(
        (no.typ, no.payload),
        (frame::ERROR, b"browser_only:engine/qwen3".to_vec())
    );
    assert!(ws_next(&mut w).await.is_none());
    // Nor can it claim a registered node's id to evict it.
    let mut w = paired(port, &home.pairing).await.unwrap();
    assert_eq!(hello_as(&mut w, "cli/me").await.unwrap().typ, frame::ERROR);
    me.listed(&[me_id(), cand(a.id())]).await;

    // A browser id already registered is not taken over by another context.
    let (mut first, id) = browser(port, &home.pairing).await.unwrap();
    let mut second = paired(port, &home.pairing).await.unwrap();
    let no = hello_as(&mut second, "browser/test-3fa2").await.unwrap();
    assert_eq!(
        (no.typ, no.payload),
        (frame::ERROR, format!("taken:{id}").into_bytes())
    );
    me.send(Frame::new(frame::ROUTE, "", &id, b"still you?".to_vec()))
        .await;
    let got = loop {
        let f = ws_next(&mut first).await.unwrap();
        if f.typ == frame::ROUTE {
            break f;
        }
    };
    assert_eq!(got.payload, b"still you?".to_vec());
    a.kill().unwrap();
    a.wait().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_flood_at_the_door_holds_few_descriptors() {
    let home = Home::new("flood");
    let mut a = home.spawn();
    let port = home.pairing.port;
    until("the door", || async {
        TcpStream::connect(("127.0.0.1", port)).await.ok()
    })
    .await;
    let fds = |pid: u32| {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let before = fds(a.id());
    // 200 sockets that upgrade and then say nothing — no token, no proof.
    let mut held = Vec::new();
    for _ in 0..200 {
        if let Ok(ws) = ws_open(port, CHROME).await {
            held.push(ws);
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let during = fds(a.id());
    assert!(
        during <= before + 24,
        "fds {before} -> {during} under a flood"
    );
    // The window is short: once it passes, a real browser is admitted.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(browser(port, &home.pairing).await.is_some());
    drop(held);
    a.kill().unwrap();
    a.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_dir_that_is_not_this_users_is_never_used() {
    // Planted as a symlink: the router never locks, binds or listens in it.
    let home = Home::new("planted");
    let elsewhere = home.dir.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.dir.join("run/zap")).unwrap();
    let mut a = home.spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "the router wrote into a planted directory"
    );
    a.kill().unwrap();
    a.wait().unwrap();

    // Left open to others by this user: tightened to 0700, then used.
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new("loose");
    std::fs::create_dir_all(home.dir.join("run/zap")).unwrap();
    std::fs::set_permissions(
        home.dir.join("run/zap"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let mut a = home.spawn();
    let sock = home.sock();
    until("the socket", || {
        Uds::join(&sock, "cli/me", frame::ROLE_CONSUMER)
    })
    .await;
    let mode = std::fs::metadata(home.dir.join("run/zap"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
    a.kill().unwrap();
    a.wait().unwrap();
}
