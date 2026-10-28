'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const file=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(file))return cache.get(file);
 const m={exports:{}};cache.set(file,m.exports);
 const js=ts.transpileModule(fs.readFileSync(file,'utf8'),{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+js+'\n})',{
  console,TextEncoder
 })(specifier=>{
  if(!specifier.startsWith('./'))throw Error('Unexpected source module');
  return load(path.normalize(path.join(path.dirname(name),specifier)));
 },m,m.exports);
 return m.exports;
}
const api=load('driveFolderTreeTrash');
const snap={revision:50,dirty:false,directories:[
 {id:'root',name:'Projects',parent_id:null,trashed_at:null},
 {id:'inner',name:'Docs',parent_id:'root',trashed_at:null},
 {id:'separate',name:'Backup',parent_id:null,trashed_at:null},
],files:[],pending:[],symlinks:[]};
const inventory=api.readTreeTrashInventory(snap);
const trash=api.trashTreeRequest(inventory,'root');
assert.equal(trash.revision,50);
assert.equal(trash.root_id,'root');
assert.throws(()=>api.trashTreeRequest(inventory,'bad'),/no longer active/i);
const p=api.readTreePreview({
 revision:50,ready:true,folders:2,files:1,logical_bytes:14,
 examples:['Projects/','Docs/','notes.txt'],problems:[]
},trash);
assert.equal(p.files,1);
const done=api.readTreeApplied({
 revision:53,dirty:true,folders:2,files:1,
},trash,p);
assert.equal(done.folders,2);
assert.throws(()=>api.readTreeApplied({
 revision:50,dirty:true,folders:2,files:1,
},trash,p),/uncertain/i);
assert.throws(()=>api.readTreePreview({
 revision:51,ready:true,folders:2,files:1,logical_bytes:14,examples:[],problems:[]
},trash),/stale/i);
assert.throws(()=>api.readTreePreview({
 revision:50,ready:false,folders:2,files:0,logical_bytes:0,examples:[],problems:['bad']
},trash),/invalid/i);
const runs=api.readTreeTrashRuns({
 revision:53,runs:[{root_id:'root',name:'Projects',folder_count:2,file_count:1,created_at:42}]
});
const restore=api.restoreTreeRequest(runs,'root');
assert.equal(restore.revision,53);
assert.throws(()=>api.restoreTreeRequest(runs,'deleted'),/no longer exists/i);
assert.throws(()=>api.readTreeTrashRuns({
 revision:53,runs:[{root_id:'root',name:'Projects',folder_count:2,file_count:1,created_at:42},
                   {root_id:'root',name:'Projects',folder_count:2,file_count:1,created_at:42}]
}),/invalid/i);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_tree_trash.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderTreeTrash.tsx'),'utf8');
for(const marker of [
 'SAVEPOINT drive_tree_trash_atomic','SAVEPOINT drive_tree_restore_atomic',
 'drive_tree_trash_members','drive_tree_trash_runs',
 'trash_drive_directory','trash_drive_file','restore_drive_trash_item',
 'finish_savepoint','journal_count','complete_tree_moves_to_trash_and_exact_restore_is_atomic',
 'failed_mid_restore_rolls_back_root_recovery_and_preserves_journal',
 'authoritative_v2_content_is_trashed_and_restored_not_legacy_fallback'
])assert.ok(native.includes(marker),'Missing essential native guard/test: '+marker);
for(const marker of [
 'cmd_drive_folder_tree_trash_preview','cmd_drive_folder_tree_trash_apply',
 'cmd_drive_folder_tree_restore_preview','cmd_drive_folder_tree_restore_apply',
 'cmd_drive_folder_tree_trash_runs','readTreeApplied',
 'validateDriveSyncResult','never auto-retry'
])assert.ok(ui.includes(marker),'Missing frontend contract: '+marker);
console.log('PASS Recursive Drive Folder Trash & Complete Recovery: strict plans, durable journal, atomic preview/apply, original-path-only recovery');
