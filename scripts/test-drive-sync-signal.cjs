'use strict';
// Pure renderer-local Drive sync bridge. No native IPC or Telegram calls.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
function load(file, stubs = {}) {
  const exports = {};
  vm.runInNewContext(ts.transpileModule(fs.readFileSync(file, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, { exports, require: name => {
    if (name in stubs) return stubs[name];
    throw new Error('Unexpected signal import: ' + name);
  } });
  return exports;
}
const continuity = load(path.join(root, 'app/src/deviceContinuity.ts'));
const signal = load(path.join(root, 'app/src/driveSyncSignal.ts'),
  { './deviceContinuity': continuity });
let verified = 0, exceptions = 0;
const unsubscribe = signal.subscribeToVerifiedDriveSync(() => { verified++; });
const unsubscribeThrowing = signal.subscribeToVerifiedDriveSync(() => {
  exceptions++;
  throw new Error('dead view error');
});
for (const invalid of [
  null,
  { dirty: false, revision: '1', source: 'current' },
  { dirty: false, revision: 1, source: 'unsupported' },
]) {
  assert.throws(() => signal.publishVerifiedDriveSync(invalid), /Native Drive/);
}
assert.equal(signal.publishVerifiedDriveSync({ revision: 1, dirty: true, source: 'merged-published' }), false);
assert.equal(signal.publishVerifiedDriveSync({ revision: 1, dirty: false, source: 'qa-local' }), false);
assert.equal(verified, 0, 'never clear warnings for dirty, invalid or local-QA outcomes');
for (const source of ['current', 'remote', 'remote-merged', 'merged-published']) {
  assert.equal(signal.publishVerifiedDriveSync({ revision: 2, dirty: false, source }), true);
}
assert.equal(verified, 4);
assert.equal(exceptions, 4);
unsubscribe();
unsubscribeThrowing();
assert.equal(signal.publishVerifiedDriveSync({ revision: 3, dirty: false, source: 'current' }), true);
assert.equal(verified, 4, 'unmounted listener not invoked');
console.log('PASS renderer Drive metadata sync signals: verified sources, malformed outcomes, QA/dirty, unsubscribe');
