'use strict';
// Isolated deferred-response regression for the real useDeviceContinuity hook.
// No Telegram credentials, native mount access, or cloud writes.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const file = path.join(root, 'app/src/hooks/useDeviceContinuity.ts');
const compiled = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText;

const mountStatusPath = path.join(root, 'app/src/driveMountStatus.ts');
const mountStatusExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(mountStatusPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: mountStatusExports });
const helperPath = path.join(root, 'app/src/deviceContinuity.ts');
const helperExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(helperPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: helperExports });
const goodSync = { source: 'current', revision: 1, dirty: false };
const snapshot = { revision: 1, dirty: false, directories: [], files: [], pending: [] };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function harness(pauseCommand = '', pauseIndex = 1, isMobile = true, overrides = {}) {
  const counts = new Map();
  const calls = [];
  const warnings = [];
  const effects = [];
  const intervals = [];
  let state;
  let stateUpdates = 0;
  const pause = deferred();
  const invoke = async command => {
    calls.push(command);
    const index = (counts.get(command) || 0) + 1;
    counts.set(command, index);
    if (command === pauseCommand && index === pauseIndex) return pause.promise;
    if (overrides[command] && Object.prototype.hasOwnProperty.call(overrides[command], index)) {
      return overrides[command][index];
    }
    switch (command) {
      case 'cmd_drive_status': return { supported: false, mounted: false, native_recovery: false, mount_point: null };
      case 'cmd_drive_get_snapshot': return snapshot;
      case 'cmd_drive_recover_pending': return undefined;
      case 'cmd_drive_sync_metadata': return goodSync;
      case 'cmd_drive_crypto_status': return { configured: false, unlocked: false };
      default: throw new Error('Unexpected Drive native command: ' + command);
    }
  };
  const fakeReact = {
    useState: initial => {
      state = initial;
      return [initial, change => {
        stateUpdates++;
        state = typeof change === 'function' ? change(state) : change;
      }];
    },
    useRef: value => ({ current: value }),
    useCallback: fn => fn,
    useEffect: callback => effects.push(callback),
  };
  const compiledExports = {};
  vm.runInNewContext(compiled, {
    exports: compiledExports,
    require: name => {
      if (name === 'react') return fakeReact;
      if (name === '@tauri-apps/api/core') return { invoke };
      if (name === '../driveMountStatus') return mountStatusExports;
      if (name === '../deviceContinuity') return {
        ...helperExports,
        classifyDeviceContinuity: () => ({
          restoredFromRemote: false,
          directoryCount: 0,
          fileCount: 0,
          needsDriveUnlock: false,
          syncSource: 'synthetic',
        }),
      };
      throw new Error('Unexpected import: ' + name);
    },
    window: {
      setInterval: callback => { intervals.push(callback); return intervals.length; },
      clearInterval: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
    },
    console: { warn: (...args) => warnings.push(args) },
  }, { filename: file });
  compiledExports.useDeviceContinuity({ authenticated: true, authResolved: true, isMobile });
  assert.equal(effects.length, 2);
  // Mobile disables native mounting; desktop exercises pending-file recovery
  // while a synthetic unsupported status prevents any actual native mount.
  const stopMount = !isMobile ? effects[0]() : () => {};
  const stopMetadata = effects[1]();
  const cleanup = () => { stopMetadata(); stopMount(); };
  const settle = async () => { for (let i = 0; i < 6; i++) await new Promise(done => setImmediate(done)); };
  return {
    calls, warnings, intervals, pause, settle,
    get state() { return state; },
    get stateUpdates() { return stateUpdates; },
    cleanup,
    tick() { assert.ok(intervals.length, 'periodic timer must exist'); intervals[0](); },
  };
}

