# zapd — ZAP universal router (`zap-proto/zapd`)

Full docs: **README.md**. Quick management notes:

- **What:** one brand-neutral, schema-agnostic router per **user login**, on the
  shared UDS `$XDG_RUNTIME_DIR/zap/zapd.sock` → `~/.zap/run/zapd.sock`. Does
  `registry + route + presence`, nothing else. Binary ZAP router envelope; no
  JSON, no capnp, no leases.
- **Run/install:** `curl -fsSL https://raw.githubusercontent.com/zap-proto/zapd/main/install.sh | sh`
  or `npm i -g @hanzo/zapd`. Service units in `dist/` (launchd/systemd).
- **CI/CD:** `.github/workflows/{ci,release}.yml` — clippy+test+e2e + multi-arch
  release on **native runners, no QEMU** (x86_64-darwin cross-compiled on arm64).
  Tag `v*` → 4 binaries to GitHub Releases + `@hanzo/zapd` (needs `NPM_TOKEN`).
- **Test:** `cargo test` (frame codec) + `python3 tests/e2e.py <sock>` (router e2e).
- **Do NOT** add capnp/JSON to the router. PQ-identity verification of `hello`
  is the planned next layer (see ../identity/identity.zap).
