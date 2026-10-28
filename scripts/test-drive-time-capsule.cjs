'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const filename=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(filename))return cache.get(filename);
 const module={exports:{}};cache.set(filename,module.exports);
 const compiled=ts.transpileModule(fs.readFileSync(filename,'utf8'),
  {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
 vm.runInNewContext('(function(require,module,exports){'+compiled+'\n})',{console,TextEncoder})
   (specifier=>{
     if(!specifier.startsWith('./'))throw Error('Unexpected dependency '+specifier);
     return load(path.normalize(path.join(path.dirname(name),specifier)));
   },module,module.exports);
 return module.exports;
}
const m=load('driveTimeCapsule');
const a={seq:1,captured_at:1700000000000,revision:11,dirty:false,item_count:3,
 fingerprint:'a'.repeat(64)};
const b={...a,seq:2,revision:12,captured_at:1700000000100,dirty:true,
 fingerprint:'b'.repeat(64)};
assert.equal(m.readCapsuleInfo(a).item_count,3);
assert.equal(m.readCapsuleHistory([b,a]).length,2);
assert.equal(m.readCapsuleHistory([]).length,0);
assert.throws(()=>m.readCapsuleHistory([a,b]),/Unsorted/i);
assert.throws(()=>m.readCapsuleHistory([a,a]),/duplicated/i);
assert.throws(()=>m.readCapsuleHistory(Array(13).fill(a)),/Invalid/i);
assert.throws(()=>m.readCapsuleInfo({...a,fingerprint:'oops'}),/Invalid/i);
assert.throws(()=>m.readCapsuleInfo({...a,dirty:'no'}),/Invalid/i);
assert.throws(()=>m.readCapsuleInfo({...a,item_count:6000}),/Invalid/i);
const diff={
 before:a,after:b,
 created:1,removed:1,moved:1,renamed:0,ancestor_path_changed:0,
 backing_state_changed:0,availability_changed:1,content_metadata_changed:0,
 changes:[
  {change:'created',kind:'file',old_path:null,new_path:'/New/file.txt',
   previous_backing:null,current_backing:'v2'},
  {change:'removed',kind:'folder',old_path:'/Gone',new_path:null,
   previous_backing:'metadata',current_backing:null},
  {change:'moved',kind:'file',old_path:'/Old/report.pdf',new_path:'/New/report.pdf',
   previous_backing:'v2',current_backing:'v2'},
  {change:'availability_changed',kind:'file',old_path:'/New/offline.pdf',new_path:'/New/offline.pdf',
   previous_backing:'v2',current_backing:'v2_unavailable'},
 ]};
assert.equal(m.readCapsuleComparison(diff,1,2).changes.length,4);
assert.throws(()=>m.readCapsuleComparison(diff,2,1),/stale|corrupt/i);
assert.throws(()=>m.readCapsuleComparison({...diff,created:2},1,2),/stale|corrupt/i);
assert.throws(()=>m.readCapsuleComparison({...diff,
 changes:[{...diff.changes[0],change:'deleted_forever'},...diff.changes.slice(1)]},1,2),/stale|corrupt/i);
assert.throws(()=>m.readCapsuleComparison({...diff,
 changes:[{...diff.changes[0],new_path:'../../outside'},...diff.changes.slice(1)]},1,2),/stale|corrupt/i);
assert.ok(m.snapshotCaption(b).includes('unsynced locally'));

const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_time_capsule.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveTimeCapsule.tsx'),'utf8');
const registry=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const modules=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const name of ['cmd_drive_time_capsule_list','cmd_drive_time_capsule_capture',
                    'cmd_drive_time_capsule_compare']){
 assert.equal(registry.split('commands::'+name+',').length-1,1,name+' should register once');
 assert.ok(ui.includes(name),'UI invoke '+name);
}
for(const item of [
 'drive_local_time_capsules','SAVEPOINT drive_time_capsule_capture',
 'ROLLBACK TO drive_time_capsule_capture',
 'LEFT JOIN logical_files','LEFT JOIN drive_objects',
 'Raw active records include completed, unavailable, or shadowed content',
 'fn compare(','captures_raw_unavailable_backing_without_false_deletion',
 'retains_only_twelve_capsules_and_compares_saved_pair',
 'authoritative_v2_keeps_identity_without_dual_counting',
]) assert.ok(rust.includes(item),'Missing native history safety contract '+item);
assert.ok(/const\s+MAX_HISTORY\s*:\s*usize\s*=\s*12\s*;/.test(rust),'Retain exactly 12 Drive history snapshots');
assert.ok(modules.includes('pub mod drive_time_capsule;'));
assert.ok(modules.includes('pub use drive_time_capsule::*;'));
assert.equal(settings.split("<DriveTimeCapsule active={isOpen && activeTab === 'general'} />").length-1,1);
assert.ok(ui.includes('not file backups')||ui.includes('not file backups,'));
console.log('PASS Drive Time Capsule: bounded private snapshots, raw/hidden availability, SHA256 integrity, strict history comparison and UI IPC contract');
