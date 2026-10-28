'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', {paths:[path.join(root,'app')]}));
const transpiled = ts.transpileModule(
  fs.readFileSync(path.join(root, 'app/src/drivePendingRescue.ts'), 'utf8'),
  {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}
);
const mod = {};
vm.runInNewContext(transpiled.outputText, {exports: mod});
const rows = [
  {id:'stage-01',name:'Photos.zip',size:0},
  {id:'stage_02',name:'draft.txt',size:512},
];
assert.equal(mod.parseRescueCandidates(rows).length,2);
assert.equal(mod.parseRescueCandidates([]).length,0);
assert.throws(()=>mod.parseRescueCandidates([...rows,rows[0]]),/Corrupt/);
assert.throws(()=>mod.parseRescueCandidates([{...rows[0],id:'../unsafe'}]),/Corrupt/);
assert.throws(()=>mod.parseRescueCandidates([{...rows[0],size:-1}]),/Corrupt/);
assert.throws(()=>mod.parseRescueCandidates([{...rows[0],size:Infinity}]),/Corrupt/);
assert.throws(()=>mod.parseRescueCandidates(Array(101).fill(rows[0])),/Invalid/);
const outcome={pending_id:'stage-01',count:2,bytes:123,restored:1};
assert.equal(mod.parseRescueResult(outcome).restored,1);
assert.throws(()=>mod.parseRescueResult({...outcome,bytes:2147483649}),/Invalid/);
assert.throws(()=>mod.parseRescueResult({...outcome,restored:3}),/Invalid/);
assert.throws(()=>mod.parseRescueResult({...outcome,pending_id:'../broken'}),/Invalid/);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/drive_pending_rescue.rs'),'utf8');
for (const part of ['fn export_pack(', 'fn restore_pack(', 'fn verify_archive(',
  'fn match_manifest(', 'fn valid_part_name(', 'MAX_PACK_BYTES',
  'CompressionMethod::Stored', 'fs::hard_link(', 'file_digest(',
  'cmd_drive_rescue_candidates', 'cmd_drive_rescue_export', 'cmd_drive_rescue_restore']) {
  assert.ok(native.includes(part), 'missing native rescue workflow: '+part);
}
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DrivePendingRescue.tsx'),'utf8');
for (const cmd of ['cmd_drive_rescue_candidates','cmd_drive_rescue_export','cmd_drive_rescue_restore']) {
  assert.ok(ui.includes(cmd),'UI lacks '+cmd);
}
assert.ok(ui.includes('plaintext'), 'backup confidentiality warning missing');
assert.ok(ui.includes('not remotely uploaded pieces'), 'partial data limitation missing');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
assert.equal((settings.match(/<DrivePendingRescue active=/g)||[]).length,1,
  'Settings must mount exactly one rescue panel');
assert.equal((lib.match(/drive_pending_rescue::cmd_drive_rescue_export,/g)||[]).length,1,
  'native export command must be registered exactly once');
console.log('PASS Drive Pending Rescue decoder validation + native/UI wiring contract');
