'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const typescript = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const modulePath = path.join(root, 'app/src/driveHealthDoctor.ts');
const compiled = typescript.transpileModule(fs.readFileSync(modulePath, 'utf8'), {
  compilerOptions: { module: typescript.ModuleKind.CommonJS, target: typescript.ScriptTarget.ES2020 },
}).outputText;
const output = {};
vm.runInNewContext(compiled, { exports: output });
const sample = Object.freeze({
  scanned: 3, closed: 2, open_writers: 1, read_only: true, truncated: false,
  issues: Object.freeze([
    Object.freeze({ pending_id: 'p1', name: 'test.bin', severity: 'critical',
      code: 'missing_dirty_stage', advice: 'Preserve local data.' }),
  ]),
});
const health = output.parseDriveHealthReport(sample);
assert.equal(health.scanned, 3);
assert.equal(health.issues[0].code, 'missing_dirty_stage');
assert.equal(sample.issues.length, 1, 'input stays unchanged');
const fail = (value) => assert.throws(() => output.parseDriveHealthReport(value), /invalid/i);
fail({ ...sample, read_only: false });
fail({ ...sample, closed: -1 });
fail({ ...sample, open_writers: Number.MAX_SAFE_INTEGER + 1 });
fail({ ...sample, issues: [{ ...sample.issues[0], severity: 'success' }] });
fail({ ...sample, issues: [{ ...sample.issues[0], advice: 42 }] });
fail({ ...sample, issues: Array.from({ length: 201 }, () => sample.issues[0]) });
fail({ ...sample, issues: [{ ...sample.issues[0], code: '<script>' }] });
const ui = fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveHealthDoctor.tsx'), 'utf8');
assert.match(ui, /cmd_drive_health_inspect/, 'UI must invoke the real native command');
assert.match(ui, /parseDriveHealthReport\(/, 'UI validates native IPC before treating it as safe');
assert.match(ui, /No uploads, repairs, deletions or cloud access/, 'read-only scope is visible');
assert.match(ui, /truncated/, 'incomplete scans cannot look healthy');
const settings = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
const lib = fs.readFileSync(path.join(root, 'app/src-tauri/src/lib.rs'), 'utf8');
assert.match(settings, /<DriveHealthDoctor active=\{isOpen && activeTab === 'general'\}/,
  'read-only Health Doctor must be reachable from the real Settings screen');
assert.match(lib, /mod drive_health;/, 'native module must compile within the real TeraRelay crate');
assert.match(lib, /drive_health::cmd_drive_health_inspect/, 'IPC command must be registered in Tauri');
console.log('PASS Drive Health Doctor: strict IPC parsing, read-only Settings UI and registered native command');
