#!/bin/sh
# Install zapd — the ZAP router's operator tool (`zapd pair`, `zapd ls`).
# Native binary, no build, no QEMU.
#   curl -fsSL https://raw.githubusercontent.com/zap-proto/zapd/main/install.sh | sh
#
# SUPPLY CHAIN: the release publishes `<asset>.tar.gz.sha256` next to each
# tarball (`shasum -a 256 <asset>.tar.gz`). We download that digest and verify
# the tarball against it with SHA-256 BEFORE extracting, chmod, or exec. Fail
# closed on a missing digest, missing hasher, or mismatch.
set -e
REPO="zap-proto/zapd"
os=$(uname -s); arch=$(uname -m)
case "$os" in
  Darwin) o=apple-darwin ;;
  Linux)  o=unknown-linux-musl ;;
  *) echo "zapd: unsupported OS $os (macOS/Linux only)"; exit 1 ;;
esac
case "$arch" in
  arm64|aarch64) a=aarch64 ;;
  x86_64|amd64)  a=x86_64 ;;
  *) echo "zapd: unsupported arch $arch"; exit 1 ;;
esac
target="${a}-${o}"
asset="zapd-${target}.tar.gz"
ver="${ZAPD_VERSION:-latest}"
if [ "$ver" = latest ]; then
  url="https://github.com/$REPO/releases/latest/download/${asset}"
else
  url="https://github.com/$REPO/releases/download/v${ver}/${asset}"
fi
dest="${ZAPD_BIN:-/usr/local/bin}"
[ -d "$dest" ] && [ -w "$dest" ] || dest="$HOME/.local/bin"
mkdir -p "$dest"
echo "zapd: installing $target -> $dest"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Pick a SHA-256 hasher; fail closed if none (do not install unverified).
if command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
  echo "zapd: no sha256 tool (shasum/sha256sum) found; refusing to install unverified binary"; exit 1
fi

# Download the tarball and its published digest, then verify BEFORE extracting.
curl -fsSL "$url" -o "$tmp/$asset"
curl -fsSL "$url.sha256" -o "$tmp/$asset.sha256"
expected=$(cut -d' ' -f1 < "$tmp/$asset.sha256")
case "$expected" in
  [0-9a-fA-F]*) ;;
  *) echo "zapd: malformed .sha256 digest from release; aborting"; exit 1 ;;
esac
actual=$(sha256 "$tmp/$asset")
# Lowercase both for a case-insensitive compare (shasum emits lowercase already).
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
if [ "$expected" != "$actual" ]; then
  echo "zapd: checksum mismatch for $asset"
  echo "  expected $expected"
  echo "  actual   $actual"
  exit 1
fi
tar -xz -C "$tmp" -f "$tmp/$asset"

# ATOMIC install: stage into $dest, verify it runs, then rename() over the path.
# Overwriting a running binary in place fails (ETXTBSY) or corrupts it.
# rename() is atomic on one filesystem; live processes keep the old inode, new
# launches get the new binary. Never `cp`/`install` in place.
staged="$dest/.zapd.new.$$"
install -m 0755 "$tmp/zapd" "$staged"
"$staged" --version >/dev/null 2>&1 || { echo "zapd: staged binary failed --version, aborting (not swapping in a bad binary)"; rm -f "$staged"; exit 1; }
mv -f "$staged" "$dest/zapd"
echo "zapd: installed $("$dest/zapd" --version 2>/dev/null || echo "$dest/zapd") (sha256 verified)"
case ":$PATH:" in *":$dest:"*) ;; *) echo "zapd: add $dest to your PATH" ;; esac

# The router runs inside the processes that speak ZAP; this binary never
# serves. An earlier install kept a `zapd` daemon up with a user service, which
# this binary would now fail on in a restart loop — take it down.
case "$os" in
  Linux)
    ud="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    if [ -f "$ud/zapd.service" ]; then
      systemctl --user disable --now zapd.service 2>/dev/null || true
      rm -f "$ud/zapd.service"
      systemctl --user daemon-reload 2>/dev/null || true
      echo "zapd: removed the zapd.service user unit"
    fi
    ;;
  Darwin)
    la="$HOME/Library/LaunchAgents/zap.zapd.plist"
    if [ -f "$la" ]; then
      launchctl unload "$la" 2>/dev/null || true
      rm -f "$la"
      echo "zapd: removed the zap.zapd launchd agent"
    fi
    ;;
esac
echo "zapd: ready — \`zapd pair\` prints the browser pairing code, \`zapd ls\` lists nodes."
