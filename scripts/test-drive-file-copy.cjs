'use strict';
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const filename=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(filename))return cache.get(filename);
 const mod={exports:{}};cache.set(filename,mod.exports);
 const code=ts.transpileModule(fs.readFileSync(filename,'utf8'),{
   compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{
   console,TextEncoder,
 })(n=>{
   if(!n.startsWith('./'))throw Error('Unexpected fixture module '+n);
   return load(path.normalize(path.join(path.dirname(name),n)));
 },mod,mod.exports);
 return mod.exports;
}
const copy=load('driveFileCopy');
const snapshot={
 revision:19,dirty:false,
 directories:[{id:'dest',name:'Copies',parent_id:null,trashed_at:null}],
 files:[
  {entry_id:'a',file_id:'o1',display_name:'report.txt',directory_id:null,
    total_size:10,updated_at:1},
  {entry_id:'b',file_id:'o2',display_name:'invoice.pdf',directory_id:null,
    total_size:20,updated_at:1},
 ],pending:[],symlinks:[],
};
const inventory=copy.instantCopyInventory(snapshot);
assert.equal(inventory.revision,19);
assert.equal(inventory.files.length,2);
assert.equal(inventory.folders.length,1);
assert.equal(inventory.files.find(f=>f.id==='a').path,'/report.txt');
const migrated={...snapshot,files:[...snapshot.files,
  {...snapshot.files[0],storage_version:2,display_name:'newer.txt'}]};
const migratedInventory=copy.instantCopyInventory(migrated);
assert.equal(migratedInventory.files.filter(f=>f.id==='a').length,1);
assert.equal(migratedInventory.files.find(f=>f.id==='a').name,'newer.txt');
assert.equal(migratedInventory.files.find(f=>f.id==='a').path,'/newer.txt');
const request=copy.buildInstantCopyRequest(inventory,['a','b'],'dest','Backup ', '');
assert.equal(request.files.length,2);
assert.equal(request.files[0].new_name,'Backup report.txt');
assert.equal(request.files[0].expected_parent_id,null);
assert.equal(request.destination_parent_id,'dest');
const checked=copy.readInstantCopyPreview({
 revision:19,ready:true,count:2,total_bytes:30,problems:[],
},request);
assert.ok(checked.ready);
assert.equal(copy.readInstantCopyApplied({
 copied:2,revision:20,dirty:true,
},request).copied,2);
const invalid=(fn,regex)=>assert.throws(fn,regex);
invalid(()=>copy.buildInstantCopyRequest(inventory,[],'dest','Copy ',''),/Select/);
invalid(()=>copy.buildInstantCopyRequest(inventory,['a','a'],'dest','Copy ',''),/distinct/);
invalid(()=>copy.buildInstantCopyRequest(inventory,['missing'],'dest','Copy ',''),/unavailable/);
invalid(()=>copy.buildInstantCopyRequest(inventory,['a'],'unknown','Copy ',''),/Destination/);
invalid(()=>copy.buildInstantCopyRequest(inventory,['a'],'dest','CON:',''),/Invalid/);
const collision={...inventory,files:inventory.files.map(f=>({...f,name:'duplicate.txt'}))};
invalid(()=>copy.buildInstantCopyRequest(collision,['a','b'],'dest','', ''),/same copy filename/);
invalid(()=>copy.readInstantCopyPreview({
 revision:20,ready:true,count:2,total_bytes:30,problems:[],
},request),/stale/);
invalid(()=>copy.readInstantCopyApplied({
 copied:1,revision:20,dirty:true,
},request),/uncertain/);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_file_copy.rs'),'utf8');
const moduleSource=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFileCopy.tsx'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const term of ['SAVEPOINT drive_instant_file_copies',
  'ROLLBACK TO drive_instant_file_copies','mark_drive_dirty',
  'fn has_pending','fn source_kind','fn fresh_id','drive_entries_v2',
  'drive_file_entries','fn sqlite_insert_abort_rolls_back_full_batch',
  'fn ignored_copy_insert_is_not_success']){
 assert.ok(rust.includes(term),'native copy requirement '+term);
}
assert.match(moduleSource,/pub mod drive_file_copy;/);
for(const cmd of ['cmd_drive_file_copy_preview','cmd_drive_file_copy_apply']){
 assert.ok(lib.includes('commands::'+cmd+','));
 assert.ok(ui.includes(cmd));
}
assert.ok(ui.includes('No automatic retries.'));
assert.ok(ui.includes('validateDriveSyncResult'));
assert.match(settings,/<DriveFileCopy active=\{isOpen && activeTab === 'general'\} \/>/);
console.log('PASS Drive Instant File Copies: inventory, plans, validation, native transaction and UI IPC contracts');
