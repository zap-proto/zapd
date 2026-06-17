#!/bin/sh
# Install zapd — the ZAP universal router. Native binary, no build, no QEMU.
#   curl -fsSL https://raw.githubusercontent.com/zap-proto/zapd/main/install.sh | sh
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
ver="${ZAPD_VERSION:-latest}"
if [ "$ver" = latest ]; then
  url="https://github.com/$REPO/releases/latest/download/zapd-${target}.tar.gz"
else
  url="https://github.com/$REPO/releases/download/v${ver}/zapd-${target}.tar.gz"
fi
dest="${ZAPD_BIN:-/usr/local/bin}"
[ -d "$dest" ] && [ -w "$dest" ] || dest="$HOME/.local/bin"
mkdir -p "$dest"
echo "zapd: installing $target -> $dest"
tmp=$(mktemp -d)
curl -fsSL "$url" | tar -xz -C "$tmp"
install -m 0755 "$tmp/zapd" "$dest/zapd"
rm -rf "$tmp"
echo "zapd: installed $("$dest/zapd" --version 2>/dev/null || echo "$dest/zapd")"
case ":$PATH:" in *":$dest:"*) ;; *) echo "zapd: add $dest to your PATH" ;; esac
