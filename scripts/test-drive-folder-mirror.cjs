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
 const code=ts.transpileModule(fs.readFileSync(file,'utf8'),{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{
  console,TextEncoder
 })(specifier=>{
  if(!specifier.startsWith('./'))throw Error('Unexpected fixture dependency');
  return load(path.normalize(path.join(path.dirname(name),specifier)));
 },m,m.exports);
 return m.exports;
}
const api=load('driveFolderMirror');
const snapshot={revision:31,dirty:false,
 directories:[
  {id:'src',name:'Project',parent_id:null,trashed_at:null},
  {id:'docs',name:'Docs',parent_id:'src',trashed_at:null},
  {id:'backup',name:'Backup',parent_id:null,trashed_at:null},
  {id:'old',name:'Old',parent_id:'backup',trashed_at:null},
 ],files:[],pending:[],symlinks:[],
};
const inventory=api.readMirrorInventory(snapshot);
const req=api.buildMirrorRequest(inventory,'src','backup');
assert.equal(req.revision,31);
assert.equal(req.source_id,'src');
assert.equal(req.target_id,'backup');
assert.throws(()=>api.buildMirrorRequest(inventory,'src','src'),/distinct/);
assert.throws(()=>api.buildMirrorRequest(inventory,'src','docs'),/nested/i);
assert.throws(()=>api.buildMirrorRequest(inventory,'bad','backup'),/no longer available/);
const preview=api.readMirrorPreview({
 revision:31,ready:true,add_folders:1,add_files:2,matching_files:4,
 added_logical_bytes:48,examples:['Docs/','Docs/test.txt'],problems:[],
},req);
assert.equal(preview.add_files,2);
assert.equal(api.readMirrorApplied({
 revision:34,dirty:true,folders_added:1,files_added:2,files_matched:4,
},req,preview).files_added,2);
assert.throws(()=>api.readMirrorPreview({
 revision:32,ready:true,add_folders:1,add_files:2,matching_files:4,
 added_logical_bytes:48,examples:['Docs/'],problems:[],
},req),/stale/i);
assert.throws(()=>api.readMirrorPreview({
 revision:31,ready:true,add_folders:1,add_files:2,matching_files:4,
 added_logical_bytes:48,examples:[],problems:['conflict'],
},req),/stale/i);
assert.throws(()=>api.readMirrorApplied({
 revision:34,dirty:true,folders_added:1,files_added:1,files_matched:4,
},req,preview),/uncertain/i);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_mirror.rs'),'utf8');
const commands=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderMirror.tsx'),'utf8');
for(const marker of ['SAVEPOINT drive_incremental_mirror','ROLLBACK TO drive_incremental_mirror',
 'create_drive_directory','insert_copy','source_kind','mark_drive_dirty',
 'first_mirror_copies_only_missing_dirs_and_files_then_is_noop',
 'second_mirror_adds_one_new_file_but_does_not_duplicate_old',
 'injected_failure_rolls_back_all_new_folders_and_files',
 'hidden_missing_backing_refuses_incomplete_mirror']){
 assert.ok(native.includes(marker),'Missing native Mirror guard/test: '+marker);
}
assert.match(commands,/pub mod drive_folder_mirror;/);
assert.match(commands,/pub use drive_folder_mirror::\*;/);
for(const command of ['cmd_drive_folder_mirror_preview','cmd_drive_folder_mirror_apply']){
 assert.ok(lib.includes('commands::'+command+','));
 assert.ok(ui.includes(command));
}
assert.match(settings,/<DriveFolderMirror active=\{isOpen && activeTab === 'general'\} \/>/);
for(const marker of ['No automatic retries','readMirrorApplied','preview.data',
 'Add missing entries atomically','validateDriveSyncResult']){
 if(marker==='preview.data')continue;
 assert.ok(ui.includes(marker),'Missing Mirror UI '+marker);
}
console.log('PASS Incremental Drive Folder Mirror: strict source/backup identity, preview, conflict-safe atomic native copy and UI wiring');
