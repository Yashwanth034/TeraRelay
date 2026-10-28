#!/usr/bin/env node
'use strict';

// Source-only regression guard, NOT a native RAR extraction test.
// Protects the fail-closed temporary RAR preview policy while a safe decoder
// and its cross-platform/licensing implications are evaluated.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');

const root = path.join(__dirname, '..');
const source = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/archive.rs'), 'utf8');
const manifest = fs.readFileSync(path.join(root, 'app/src-tauri/Cargo.toml'), 'utf8');
const lock = fs.readFileSync(path.join(root, 'app/src-tauri/Cargo.lock'), 'utf8');

function extractBetween(start, end) {
  const from = source.indexOf(start);
  const until = source.indexOf(end, from + start.length);
  assert.ok(from >= 0 && until > from, `Cannot isolate archive command ${start}`);
  return source.slice(from, until);
}

const listing = extractBetween('pub async fn cmd_list_archive_contents(', 'pub async fn cmd_extract_archive_entry(');
const extracting = extractBetween('pub async fn cmd_extract_archive_entry(', '// ── Shared preparation');
const rejectRar = /ArchiveType::Rar\s*=>\s*Err\(rar_preview_unavailable\(\)\)/;
for (const [name, command] of [['listing', listing], ['extraction', extracting]]) {
  assert.match(command, rejectRar, `RAR ${name} must fail closed before unsafe extraction`);
  assert.match(command, /ArchiveType::Zip\s*=>/, `ZIP ${name} must remain available`);
  assert.match(command, /ArchiveType::SevenZ\s*=>/, `7z ${name} must remain available`);
}
assert.match(source, /fn rar_preview_unavailable\(\) -> String/);
assert.match(source, /RAR in-app preview is temporarily disabled/);
assert.match(source, /download the RAR file/);
assert.doesNotMatch(source, /rar::Archive::extract_all/);
assert.doesNotMatch(manifest, /^\s*rar\s*=/m);
assert.doesNotMatch(lock, /\nname = "rar"\n/);

// The frontend must show the returned safety explanation rather than hiding
// backend errors or indefinitely displaying a loading spinner.
const viewer = fs.readFileSync(
  path.join(root, 'app/src/components/desktop/dashboard/ArchiveViewerModal.tsx'),
  'utf8',
);
assert.match(viewer, /invoke<ArchiveEntry\[\]>\('cmd_list_archive_contents'/);
assert.match(viewer, /catch \(e\) \{[\s\S]{0,100}setError\(String\(e\)\)/);
assert.match(viewer, /\{error && !loading && \(/);
assert.match(viewer, /\{error\}/);
console.log('PASS source-only RAR fail-closed guard, unsafe decoder absent, frontend shows safety error');
