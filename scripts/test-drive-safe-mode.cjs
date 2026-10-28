'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname,'..');
const ts = require(require.resolve('typescript', { paths: [path.join(root,'app')] }));
const exported = {};
const source = fs.readFileSync(path.join(root,'app/src/driveSafeMode.ts'),'utf8');
vm.runInNewContext(ts.transpileModule(source,{
  compilerOptions: { module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020 },
}).outputText,{exports:exported});
const status = { read_only:true,mounted:true,in_flight_uploads_may_finish:true,scope:'linux_fuse_mount_only'};
assert.deepEqual(JSON.parse(JSON.stringify(exported.parseDriveSafeMountStatus(status))),status);
for (const invalid of [
  null,[],{}, {...status,read_only:'true'}, {...status,mounted:1},
  {...status,scope:'entire_drive'}, {...status,in_flight_uploads_may_finish:false},
]) {
  assert.throws(()=>exported.parseDriveSafeMountStatus(invalid),/Invalid/);
}
const ui = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveSafeMode.tsx'),'utf8');
for (const phrase of [
  'cmd_drive_safe_mount_set','cmd_drive_safe_mount_status','parseDriveSafeMountStatus',
  'Already-running uploads may finish','Linux FUSE mount','Settings commands',
]) assert.ok(ui.includes(phrase),'missing Safe Mode safety contract: '+phrase);
const native = fs.readFileSync(path.join(root,'app/src-tauri/src/drive_safe_mode.rs'),'utf8');
assert.ok(native.includes('read_only: bool'));
assert.ok(native.includes('in_flight_uploads_may_finish'));
const core = fs.readFileSync(path.join(root,'app/src-tauri/src/drive_mount.rs'),'utf8');
assert.ok(core.includes('safe_mode::writable_open(flags)'));
assert.ok(core.includes('|| state.safe_mount_read_only()'));
const lib = fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.ok(lib.includes('drive_mount::safe_mode::cmd_drive_safe_mount_status'));
assert.ok(lib.includes('drive_mount::safe_mode::cmd_drive_safe_mount_set'));
assert.ok(settings.includes('<DriveSafeMode active={isOpen && activeTab === \'general\'} />'));
console.log('PASS native FUSE Safe Read-Only Mount: strict UI IPC shape, guarded actions, registered Settings');
