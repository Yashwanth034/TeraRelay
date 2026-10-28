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
 const code=ts.transpileModule(fs.readFileSync(file,'utf8'),{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{
  console,TextEncoder,
 })(n=>{
  if(!n.startsWith('./'))throw Error('Unexpected fixture dependency');
  return load(path.normalize(path.join(path.dirname(name),n)));
 },mod,mod.exports);
 return mod.exports;
}
const blueprint=load('driveFolderBlueprint');
const raw={
 revision:37,dirty:false,files:[],pending:[],symlinks:[],
 directories:[
 {id:'root',name:'Existing',parent_id:null,trashed_at:null},
 {id:'child',name:'Archive',parent_id:'root',trashed_at:null},
 ],
};
const inv=blueprint.blueprintInventory(raw);
assert.equal(inv.revision,37);
const request=blueprint.buildBlueprint(inv,'root',
 'Product/API/Tests\nProduct\nProduct/API\nProduct/Docs\n');
assert.equal(request.paths.length,4);
assert.equal(request.destination_parent_id,'root');
assert.equal(request.paths[0],'Product/API/Tests');
assert.equal(request.revision,37);
assert.equal(blueprint.readBlueprintPreview({
 revision:37,ready:true,count:4,paths:request.paths,truncated:false,problems:[],
},request).ready,true);
assert.equal(blueprint.readBlueprintApplied({created:4,revision:41,dirty:true},request).created,4);
const invalid=(input,pattern)=>assert.throws(
 ()=>blueprint.buildBlueprint(inv,'root',input),pattern);
invalid('A/A1',/list parent/i);
invalid('A\nA',/duplicate/i);
invalid('A\nA/a\nA/A',/duplicate/i);
invalid('/absolute',/relative/i);
invalid('A/',/relative/i);
invalid('A\\B',/relative/i);
invalid('A/CON',/invalid portable/i);
invalid('A/\u0000x',/invalid portable/i);
invalid(' A',/invalid portable/i);
invalid('A//B',/invalid portable/i);
invalid('',/1.512/i);
invalid('A\n'.repeat(513),/1.512/i);
invalid('A'.repeat(65537),/64 KiB/i);
assert.throws(()=>blueprint.buildBlueprint(inv,'unavailable','A'),/Destination/i);
assert.throws(()=>blueprint.readBlueprintPreview({
 revision:38,ready:true,count:4,paths:request.paths,truncated:false,problems:[],
},request),/Invalid or stale/);
assert.throws(()=>blueprint.readBlueprintApplied({
 created:3,revision:42,dirty:true,
},request),/uncertain/);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_blueprint.rs'),'utf8');
const mod=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderBlueprint.tsx'),'utf8');
for(const term of ['SAVEPOINT drive_folder_blueprint','ROLLBACK TO drive_folder_blueprint',
 'create_drive_directory','notify_namespace_changed','fn ignored_insert_must_not_report_success',
 'fn partial_failure_rolls_back_all_new_folders']){
 assert.ok(rust.includes(term),'native requirement '+term);
}
assert.match(mod,/pub mod drive_folder_blueprint;/);
assert.match(mod,/pub use drive_folder_blueprint::\*;/);
for(const name of ['cmd_drive_folder_blueprint_preview','cmd_drive_folder_blueprint_apply']){
 assert.ok(ui.includes(name));assert.ok(lib.includes('commands::'+name+','));
}
assert.match(settings,/<DriveFolderBlueprint active=\{isOpen && activeTab === 'general'\} \/>/);
for(const term of ['readBlueprintPreview','readBlueprintApplied','cmd_drive_sync_metadata',
 'No automatic retries','Create verified hierarchy']){
 assert.ok(ui.includes(term),'UI requirement '+term);
}
console.log('PASS Drive Folder Blueprint: strict outline, collision validation contract, atomic native planner/rollback and UI/IPC wiring');
