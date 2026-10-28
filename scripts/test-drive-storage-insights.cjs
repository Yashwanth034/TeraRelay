'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root,'app')] }));
function load(file, extra = {}) {
  const out = {};
  vm.runInNewContext(ts.transpileModule(fs.readFileSync(file,'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, { exports: out, require: name => {
    if (name in extra) return extra[name];
    throw Error('Unknown module ' + name);
  } });
  return out;
}
const finder = { DRIVE_FINDER_MAX_ROWS: 25000 };
const insights = load(path.join(root,'app/src/driveStorageInsights.ts'),
  { './driveLocalFinder': finder });
const F = (id,path) => ({ id,kind:'folder',name:path.split('/').pop(),path,size:null });
const R = (id,path,size,kind='file') => ({ id,kind,name:path.split('/').pop(),path,size });
const rows = [
  F('d','/Documents'), F('p','/Documents/Photos'), F('empty','/Empty'),
  R('one','/Documents/Photos/photo.jpg',512),
  R('two','/Documents/Photos/other.png',1024),
  R('unknown','/Documents/no-size.bin',null),
  R('root','/README.txt',0),
  R('write','/Documents/writing.txt',2048,'pending'),
  R('pending-unknown','/Documents/unknown-write.txt',null,'pending'),
  R('one','/Documents/Photos/photo.jpg',512), // duplicate ID, not counted twice
  R('forged','/Documents/../forged.txt',998),
  R('slash','/Documents//evil.txt',998),
  R('negative','/Documents/negative.txt',-4),
];
const result = insights.summarizeDriveStorage(rows);
assert.equal(result.files, 5, 'only one copy per valid unique logical file ID');
assert.equal(result.folders, 3);
assert.equal(result.pendingWrites, 2);
assert.equal(result.knownFileBytes, 1536n, 'pending bytes and duplicate IDs not added to logical file bytes');
assert.equal(result.unknownFileSizes, 2, 'unknown and malformed size count honestly');
assert.equal(result.knownPendingBytes, 2048n);
assert.equal(result.unknownPendingSizes, 1);
assert.deepEqual(Array.from(result.largestFolders, f => f.path),
  ['/Documents', '/Documents/Photos', '/Empty']);
assert.equal(result.largestFolders[0].knownBytes, 1536n);
assert.equal(result.largestFolders[0].knownFiles, 2);
assert.equal(result.largestFolders[0].unknownFiles, 2);
assert.equal(result.largestFolders[1].knownBytes, 1536n);
assert.equal(result.largestFolders[1].knownFiles, 2);
assert.equal(result.largestFolders[1].unknownFiles, 0);
assert.equal(result.largestFolders[2].knownBytes, 0n);
assert.equal(insights.summarizeDriveStorage(rows, 1).largestFolders.length, 1);
assert.equal(insights.summarizeDriveStorage(rows, -2).largestFolders.length, 0);
assert.equal(insights.summarizeDriveStorage(rows, 100).largestFolders.length, 3);
const huge = insights.summarizeDriveStorage([
  F('x','/Huge'),
  R('huge1','/Huge/one',Number.MAX_SAFE_INTEGER),
  R('huge2','/Huge/two',Number.MAX_SAFE_INTEGER),
]);
assert.equal(huge.knownFileBytes, 2n*BigInt(Number.MAX_SAFE_INTEGER),
  'exact integer totals must not overflow Number.MAX_SAFE_INTEGER');
assert.equal(huge.largestFolders[0].knownBytes, huge.knownFileBytes);
const oversized = Array.from({length:25015},(_,i)=>R('f'+i,'/Root/file-'+i+'.dat',1));
const bounded = insights.summarizeDriveStorage(oversized);
assert.equal(bounded.files,25000, 'summary processes only the locally indexed safety cap');
assert.equal(bounded.knownFileBytes,25000n);
const sorted = [...rows].sort(()=>0);
assert.equal(rows.length, sorted.length,'input not mutated');
const settings = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.match(settings,/summarizeDriveStorage\(driveFinderEntries\)/,
  'UI storage summary reuses the already validated and bounded local index');
assert.match(settings,/formatDriveTrashBytes\(driveFinderInsights\.knownFileBytes\)/);
assert.match(settings,/formatDriveTrashBytes\(driveFinderInsights\.knownPendingBytes\)/);
assert.match(settings,/largestFolders\.map/);
assert.match(settings,/logical listed file bytes/i);
assert.match(settings,/not disk usage|not physical storage|not cloud quota/i);
console.log('PASS local Drive storage insights: exact file/pending totals, folder rollups, unknown sizes, 25k cap and UI');
