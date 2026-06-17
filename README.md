# zapd — the ZAP universal router

One brand-neutral daemon per machine. Every ZAP service — browser extensions
(via the native host), IDE extensions, CLI agents, hanzo-mcp — connects to the
**one shared Unix socket** and is multiplexed here. Launched once, shared by
all. Brands ship thin white-label wrappers (`@hanzo/zapd`, `@lux/zapd`,
`@zoo/zapd`) over this same binary.

## What it is

```
zapd =
  registry   # who is connected:  id → connection, role, brand, caps
  router     # forward an opaque frame from A to B by its `to` field
  presence   # broadcast peer connected / disconnected
```

…and nothing else. **No leases. No schema parsing. No `.capnp`. No `.zap`
payload parsing. No browser/payments/PQ logic.** The router is dumb and strong:
opaque frame in → look up destination → opaque frame out.

- **Leasing / exclusivity** is a provider-local concern (a provider may reply
  `busy`); the router never locks.
- **E2E post-quantum encryption** (X25519 + ML-KEM, AEAD channel) and
  **payments** ride *inside* `payload` as opaque bytes, end-to-end between
  peers — the router cannot read them.
- **PQ identity** (DID + ML-DSA) is verified at `hello`; the router stamps the
  verified `from` onto every forwarded frame so a peer can never spoof another.

Typed protocols live in their own `.zap` schemas, never here:
`zap-proto/browser`, `zap-proto/payments`, `zap-proto/identity`.

## Socket

Brand-neutral, never TCP:

```
$ZAP_SOCK  ›  $XDG_RUNTIME_DIR/zap/zapd.sock  ›  ~/.zap/run/zapd.sock
```

Socket-activation friendly (`ZAP_LISTEN_FD`); single-instance guard refuses to
fight a live router for the path.

## ZAP router envelope (little-endian, binary — not JSON, not capnp)

```
u32 len            bytes that follow
u8  type
u16 flags
u16 from_len
u16 to_len
u32 payload_len
bytes from         source id (router stamps the verified id)
bytes to           destination (empty ⇒ the frame is for zapd)
bytes payload      opaque
```

Routing rule: `to` empty ⇒ for zapd (`hello`, `providers.list`); `to` set ⇒
forward opaquely. Correlation lives in the payload's `.zap` schema, not here.

Types: `HELLO(1) WELCOME(2) PROVIDERS_LIST(3) PROVIDERS(4) PEER_CONNECTED(5)
PEER_DISCONNECTED(6) ERROR(7)`; pass-through `ROUTE(16) RESPONSE(17) EVENT(18)`.

## Run

```sh
cargo run                      # binds the shared socket
ZAP_SOCK=/tmp/zapd.sock cargo run --release
```

## Test

```sh
cargo build
ZAP_SOCK=/tmp/zapd-e2e.sock ./target/debug/zapd --log warn &
python3 tests/e2e.py /tmp/zapd-e2e.sock
```

Exercises register → `providers.list` → opaque route (verified `from`, payload
byte-identical) → response → presence-on-disconnect.

## Install

```sh
# curl | sh (macOS/Linux, arm64/amd64 — native binary, no build)
curl -fsSL https://raw.githubusercontent.com/zap-proto/zapd/main/install.sh | sh

# or npm (brand wrapper over the same canonical binary)
npm i -g @hanzo/zapd
```

Run it once per login (it self-binds the shared socket; everything shares it):

```sh
# macOS
cp dist/zap.zapd.plist ~/Library/LaunchAgents/ && launchctl load ~/Library/LaunchAgents/zap.zapd.plist
# Linux
mkdir -p ~/.config/systemd/user && cp dist/zapd.service ~/.config/systemd/user/ && systemctl --user enable --now zapd
```

## CI / release

- **ci** (`.github/workflows/ci.yml`): clippy `-D warnings` + `cargo test` + e2e on
  4 **native** targets (macOS arm64/amd64, Linux musl amd64/arm64) — no QEMU.
- **release** (`.github/workflows/release.yml`): on `v*` tag, builds all 4 targets,
  uploads `zapd-<target>.tar.gz` (+ sha256) to the GitHub Release, and publishes
  `@hanzo/zapd` to npm.
