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
    if (!name.startsWith('./')) throw new Error('Unexpected dependency: ' + name);
    return load(path.normalize(path.join(path.dirname(file), name)));
  };
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})', { console })(
    localRequire, module, module.exports);
  return module.exports;
}
const explorer = load('driveDuplicateExplorer');
const a = 'a'.repeat(64);
const b = 'b'.repeat(64);
const file = (id, directory_id, file_id, content_fingerprint, overrides = {}) => ({
  entry_id: id, directory_id, file_id, display_name: id + '.txt',
  plaintext_size: 10, total_size: 10, encryption_version: 1, crypto_id: 'key-1',
  storage_version: 2, content_fingerprint, ...overrides,
});
const snapshot = {
  revision: 8, dirty: false,
  directories: [{ id: 'work', parent_id: null, name: 'Work', trashed_at: null },
    { id: 'archive', parent_id: null, name: 'Archive', trashed_at: null }],
  pending: [],
  files: [
    file('one', 'work', 'remote-a', a),
    file('two', 'archive', 'remote-a', a),
    file('three', null, 'remote-b', b),
    file('four', 'work', 'remote-c', b),
    file('wrong-key', null, 'remote-d', a, { crypto_id: 'key-2' }),
    file('unknown', null, 'remote-e', null),
    file('wrong-size', null, 'remote-f', b, { total_size: 100, plaintext_size: 100 }),
  ],
};
const report = explorer.inspectDriveDuplicates(snapshot);
assert.equal(report.scanned, 7);
assert.equal(report.eligible, 6);
assert.equal(report.groups.length, 2);
const shared = report.groups.find(g => g.sharedObject);
const separate = report.groups.find(g => !g.sharedObject);
assert.ok(shared);
assert.ok(separate);
assert.equal(shared.remoteObjects, 1);
assert.equal(shared.logicalExtraBytes, 10);
assert.equal(shared.files[0].path, '/Archive/two.txt');
assert.equal(shared.files[1].path, '/Work/one.txt');
assert.equal(separate.remoteObjects, 2);
assert.equal(separate.logicalExtraBytes, 10);
assert.equal(report.excluded, 1);
assert.equal(report.excluded + report.eligible, report.scanned);
const namespaceCollision = explorer.inspectDriveDuplicates({
  ...snapshot, files: [
    file('v1', null, 'same-opaque-id', a, { storage_version: 1 }),
    file('v2', 'work', 'same-opaque-id', a, { storage_version: 2 }),
  ],
});
assert.equal(namespaceCollision.groups.length, 1);
assert.equal(namespaceCollision.groups[0].remoteObjects, 2,
  'storage-version namespaces must never conflate the same opaque file ID');
assert.equal(namespaceCollision.groups[0].sharedObject, false);
assert.equal(explorer.filterDuplicateGroups(report.groups, '', 'shared', 'impact').length, 1);
assert.equal(explorer.filterDuplicateGroups(report.groups, '', 'separate', 'impact').length, 1);
assert.equal(explorer.filterDuplicateGroups(report.groups, 'archive', 'all', 'impact').length, 1);
assert.equal(explorer.filterDuplicateGroups(report.groups, 'irrelevant', 'all', 'impact').length, 0);
assert.deepEqual(Array.from(explorer.filterDuplicateGroups(report.groups, '', 'all', 'count'), g => g.files.length), [2, 2]);
assert.throws(() => explorer.inspectDriveDuplicates({ ...snapshot, revision: 'bad' }), /invalid/);
assert.equal(explorer.inspectDriveDuplicates({ ...snapshot, files: snapshot.files.map(
  f => ({ ...f, content_fingerprint: null }),
) }).groups.length, 0, 'never infer duplicates from names or size');
const withBadRows = explorer.inspectDriveDuplicates({
  ...snapshot,
  files: [...snapshot.files,
    file('two', 'archive', 'remote-z', a),
    file('bad-id/../', null, 'remote-z', a),
    file('untrusted', null, 'remote-z', a, { encryption_version: 1, crypto_id: null }),
    file('name-with-invalid-parent', 'missing-folder', 'remote-z', a),
    file('bad-fingerprint', null, 'remote-z', 'ab12'),
    file('huge', null, 'remote-z', a, { total_size: 1e30, plaintext_size: 1e30 }),
  ],
});
assert.ok(withBadRows.excluded >= 7);
assert.equal(withBadRows.excluded + withBadRows.eligible, withBadRows.scanned,
  'each scanned file is accounted for exactly once');
assert.ok(withBadRows.groups.every(g => g.files.every(f => !f.path.includes('bad-id/../'))));
const csv = explorer.duplicateReviewCsv(report.groups);
assert.ok(csv.includes('/Work/one.txt'));
assert.ok(csv.includes('Shared remote object'));
assert.ok(csv.includes('Inspect manually'));
assert.ok(!csv.includes('delete file'));
const malicious = { ...shared, files: [{ ...shared.files[0], path: '=HYPERLINK("evil")' }] };
assert.ok(explorer.duplicateReviewCsv([malicious]).includes('"\'=HYPERLINK(""evil"")"'));
const component = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveDuplicateExplorer.tsx'), 'utf8');
for (const word of ['cmd_drive_get_snapshot', 'inspectDriveDuplicates', 'duplicateReviewCsv',
  'navigator.clipboard.writeText', 'matching-content groups', 'No matching-content groups',
  'Deleting one name may not save any remote space']) {
  assert.ok(component.includes(word), 'UI missing '+word);
}
assert.ok(!component.includes('cmd_drive_delete'), 'no destructive API allowed');
assert.ok(fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8').includes('<DriveDuplicateExplorer active='), 'Settings must integrate Duplicate Explorer');
console.log('PASS Drive Duplicate Explorer: real snapshot validation, encryption boundaries, shared vs distinct objects, safe CSV and read-only UI');
