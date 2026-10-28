'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
  const file=path.resolve(root,'app/src',name+'.ts');
  if(cache.has(file))return cache.get(file);
  const mod={exports:{}};cache.set(file,mod.exports);
  const source=ts.transpileModule(fs.readFileSync(file,'utf8'),{
    compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
  }).outputText;
  vm.runInNewContext('(function(require,module,exports){'+source+'\n})',{
    console,TextEncoder,
  })(specifier=>{
    if(!specifier.startsWith('./'))throw Error('Unexpected fixture dependency: '+specifier);
    return load(path.normalize(path.join(path.dirname(name),specifier)));
  },mod,mod.exports);
  return mod.exports;
}
const api=load('driveBatchTrash');
const snapshot={revision:31,dirty:false,
  directories:[{id:'docs',name:'Documents',parent_id:null,trashed_at:null}],
  files:[
    {entry_id:'one',file_id:'object-one',directory_id:'docs',display_name:'alpha.txt',
      total_size:13,storage_version:1,updated_at:1},
    {entry_id:'two',file_id:'object-two',directory_id:'docs',display_name:'beta.txt',
      total_size:17,storage_version:1,updated_at:1},
  ],
  pending:[],symlinks:[],
};
const inventory=api.readBatchTrashInventory(snapshot);
assert.equal(inventory.revision,31);
assert.equal(inventory.files.length,2);
assert.equal(inventory.files[0].storageVersion,1);
const request=api.buildBatchTrashRequest(inventory,['one','two']);
assert.equal(request.files.length,2);
assert.equal(request.files[0].expected_parent_id,'docs');
assert.equal(request.files[0].expected_storage_version,1);
assert.equal(api.readBatchTrashPreview({
  revision:31,ready:true,count:2,logical_bytes:30,problems:[],
},request).ready,true);
assert.equal(api.readBatchTrashApplied({
  revision:33,dirty:true,trashed:2,
},request).trashed,2);
assert.throws(()=>api.buildBatchTrashRequest(inventory,[]),/1.100/);
assert.throws(()=>api.buildBatchTrashRequest(inventory,['one','one']),/distinct/);
assert.throws(()=>api.buildBatchTrashRequest(inventory,['missing']),/disappeared/);
assert.throws(()=>api.readBatchTrashPreview({
  revision:30,ready:true,count:2,logical_bytes:30,problems:[],
},request),/stale/);
assert.throws(()=>api.readBatchTrashPreview({
  revision:31,ready:true,count:2,logical_bytes:30,problems:['failure'],
},request),/stale/);
assert.throws(()=>api.readBatchTrashApplied({
  revision:31,dirty:true,trashed:2,
},request),/uncertain/);
const migrated={...snapshot,files:[...snapshot.files,
  {...snapshot.files[0],storage_version:2,display_name:'newer-alpha.txt'}]};
const preferred=api.readBatchTrashInventory(migrated);
assert.equal(preferred.files.filter(f=>f.entryId==='one').length,1);
assert.equal(preferred.files.find(f=>f.entryId==='one').storageVersion,2);
assert.equal(preferred.files.find(f=>f.entryId==='one').name,'newer-alpha.txt');
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_batch_trash.rs'),'utf8');
const moduleSource=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBatchTrash.tsx'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const marker of [
  'SAVEPOINT drive_atomic_batch_trash','ROLLBACK TO drive_atomic_batch_trash',
  'trash_drive_file(conn, &file.entry_id)',
  'source_kind(conn, file)',
  'second_trash_trigger_failure_rolls_back_first_file_and_revision',
  'ignored_trash_update_cannot_claim_success',
  'pending_same_path_and_copy_on_write_base_are_rejected',
  'authoritative_v2_is_selected_and_old_legacy_shadow_is_retired',
]){
  assert.ok(rust.includes(marker),'missing native batch Trash safety '+marker);
}
assert.match(moduleSource,/pub mod drive_batch_trash;/);
assert.match(moduleSource,/pub use drive_batch_trash::\*;/);
for(const command of ['cmd_drive_batch_trash_preview','cmd_drive_batch_trash_apply']){
  assert.ok(lib.includes('commands::'+command+','));
  assert.ok(ui.includes(command));
}
for(const marker of ['Recently Deleted','No automatic retries','readBatchTrashApplied',
  'Move verified batch to Trash','validateDriveSyncResult']){
  assert.ok(ui.includes(marker),'missing UI contract '+marker);
}
assert.match(settings,/<DriveBatchTrash active=\{isOpen && activeTab === 'general'\} \/>/);
console.log('PASS Atomic Batch Trash: complete-file planner, V2 precedence, optimistic preview, transaction safety and UI wiring');
