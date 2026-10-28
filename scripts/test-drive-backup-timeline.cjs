'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const p=path.join(root,'app/src/driveBackupTimeline.ts');
const code=ts.transpileModule(fs.readFileSync(p,'utf8'),
 {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
const m={exports:{}};
vm.runInNewContext('(function(module,exports){'+code+'\n})',{console})(m,m.exports);
const api=m.exports;
const check={source_id:'src',backup_id:'bak',status:'healthy',explanation:'Metadata matches',
 source_files:1,backup_files:1,only_source:0,only_backup:0,different:0,unverified:0,samples:[]};
const scan={revision:4,dirty:false,checked_at:1700000000000,healthy:1,drift:0,
 unverified:0,blocked:0,checks:[check]};
const history={max_runs:40,runs:[{id:9,scan}]};
assert.equal(api.readAuditHistory(history).runs.length,1);
assert.throws(()=>api.readAuditHistory({...history,max_runs:999}),/Invalid/);
assert.throws(()=>api.readAuditHistory({...history,runs:[{id:9,scan},{id:9,scan}]}),/Invalid|order/);
assert.throws(()=>api.readAuditHistory({...history,runs:[{id:9,scan:{...scan,dirty:true}}]}),/inaccurately/);
assert.throws(()=>api.readAuditHistory({...history,runs:[{id:9,scan:{...scan,
 checks:[{...check,different:1}]}}]}),/inaccurately/);
assert.throws(()=>api.readAuditHistory({...history,runs:[{id:9,scan:{...scan,healthy:0}}]}),/summary/);
const newer={id:10,scan:{...scan,checked_at:1700000000200,
 healthy:0,drift:1,checks:[{...check,status:'drift',only_source:1}]}};
const changes=api.auditChanges(newer,history.runs[0]);
assert.equal(changes[0].direction,'worse');
assert.equal(api.auditChanges(history.runs[0],newer)[0].direction,'improved');
assert.equal(api.auditChanges(history.runs[0],null)[0].direction,'new');
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_backup_timeline.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBackupTimeline.tsx'),'utf8');
for(const c of ['cmd_drive_backup_timeline_list','cmd_drive_backup_timeline_record',
 'cmd_drive_backup_timeline_clear']){
 assert.ok(native.includes('fn '+c),'missing Rust '+c);
 assert.ok(ui.includes("'"+c+"'"),'missing UI '+c);
}
for(const guard of ['SAVEPOINT drive_backup_timeline_save','sha2::{Digest, Sha256}',
 'LIMIT: usize = 40','fn valid','fn history','fn record'])
 assert.ok(native.includes(guard),'missing Rust guard '+guard);
assert.ok(ui.includes('not automatic monitoring'));
console.log('PASS Drive Backup Assurance Timeline: retained history, integrity, strict verdicts, changes and UI native IPC contracts');
