'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const js=ts.transpileModule(fs.readFileSync(path.join(root,'app/src/driveBackupBatchRepair.ts'),'utf8'),{
 compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}
}).outputText;
const m={exports:{}};
vm.runInNewContext('(function(require,module,exports){'+js+'\n})',{console})(require,m,m.exports);
const lib=m.exports;
const raw={
 revision:7,plan_token:'a'.repeat(64),ready:true,
 total_folders:1,total_files:2,total_matches:3,added_logical_bytes:23,
 pairs:[{source_id:'src1',backup_id:'dst1',source_name:'Original',backup_name:'Backup',
 add_folders:1,add_files:2,matching_files:3,added_logical_bytes:23,
 examples:['Docs/','Docs/a.txt'],problems:[]}],problems:[]
};
const preview=lib.readBatchRepairPreview(raw);
assert.equal(preview.ready,true);
assert.equal(preview.pairs[0].add_files,2);
const req=lib.batchRepairRequest(preview);
assert.equal(req.revision,7);
assert.equal(req.plan_token,'a'.repeat(64));
const ok=lib.readBatchRepairApplied({
 revision:9,dirty:true,pairs_updated:1,folders_added:1,files_added:2,files_matched:3
},req,preview);
assert.equal(ok.files_added,2);
assert.throws(()=>lib.readBatchRepairApplied({
 revision:7,dirty:true,pairs_updated:1,folders_added:1,files_added:2,files_matched:3
},req,preview),/uncertain/i);
for(const bad of [
 {...raw,plan_token:'a'}, {...raw,total_files:3},
 {...raw,pairs:[{...raw.pairs[0],problems:['unsafe']}]},
 {...raw,pairs:[raw.pairs[0],raw.pairs[0]]},
 {...raw,ready:false}
]){
 if(bad.ready===false)assert.throws(()=>lib.batchRepairRequest(lib.readBatchRepairPreview(bad)),/fresh successful/i);
 else assert.throws(()=>lib.readBatchRepairPreview(bad),/invalid|inconsistent|duplicate/i);
}
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_backup_batch_repair.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBackupBatchRepair.tsx'),'utf8');
for(const marker of ['SAVEPOINT drive_watchlist_batch_repair','drive_folder_mirror::apply',
 'drive_folder_mirror::preview','check_roots','plan_token',
 'failure_in_second_pair_rolls_back_all_first_pair_changes',
 'altered_watchlist_while_revision_unchanged_blocks_stale_confirmation'])
 assert.ok(native.includes(marker),'Missing atomic safety feature/test '+marker);
for(const marker of ['cmd_drive_backup_batch_repair_preview',
 'cmd_drive_backup_batch_repair_apply','readBatchRepairApplied',
 'validateDriveSyncResult','never auto-repeat'])
 assert.ok(ui.includes(marker),'Missing frontend guarded action '+marker);
console.log('PASS Drive Backup Watchlist Batch Repair — strict IPC, stale-plan prevention, all-or-nothing native path and Settings UX');
