'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const cache = new Map();
function load(name) {
  const file = path.join(root, 'app/src', name + '.ts');
  if (cache.has(file)) return cache.get(file);
  const mod = { exports: {} };
  cache.set(file, mod.exports);
  const code = ts.transpileModule(fs.readFileSync(file, 'utf8'),
    { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }).outputText;
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})',
    { console, TextEncoder })(spec => {
      if (!spec.startsWith('./')) throw Error('Unexpected dependency ' + spec);
      return load(path.normalize(path.join(path.dirname(name), spec)));
    }, mod, mod.exports);
  return mod.exports;
}
const bulk = load('driveBulkOrganizer');
const raw = {
  revision: 71, dirty: false,
  directories: [
    { id: 'in', name: 'Incoming', parent_id: null, trashed_at: null },
    { id: 'out', name: 'Archive', parent_id: null, trashed_at: null },
    { id: 'next', name: 'Reports', parent_id: null, trashed_at: null },
  ],
  files: [
    { entry_id: 'first', display_name: 'budget.csv', directory_id: 'in', total_size: 7 },
    { entry_id: 'second', display_name: 'weekly.csv', directory_id: 'in', total_size: 5 },
    { entry_id: 'comma', display_name: 'a,b.txt', directory_id: null, total_size: 4 },
  ],
  pending: [], symlinks: [],
};
const snapshot = bulk.readOrganizerSnapshot(raw);
const mapping = 'source_path,new_path\r\n'
  + '/Incoming/budget.csv,/Archive/budget-2026.csv\r\n'
  + '/Incoming/weekly.csv,/Reports/weekly.csv\r\n';
const result = bulk.buildOrganizerCsvRequest(snapshot, mapping);
assert.equal(result.revision, 71);
assert.equal(result.changes.length, 2);
assert.deepEqual(Array.from(result.changes, c => [c.entry_id, c.new_name, c.new_directory_id]),
  [['first', 'budget-2026.csv', 'out'], ['second', 'weekly.csv', 'next']]);
assert.equal(result.changes[0].expected_directory_id, 'in');
const quoted = bulk.buildOrganizerCsvRequest(snapshot,
  '\uFEFFsource_path,new_path\n"/a,b.txt","/Reports/a,b.txt"');
assert.equal(quoted.changes[0].entry_id, 'comma');
assert.equal(quoted.changes[0].new_name, 'a,b.txt');

const bad = (csv, pattern) => assert.throws(() => bulk.buildOrganizerCsvRequest(snapshot, csv), pattern);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/x\n/Incoming/budget.csv,/Reports/y',
  /duplicate source/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/X\n/Incoming/weekly.csv,/Archive/x',
  /duplicate target/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Missing/a', /parent must already exist/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Incoming/budget.csv', /no change/i);
bad('source_path,new_path\n/Incoming/missing.csv,/Archive/foo', /source file not found/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/CON.txt', /nonportable/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/../foo', /destination parent/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/', /full absolute/i);
bad('source_path,new_path\n/Incoming/budget.csv,/Archive/foo\n/Incoming/weekly.csv',
  /exactly two columns/i);
bad('source_path,new_path,extra\n/Incoming/budget.csv,/Archive/foo',
  /exactly two columns/i);
bad('source_path,new_path\n"/Incoming/budget.csv,/Archive/foo', /Unterminated/i);
bad('source_path,new_path\n"/Incoming/budget.csv"oops,/Archive/foo', /Malformed/i);
bad('bad_header,new_path\n/Incoming/budget.csv,/Archive/foo', /begin with/i);
bad('source_path,new_path\n', /include 1/i);
assert.throws(() => bulk.buildOrganizerCsvRequest(snapshot, 'x'.repeat(128 * 1024 + 1)),
  /128 KiB/i);
const many = 'source_path,new_path\n'
  + Array.from({ length: 101 }, (_, i) => '/Incoming/missing' + i + ',/Archive/new' + i).join('\n');
bad(many, /at most 100/i);
const duplicatePaths = { ...snapshot, files: [
  ...snapshot.files, { ...snapshot.files[0], id: 'another' },
] };
assert.throws(() => bulk.buildOrganizerCsvRequest(duplicatePaths,
  'source_path,new_path\n/Incoming/budget.csv,/Archive/new'),
  /ambiguous source/i);
const duplicateFolderPaths = { ...snapshot, folders: [
  ...snapshot.folders, { id: 'other', path: '/Archive' },
] };
assert.throws(() => bulk.buildOrganizerCsvRequest(duplicateFolderPaths,
  'source_path,new_path\n/Incoming/budget.csv,/Archive/new'),
  /ambiguous destination/i);
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveBulkOrganizer.tsx'), 'utf8');
for (const marker of ['buildOrganizerCsvRequest', 'csvMode', 'csvText',
  'File mappings: source_path,new_path', 'cmd_drive_bulk_preview',
  'cmd_drive_bulk_apply', 'previewReady',
  'request?.changes.length', 'setPreview(null)',
  'Apply exactly']) {
  assert.ok(ui.includes(marker), 'Missing file CSV workflow marker: ' + marker);
}
assert.ok(ui.includes('disabled={busy !== null || !request}'),
  'Apply must require a fresh native preview before mutation');
console.log('PASS Drive File CSV: exact per-file cross-folder mapping, quoting, bounds, portable names, preview-gated native transaction');
