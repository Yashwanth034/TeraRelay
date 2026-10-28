'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const cache = new Map();
function load(name) {
  const filename = path.resolve(root, 'app/src', name + '.ts');
  if (cache.has(filename)) return cache.get(filename);
  const mod = { exports: {} };
  cache.set(filename, mod.exports);
  const code = ts.transpileModule(fs.readFileSync(filename, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText;
  const localRequire = modulePath => {
    if (!modulePath.startsWith('./')) throw Error('Unexpected fixture dependency');
    return load(path.join(path.dirname(name), modulePath));
  };
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})', {
    console, TextEncoder,
  })(localRequire, mod, mod.exports);
  return mod.exports;
}
const operations = load('driveFolderOperations');
const base = {
  revision: 15, dirty: false,
  directories: [
    { id: 'a', name: 'Alpha', parent_id: null, trashed_at: null },
    { id: 'child', name: 'Docs', parent_id: 'a', trashed_at: null },
    { id: 'b', name: 'Beta', parent_id: null, trashed_at: null },
    { id: 'c', name: 'Gamma', parent_id: null, trashed_at: null },
  ],
  files: [], pending: [], symlinks: [],
};
const inventory = operations.parseFolderInventory(base);
assert.equal(inventory.folders.length, 4);
assert.equal(inventory.folders.find(x => x.id === 'child').path, '/Alpha/Docs');
assert.equal(inventory.revision, 15);
const options = { find: 'Alpha', replace: 'Projects', prefix: '2026-', suffix: '-final',
  destinationId: 'b' };
const plan = operations.buildFolderPlan(inventory, ['a'], options);
assert.equal(plan.revision, 15);
assert.equal(plan.changes[0].folder_id, 'a');
assert.equal(plan.changes[0].expected_name, 'Alpha');
assert.equal(plan.changes[0].expected_parent_id, null);
assert.equal(plan.changes[0].new_parent_id, 'b');
assert.equal(plan.changes[0].new_name, '2026-Projects-final');
const unchanged = operations.buildFolderPlan(inventory, ['child'], {
  ...options, find: '', prefix: '', suffix: '', destinationId: 'same',
});
assert.equal(unchanged.changes[0].new_parent_id, 'a');
assert.throws(() => operations.buildFolderPlan(inventory, ['a', 'a'], options), /distinct/);
assert.throws(() => operations.buildFolderPlan(inventory, ['unknown'], options), /visible/);
assert.throws(() => operations.buildFolderPlan(inventory, ['a'],
  { ...options, destinationId: 'not-found' }), /not visible/);
assert.throws(() => operations.buildFolderPlan(inventory, ['a'],
  { ...options, prefix: '../' }), /Invalid portable/);
assert.throws(() => operations.buildFolderPlan(inventory, ['a'],
  { ...options, prefix: 'CON:' }), /Invalid portable/);
assert.throws(() => operations.buildFolderPlan(inventory, ['a'],
  { ...options, find: 'Alpha', replace: 'NUL', prefix: '', suffix: '', destinationId: 'same' }), /Invalid portable/);
const skipped = operations.parseFolderInventory({
  ...base, directories: [...base.directories,
    { id: 'bad', name: '../secret', parent_id: null, trashed_at: null },
    { id: 'orphan', name: 'lost', parent_id: 'unknown', trashed_at: null }],
});
assert.equal(skipped.folders.length, 4);
assert.ok(skipped.skipped > 0);
assert.throws(() => operations.parseFolderInventory({ ...base, revision: 'bad' }), /revision/);
const valid = operations.parseFolderPreview({
  revision: plan.revision, ready: true, count: 1, problems: [],
}, plan);
assert.ok(valid.ready);
assert.throws(() => operations.parseFolderPreview({
  revision: plan.revision + 1, ready: true, count: 1, problems: [],
}, plan), /stale/);
assert.throws(() => operations.parseFolderPreview({
  revision: plan.revision, ready: true, count: 99, problems: [],
}, plan), /stale/);
const outcome = operations.parseFolderApplied({
  changed: 1, revision: plan.revision + 1, dirty: true,
}, plan);
assert.equal(outcome.changed, 1);
assert.throws(() => operations.parseFolderApplied({
  changed: 1, revision: plan.revision, dirty: true,
}, plan), /unexpected/);
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveFolderOperations.tsx'), 'utf8');
const settings = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
const rust = fs.readFileSync(path.join(root,
  'app/src-tauri/src/commands/drive_folder_operations.rs'), 'utf8');
const mod = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/mod.rs'), 'utf8');
const lib = fs.readFileSync(path.join(root, 'app/src-tauri/src/lib.rs'), 'utf8');
for (const term of ['cmd_drive_folder_preview', 'cmd_drive_folder_apply', 'cmd_drive_sync_metadata',
  'canApply', 'uncertain', 'aria-label', 'Checking cloud metadata sync']) {
  assert.ok(ui.includes(term), 'UI requires ' + term);
}
assert.match(settings, /<DriveFolderOperations active=\{isOpen && activeTab === 'general'\} \/>/);
assert.match(mod, /pub mod drive_folder_operations;/);
assert.match(lib, /commands::cmd_drive_folder_preview,/);
assert.match(lib, /commands::cmd_drive_folder_apply,/);
for (const term of ['SAVEPOINT drive_folder_batch', 'ROLLBACK TO drive_folder_batch',
  'rename_drive_directory', 'is_ancestor(', 'portable_name_key', 'fn later_failure_rolls_back']) {
  assert.ok(rust.includes(term), 'Rust folder planner requires ' + term);
}
console.log('PASS Folder Operations: TS plan, real folder ancestry, invalid names, native result parsing and UX/IPC registration');
