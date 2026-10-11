#!/usr/bin/env node
const { execFileSync } = require('node:child_process');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const tauriDir = path.join(root, 'app', 'src-tauri');

const tree = execFileSync(
  'cargo',
  ['tree', '--target', 'aarch64-linux-android', '-e', 'features'],
  { cwd: tauriDir, encoding: 'utf8' },
);

if (tree.includes('openssl-sys')) {
  throw new Error('Android dependency graph must not include openssl-sys; use Rust TLS-compatible dependencies instead.');
}

if (!tree.includes('librqbit feature "rust-tls"')) {
  throw new Error('Android dependency graph must enable librqbit rust-tls.');
}

console.log('PASS Android Rust TLS dependency contract');
