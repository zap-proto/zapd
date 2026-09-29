# zapd — the ZAP router

The one local router every ZAP node of a user login connects to — the browser
extension, agents' MCP servers, the dev CLI, the IDE, the desktop app — as a
**library**, not a daemon. Every process that speaks ZAP embeds it and stands
for election; the kernel picks one; when that process exits, another takes
over and every node reconnects. Nobody starts anything.

Design: [HIP-0069 — ZAP Mesh](https://github.com/hanzoai/hips/blob/main/HIPs/hip-0069-service-discovery-and-auto-bridge.md).

## What it is

```
zapd =
  registry   # who is connected: <kind>/<host>/<name> → connection, role, brand, caps
  route      # forward an opaque frame from A to B by its `to` field
  presence   # broadcast node connected / disconnected
  election   # which of this user's processes is the router
  door       # the loopback WebSocket a browser extension connects to
```

The router never parses a payload, never speaks a schema, never holds a lease.

## Election

Each candidate blocks on an `fcntl` write lock on `<runtime>/zapd.lock`
(`<runtime>` = `$XDG_RUNTIME_DIR/zap`, else `~/.zap/run`). The kernel grants it
to one process, which unlinks any stale socket, binds `<runtime>/zapd.sock`
(0600) and the browser door `127.0.0.1:<port>`, and serves until it exits. Exit
of any kind releases the lock and wakes exactly one waiter. Record locks are
per process and not inherited across `fork`, so a forked child never pins a
dead router's seat.

Each user's runtime directory, lock and door are their own. The runtime
directory is created `0700`; one that is a symlink or another user's is never
used, and one this user left open is tightened. The door port is picked free
(20000–29999) when the pairing is minted and kept in it.

## The browser door

A browser extension can open a WebSocket and nothing else. The door admits a
connection only if

1. `Origin` is our Blink extension (`chrome-extension://biingenefmanpecedoafkfajbnlgdmbl`)
   or any `moz-extension://` / `safari-web-extension://` origin, and `Host` is
   this loopback port — no web page gets a socket; and
2. it proves the pairing token, after the router proves it first
   (HMAC-SHA256, fresh nonces both ways; the token never crosses the wire).

A paired browser may only be a browser: its HELLO must name a `browser/…` id,
and it can never take over an id that is registered. It may address the router
or answer a call, never call another node, and 60 s of silence closes it. At
most eight connections are being admitted at once, each for at most 2 s, and a
message is at most 16 MiB.

**Pairing, once per browser:** run `zapd pair` (or `hanzo-mcp pair`) and paste
the code into the extension's popup. The code is `ws://127.0.0.1:<port>/#<token>`
and lives in `<state>/zap/pair` (`~/.local/state/zap/pair` on Linux), 0600.
`zapd pair --reset` mints a new token; every browser pairs again.

## The envelope

Little-endian, binary, the same bytes on the socket and — one frame per binary
message — on the WebSocket:

```
u32 len            bytes that follow; must equal 11 + from_len + to_len + payload_len
u8  type
u16 flags
u16 from_len
u16 to_len
u32 payload_len
bytes from         sender id (the router overwrites it with the registered id)
bytes to           destination (empty ⇒ the frame is for the router)
bytes payload      opaque
```

Types: `HELLO(1) WELCOME(2) PROVIDERS_LIST(3) PROVIDERS(4) PEER_CONNECTED(5)
PEER_DISCONNECTED(6) ERROR(7) AUTH(8)`; forwarded untouched: `ROUTE(16)
RESPONSE(17) EVENT(18)`. A node says `HELLO` as `<kind>/<name>`; `WELCOME` is
addressed to its full id `<kind>/<host>/<name>`. `PROVIDERS` lists every node.
The descriptor a `HELLO` carries and `PROVIDERS` returns is `role(u8)
brand(str) caps(u16 n, str…) attrs(u16 n, (str, str)…)`; strings are u16-length
UTF-8.

## Embedding

Rust:

```rust
zapd::embed();                                            // stand for router
let me = zapd::Node::join("dev/4242", zapd::frame::ROLE_CONSUMER, "hanzo", &[]);
let nodes = me.nodes(Duration::from_secs(2)).await?;
let reply = me.call("browser/dgx/chromium-3fa2", payload, Duration::from_secs(30)).await?;
```

Python (`python/`, built with maturin, `import zapd`):

```python
zapd.embed()
me = zapd.Node("agent/hanzo-4242")
me.nodes(); me.call(to, payload, timeout=30.0); zapd.pair()
```

## Operate

```sh
zapd pair            # the pairing code for the browser extension
zapd pair --reset    # new token; browsers pair again
zapd ls              # the nodes on this machine's router
```

## Test

```sh
cargo test                                   # frames, ids, pairing, door, and the
                                             # multi-process election end to end
cd python && maturin develop && pytest       # the binding, end to end
```

## Install the operator tool

```sh
curl -fsSL https://raw.githubusercontent.com/zap-proto/zapd/main/install.sh | sh
# or
npm i -g @zap-proto/zapd
```
