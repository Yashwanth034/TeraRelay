'use strict';
// Synthetic regression for desktop native Drive mount intent, failure reporting,
// and native-worker handoff. No Telegram, real mount, or user data is accessed.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const hookPath = path.join(root, 'app/src/hooks/useDeviceContinuity.ts');
const compiled = ts.transpileModule(fs.readFileSync(hookPath, 'utf8'), {
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

function makeHarness(status, mount) {
  let currentStatus = status;
  let mountHandler = mount || (async () => undefined);
  let state;
  let mountCalls = 0;
  let statusCalls = 0;
  const effects = [];
  const intervals = [];
  const events = new Map();
  const fakeReact = {
    useState(initial) {
      state = initial;
      return [initial, change => { state = typeof change === 'function' ? change(state) : change; }];
    },
    useRef: value => ({ current: value }),
    useCallback: fn => fn,
    useEffect: fn => { effects.push(fn); },
  };
  const invoke = async name => {
    if (name === 'cmd_drive_status') {
      statusCalls++;
      if (currentStatus instanceof Error) throw currentStatus;
      if (typeof currentStatus === 'function') return currentStatus();
      return { ...currentStatus };
    }
    if (name === 'cmd_drive_mount') {
      mountCalls++;
      return mountHandler();
    }
    if (name === 'cmd_drive_unmount' || name === 'cmd_drive_recover_pending') return undefined;
    if (name === 'cmd_drive_get_snapshot') return { revision: 1, dirty: false, directories: [], files: [], pending: [] };
    if (name === 'cmd_drive_sync_metadata') return { source: 'current', revision: 1, dirty: false };
    if (name === 'cmd_drive_crypto_status') return { configured: false, unlocked: false };
    throw new Error('Unexpected native command: ' + name);
  };
  const fakeWindow = {
    setInterval: fn => { intervals.push(fn); return intervals.length; },
    clearInterval: () => {},
    addEventListener: (event, fn) => events.set(event, fn),
    removeEventListener: event => events.delete(event),
  };
  const exports = {};
  const fakeRequire = name => {
    if (name === 'react') return fakeReact;
    if (name === '@tauri-apps/api/core') return { invoke };
    if (name === '../driveMountStatus') return mountStatusExports;
    if (name === '../deviceContinuity') return { ...helperExports, classifyDeviceContinuity: () => ({
      restoredFromRemote: false, directoryCount: 0, fileCount: 0,
      needsDriveUnlock: false, syncSource: 'synthetic',
    }) };
    throw new Error('Unexpected dependency: ' + name);
  };
  vm.runInNewContext(compiled, { exports, require: fakeRequire, window: fakeWindow, console: { warn() {} } }, { filename: hookPath });
  exports.useDeviceContinuity({ authenticated: true, authResolved: true, isMobile: false });
  assert.equal(effects.length, 2, 'native recovery and metadata sync are separate effects');
  const cleanup = effects[0]();
  const settle = async () => { for (let i = 0; i < 5; i++) await new Promise(resolve => setImmediate(resolve)); };
  return {
    get state() { return state; },
    get mountCalls() { return mountCalls; },
    get statusCalls() { return statusCalls; },
    setStatus: next => { currentStatus = next; },
    setMount: next => { mountHandler = next; },
    tick: async () => { assert.ok(intervals.length >= 1); intervals[0](); await settle(); },
    focus: async () => { events.get('focus')(); await settle(); },
    settle,
    runMetadata: async () => { effects[1](); await settle(); },
    cleanup,
  };
}

(async () => {
  const missing = { supported: true, mounted: false, native_recovery: false, mount_point: null };
  const mounted = { ...missing, mounted: true, mount_point: '/synthetic/Drive Mount' };
  const nativeRecovering = { ...missing, native_recovery: true };
  // Rust macOS cmd_drive_mount intentionally uses
  // mark_provider_connected(None). Mounted without mount_point is valid only
  // when the native provider kind is macos-file-provider.
  const macosProvider = {
    supported: true, mounted: true, native_recovery: false,
    mount_point: null, provider_kind: 'macos-file-provider',
  };
  const macosMounted = makeHarness(macosProvider);
  await macosMounted.settle();
  assert.equal(macosMounted.mountCalls, 0,
    'already-connected macOS File Provider must not remount because path is absent');
  assert.equal(macosMounted.state.nativeMountError, null,
    'pathless native macOS provider is a legitimate mounted state');
  macosMounted.setStatus({
    ...macosProvider, mounted: false, native_recovery: false,
  });
  await macosMounted.tick();
  assert.equal(macosMounted.mountCalls, 0,
    'native mount ownership survives a later explicit macOS provider disconnect');
  macosMounted.cleanup();

  const invalidPathlessLinux = makeHarness({
    ...macosProvider, provider_kind: 'fuse',
  });
  await invalidPathlessLinux.settle();
  assert.equal(invalidPathlessLinux.mountCalls, 0);
  assert.match(invalidPathlessLinux.state.nativeMountError, /Invalid native Drive mount status/);
  invalidPathlessLinux.setStatus(macosProvider);
  await invalidPathlessLinux.focus();
  assert.equal(invalidPathlessLinux.state.nativeMountError, null,
    'healthy macOS response clears previously invalid mount-status error');
  assert.equal(invalidPathlessLinux.mountCalls, 0);
  invalidPathlessLinux.cleanup();

  const native = makeHarness(nativeRecovering);
  await native.settle();
  assert.equal(native.mountCalls, 0, 'native worker in progress must own mounting even on first effect');
  native.setStatus(missing);
  await native.tick();
  assert.equal(native.mountCalls, 0, 'native handoff must preserve cancellation after worker stops');
  native.cleanup();

  const manuallyUnmounted = makeHarness(mounted);
  await manuallyUnmounted.settle();
  manuallyUnmounted.setStatus(missing);
  await manuallyUnmounted.tick();
  await manuallyUnmounted.focus();
  assert.equal(manuallyUnmounted.mountCalls, 0, 'poll/focus must not undo explicit user unmount');
  manuallyUnmounted.cleanup();

  const mountedOnce = makeHarness(missing);
  await mountedOnce.settle();
  assert.equal(mountedOnce.mountCalls, 1);
  mountedOnce.setStatus(missing);
  await mountedOnce.tick();
  assert.equal(mountedOnce.mountCalls, 1, 'successful mount hands lifecycle to native');
  assert.equal(mountedOnce.state.nativeMountError, null);
  mountedOnce.cleanup();

  // An unresolved native status must become visible without starting an
  // unsafe mount while the original status probe is still outstanding.
  let resolveStalledStatus;
  const stalledStatus = makeHarness(() => new Promise(resolve => { resolveStalledStatus = resolve; }));
  await stalledStatus.settle();
  for (let i = 0; i < 4; i++) await stalledStatus.tick();
  assert.match(stalledStatus.state.nativeMountError, /taking longer than expected/);
  assert.equal(stalledStatus.statusCalls, 1, 'stalled probe remains single-flight');
  assert.equal(stalledStatus.mountCalls, 0, 'unknown mount status must not trigger mount');
  await stalledStatus.focus();
  assert.equal(stalledStatus.statusCalls, 1, 'focus cannot duplicate an unresolved native status request');
  resolveStalledStatus(missing);
  await stalledStatus.settle();
  assert.equal(stalledStatus.mountCalls, 1, 'valid delayed unmounted status starts one safe mount');
  assert.equal(stalledStatus.state.nativeMountError, null, 'eventual valid status and mount clear stall');
  stalledStatus.cleanup();

  let resolveStalledMount;
  const stalledMount = makeHarness(missing, () => new Promise(resolve => { resolveStalledMount = resolve; }));
  await stalledMount.settle();
  for (let i = 0; i < 4; i++) await stalledMount.tick();
  assert.match(stalledMount.state.nativeMountError, /taking longer than expected/);
  assert.equal(stalledMount.mountCalls, 1, 'stalled native mount must never start a second mount');
  await stalledMount.focus();
  assert.equal(stalledMount.mountCalls, 1, 'focus cannot race a stalled native mount');
  resolveStalledMount();
  await stalledMount.settle();
  assert.equal(stalledMount.state.nativeMountError, null, 'late successful mount clears stall warning');
  await stalledMount.tick();
  assert.equal(stalledMount.mountCalls, 1, 'native handoff remains active after delayed completion');
  stalledMount.cleanup();

  let resolveDisposedStatus;
  const disposedStall = makeHarness(() => new Promise(resolve => { resolveDisposedStatus = resolve; }));
  await disposedStall.settle();
  disposedStall.cleanup();
  for (let i = 0; i < 4; i++) await disposedStall.tick();
  assert.equal(disposedStall.state.nativeMountError, null, 'closed hook does not report old stalled status');
  resolveDisposedStatus(missing);
  await disposedStall.settle();
  assert.equal(disposedStall.mountCalls, 0, 'late status after disposal cannot start mount');

  let attempts = 0;
  const transient = makeHarness(missing, async () => {
    attempts++;
    if (attempts === 1) throw new Error('synthetic provider failure');
  });
  await transient.settle();
  assert.match(transient.state.nativeMountError, /synthetic provider failure/);
  await transient.runMetadata();
  assert.equal(transient.state.phase, 'ready');
  assert.match(transient.state.nativeMountError, /synthetic provider failure/,
    'successful metadata sync must not hide a native mount failure');
  await transient.tick();
  assert.equal(transient.mountCalls, 2, 'retry remains enabled until successful mount');
  assert.equal(transient.state.nativeMountError, null, 'successful retry clears user-visible error');
  await transient.tick();
  assert.equal(transient.mountCalls, 2, 'handoff stops further re-mount attempts');
  transient.cleanup();

  const probe = makeHarness(new Error('synthetic status failure'));
  await probe.settle();
  assert.match(probe.state.nativeMountError, /synthetic status failure/);
  probe.setStatus(missing);
  await probe.focus();
  assert.equal(probe.mountCalls, 1);
  assert.equal(probe.state.nativeMountError, null);
  probe.cleanup();

  const unsupported = {
    supported: false, mounted: false, native_recovery: false, mount_point: null,
  };
  const unsupportedAfterBadProbe = makeHarness({ ...mounted, mount_point: null });
  await unsupportedAfterBadProbe.settle();
  assert.match(unsupportedAfterBadProbe.state.nativeMountError, /Invalid native Drive mount status/);
  const attemptsBeforeUnsupported = unsupportedAfterBadProbe.mountCalls;
  unsupportedAfterBadProbe.setStatus(unsupported);
  await unsupportedAfterBadProbe.focus();
  assert.equal(unsupportedAfterBadProbe.state.nativeMountError, null,
    'valid unsupported native provider clears a stale malformed-probe warning');
  assert.equal(unsupportedAfterBadProbe.mountCalls, attemptsBeforeUnsupported,
    'unsupported provider must not trigger native mounting');
  await unsupportedAfterBadProbe.tick();
  assert.equal(unsupportedAfterBadProbe.mountCalls, attemptsBeforeUnsupported,
    'unsupported periodic status never initiates a mount');
  assert.equal(unsupportedAfterBadProbe.state.securityError, null,
    'unsupported-provider recovery does not change encryption state');
  unsupportedAfterBadProbe.setStatus(missing);
  await unsupportedAfterBadProbe.focus();
  assert.equal(unsupportedAfterBadProbe.mountCalls, attemptsBeforeUnsupported + 1,
    'a later supported provider can recover normally');
  assert.equal(unsupportedAfterBadProbe.state.nativeMountError, null);
  unsupportedAfterBadProbe.cleanup();

  const unsupportedAfterNativeError = makeHarness(new Error('temporary status probe failed'));
  await unsupportedAfterNativeError.settle();
  assert.match(unsupportedAfterNativeError.state.nativeMountError, /temporary status probe failed/);
  unsupportedAfterNativeError.setStatus(unsupported);
  await unsupportedAfterNativeError.tick();
  assert.equal(unsupportedAfterNativeError.state.nativeMountError, null,
    'valid unsupported status clears a stale native-error report');
  assert.equal(unsupportedAfterNativeError.mountCalls, 0);
  await unsupportedAfterNativeError.runMetadata();
  assert.equal(unsupportedAfterNativeError.state.phase, 'ready');
  assert.equal(unsupportedAfterNativeError.state.nativeMountError, null,
    'metadata refresh must not revive stale native provider errors');
  unsupportedAfterNativeError.cleanup();

  // The automatic recovery poll runs separately from the dashboard. It must
  // reject contradictory or malformed native IPC before attempting Mount.
  for (const malformed of [
    null,
    { ...missing, supported: 'true' },
    { ...missing, mounted: 'false' },
    { ...missing, native_recovery: 'maybe' },
    { ...mounted, supported: false },
    { ...mounted, mount_point: null },
    { ...mounted, mount_point: 123 },
    { ...mounted, mount_point: '   ' },
  ]) {
    const bad = makeHarness(malformed);
    await bad.settle();
    assert.equal(bad.mountCalls, 0, 'malformed native status never authorizes a mount');
    assert.match(bad.state.nativeMountError, /Invalid native Drive mount status/,
      'malformed native status is surfaced independently from cloud metadata');
    await bad.runMetadata();
    assert.equal(bad.state.phase, 'ready',
      'normal metadata sync cannot erase a native mount-status error');
    assert.match(bad.state.nativeMountError, /Invalid native Drive mount status/);
    bad.setStatus(missing);
    await bad.focus();
    assert.equal(bad.mountCalls, 1, 'fixed native status permits safe automatic recovery');
    assert.equal(bad.state.nativeMountError, null);
    bad.cleanup();
  }

  const damagedAfterHandoff = makeHarness(mounted);
  await damagedAfterHandoff.settle();
  assert.equal(damagedAfterHandoff.mountCalls, 0);
  damagedAfterHandoff.setStatus({ ...mounted, mount_point: null });
  await damagedAfterHandoff.tick();
  assert.match(damagedAfterHandoff.state.nativeMountError, /Invalid native Drive mount status/);
  damagedAfterHandoff.setStatus(missing);
  await damagedAfterHandoff.tick();
  assert.equal(damagedAfterHandoff.mountCalls, 0,
    'recovering from malformed status must not override an explicit native unmount');
  assert.equal(damagedAfterHandoff.state.nativeMountError, null,
    'valid unmounted native status clears stale malformed-probe warning');
  damagedAfterHandoff.cleanup();

  const app = fs.readFileSync(path.join(root, 'app/src/App.tsx'), 'utf8');
  assert.match(app, /continuity\.nativeMountError && !continuityMountWarningRef\.current/);
  assert.match(app, /TeraRelay Drive is available again/);

  console.log('PASS desktop Drive recovery: validation, races, stalled calls and warning cleanup');
})().catch(error => { console.error(error); process.exitCode = 1; });
