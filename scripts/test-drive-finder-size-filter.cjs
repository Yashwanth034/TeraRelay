'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname,'..');
const ts = require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const exportsObj={};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(
  path.join(root,'app/src/driveFinderSizeFilter.ts'),'utf8'),{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
}).outputText,{exports:exportsObj});
const filter=exportsObj.filterDriveFinderBySize;
const row=(id,kind,size)=>Object.freeze({id,kind,name:id,path:'/'+id,size});
const mib=1024*1024;
const rows=Object.freeze([
  row('directory','folder',Number.MAX_SAFE_INTEGER),
  row('missing','file',null),
  row('invalid','pending',-2),
  row('empty','file',0),
  row('tiny','file',9*mib),
  row('ten','file',10*mib),
  row('huge','file',100*mib),
  row('gig','file',1024*mib),
  row('pending-large','pending',2048*mib),
  row('fractional','file',100*mib+0.5),
]);
const ids=arr=>Array.from(arr,x=>x.id);
assert.deepEqual(ids(filter(rows,'all')),ids(rows),'all includes folders/unknown sizes');
assert.deepEqual(ids(filter(rows,'empty')),['empty']);
assert.deepEqual(ids(filter(rows,'10m')),['ten','huge','gig','pending-large']);
assert.deepEqual(ids(filter(rows,'100m')),['huge','gig','pending-large']);
assert.deepEqual(ids(filter(rows,'1g')),['gig','pending-large']);
assert.equal(rows[0].size,Number.MAX_SAFE_INTEGER,'filters never mutate source rows');
const many=Array.from({length:25000},(_,i)=>row('id-'+i,'file',i*mib));
assert.equal(filter(many,'100m').length,24900);
assert.equal(filter(many,'1g').length,25000-1024);
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.match(settings,/aria-label="Filter local Drive finder by file size"/);
assert.match(settings,/filterDriveFinderBySize\(filterDriveFinderEntries/);
assert.match(settings,/setDriveFinderSizeFilter\(event\.target\.value as DriveFinderSizeFilter\);[\s\S]*?setDriveFinderVisibleLimit\(50\);/);
assert.match(settings,/<option value="empty">Empty files \(0 bytes\)<\/option>/);
assert.match(settings,/driveFinderMatches\)/,
  'CSV copy must use the same filtered and sorted current matches');
assert.match(settings,/summarizeDriveStorage\(driveFinderEntries\)/,
  'storage overview must not change with Finder search or size threshold');
console.log('PASS local Drive finder size thresholds: 0 B, 10 MiB, 100 MiB, 1 GiB, unknown safety, 25k rows');