(async () => {
  // Every phase must abort the NEXT command if the view was disposed while an
  // earlier native operation was unresolved. The native operation itself is
  // intentionally not cancelled.
  for (const scenario of [
    ['cmd_drive_get_snapshot', 1, ['cmd_drive_get_snapshot'], snapshot],
    ['cmd_drive_sync_metadata', 1,
      ['cmd_drive_get_snapshot', 'cmd_drive_sync_metadata'], goodSync],
    ['cmd_drive_get_snapshot', 2,
      ['cmd_drive_get_snapshot', 'cmd_drive_sync_metadata', 'cmd_drive_get_snapshot'], snapshot],
    ['cmd_drive_crypto_status', 1,
      ['cmd_drive_get_snapshot', 'cmd_drive_sync_metadata', 'cmd_drive_get_snapshot', 'cmd_drive_crypto_status'],
      { configured: false, unlocked: false }],
  ]) {
    const [command, index, expected, answer] = scenario;
    const h = harness(command, index);
    await h.settle();
    assert.deepEqual(h.calls, expected, 'expected current phase before cancellation: ' + command);
    const before = h.stateUpdates;
    h.cleanup();
    h.pause.resolve(answer);
    await h.settle();
    assert.deepEqual(h.calls, expected, 'disposed metadata flow cannot issue extra commands: ' + command);
    assert.equal(h.stateUpdates, before, 'disposed metadata flow cannot update UI');
    assert.equal(h.warnings.length, 0, 'late native completion should not log stale errors');
  }

  // Desktop pending-file repair can be slow. Teardown before it finishes
  // must not schedule any later cloud metadata sync for that old session.
  const pending = harness('cmd_drive_recover_pending', 1, false);
  await pending.settle();
  assert.deepEqual(pending.calls.filter(cmd => cmd !== 'cmd_drive_status'),
    ['cmd_drive_get_snapshot', 'cmd_drive_recover_pending']);
  const beforePending = pending.stateUpdates;
  pending.cleanup();
  pending.pause.resolve(undefined);
  await pending.settle();
  assert.deepEqual(pending.calls.filter(cmd => cmd !== 'cmd_drive_status'),
    ['cmd_drive_get_snapshot', 'cmd_drive_recover_pending'],
    'disposed pending-file repair must not begin metadata sync');
  assert.equal(pending.stateUpdates, beforePending);

  // An already-running sync may fail after sign-out. No new snapshot/crypto
  // request, stale degraded UI, or warning is allowed.
  const rejected = harness('cmd_drive_sync_metadata', 1);
  await rejected.settle();
  const previous = rejected.stateUpdates;
  rejected.cleanup();
  rejected.pause.reject(new Error('synthetic late auth failure'));
  await rejected.settle();
  assert.deepEqual(rejected.calls, ['cmd_drive_get_snapshot', 'cmd_drive_sync_metadata']);
  assert.equal(rejected.stateUpdates, previous);
  assert.equal(rejected.warnings.length, 0);

  // Successful initial metadata recovery must still schedule the periodic
  // refresh and update the ready state.
  const ready = harness();
  await ready.settle();
  assert.equal(ready.state.phase, 'ready');
  assert.ok(ready.intervals.length, 'active session starts periodic metadata checks');
  ready.cleanup();

  // A periodic metadata sync already pending at sign-out must not be followed
  // by a snapshot/crypto command.
  const periodic = harness('cmd_drive_sync_metadata', 2);
  await periodic.settle();
  assert.equal(periodic.state.phase, 'ready');
  periodic.tick();
  await periodic.settle();
  const periodicCalls = periodic.calls.slice();
  const periodicUpdates = periodic.stateUpdates;
  periodic.cleanup();
  periodic.pause.resolve(goodSync);
  await periodic.settle();
  assert.deepEqual(periodic.calls, periodicCalls);
  assert.equal(periodic.stateUpdates, periodicUpdates);

  // A periodic snapshot can also finish after teardown. Guard its follow-up
  // crypto check and prevent stale state changes.
  const lateSnapshot = harness('cmd_drive_get_snapshot', 3);
  await lateSnapshot.settle();
  lateSnapshot.tick();
  await lateSnapshot.settle();
  const lateCalls = lateSnapshot.calls.slice();
  const lateUpdates = lateSnapshot.stateUpdates;
  lateSnapshot.cleanup();
  lateSnapshot.pause.resolve(snapshot);
  await lateSnapshot.settle();
  assert.deepEqual(lateSnapshot.calls, lateCalls);
  assert.equal(lateSnapshot.stateUpdates, lateUpdates);

  // A delayed *failed* periodic crypto lookup was still logging after
  // teardown. Failures from an expired session must not produce stale warnings.
  const lateCryptoFailure = harness('cmd_drive_crypto_status', 2);
  await lateCryptoFailure.settle();
  lateCryptoFailure.tick();
  await lateCryptoFailure.settle();
  const cryptoCalls = lateCryptoFailure.calls.slice();
  const cryptoUpdates = lateCryptoFailure.stateUpdates;
  lateCryptoFailure.cleanup();
  lateCryptoFailure.pause.reject(new Error('synthetic stale encryption status rejection'));
  await lateCryptoFailure.settle();
  assert.deepEqual(lateCryptoFailure.calls, cryptoCalls);
  assert.equal(lateCryptoFailure.stateUpdates, cryptoUpdates);
  assert.equal(lateCryptoFailure.warnings.length, 0,
    'rejected periodic encryption status after cleanup must not warn');

  // IPC generics are compile-time only. Malformed snapshots must not crash
  // rendering or make an unverified cloud restore look healthy.
  assert.throws(() => helperExports.validateDriveSnapshot(null), /not an object/);
  assert.throws(() => helperExports.validateDriveSnapshot({ ...snapshot, files: null }), /invalid revision/);
  assert.throws(() => helperExports.validateDriveSnapshot({ ...snapshot, revision: -1 }), /invalid revision/);
  assert.throws(() => helperExports.validateDriveSnapshot({ ...snapshot, dirty: 'false' }), /invalid revision/);
  assert.throws(() => helperExports.validateDriveSyncResult({ source: 'new-unknown', revision: 1, dirty: false }),
    /unverified result/);
  assert.throws(() => helperExports.validateDriveCryptoStatus({ configured: false, unlocked: true }),
    /invalid/);
  assert.equal(helperExports.validateDriveSnapshot(snapshot).files.length, 0);
  assert.equal(helperExports.validateDriveSyncResult(goodSync).source, 'current');
  assert.equal(helperExports.validateDriveCryptoStatus({ configured: true, unlocked: false }).configured, true);

  const invalidBefore = harness('', 1, true, {
    cmd_drive_get_snapshot: { 1: { ...snapshot, files: 'not-an-array' } },
  });
  await invalidBefore.settle();
  assert.equal(invalidBefore.state.phase, 'degraded',
    'malformed initial snapshot does not get reclassified as pristine/ready after sync');
  assert.match(invalidBefore.state.error, /Could not validate initial Drive snapshot/);
  assert.equal(invalidBefore.state.nativeMountError, null);
  invalidBefore.cleanup();

  const invalidAfter = harness('', 1, true, {
    cmd_drive_get_snapshot: { 2: { ...snapshot, directories: null } },
  });
  await invalidAfter.settle();
  assert.equal(invalidAfter.state.phase, 'degraded',
    'malformed refreshed snapshot cannot report successful Drive initialization');
  assert.match(invalidAfter.state.error, /Could not validate refreshed Drive snapshot/);
  assert.equal(invalidAfter.state.directoryCount, 0, 'last valid snapshot remains usable');
  invalidAfter.cleanup();

  const invalidSync = harness('', 1, true, {
    cmd_drive_sync_metadata: { 1: { source: 'future-unverified', dirty: false, revision: 3 } },
  });
  await invalidSync.settle();
  assert.equal(invalidSync.state.phase, 'degraded', 'unknown sync result cannot mark metadata current');
  assert.match(invalidSync.state.error, /unverified result/);
  invalidSync.cleanup();

  const invalidCrypto = harness('', 1, true, {
    cmd_drive_crypto_status: { 1: { configured: 'false', unlocked: false } },
  });
  await invalidCrypto.settle();
  assert.equal(invalidCrypto.state.phase, 'degraded');
  assert.match(invalidCrypto.state.securityError, /encryption status is invalid/);
  assert.equal(invalidCrypto.state.needsDriveUnlock, false,
    'invalid crypto payload cannot claim a securely unlocked Drive');
  invalidCrypto.cleanup();

  const corruptPeriodic = harness('', 1, true, {
    cmd_drive_get_snapshot: { 3: { ...snapshot, pending: false } },
  });
  await corruptPeriodic.settle();
  assert.equal(corruptPeriodic.state.phase, 'ready');
  corruptPeriodic.tick();
  await corruptPeriodic.settle();
  assert.equal(corruptPeriodic.state.phase, 'degraded',
    'corrupt periodic snapshot should be visible as degraded, not silently accepted');
  assert.match(corruptPeriodic.state.error, /Native Drive snapshot has invalid/);
  assert.equal(corruptPeriodic.state.fileCount, 0, 'retain previous validated counts on periodic failure');
  corruptPeriodic.cleanup();

  const recoveredPeriodic = harness('', 1, true, {
    cmd_drive_sync_metadata: { 2: { ...goodSync, dirty: 'false' } },
  });
  await recoveredPeriodic.settle();
  recoveredPeriodic.tick();
  await recoveredPeriodic.settle();
  assert.equal(recoveredPeriodic.state.phase, 'degraded');
  assert.match(recoveredPeriodic.state.error, /unverified result/);
  recoveredPeriodic.tick();
  await recoveredPeriodic.settle();
  assert.equal(recoveredPeriodic.state.phase, 'ready',
    'subsequent correct periodic sync recovers from malformed transient result');
  recoveredPeriodic.cleanup();

  console.log('PASS Drive continuity: cancellation, fail-closed snapshots/sync/crypto, periodic recovery');
})().catch(error => { console.error(error); process.exitCode = 1; });
