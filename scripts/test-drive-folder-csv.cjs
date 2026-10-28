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
const { buildFolderCsvPlan, parseFolderInventory, parseFolderPreview } = load('driveFolderOperations');
const data = {
  revision: 47, dirty: false,
  directories: [
    { id: 'a', name: 'Alpha', parent_id: null, trashed_at: null },
    { id: 'docs', name: 'Docs', parent_id: 'a', trashed_at: null },
    { id: 'b', name: 'Beta', parent_id: null, trashed_at: null },
    { id: 'c', name: 'Gamma', parent_id: null, trashed_at: null },
    { id: 'comma', name: 'A,B', parent_id: null, trashed_at: null },
  ],
  files: [], pending: [], symlinks: [],
};
const inventory = parseFolderInventory(data);
const plan = buildFolderCsvPlan(inventory,
  'source_path,new_path\r\n/Alpha,/Beta/AlphaRenamed\r\n/Gamma,/Beta/Gamma2\r\n');
assert.equal(plan.revision, 47);
assert.equal(plan.changes.length, 2);
assert.deepEqual([plan.changes[0].folder_id, plan.changes[0].new_name, plan.changes[0].new_parent_id],
  ['a', 'AlphaRenamed', 'b']);
assert.equal(plan.changes[1].folder_id, 'c');
assert.equal(plan.changes[1].new_parent_id, 'b');
assert.equal(parseFolderPreview({
  revision: 47, count: 2, ready: true, problems: [],
}, plan).ready, true);
const quoted = buildFolderCsvPlan(inventory,
  '\uFEFFsource_path,new_path\n"/A,B","/Beta/New,B"');
assert.equal(quoted.changes[0].new_name, 'New,B');
const bad = (mapping, regex) => assert.throws(() => buildFolderCsvPlan(inventory, mapping), regex);
bad('source_path,new_path\n/Alpha,/Beta/X\n/Alpha,/Gamma/Y', /duplicate source/i);
bad('source_path,new_path\n/Alpha,/Beta/X\n/Gamma,/Beta/X', /repeated target/i);
bad('source_path,new_path\n/Alpha,/Beta/X\n/Gamma,/Beta/x', /repeated target/i);
bad('source_path,new_path\n/Alpha,/Missing/X', /parent must already exist/i);
bad('source_path,new_path\n/Alpha,/Alpha', /no folder change/i);
bad('source_path,new_path\n/Missing,/Beta/X', /source is not in/i);
bad('source_path,new_path\n/Alpha,/Beta/CON', /unsafe destination/i);
bad('source_path,new_path\n/Alpha,/Beta/../X', /parent must already exist/i);
bad('source_path,new_path\n/Alpha,/Beta/X\n/Docs', /exactly two columns/i);
bad('source_path,new_path,extra\n/Alpha,/Beta/X', /exactly two columns/i);
bad('source_path,new_path\n"/Alpha,/Beta/X', /unterminated/i);
bad('source_path,new_path\n"/Alpha"oops,/Beta/X', /malformed/i);
bad('source_path,new_path\n"/A""B",/Beta/X', /source is not in/i);
bad('source_path,new_path\n/Alpha,/Beta/X\n/Gamma,', /full absolute folder paths/i);
bad('wrong_path,new_path\n/Alpha,/Beta/X', /header/i);
bad('source_path,new_path\n', /include 1/i);
bad('source_path,new_path\n/Alpha,/Beta/X\n'.repeat(51),
  /header|at most 100|duplicate/i);
assert.throws(() => buildFolderCsvPlan(inventory, 'x'.repeat(128 * 1024 + 1)), /128 KiB/i);
const tooMany = 'source_path,new_path\n'
  + Array.from({ length: 101 }, (_, i) => '/Folder' + i + ',/Beta/New' + i).join('\n');
bad(tooMany, /at most 100/i);
const duplicateInventory = { ...inventory, folders: [...inventory.folders,
  { id: 'other-alpha', name: 'Alpha', path: '/Alpha', parentId: null },
  { id: 'other-beta', name: 'Beta', path: '/Beta', parentId: null }] };
assert.throws(() => buildFolderCsvPlan(duplicateInventory,
  'source_path,new_path\n/Alpha,/Gamma/X'), /ambiguous source/i);
assert.throws(() => buildFolderCsvPlan(duplicateInventory,
  'source_path,new_path\n/Gamma,/Beta/X'), /ambiguous target parent/i);
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveFolderOperations.tsx'), 'utf8');
for (const term of ['buildFolderCsvPlan', 'csvMode', 'csvText', 'Folder move and rename CSV mappings',
  'onClick={check}', 'canApply', 'cmd_drive_folder_preview', 'cmd_drive_folder_apply']) {
  assert.ok(ui.includes(term), 'CSV integration missing: ' + term);
}
assert.ok(ui.includes('disabled={!!busy || !plan}'),
  'CSV preview must not depend on checkbox selections');
console.log('PASS Drive Folder CSV: exact paths, CSV quoting, duplicate guards, portability, native preview and UI wiring');
