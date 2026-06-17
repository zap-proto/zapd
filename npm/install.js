#!/usr/bin/env node
// Downloads the canonical zap-proto/zapd binary for this platform/arch from the
// matching GitHub release. Native binaries only — no QEMU, no build on install.
const https = require('https');
const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const VERSION = require('./package.json').version;
const REPO = 'zap-proto/zapd';
const TARGETS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
};

const key = `${process.platform}-${process.arch}`;
const target = TARGETS[key];
if (!target) {
  console.error(`[zapd] unsupported platform ${key} (darwin/linux × x64/arm64 only)`);
  process.exit(1);
}

const binDir = path.join(__dirname, 'bin');
fs.mkdirSync(binDir, { recursive: true });
const url = `https://github.com/${REPO}/releases/download/v${VERSION}/zapd-${target}.tar.gz`;
const tarPath = path.join(binDir, 'zapd.tar.gz');

function download(u, dest, cb) {
  https.get(u, (res) => {
    if (res.statusCode === 301 || res.statusCode === 302) return download(res.headers.location, dest, cb);
    if (res.statusCode !== 200) { console.error(`[zapd] download failed ${res.statusCode}: ${u}`); process.exit(1); }
    const f = fs.createWriteStream(dest);
    res.pipe(f);
    f.on('finish', () => f.close(cb));
  }).on('error', (e) => { console.error('[zapd]', e.message); process.exit(1); });
}

console.log(`[zapd] downloading ${target} v${VERSION}`);
download(url, tarPath, () => {
  // ATOMIC install: never overwrite the live binary in place. A running zapd
  // (browser native host, or the router) holds the inode; extracting over it
  // mid-exec yields a corrupt/partial binary → ETXTBSY crash-loop. Stage to a
  // temp dir, chmod, verify it runs, then rename() over the path — atomic on
  // macOS/Linux; existing processes keep the old inode, new launches get the new.
  const stageDir = fs.mkdtempSync(path.join(binDir, '.stage-'));
  execFileSync('tar', ['-xzf', tarPath, '-C', stageDir]);
  fs.unlinkSync(tarPath);
  const staged = path.join(stageDir, 'zapd');
  fs.chmodSync(staged, 0o755);
  try {
    execFileSync(staged, ['--version'], { stdio: 'ignore' });
  } catch (e) {
    console.error('[zapd] staged binary failed --version; aborting install (not swapping in a bad binary)');
    fs.rmSync(stageDir, { recursive: true, force: true });
    process.exit(1);
  }
  fs.renameSync(staged, path.join(binDir, 'zapd'));
  fs.rmSync(stageDir, { recursive: true, force: true });
  console.log('[zapd] installed bin/zapd (atomic)');
});
