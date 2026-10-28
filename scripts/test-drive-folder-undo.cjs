'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const file=fs.readFileSync(path.join(root,'app/src/driveFolderUndo.ts'),'utf8');
const code=ts.transpileModule(file,{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
const fixtureModule={exports:{}};
vm.runInNewContext('(function(module,exports){'+code+'\n})',{TextEncoder})(fixtureModule,fixtureModule.exports);
const undo=fixtureModule.exports;
const hash='a'.repeat(64);
const id='folder-'+'b'.repeat(32);
const entry={folder_id:'d',previous_name:'Original',previous_parent_id:null,
  current_name:'Moved',current_parent_id:'target',subtree_sha256:hash};
const history=undo.parseFolderUndoHistory([{id,created_at:10,applied_revision:20,
  undone_at:null,entries:[entry]}]);
assert.equal(history.length,1);
assert.equal(history[0].entries[0].previous_name,'Original');
const preview=undo.parseFolderUndoPreview({id,revision:25,ready:true,count:1,
  problems:[],entries:[entry]},id);
assert.equal(preview.count,1);
assert.equal(undo.parseFolderUndoApplied({restored:1,revision:26,dirty:true},preview).restored,1);
assert.throws(()=>undo.parseFolderUndoHistory([{id,created_at:10,applied_revision:20,
  undone_at:null,entries:[entry,entry]}]),/damaged|invalid/i);
assert.throws(()=>undo.parseFolderUndoHistory([{id,created_at:10,applied_revision:20,
  undone_at:null,entries:[{...entry,subtree_sha256:'wrong'}]}]),/damaged/);
assert.throws(()=>undo.parseFolderUndoHistory([{id:'bad',created_at:10,
  applied_revision:20,undone_at:null,entries:[entry]}]),/invalid/);
assert.throws(()=>undo.parseFolderUndoPreview({id,revision:25,ready:true,count:2,
  problems:[],entries:[entry]},id),/inconsistent/);
assert.throws(()=>undo.parseFolderUndoApplied({restored:1,revision:25,dirty:true},preview),
  /uncertain/);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_undo.rs'),'utf8');
const apply=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_operations.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderUndo.tsx'),'utf8');
for(const s of ['SAVEPOINT drive_folder_undo_apply','ROLLBACK TO drive_folder_undo_apply',
 'record_applied','digest_subtree','MAX_HISTORY','subtree_sha256','cmd_drive_folder_undo_history',
 'cmd_drive_folder_undo_preview','cmd_drive_folder_undo_apply',
 'later_update_failure_rolls_back_earlier_undo_and_history']) {
 assert.ok(native.includes(s),'missing native guard '+s);
}
assert.ok(apply.includes('drive_folder_undo::record_applied'));
for(const s of ['cmd_drive_folder_undo_history','cmd_drive_folder_undo_preview',
 'cmd_drive_folder_undo_apply','can','uncertain','aria-label','verify']) {
 assert.ok(ui.toLowerCase().includes(s.toLowerCase()),'missing UI '+s);
}
const registry=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const commands=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for (const s of ['cmd_drive_folder_undo_history','cmd_drive_folder_undo_preview',
  'cmd_drive_folder_undo_apply']) {
 assert.equal(registry.split('commands::'+s+',').length-1,1,'native handler exactly once: '+s);
}
assert.equal((commands.match(/pub mod drive_folder_undo;/g)||[]).length,1);
assert.equal((settings.match(/<DriveFolderUndo active=\{isOpen && activeTab === 'general'\} \/>/g)||[]).length,1);
console.log('PASS Folder Batch Undo durable native SQLite journal, strict TS decoder, guarded preview/apply UI and IPC wiring');
