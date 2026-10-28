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
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})', { console })(
    name => {
      if (!name.startsWith('./')) throw new Error('External test dependency ' + name);
      return load(path.normalize(path.join(path.dirname(file), name)));
    }, module, module.exports);
  return module.exports;
}
const work = load('driveCollections');
const snapshot = {
  revision: 6, dirty: true,
  directories: [
    { id: 'work', parent_id: null, name: 'Work', trashed_at: null },
    { id: 'photos', parent_id: null, name: 'Photos', trashed_at: null },
  ],
  files: [
    { entry_id: 'one', directory_id: 'work', display_name: 'draft.txt', total_size: 10 },
    { entry_id: 'two', directory_id: 'photos', display_name: 'image.png', total_size: 20 },
  ], pending: [
    { id: 'pending', directory_id: null, display_name: 'unfinished.bin', size: 15 },
  ],
};
const inventory = work.collectionInventory(snapshot);
assert.equal(inventory.files.length, 2);
assert.equal(inventory.revision, 6);
assert.deepEqual(Array.from(inventory.files, f => f.path).sort(), ['/Photos/image.png', '/Work/draft.txt']);
assert.equal(work.collectionInventory({ ...snapshot, files: [] }).files.length, 0);
assert.throws(() => work.collectionInventory({ ...snapshot, revision: NaN }), /invalid/);
const records = work.parseCollections([
  { id: 'collection-a', name: 'Project Alpha', revision: 2, entry_ids: ['one', 'missing'] },
]);
assert.equal(records.length, 1);
const resolved = work.resolveCollection(records[0], inventory);
assert.equal(resolved.available.length, 1);
assert.equal(resolved.missingCount, 1);
assert.equal(resolved.available[0].path, '/Work/draft.txt');
const create = work.buildCollectionSave(inventory, null, 'New Group', ['one', 'two']);
assert.equal(create.collectionId, null);
assert.equal(create.entryIds.length, 2);
const update = work.buildCollectionSave(inventory, records[0], 'Updated Group', ['two']);
assert.equal(update.expectedRevision, 2);
assert.equal(update.collectionId, 'collection-a');
assert.throws(() => work.buildCollectionSave(inventory, records[0], 'Invalid/Name', []), /clean/);
assert.throws(() => work.buildCollectionSave(inventory, null, ' project ', []), /clean/);
assert.throws(() => work.buildCollectionSave(inventory, null, 'Valid', ['one', 'one']), /different/);
assert.throws(() => work.buildCollectionSave(inventory, null, 'Valid', ['pending']), /missing/);
assert.throws(() => work.parseCollections([{ ...records[0], revision: -5 }]), /invalid/);
assert.throws(() => work.parseCollections([{ ...records[0], entry_ids: ['one', 'one'] }]), /invalid/);
assert.throws(() => work.parseCollections([records[0], records[0]]), /invalid/);
const csv = work.collectionMembersCsv(resolved);
assert.ok(csv.includes('/Work/draft.txt'));
assert.ok(csv.includes('1 missing references excluded'));
const malicious = { ...resolved, available: [{ id: 'form', name: 'x', path: '=CMD("bad")', size: 1 }] };
assert.ok(work.collectionMembersCsv(malicious).includes('"\'=CMD(""bad"")"'));
const ui = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/DriveCollections.tsx'), 'utf8');
for (const term of ['cmd_drive_collections_list', 'cmd_drive_collections_save',
  'cmd_drive_collections_delete', 'buildCollectionSave', 'parseCollections',
  'device only', 'Deleting a collection never deletes any Drive file',
  'Confirm remove collection only', 'navigator.clipboard.writeText']) {
  assert.ok(ui.includes(term), 'UI missing ' + term);
}
const rust = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/drive_collections.rs'), 'utf8');
for (const term of ['SAVEPOINT drive_local_collection_edit', 'ROLLBACK TO drive_local_collection_edit',
  'drive_local_collection_members', 'check_members', 'fn atomic_membership_rewrite_rolls_back_failed_second_insert',
  'cmd_drive_collections_list', 'cmd_drive_collections_save', 'cmd_drive_collections_delete']) {
  assert.ok(rust.includes(term), 'native implementation missing ' + term);
}
assert.ok(!rust.includes('delete_drive_file'), 'collection delete may never delete Drive content');
const settings = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
assert.ok(settings.includes('<DriveCollections active={isOpen && activeTab === \'general\'} />'), 'Collections must render in Settings');
const commands = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/mod.rs'), 'utf8');
const lib = fs.readFileSync(path.join(root, 'app/src-tauri/src/lib.rs'), 'utf8');
assert.ok(commands.includes('pub mod drive_collections;') && commands.includes('pub use drive_collections::*;'));
for (const name of ['cmd_drive_collections_list','cmd_drive_collections_save','cmd_drive_collections_delete']) {
  assert.ok(lib.includes('commands::' + name), 'Rust IPC missing ' + name);
}
console.log('PASS Drive Virtual Collections: real folder paths, strict membership, optimistic revisions, local-safe mutations and live UI contracts');
