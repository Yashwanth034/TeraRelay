'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', {paths:[path.join(root,'app')]}));
const cache = new Map();
function load(name) {
  const absolute = path.resolve(root, 'app/src', name + '.ts');
  if (cache.has(absolute)) return cache.get(absolute);
  const module = {exports:{}};
  cache.set(absolute, module.exports);
  const code = ts.transpileModule(fs.readFileSync(absolute, 'utf8'),
    {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
  const localRequire = specifier => {
    if (!specifier.startsWith('./')) throw Error('Unexpected fixture dependency');
    return load(path.normalize(path.join(path.dirname(name),specifier)));
  };
  vm.runInNewContext('(function(require,module,exports){' + code + '\n})',
    {TextEncoder,console})(localRequire,module,module.exports);
  return module.exports;
}
const clone = load('driveFolderClone');
const raw = {
  revision: 401, dirty: false,
  directories: [
    {id:'src',name:'Projects',parent_id:null,trashed_at:null},
    {id:'child',name:'Design',parent_id:'src',trashed_at:null},
    {id:'leaf',name:'Releases',parent_id:'child',trashed_at:null},
    {id:'destination',name:'Archive',parent_id:null,trashed_at:null},
  ],
  files: [],pending: [],symlinks: [],
};
const inventory = clone.readCloneInventory(raw);
assert.equal(inventory.folders.length,4);
assert.equal(inventory.folders.find(f=>f.id==='leaf').path,'/Projects/Design/Releases');
const request = clone.buildCloneRequest(inventory,'src','destination','New project');
assert.equal(request.revision,401);
assert.equal(request.source_id,'src');
assert.equal(request.expected_source_name,'Projects');
assert.equal(request.destination_parent_id,'destination');
assert.equal(request.new_root_name,'New project');
assert.throws(()=>clone.buildCloneRequest(inventory,'missing',null,'Valid'),/Source/);
assert.throws(()=>clone.buildCloneRequest(inventory,'src','bad','Valid'),/Destination/);
assert.throws(()=>clone.buildCloneRequest(inventory,'src',null,'NUL'),/portable/);
assert.throws(()=>clone.buildCloneRequest(inventory,'src',null,'a/b'),/portable/);
assert.throws(()=>clone.buildCloneRequest(inventory,'src',null,'A'.repeat(256)),/portable/);
assert.equal(clone.validCloneName('Monthly Template 2027'),true);
const validPreview = clone.decodeClonePreview({
  revision:401,ready:true,count:3,not_copied_files:0,not_copied_pending:0,
  paths:['New project','New project/Design','New project/Design/Releases'],
  truncated:false,problems:[],
},request);
assert.ok(validPreview.ready);
assert.equal(validPreview.count,3);
const decoded = clone.decodeCloneApplied({
  created:3,root_id:'different-new-id',revision:402,dirty:true,
},request,validPreview);
assert.equal(decoded.created,3);
for(const malformed of [
  {revision:400,ready:true,count:3,not_copied_files:0,not_copied_pending:0,paths:[],truncated:false,problems:[]},
  {revision:401,ready:true,count:513,not_copied_files:0,not_copied_pending:0,paths:[],truncated:false,problems:[]},
  {revision:401,ready:false,count:0,not_copied_files:0,not_copied_pending:0,paths:['fake'],truncated:false,problems:[]},
  {revision:401,ready:true,count:3,not_copied_files:0,not_copied_pending:0,paths:[],truncated:false,problems:['must be empty']},
]) assert.throws(()=>clone.decodeClonePreview(malformed,request),/Invalid or stale/);
for(const malformed of [
  {created:2,root_id:'new-id',revision:402,dirty:true},
  {created:3,root_id:'src',revision:402,dirty:true},
  {created:3,root_id:'new-id',revision:401,dirty:true},
  {created:3,root_id:'new-id',revision:402,dirty:false},
]) assert.throws(()=>clone.decodeCloneApplied(malformed,request,validPreview),/cannot be verified/);
const ui = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderClone.tsx'),'utf8');
const rust = fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_clone.rs'),'utf8');
for(const s of ['cmd_drive_folder_clone_preview','cmd_drive_folder_clone_apply',
  'cmd_drive_sync_metadata','canApply','uncertain','aria-label','empty']) {
  assert.ok(ui.includes(s), 'UI missing '+s);
}
for(const s of ['SAVEPOINT drive_folder_clone_tree','ROLLBACK TO drive_folder_clone_tree',
  'create_drive_directory','MAX_CLONE_FOLDERS','late_insert_failure_rolls_back_all_new_directories',
  'silently_ignored_insert_cannot_be_reported_as_a_success']) {
  assert.ok(rust.includes(s),'Native tree cloner missing '+s);
}
const registry = fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const commands = fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const settings = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for (const name of ['cmd_drive_folder_clone_preview','cmd_drive_folder_clone_apply']) {
  assert.equal(registry.split('commands::'+name+',').length-1,1,
    'Native IPC must register exactly once: '+name);
}
assert.equal((commands.match(/pub mod drive_folder_clone;/g)||[]).length,1);
assert.equal((commands.match(/pub use drive_folder_clone::\*;/g)||[]).length,1);
assert.equal((settings.match(/<DriveFolderClone active=\{isOpen && activeTab === 'general'\} \/>/g)||[]).length,1);
console.log('PASS Folder Structure Cloner: TS planner, native IPC registration, accessible UI, and atomic Rust rollback fixture');
