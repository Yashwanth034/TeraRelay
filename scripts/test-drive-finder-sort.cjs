'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root,'app')] }));
const mod = {};
vm.runInNewContext(ts.transpileModule(
  fs.readFileSync(path.join(root,'app/src/driveFinderSort.ts'),'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, { exports: mod });
const entry = (id, kind, size, pathValue) => Object.freeze({
  id, kind, name: pathValue.split('/').pop(), size, path:pathValue,
});
const input = Object.freeze([
  entry('folder','folder',Number.MAX_SAFE_INTEGER,'/A-folder'),
  entry('missing','file',null,'/Missing.bin'),
  entry('small','file',2048,'/Small.bin'),
  entry('equal','pending',2048,'/Writing.bin'),
  entry('big','file',Number.MAX_SAFE_INTEGER,'/Big.bin'),
  entry('bad','file',-1,'/Bad.bin'),
  entry('zero','file',0,'/Zero.bin'),
  entry('infinite','pending',Number.POSITIVE_INFINITY,'/Corrupt.bin'),
]);
const ids = rows => Array.from(rows,r=>r.id);
assert.deepEqual(ids(mod.sortDriveFinderEntries(input,'largest')),
  ['big','small','equal','zero','folder','missing','bad','infinite']);
assert.deepEqual(ids(mod.sortDriveFinderEntries(input,'smallest')),
  ['zero','small','equal','big','folder','missing','bad','infinite']);
assert.deepEqual(ids(mod.sortDriveFinderEntries(input,'path')),
  ['folder','bad','big','infinite','missing','small','equal','zero']);
assert.deepEqual(ids(input),['folder','missing','small','equal','big','bad','zero','infinite'],
  'sort must never mutate native snapshot or finder rows');
const huge = Array.from({length:5000},(_,i)=>entry('id-'+i,'file',i,'/File-'+i));
const byBiggest = mod.sortDriveFinderEntries(huge,'largest');
assert.equal(byBiggest[0].size,4999);
assert.equal(byBiggest.at(-1).size,0);
assert.equal(huge[0].size,0);
const settings = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.match(settings,/aria-label="Sort local Drive search results"/);
assert.match(settings,/<option value="largest">Largest first<\/option>/);
assert.match(settings,/<option value="smallest">Smallest first<\/option>/);
assert.match(settings,/sortDriveFinderEntries\(filterDriveFinderBySize\(filterDriveFinderEntries/,
  'size filtering precedes stable path and largest/smallest presentation sorting');
assert.match(settings,/setDriveFinderSort\(event\.target\.value as DriveFinderSortOrder\);\s*setDriveFinderVisibleLimit\(50\);/,
  'sort changes must reset finder result pagination to the first 50');
console.log('PASS active Drive Finder sort: largest/smallest, unknown last, stable ties, 5k rows, and UI selector');
