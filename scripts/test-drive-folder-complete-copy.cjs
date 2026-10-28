'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cached=new Map();
function load(name){
 const file=path.resolve(root,'app/src',name+'.ts');
 if(cached.has(file))return cached.get(file);
 const mod={exports:{}};cached.set(file,mod.exports);
 const source=ts.transpileModule(fs.readFileSync(file,'utf8'),{
   compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+source+'\n})',{
   console,TextEncoder,
 })(n=>{
   if(!n.startsWith('./'))throw Error('Unexpected fixture dependency');
   return load(path.normalize(path.join(path.dirname(name),n)));
 },mod,mod.exports);
 return mod.exports;
}
const m=load('driveFolderCompleteCopy');
const snap={revision:24,dirty:false,files:[
 {entry_id:'f1',file_id:'content',directory_id:'child',display_name:'test.txt',total_size:11,updated_at:1}
],pending:[],symlinks:[],directories:[
 {id:'src',name:'Product',parent_id:null,trashed_at:null},
 {id:'child',name:'Docs',parent_id:'src',trashed_at:null},
 {id:'target',name:'Destination',parent_id:null,trashed_at:null},
]};
const inventory=m.readCompleteCopyInventory(snap);
assert.equal(inventory.folders.length,3);
const req=m.buildCompleteCopyRequest(inventory,'src','target','Copied Product');
assert.equal(req.source_id,'src');
assert.equal(req.destination_parent_id,'target');
assert.equal(req.expected_source_parent_id,null);
assert.equal(req.revision,24);
const preview=m.decodeCompleteCopyPreview({
 revision:24,ready:true,folders:2,files:1,referenced_bytes:11,problems:[],
},req);
assert.ok(preview.ready);
assert.equal(m.decodeCompleteCopyApplied({
 revision:26,root_id:'new-copy-root',folders:2,files:1,dirty:true,
},req,preview).root_id,'new-copy-root');
assert.throws(()=>m.buildCompleteCopyRequest(inventory,'missing','target','Copy'),/Source/);
assert.throws(()=>m.buildCompleteCopyRequest(inventory,'src','missing','Copy'),/Destination/);
assert.throws(()=>m.buildCompleteCopyRequest(inventory,'src','target','CON'),/portable/);
assert.throws(()=>m.decodeCompleteCopyPreview({
 revision:23,ready:true,folders:2,files:1,referenced_bytes:11,problems:[],
},req),/stale/);
assert.throws(()=>m.decodeCompleteCopyPreview({
 revision:24,ready:true,folders:2,files:1,referenced_bytes:11,problems:['late error'],
},req),/stale/);
assert.throws(()=>m.decodeCompleteCopyApplied({
 revision:26,root_id:'src',folders:2,files:1,dirty:true,
},req,preview),/cannot be confirmed/);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_complete_copy.rs'),'utf8');
const helper=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_file_copy.rs'),'utf8');
const mod=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderCompleteCopy.tsx'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const needle of ['SAVEPOINT drive_complete_copy','ROLLBACK TO drive_complete_copy',
  'source_kind','fresh_id','insert_copy','notify_namespace_changed',
  'unfinished','symlink','injected_late_file_insert_error_rolls_back_all_folders',
  'silent_file_insert_ignore_cannot_claim_success']){
 assert.ok(rust.toLowerCase().includes(needle.toLowerCase()),'native requirement: '+needle);
}
for(const needle of ['pub(super) fn source_kind','pub(super) fn insert_copy','pub(super) fn fresh_id']){
 assert.ok(helper.includes(needle),'reused safe copy helper: '+needle);
}
assert.match(mod,/pub mod drive_folder_complete_copy;/);
assert.match(mod,/pub use drive_folder_complete_copy::\*;/);
for(const command of ['cmd_drive_complete_copy_preview','cmd_drive_complete_copy_apply']){
 assert.ok(lib.includes('commands::'+command+','));
 assert.ok(ui.includes(command));
}
assert.match(settings,/<DriveFolderCompleteCopy active=\{isOpen && activeTab === 'general'\} \/>/);
for(const needle of ['No automatic retries','decodeCompleteCopyApplied',
  'validateDriveSyncResult','Create verified full copy']){
 assert.ok(ui.includes(needle),'UI contract: '+needle);
}
console.log('PASS Complete Folder Copy: strict planner, native atomic full-tree copy contracts, UI registration');
