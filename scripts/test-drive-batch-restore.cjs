'use strict';
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name) {
 const file=path.join(root,'app/src',name+'.ts');
 if(cache.has(file))return cache.get(file);
 const mod={exports:{}};cache.set(file,mod.exports);
 const code=ts.transpileModule(fs.readFileSync(file,'utf8'),{
   compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',
  {console,TextEncoder})(specifier=>{
   if(!specifier.startsWith('./'))throw Error('Unexpected TS fixture dependency');
   return load(path.normalize(path.join(path.dirname(name),specifier)));
  },mod,mod.exports);
 return mod.exports;
}
const m=load('driveBatchRestore');
const raw={
 revision:29,dirty:false,files:[],pending:[],symlinks:[],
 directories:[{id:'parent',name:'Existing',parent_id:null,trashed_at:null}],
};
const list=[
 {kind:'file',id:'file-one',name:'old.txt',deleted_at:1730000000000,
  original_directory_id:'parent',size:15,restorable:true},
 {kind:'file',id:'file-two',name:'notes.pdf',deleted_at:1730000000001,
  original_directory_id:null,size:45,restorable:true},
 {kind:'folder',id:'folder-one',name:'Old Project',deleted_at:1730000000002,
  original_directory_id:null,size:null,restorable:true},
 {kind:'symlink',id:'link-one',name:'Shortcut',deleted_at:1730000000003,
  original_directory_id:'parent',size:null,restorable:true},
 {kind:'file',id:'unavailable',name:'broken.bin',deleted_at:1730000000004,
  original_directory_id:null,size:10,restorable:false},
];
const inventory=m.readBatchRestoreInventory(raw,list);
assert.equal(inventory.revision,29);assert.equal(inventory.entries.length,5);
const req=m.buildBatchRestoreRequest(inventory,['file:file-one','folder:folder-one','symlink:link-one']);
assert.equal(req.items.length,3);assert.equal(req.items[0].expected_parent_id,'parent');
assert.equal(req.items[0].expected_deleted_at,1730000000000);
assert.equal(req.items[1].expected_name,'Old Project');
assert.throws(()=>m.buildBatchRestoreRequest(inventory,[]),/1.100/i);
assert.throws(()=>m.buildBatchRestoreRequest(inventory,Array(101).fill('file:file-one')),/1.100/i);
assert.throws(()=>m.buildBatchRestoreRequest(inventory,['file:file-one','file:file-one']),/distinct/i);
assert.throws(()=>m.buildBatchRestoreRequest(inventory,['file:missing']),/unavailable/i);
assert.throws(()=>m.buildBatchRestoreRequest(inventory,['file:unavailable']),/unavailable/i);
const p={revision:29,ready:true,count:3,restorable_bytes:15,
 planned:[
  {kind:'file',path:'/Existing/old.txt',size:15},
  {kind:'folder',path:'/Old Project',size:null},
  {kind:'symlink',path:'/Existing/Shortcut',size:null},
 ],problems:[]};
assert.equal(m.readBatchRestorePreview(p,req).ready,true);
assert.equal(m.readBatchRestorePreview(p,req).planned[0].path,'/Existing/old.txt');
assert.throws(()=>m.readBatchRestorePreview({...p,planned:[]},req),/Invalid or stale/i);
assert.throws(()=>m.readBatchRestorePreview({...p,revision:30},req),/Invalid or stale/i);
assert.throws(()=>m.readBatchRestorePreview({...p,count:2},req),/Invalid or stale/i);
assert.throws(()=>m.readBatchRestorePreview({...p,problems:['Collision']},req),/Invalid or stale/i);
assert.equal(m.readBatchRestoreApplied({restored:3,revision:35,dirty:true},req).restored,3);
assert.throws(()=>m.readBatchRestoreApplied({restored:2,revision:35,dirty:true},req),/uncertain/i);
assert.throws(()=>m.readBatchRestoreApplied({restored:3,revision:29,dirty:true},req),/uncertain/i);
const backend=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_batch_restore.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBatchRestore.tsx'),'utf8');
const mount=fs.readFileSync(path.join(root,'app/src-tauri/src/drive_mount.rs'),'utf8');
for(const marker of ['BatchRestoreUnmountGuard','acquire_unmounted_batch_restore',
  'self.inner.mounting.swap(true, Ordering::SeqCst)',
  'desktop::mountpoint_is_active(p)',
  'batch_restore_gate_excludes_mount_start_and_releases_on_drop']){
 assert.ok(mount.includes(marker),'Missing exclusive Linux mount safety gate: '+marker);
}
for(const marker of [
 'SAVEPOINT drive_batch_exact_restore','ROLLBACK TO drive_batch_exact_restore',
 'restore_drive_trash_item','expected_deleted_at','original parent','portable_name_key',
 'list_drive_trash','drive_snapshot','original_path','cmd_drive_batch_restore_preview',
 'cmd_drive_batch_restore_apply','acquire_unmounted_batch_restore()', 'Row {line}',
])assert.ok(backend.includes(marker),'Missing strict native recovery guard: '+marker);
for(const marker of [
 'buildBatchRestoreRequest','readBatchRestorePreview','readBatchRestoreApplied',
 'cmd_drive_list_trash','cmd_drive_get_snapshot','cmd_drive_batch_restore_preview',
 'cmd_drive_batch_restore_apply','cmd_drive_sync_metadata',
 'No automatic retries','canApply','setPreview(null)',
 'preview.data.planned',
])assert.ok(ui.includes(marker),'Missing UI native guard: '+marker);
const mod=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.ok(mod.includes('pub mod drive_batch_restore;'));
assert.ok(mod.includes('pub use drive_batch_restore::*;'));
for(const name of ['cmd_drive_batch_restore_preview','cmd_drive_batch_restore_apply']){
 assert.equal(lib.split('commands::'+name+',').length-1,1,name+' must register once');
}
assert.equal(settings.split("<DriveBatchRestore active={isOpen && activeTab === 'general'} />").length-1,1);
console.log('PASS Drive Atomic Batch Restore: strict tombstone identities, complete path previews, atomic recovery and UI/native IPC wiring');
