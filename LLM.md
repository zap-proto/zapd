# zapd — the ZAP router (`zap-proto/zapd`)

Full docs: **README.md**. Design: HIP-0069 (hanzoai/hips).

- **What:** a library, not a daemon. Every process that speaks ZAP calls
  `zapd::embed()` (router candidacy) and `zapd::Node::join()` (its own seat).
  An `fcntl` lock on `<runtime>/zapd.lock` elects one router per user login;
  its exit hands the seat to the next candidate. Doors: `<runtime>/zapd.sock`
  (0600) and the browser WebSocket on the port the pairing names (picked free
  at mint, 20000–29999). The runtime dir is validated (`private_runtime`)
  before anything locks, binds or connects in it.
- **Modules:** `frame` (envelope, + `AUTH=8`), `id` (`<kind>/<host>/<name>`,
  host stamped by the router), `router` (registry + route + presence;
  transport-free), `elect` (lock, doors, the one runtime thread), `door`
  (Origin/Host check, pairing proof, browser-only HELLO, no id take-over,
  browsers never originate, eight admissions at a time), `pair` (the
  code file `<state>/zap/pair`, 0600, lstat-checked), `node` (a process's seat;
  reconnects 50 ms→1 s; one call at a time, matched by responder `from`; talks
  only to a socket whose peer pid is the lock holder).
- **POSIX record locks drop when ANY fd of the file closes.** The lock file is
  opened once per process (`elect::lock_fd`) and never closed; never open it
  anywhere else, or the router in this process loses its seat.
- **Binary:** `zapd pair [--reset]`, `zapd ls`. It never serves.
- **Release:** tag `v*` → binaries + npm `@zap-proto/zapd`, abi3 wheels to
  PyPI `zapd` (twine; the token is KMS `python-sdk-publish/PYPI_TOKEN`, read
  with the repo's KMS_CLIENT_ID/SECRET, identity hanzo-ci), crate to crates.io `zapd`. Versions follow the tags (v1.1.1 was
  the last before the library).
- **Python:** `python/` — PyO3 binding of the same crate (`import zapd`),
  abi3-py310, built with maturin. The one router implementation; do not port it.
- **Test:** `cargo test` runs the election end to end with real processes (the
  test binary re-runs itself as the ignored `candidate` test, each with its own
  `XDG_RUNTIME_DIR` / `XDG_STATE_HOME`). `cd python && pytest` after
  `maturin develop` does the same through Python.
- **Build here:** `~/.cargo/config.toml` sets `rustc-wrapper = zccache`, which
  breaks `cargo clippy`; run clippy with `RUSTC_WRAPPER=`.
- **Do NOT** add a schema, capnp or JSON to the router, or a second transport
  for the browser. Payloads are opaque; MCP traffic rides them as MCP's own
  JSON-RPC bytes.
