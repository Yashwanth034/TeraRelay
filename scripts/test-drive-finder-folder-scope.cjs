'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const out={};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(
path.join(root,'app/src/driveFinderFolderScope.ts'),'utf8'),{
compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}
}).outputText,{exports:out});
const folder=(id,pathValue)=>Object.freeze({id,kind:'folder',name:pathValue.split('/').at(-1),path:pathValue,size:null});
const file=(id,pathValue,kind='file')=>Object.freeze({id,kind,name:pathValue.split('/').at(-1),path:pathValue,size:10});
const entries=Object.freeze([
 folder('work','/Work'),
 folder('team','/Work/Team'),
 file('one','/Work/report.txt'),
 file('two','/Work/Team/notes.txt'),
 file('pending','/Work/Team/unsent.part','pending'),
 folder('spoof','/WorkCopy'),
 file('three','/WorkCopy/other.txt'),
 folder('top','/Pictures'),
 file('four','/Pictures/image.jpg'),
 file('bad','/Work/../private'),
 file('bad2','/Work//double.txt'),
 file('bad3','/Work/\u2028separator'),
]);
const ids=rows=>Array.from(rows,x=>x.id);
const scope=out.filterDriveFinderWithinFolder;
assert.deepEqual(ids(scope(entries,'')),ids(entries),'root remains full user index');
assert.deepEqual(ids(scope(entries,'/Work')),['team','one','two','pending'],
  'folder scope includes descendants but not selected folder itself');
assert.deepEqual(ids(scope(entries,'/Work/Team')),['two','pending']);
assert.deepEqual(ids(scope(entries,'/WorkCopy')),['three']);
assert.deepEqual(ids(scope(entries,'/Pictures')),['four']);
assert.deepEqual(ids(scope(entries,'/NotFound')),[]);
for(const invalid of ['/Work/','/Work//Team','/Work/../private','/Work/./Team','Work',
'/Work\\Team','/Work\u0000','/Work\u2029', '/', '//']){
 assert.equal(scope(entries,invalid).length,0,
  'invalid scoping path must not degrade into global lookup: '+JSON.stringify(invalid));
}
assert.equal(out.parentDriveFinderFolder('/Work/Team'),' /Work'.trim());
assert.equal(out.parentDriveFinderFolder('/Work'),'');
assert.equal(out.parentDriveFinderFolder(''),'');
assert.equal(out.parentDriveFinderFolder('/Work/../Team'),'');
assert.equal(out.parentDriveFinderFolder('/Work/Team/Leaf'),'/Work/Team');
assert.equal(entries.length,12,'snapshot stays intact');
const source=fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.match(source,/filterDriveFinderWithinFolder\(driveFinderEntries, driveFinderFolderScope\)/,
  'folder scope applied before kind and text search');
assert.match(source,/aria-label=\{`Browse this Drive folder: \$\{entry\.name\}`\}/);
assert.match(source,/aria-label="Go up one Drive folder"/);
assert.match(source,/aria-label="Show all Drive folders"/);
assert.match(source,/setDriveFinderFolderScope\(entry\.path\)/);
assert.match(source,/setDriveFinderFolderScope\(parentDriveFinderFolder\(driveFinderFolderScope\)\)/);
assert.match(source,/setDriveFinderVisibleLimit\(50\)/);
assert.match(source,/formatDriveFinderCsv\(driveFinderMatches\)/,
  'copied CSV must honor scoped current search results');
assert.match(source,/summarizeDriveStorage\(driveFinderEntries\)/,
  'overall indexed storage size must remain whole-index total');
console.log('PASS Drive Finder folder navigation: safe descendant scope, collision prefix, invalid paths, parent and UI contracts');
