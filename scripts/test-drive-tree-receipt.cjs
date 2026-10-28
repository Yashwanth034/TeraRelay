'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const js=ts.transpileModule(fs.readFileSync(path.join(root,'app/src/driveTreeReceipt.ts'),'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
const m={exports:{}};vm.runInNewContext('(function(require,module,exports){'+js+'\n})',{console})(require,m,m.exports);
const r={root_id:'folder',path:'/safe/receipt.json',folders:2,files:1,revision:42,checksum:'a'.repeat(64),imported:false};
assert.equal(m.exports.readTreeReceipt(r,r.path,false).root_id,'folder');
for(const v of [{...r,root_id:'../escape'},{...r,folders:513},{...r,files:2001},{...r,checksum:'a'},{...r,revision:1.5},{...r,imported:true},{...r,path:'/wrong.json'}])
 assert.throws(()=>m.exports.readTreeReceipt(v,r.path,false),/invalid/i);
const read=f=>fs.readFileSync(path.join(root,f),'utf8');
const native=read('app/src-tauri/src/commands/drive_tree_receipt.rs');
const ui=read('app/src/components/desktop/dashboard/DriveTreeReceipt.tsx');
const mod=read('app/src-tauri/src/commands/mod.rs');
const registry=read('app/src-tauri/src/lib.rs');
const settings=read('app/src/components/desktop/dashboard/SettingsModal.tsx');
for(const needle of ['SAVEPOINT drive_portable_tree_receipt','validate_portable_tree_journal','fs::hard_link','create_new(true)','Sha256::digest','cmd_drive_tree_receipt_export','cmd_drive_tree_receipt_import','changed_content_identity_and_tombstone_refuse_import_without_partial_journal'])
 assert.ok(native.includes(needle),'Missing guard: '+needle);
for(const needle of ['cmd_drive_tree_receipt_export','cmd_drive_tree_receipt_import','Nothing restores automatically','not a digital signature'])
 assert.ok(ui.includes(needle),'Missing user UI guard '+needle);
for(const [content,term] of [[mod,'pub mod drive_tree_receipt;'],[mod,'pub use drive_tree_receipt::*;'],[settings,"import { DriveTreeReceipt }"],[settings,'<DriveTreeReceipt active='],[registry,'commands::cmd_drive_tree_receipt_export,'],[registry,'commands::cmd_drive_tree_receipt_import,']])
 assert.equal(content.split(term).length-1,1,'Expected exact one registration '+term);
console.log('PASS portable folder recovery receipt strict IPC, native rollback and safe Settings wiring');
