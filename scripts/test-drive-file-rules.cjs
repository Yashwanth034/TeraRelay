'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const base = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(base, 'app')] }));
const cache = new Map();
function load(name) {
  const file = path.resolve(base, 'app/src', name + '.ts');
  if (cache.has(file)) return cache.get(file);
  const module = { exports: {} };
  cache.set(file, module.exports);
  const transpiled = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText;
  const localRequire = (other) => {
    if (!other.startsWith('./')) throw Error('Unexpected external fixture dependency: ' + other);
    return load(path.normalize(path.join(path.dirname(name), other)));
  };
  vm.runInNewContext('(function(require,module,exports){' + transpiled + '\n})',
    { console, TextEncoder })(localRequire, module, module.exports);
  return module.exports;
}
const organizer = load('driveBulkOrganizer');
const snapshot = organizer.readOrganizerSnapshot({
  revision: 45, dirty: false,
  directories: [
    { id: 'incoming', parent_id: null, name: 'Incoming', trashed_at: null },
    { id: 'nested', parent_id: 'incoming', name: 'Nested', trashed_at: null },
    { id: 'archive', parent_id: null, name: 'Archive', trashed_at: null },
    { id: 'photos', parent_id: null, name: 'Photos', trashed_at: null },
  ],
  files: [
    { entry_id: 'root', display_name: 'INVOICE.PDF', directory_id: null, total_size: 6 },
    { entry_id: 'pdf', display_name: 'report.pdf', directory_id: 'incoming', total_size: 3 },
    { entry_id: 'nested-pdf', display_name: 'notes.pdf', directory_id: 'nested', total_size: 4 },
    { entry_id: 'image', display_name: 'vacation.JPG', directory_id: 'incoming', total_size: 5 },
    { entry_id: 'text', display_name: 'readme.txt', directory_id: null, total_size: 5 },
  ],
  pending: [], symlinks: [],
});
const rule = (patch={}) => ({
  extension: '.pdf', nameContains: '', sourceFolderPath: '',
  destinationId: 'archive', prefix: '', suffix: '',
  ...patch,
});
const plan = organizer.buildOrganizerRulesRequest(snapshot, [
  rule({ sourceFolderPath: '/Incoming', suffix: '-archived' }),
  rule({ extension: '.jpg', destinationId: 'photos', prefix: '2026-' }),
]);
assert.equal(plan.revision, 45);
assert.deepEqual(Array.from(plan.changes, x =>
  [x.entry_id, x.new_directory_id, x.new_name]), [
  ['pdf', 'archive', 'report-archived.pdf'],
  ['nested-pdf', 'archive', 'notes-archived.pdf'],
  ['image', 'photos', '2026-vacation.JPG'],
]);
const root = organizer.buildOrganizerRulesRequest(snapshot, [
  rule({ sourceFolderPath: '/', nameContains: 'invoice', destinationId: 'archive' }),
]);
assert.deepEqual(Array.from(root.changes, x => x.entry_id), ['root']);
const name = organizer.buildOrganizerRulesRequest(snapshot, [
  rule({ extension: '', nameContains: 'rEaDMe', destinationId: 'archive' }),
]);
assert.equal(name.changes[0].entry_id, 'text');
const firstWins = organizer.buildOrganizerRulesRequest(snapshot, [
  rule({ sourceFolderPath: '/', destinationId: 'same' }),
  rule({ nameContains: 'invoice', destinationId: 'archive' }),
  rule({ sourceFolderPath: '/Incoming', destinationId: 'archive' }),
]);
assert.deepEqual(Array.from(firstWins.changes, x => x.entry_id), ['pdf','nested-pdf']);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot, []), /1.20/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot, Array(21).fill(rule())), /1.20/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot, [rule({extension: 'pdf'})]), /extension/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot, [rule({extension: '.pdf;.exe'})]), /extension/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({extension:'',nameContains:'',sourceFolderPath:''})]), /enter an extension/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({destinationId:'missing'})]), /destination folder/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({sourceFolderPath:'/missing'})]), /source folder/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({prefix:'../'})]), /unsafe/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({extension:'.zzz'})]), /would not change any/i);
assert.throws(() => organizer.buildOrganizerRulesRequest(snapshot,
  [rule({sourceFolderPath:'',destinationId:'same'})]), /would not change any/i);
const duplicateSnapshot = {
  ...snapshot, files: [
    ...snapshot.files, {
      id:'dup', name:'report.pdf', directoryId:'nested',
      path:'/Incoming/Nested/report.pdf',
    },
  ],
};
assert.throws(() => organizer.buildOrganizerRulesRequest(duplicateSnapshot,
  [rule({sourceFolderPath:'/Incoming'})]), /same target path/i);
const huge = {
  ...snapshot,
  files: Array.from({length:101}, (_,n) => ({
    id:'f' + n, name:'test'+n+'.pdf', directoryId:null,path:'/test'+n+'.pdf',
  })),
};
assert.throws(() => organizer.buildOrganizerRulesRequest(huge,
  [rule({destinationId:'archive'})]), /more than 100/i);
const ui = fs.readFileSync(path.join(base,
 'app/src/components/desktop/dashboard/DriveBulkOrganizer.tsx'), 'utf8');
for (const marker of [
  'buildOrganizerRulesRequest', 'rulesMode', 'setRules', 'Move matching files into',
  'The first matching rule wins.', 'Preview {request?.changes.length ?? 0}',
  'cmd_drive_bulk_preview','cmd_drive_bulk_apply','previewReady',
  'disabled={busy !== null || !request}', 'setPreview(null)',
  'buildOrganizerCsvRequest', 'csvMode',
]) {
  assert.ok(ui.includes(marker), 'Missing rule planning / native preview gate: ' + marker);
}
console.log('PASS Drive rule organizer: first-match routing, nested folders, case folding, exact root filter, collisions, malformed rules, 100-file limit, native preview-gated UI.');
