'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const pathTo=path.join(root,'app/src',name+'.ts');
 if(cache.has(pathTo))return cache.get(pathTo);
 const module={exports:{}};cache.set(pathTo,module.exports);
 const output=ts.transpileModule(fs.readFileSync(pathTo,'utf8'),
  {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
 vm.runInNewContext('(function(require,module,exports){'+output+'\n})',{console,TextEncoder})
 ((spec)=>{if(!spec.startsWith('./'))throw Error('Unexpected dependency');
   return load(path.normalize(path.join(path.dirname(name),spec)));},module,module.exports);
 return module.exports;
}
const {readNamespaceAudit,summarizeNamespaceAudit}=load('driveNamespaceAudit');
const report={
 revision:18,read_only:true,raw_files:4,visible_files:3,folders:2,pending:0,
 links:0,truncated:false,
 issues:[{severity:'critical',code:'missing_file_backing',kind:'file',name:'gone.bin',
  explanation:'Drive backing content is unavailable'}],
};
const v=readNamespaceAudit(report);
assert.equal(v.raw_files,4);
assert.equal(summarizeNamespaceAudit(v).critical,1);
assert.equal(summarizeNamespaceAudit({...v,issues:[]}).readyToTrust,true);
assert.equal(summarizeNamespaceAudit({...v,truncated:true,issues:[]}).readyToTrust,false);
assert.throws(()=>readNamespaceAudit({...v,read_only:false}),/invalid|incomplete/i);
assert.throws(()=>readNamespaceAudit({...v,raw_files:-1}),/invalid|incomplete/i);
assert.throws(()=>readNamespaceAudit({...v,issues:[{...v.issues[0],severity:'repaired'}]}),/invalid|incomplete/i);
assert.throws(()=>readNamespaceAudit({...v,issues:Array(201).fill(v.issues[0])}),/invalid|incomplete/i);
assert.throws(()=>readNamespaceAudit({...v,issues:[{...v.issues[0],code:'<script>'}]}),/invalid|incomplete/i);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_namespace_audit.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveNamespaceAudit.tsx'),'utf8');
const reg=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const modules=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const marker of [
  'LEFT JOIN logical_files','LEFT JOIN drive_objects','missing_file_backing',
  'orphan_file_parent','broken_folder_ancestry','portable_name_collision',
  'pending_base_missing','MAX_RAW: usize = 5_000','MAX_ISSUES: usize = 200',
  'cmd_drive_namespace_audit','unavailable_backing_is_reported_even_when_snapshot_hides_file',
  'known_pending_copy_on_write_is_not_falsely_called_unlisted',
])assert.ok(rust.includes(marker),'Missing native read-only diagnostic '+marker);
assert.ok(ui.includes('cmd_drive_namespace_audit'));
assert.ok(ui.includes('readNamespaceAudit'));
assert.ok(ui.includes('It does not download file bytes'));
assert.equal(reg.split('commands::cmd_drive_namespace_audit,').length-1,1);
assert.ok(modules.includes('pub mod drive_namespace_audit;'));
assert.ok(modules.includes('pub use drive_namespace_audit::*;'));
assert.equal(settings.split("<DriveNamespaceAudit active={isOpen && activeTab === 'general'} />").length-1,1);
console.log('PASS Drive Namespace Audit: strict typed reporting, raw-vs-visible file coverage, read-only source safety and UI registration');
