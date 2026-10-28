'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const out = {};
const source = fs.readFileSync(path.join(root,'app/src/driveTransferCenter.ts'),'utf8');
vm.runInNewContext(ts.transpileModule(source, {
  compilerOptions: {module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
}).outputText,{exports:out});
const sample = {
  paused:false,automatic_worker_running:true,total_pending:2,truncated:false,
  transfers:[
    {pending_id:'a1',file_name:'report.txt',size:12,status:'backoff',failed_attempts:2,retry_in_seconds:40},
    {pending_id:'b2',file_name:'movie.mp4',size:0,status:'uploading',failed_attempts:0,retry_in_seconds:0},
  ],
};
const parsed = out.parseTransferSnapshot(sample);
assert.equal(parsed.total_pending,2);
assert.equal(parsed.transfers[0].status,'backoff');
assert.equal(parsed.transfers[1].size,0);
assert.equal(out.parseTransferAction({attempted:true,recovered:false,remains_pending:true}).recovered,false);
const bad = value => assert.throws(()=>out.parseTransferSnapshot(value),/invalid/i);
bad({...sample,transfers:[sample.transfers[0],sample.transfers[0]]});
bad({...sample,total_pending:1});
bad({...sample,paused:'false'});
bad({...sample,truncated:false,total_pending:3});
bad({...sample,transfers:[{...sample.transfers[0],retry_in_seconds:-1}]});
bad({...sample,transfers:[{...sample.transfers[0],status:'success'}]});
bad({...sample,transfers:[{...sample.transfers[0],pending_id:'../outside'}]});
assert.throws(()=>out.parseTransferAction({attempted:false,recovered:true,remains_pending:true}),/invalid/i);
const ui = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveTransferCenter.tsx'),'utf8');
const lib = fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const modal = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const cmd of ['cmd_drive_transfer_snapshot','cmd_drive_transfer_pause','cmd_drive_transfer_retry']){
  assert.ok(ui.includes(cmd),'missing UI IPC '+cmd);
  assert.ok(lib.includes('drive_mount::transfer_center::'+cmd),'unregistered IPC '+cmd);
}
assert.ok(modal.includes('<DriveTransferCenter active='),'real Settings integration missing');
assert.ok(ui.includes('Do not blindly resend'),'ambiguous IPC must not promote blind resend');
assert.ok(ui.includes('does not cancel a Telegram send'),'pause must disclose in-flight limitation');
console.log('PASS Transfer Control Center strict IPC parsing, user-controlled queue and Settings integration');
