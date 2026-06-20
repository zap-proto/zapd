#!/usr/bin/env node
// Downloads the canonical zap-proto/zapd binary for this platform/arch from the
// matching GitHub release. Native binaries only — no QEMU, no build on install.
//
// SUPPLY CHAIN: the release publishes `<asset>.tar.gz.sha256` next to each
// tarball (`shasum -a 256 <asset>.tar.gz`). We download that digest first and
// verify the tarball against it with SHA-256 BEFORE extracting, chmod, exec, or
// the atomic rename. Fail closed on any download error, malformed digest, or
// mismatch — a tampered or truncated asset never reaches disk as an executable.
const https = require('https');
const fs = require('fs');
const path = require('path');
const { createHash } = require('crypto');
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
const asset = `zapd-${target}.tar.gz`;
const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
const sumUrl = `${url}.sha256`;
const tarPath = path.join(binDir, 'zapd.tar.gz');

function fail(msg) {
  console.error(`[zapd] ${msg}`);
  process.exit(1);
}

// Follow redirects, buffer the whole body. Used for the small .sha256 file.
function fetchText(u, cb, redirects = 0) {
  if (redirects > 5) return fail(`too many redirects fetching ${u}`);
  https.get(u, (res) => {
    if (res.statusCode === 301 || res.statusCode === 302) {
      res.resume();
      return fetchText(res.headers.location, cb, redirects + 1);
    }
    if (res.statusCode !== 200) return fail(`download failed ${res.statusCode}: ${u}`);
    const chunks = [];
    res.on('data', (c) => chunks.push(c));
    res.on('end', () => cb(Buffer.concat(chunks).toString('utf8')));
  }).on('error', (e) => fail(e.message));
}

// Follow redirects, stream to dest. Used for the (large) tarball.
function download(u, dest, cb, redirects = 0) {
  if (redirects > 5) return fail(`too many redirects fetching ${u}`);
  https.get(u, (res) => {
    if (res.statusCode === 301 || res.statusCode === 302) {
      res.resume();
      return download(res.headers.location, dest, cb, redirects + 1);
    }
    if (res.statusCode !== 200) return fail(`download failed ${res.statusCode}: ${u}`);
    const f = fs.createWriteStream(dest);
    res.pipe(f);
    f.on('finish', () => f.close(cb));
  }).on('error', (e) => fail(e.message));
}

// `shasum -a 256 file` → "<64-hex>  file". Take the first token, validate shape.
function parseSha256(text) {
  const hex = text.trim().split(/\s+/)[0]?.toLowerCase() ?? '';
  if (!/^[0-9a-f]{64}$/.test(hex)) fail(`malformed .sha256 digest: ${JSON.stringify(text)}`);
  return hex;
}

function sha256File(file) {
  return createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

console.log(`[zapd] downloading ${target} v${VERSION}`);
fetchText(sumUrl, (sumText) => {
  const expected = parseSha256(sumText);
  download(url, tarPath, () => {
    const actual = sha256File(tarPath);
    if (actual !== expected) {
      fs.unlinkSync(tarPath);
      fail(`checksum mismatch for ${asset}\n  expected ${expected}\n  actual   ${actual}`);
    }
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
      fs.rmSync(stageDir, { recursive: true, force: true });
      fail('staged binary failed --version; aborting install (not swapping in a bad binary)');
    }
    fs.renameSync(staged, path.join(binDir, 'zapd'));
    fs.rmSync(stageDir, { recursive: true, force: true });
    console.log('[zapd] installed bin/zapd (sha256 verified, atomic)');
  });
});
