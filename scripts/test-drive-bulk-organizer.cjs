'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const cache = new Map();
function load(file) {
  const absolute = path.join(root, 'app/src', file + '.ts');
  if (cache.has(absolute)) return cache.get(absolute);
  const module = { exports: {} };
  cache.set(absolute, module.exports);
  const code = ts.transpileModule(fs.readFileSync(absolute, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText;
  const localRequire = name => {
    if (!name.startsWith('./')) throw new Error('Unexpected external TS fixture dependency: ' + name);
    return load(path.normalize(path.join(path.dirname(file), name)));
  };
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})',
    { console })(localRequire, module, module.exports);
  return module.exports;
}
const bulk = load('driveBulkOrganizer');
const snapshot = {
  revision: 5, dirty: false,
  directories: [{ id: 'folder', name: 'Work', parent_id: null, trashed_at: null }],
  files: [
    { entry_id: 'one', display_name: 'report.txt', directory_id: null, total_size: 4 },
    { entry_id: 'two', display_name: 'draft.csv', directory_id: 'folder', total_size: 6 },
  ],
  pending: [{ id: 'stage', display_name: 'pending.bin', directory_id: null, size: 10 }],
};
let checked = bulk.readOrganizerSnapshot(snapshot);
assert.equal(checked.files.length, 2);
assert.equal(checked.folders.length, 1);
assert.equal(checked.files[1].path, '/Work/draft.csv');
assert.equal(checked.revision, 5);
assert.equal(bulk.targetOrganizerName('archive.tar.gz',
  { find: 'archive', replace: 'review', prefix: '2026-', suffix: '-final', destinationId: 'same' }),
  '2026-review.tar-final.gz');
const options = { find: 'draft', replace: 'reviewed', prefix: 'v1-', suffix: '', destinationId: null };
const plan = bulk.buildOrganizerRequest(checked, ['two', 'one'], options);
assert.equal(plan.revision, 5);
assert.equal(plan.changes[0].new_directory_id, null);
assert.equal(plan.changes[0].new_name, 'v1-reviewed.csv');
assert.equal(plan.changes[0].expected_directory_id, 'folder');
assert.equal(plan.changes[1].new_name, 'v1-report.txt');
assert.equal(plan.changes[1].expected_directory_id, null);
assert.throws(() => bulk.buildOrganizerRequest(checked, ['one', 'one'], options), /distinct/);
assert.throws(() => bulk.buildOrganizerRequest(checked, ['missing'], options), /no longer/);
assert.throws(() => bulk.buildOrganizerRequest(checked, ['one'],
  { ...options, destinationId: 'unknown' }), /not in this Drive/);
assert.throws(() => bulk.buildOrganizerRequest(checked, ['one'],
  { ...options, prefix: '../' }), /portable/);
assert.throws(() => bulk.buildOrganizerRequest(checked, ['one'],
  { ...options, prefix: 'X:' }), /portable/);
assert.throws(() => bulk.readOrganizerSnapshot({ ...snapshot, revision: 'oops' }), /invalid revision/);
const withCorrupt = bulk.readOrganizerSnapshot({
  ...snapshot, files: [...snapshot.files, {
    entry_id: 'spoof', display_name: '../secret', directory_id: null, total_size: 12,
  }, {
    entry_id: 'linked', display_name: 'link.txt', directory_id: null,
    total_size: 12, inode_id: 'shared-inode',
  }],
});
assert.deepEqual(Array.from(withCorrupt.files, file => file.id), ['one', 'two']);
assert.ok(withCorrupt.skipped > 0);
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveBulkOrganizer.tsx'), 'utf8');
const settings = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
for (const term of ['cmd_drive_bulk_preview', 'cmd_drive_bulk_apply', 'cmd_drive_sync_metadata',
  'previewReady', 'source', 'retry Drive sync when connected',
  "disabled={busy === 'apply'}", "if (busy === 'apply') {"]) {
  assert.ok(ui.includes(term), 'UI should include ' + term);
}
assert.match(settings, /<DriveBulkOrganizer active=\{isOpen && activeTab === 'general'\} \/>/);
const rust = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/drive_bulk_organizer.rs'), 'utf8');
for (const term of ['SAVEPOINT drive_bulk_organize', 'ROLLBACK TO drive_bulk_organize',
  'rename_drive_file', 'revision != snapshot.revision', 'validate_drive_name',
  'fn rollback_first_success_when_second_write_fails']) {
  assert.ok(rust.includes(term), 'Rust workflow must include ' + term);
}
console.log('PASS Drive Bulk Organizer: real TS planner, malformed snapshots, collision contract, preview/apply/sync UI integration');
