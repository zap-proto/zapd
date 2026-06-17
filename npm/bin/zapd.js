#!/usr/bin/env node
// Thin launcher: exec the platform binary fetched by install.js.
const { spawnSync } = require('child_process');
const path = require('path');
const bin = path.join(__dirname, 'zapd');
const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
process.exit(r.status == null ? 1 : r.status);
