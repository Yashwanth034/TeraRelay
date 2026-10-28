'use strict';
// Desktop Drive status UX regression: mocked Tauri and React render only.
// Never reads or mutates cloud data, mount contents, or login state.
const assert = require('node:assert/strict');
const path = require('node:path');
const fs = require('node:fs');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const file = path.join(root, 'app/src/components/desktop/dashboard/TopBar.tsx');
const js = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
  compilerOptions: {
    module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020,
    jsx: ts.JsxEmit.ReactJSX,
  },
}).outputText;
const statusHelperPath = path.join(root, 'app/src/driveMountStatus.ts');
const statusExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(statusHelperPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: statusExports });
const continuityHelperPath = path.join(root, 'app/src/deviceContinuity.ts');
const continuityExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(continuityHelperPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: continuityExports });
const syncSignalExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(path.join(root, 'app/src/driveSyncSignal.ts'), 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: syncSignalExports, require: name => {
  assert.equal(name, './deviceContinuity');
  return continuityExports;
} });

function harness(initialStatus) {
  let status = initialStatus;
  let error = null;
  let openerError = null;
  let openerHandler = null;
  let clipboardError = null;
  let clipboardHandler = null;
  let openCount = 0;
  let copied = [];
  let requests = 0;
  let mountCalls = 0;
  let syncCalls = 0;
  let snapshotCalls = 0;
  let snapshotHandler = async () => ({ revision: 3, dirty: false, directories: [], files: [], pending: [] });
  let syncHandler = async () => undefined;
  let mountHandler = async () => undefined;
  let statusHandler = null;
  const refs = [];
  let refIndex = 0;
  const toasts = [];
  const states = [];
  const effects = [];
  const callbacks = new Map();
  const intervals = [];
  let hookIndex = 0;
  const fakeReact = {
    useState(initial) {
      const index = hookIndex++;
      if (!(index in states)) states[index] = initial;
      return [states[index], value => {
        states[index] = typeof value === 'function' ? value(states[index]) : value;
      }];
    },
    useRef(initial) {
      const index = refIndex++;
      if (!(index in refs)) refs[index] = { current: initial };
      return refs[index];
    },
    useEffect: callback => effects.push(callback),
  };
  const jsx = (type, props) => ({ type, props: props || {} });
  const stubIcons = new Proxy({}, { get: (_t, key) => String(key) });
  const noop = () => null;
  const stubs = {
    'react': fakeReact,
    '../../../driveMountStatus': statusExports,
    '../../../deviceContinuity': continuityExports,
    '../../../driveSyncSignal': syncSignalExports,
    'react/jsx-runtime': { jsx, jsxs: jsx, Fragment: 'fragment' },
    'lucide-react': stubIcons,
    '../../../context/ThemeContext': { useTheme: () => ({ theme: 'dark', toggleTheme: noop }) },
    'react-i18next': { useTranslation: () => ({ t: text => text }) },
    '../../../context/SettingsContext': {
      useSettings: () => ({ settings: { proxyEnabled: false, proxyLiveStateEnabled: false } }),
    },
    '@tauri-apps/api/core': { invoke: async command => {
      if (command === 'cmd_drive_mount') {
        mountCalls++;
        return mountHandler();
      }
      if (command === 'cmd_drive_sync_metadata') {
        syncCalls++;
        return syncHandler();
      }
      if (command === 'cmd_drive_get_snapshot') {
        snapshotCalls++;
        return snapshotHandler();
      }
      if (command !== 'cmd_drive_status') throw new Error('Unexpected invoke ' + command);
      requests++;
      if (error) throw error;
      if (statusHandler) return statusHandler();
      return { ...status };
    } },
    '@tauri-apps/plugin-shell': { open: async mountPath => {
      openCount++;
      assert.equal(mountPath, '/synthetic/Drive Mount');
      if (openerError) throw openerError;
      if (openerHandler) return openerHandler();
    } },
    '@tauri-apps/plugin-clipboard-manager': { writeText: async content => {
      if (clipboardError) throw clipboardError;
      if (clipboardHandler) await clipboardHandler(content);
      copied.push(content);
    } },
    'sonner': { toast: {
      success: text => toasts.push(['success', text]),
      warning: text => toasts.push(['warning', text]),
      error: text => toasts.push(['error', text]),
    } },
    '../../ui/PremiumPrimitives': {
      PremiumBadge: 'PremiumBadge',
      PremiumButton: 'PremiumButton',
      PremiumIconButton: 'PremiumIconButton',
    },
  };
  const exports = {};
  vm.runInNewContext(js, {
    exports, require: name => {
      if (!(name in stubs)) throw new Error('Unexpected import ' + name);
      return stubs[name];
    },
    window: {
      setInterval: fn => { intervals.push(fn); return intervals.length; },
      clearInterval: noop,
      addEventListener: (name, fn) => callbacks.set(name, fn),
      removeEventListener: name => callbacks.delete(name),
    },
    console: { warn: noop },
  }, { filename: file });
  const props = {
    currentFolderName: 'Synthetic', selectedIds: [], viewMode: 'grid',
    setViewMode: noop, searchTerm: '', onSearchChange: noop,
    onSettingsClick: noop, onRemoteUploadClick: noop,
    onShowMoveModal: noop, onBulkDownload: noop,
    onBulkDelete: noop, onBulkShare: noop,
    onDownloadFolder: noop, onClearSelection: noop,
  };
  const render = () => { hookIndex = 0; refIndex = 0; return exports.TopBar(props); };
  render();
  assert.equal(effects.length, 2);
  const cleanup = effects[0]();
  const settle = async () => {
    for (let index = 0; index < 5; index++) await new Promise(done => setImmediate(done));
  };
  const nodes = (tree, predicate, found = []) => {
    if (Array.isArray(tree)) { tree.forEach(n => nodes(n, predicate, found)); return found; }
    if (!tree || typeof tree !== 'object') return found;
    if (predicate(tree)) found.push(tree);
    nodes(tree.props?.children, predicate, found);
    return found;
  };
  return {
    render,
    cleanup,
    settle,
    get requests() { return requests; },
    get copied() { return copied; },
    get toasts() { return toasts; },
    get openCount() { return openCount; },
    get mountCalls() { return mountCalls; },
    get syncCalls() { return syncCalls; },
    get snapshotCalls() { return snapshotCalls; },
    get states() { return states.slice(); },
    setSyncHandler: callback => { syncHandler = callback; },
    setSnapshotHandler: callback => { snapshotHandler = callback; },
    setMountHandler: callback => { mountHandler = callback; },
    setStatusHandler: callback => { statusHandler = callback; },
    setStatus: next => { status = next; },
    setError: next => { error = next; },
    setOpenerError: next => { openerError = next; },
    setOpenerHandler: next => { openerHandler = next; },
    setClipboardError: next => { clipboardError = next; },
    setClipboardHandler: next => { clipboardHandler = next; },
    refresh: async () => { intervals[0](); await settle(); },
    focus: async () => { callbacks.get('focus')(); await settle(); },
    badge: () => {
      const matches = nodes(render(), node => node.type === 'PremiumBadge');
      const drive = matches.find(node => JSON.stringify(node.props.children).includes('Drive:'));
      assert.ok(drive, 'Drive badge should be rendered');
      return drive;
    },
    openButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Open mounted TeraRelay Drive folder')[0],
    retryButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Mount TeraRelay Drive')[0],
    syncButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Sync TeraRelay Drive metadata')[0],
    copyButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Copy mounted TeraRelay Drive path')[0],
    diagnosticsButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Copy safe TeraRelay Drive diagnostics')[0],
    recheckButton: () => nodes(render(), node => node.type === 'button'
      && node.props['aria-label'] === 'Recheck TeraRelay Drive status')[0],
  };
}

(async () => {
  const mounted = { supported: true, mounted: true, native_recovery: true, mount_point: '/synthetic/Drive Mount' };
  // User-requested support diagnostics copy only verified native health, never
  // the personal mount path. Delayed probes and duplicate clicks stay safe.
  const diagnostics = harness({ ...mounted, provider_kind: 'fuse' });
  await diagnostics.settle();
  assert.ok(diagnostics.diagnosticsButton(), 'mounted Drive exposes support diagnostics');
  let resolveDiag;
  diagnostics.setStatusHandler(() => new Promise(resolve => { resolveDiag = resolve; }));
  diagnostics.diagnosticsButton().props.onClick();
  diagnostics.diagnosticsButton().props.onClick();
  await diagnostics.settle();
  const pendingDiagnosticRequests = diagnostics.requests;
  await diagnostics.refresh();
  await diagnostics.focus();
  assert.equal(diagnostics.requests, pendingDiagnosticRequests,
    'poll and focus cannot supersede a pending user-requested diagnostic status');
  assert.equal(diagnostics.diagnosticsButton().props.disabled, true, 'in-flight diagnostics cannot double copy');
  assert.equal(diagnostics.copied.length, 0, 'never copy a stale cached mount status');
  resolveDiag({ ...mounted, provider_kind: 'fuse' });
  await diagnostics.settle();
  assert.equal(diagnostics.copied.length, 1);
  assert.match(diagnostics.copied[0], /status: verified/);
  assert.match(diagnostics.copied[0], /provider: fuse/);
  assert.match(diagnostics.copied[0], /mounted: true/);
  assert.match(diagnostics.copied[0], /local_revision: 3/);
  assert.match(diagnostics.copied[0], /local_metadata_dirty: false/);
  assert.match(diagnostics.copied[0], /pending_writes: 0/);
  assert.equal(diagnostics.snapshotCalls, 1, 'verified mount also probes the aggregate local metadata snapshot');
  assert.doesNotMatch(diagnostics.copied[0], /synthetic\/Drive Mount|mount_point|passphrase|account/i);
  assert.equal(diagnostics.toasts.at(-1)[0], 'success');
  assert.equal(diagnostics.diagnosticsButton().props.disabled, false, 'completed copy is retryable');
  diagnostics.cleanup();

  const mountDuringDiagnostics = harness({ supported: true, mounted: false,
    native_recovery: false, mount_point: null });
  await mountDuringDiagnostics.settle();
  let resolveMountDiagnostics;
  mountDuringDiagnostics.setStatusHandler(() => new Promise(resolve => { resolveMountDiagnostics = resolve; }));
  mountDuringDiagnostics.diagnosticsButton().props.onClick();
  await mountDuringDiagnostics.settle();
  assert.equal(mountDuringDiagnostics.retryButton().props.disabled, true,
    'manual mount is disabled while diagnostic probe holds the native status lease');
  mountDuringDiagnostics.retryButton().props.onClick();
  assert.equal(mountDuringDiagnostics.mountCalls, 0,
    'manual mount handler must also avoid racing diagnostics');
  resolveMountDiagnostics({ supported: true, mounted: false, native_recovery: false, mount_point: null });
  await mountDuringDiagnostics.settle();
  assert.match(mountDuringDiagnostics.copied[0], /status: verified/);
  assert.equal(mountDuringDiagnostics.retryButton().props.disabled, false,
    'manual mount is available again after support diagnostic finishes');
  mountDuringDiagnostics.cleanup();

  const unsupportedDiagnostics = harness({ supported: false, mounted: false,
    native_recovery: false, mount_point: null, provider_kind: 'unsupported' });
  await unsupportedDiagnostics.settle();
  assert.ok(unsupportedDiagnostics.diagnosticsButton(),
    'unsupported native providers still expose useful support diagnostics');
  await unsupportedDiagnostics.diagnosticsButton().props.onClick();
  await unsupportedDiagnostics.settle();
  assert.match(unsupportedDiagnostics.copied[0], /supported: false/);
  assert.match(unsupportedDiagnostics.copied[0], /provider: unsupported/);
  assert.equal(unsupportedDiagnostics.toasts.at(-1)[0], 'success');
  unsupportedDiagnostics.cleanup();

  const aggregate = harness({ ...mounted, provider_kind: 'fuse' });
  await aggregate.settle();
  aggregate.setSnapshotHandler(async () => ({
    revision: 87, dirty: true,
    directories: [{ name: 'PRIVATE_DIRECTORY_NAME' }],
    files: [{ name: 'PRIVATE_FILE_NAME' }],
    pending: [{ id: 'PRIVATE_PENDING_ID', staging_path: '/private/sentinel' },
      { id: 'ANOTHER_PRIVATE_PENDING_ID' }],
  }));
  aggregate.diagnosticsButton().props.onClick();
  await aggregate.settle();
  assert.match(aggregate.copied[0], /local_revision: 87/);
  assert.match(aggregate.copied[0], /local_metadata_dirty: true/);
  assert.match(aggregate.copied[0], /pending_writes: 2/);
  assert.doesNotMatch(aggregate.copied[0], /PRIVATE_|\/private\/sentinel/,
    'only snapshot counts/dirty/revision, no individual file or staging identities');
  aggregate.setSnapshotHandler(async () => ({ revision: 'unverified', dirty: false,
    files: [], directories: [], pending: [{ id: 'SECRET' }] }));
  aggregate.diagnosticsButton().props.onClick();
  await aggregate.settle();
  assert.match(aggregate.copied.at(-1), /local_metadata: unavailable/);
  assert.doesNotMatch(aggregate.copied.at(-1), /local_revision:|pending_writes:|SECRET/);
  assert.equal(aggregate.badge().props.tone, 'success',
    'unverified snapshot does not falsify a healthy native mount status');
  aggregate.setSnapshotHandler(async () => { throw new Error('PRIVATE_BACKEND_EXCEPTION'); });
  aggregate.diagnosticsButton().props.onClick();
  await aggregate.settle();
  assert.match(aggregate.copied.at(-1), /local_metadata: unavailable/);
  assert.doesNotMatch(aggregate.copied.at(-1), /PRIVATE_BACKEND_EXCEPTION/);
  aggregate.cleanup();

  const slowSnapshot = harness({ ...mounted, provider_kind: 'fuse' });
  await slowSnapshot.settle();
  let resolveSnapshot;
  slowSnapshot.setSnapshotHandler(() => new Promise(resolve => { resolveSnapshot = resolve; }));
  slowSnapshot.diagnosticsButton().props.onClick();
  await slowSnapshot.settle();
  assert.equal(slowSnapshot.snapshotCalls, 1);
  assert.equal(slowSnapshot.copied.length, 0, 'never copy a partial report while snapshot is unresolved');
  assert.equal(slowSnapshot.diagnosticsButton().props.disabled, true);
  const probesBefore = slowSnapshot.requests;
  await slowSnapshot.refresh();
  await slowSnapshot.focus();
  assert.equal(slowSnapshot.requests, probesBefore,
    'snapshot probe retains path-action lease against timer and focus native polls');
  resolveSnapshot({ revision: 88, dirty: false, directories: [], files: [], pending: [{}] });
  await slowSnapshot.settle();
  assert.match(slowSnapshot.copied[0], /pending_writes: 1/);
  assert.equal(slowSnapshot.diagnosticsButton().props.disabled, false);
  slowSnapshot.cleanup();

  const closedSnapshot = harness(mounted);
  await closedSnapshot.settle();
  let resolveClosedSnapshot;
  closedSnapshot.setSnapshotHandler(() => new Promise(resolve => { resolveClosedSnapshot = resolve; }));
  closedSnapshot.diagnosticsButton().props.onClick();
  await closedSnapshot.settle();
  closedSnapshot.cleanup();
  resolveClosedSnapshot({ revision: 3, dirty: false, directories: [], files: [], pending: [] });
  await closedSnapshot.settle();
  assert.equal(closedSnapshot.copied.length, 0, 'closed TopBar cannot copy late aggregate snapshot');
  assert.equal(closedSnapshot.toasts.length, 0, 'closed TopBar cannot toast a late snapshot');

  const invalidDiagnostics = harness({ ...mounted, provider_kind: 'fuse' });
  await invalidDiagnostics.settle();
  invalidDiagnostics.setStatusHandler(() => ({ ...mounted, supported: 'true' }));
  await invalidDiagnostics.diagnosticsButton().props.onClick();
  await invalidDiagnostics.settle();
  assert.match(invalidDiagnostics.copied[0], /status: unavailable/);
  assert.doesNotMatch(invalidDiagnostics.copied[0], /mounted: true|synthetic\/Drive Mount/);
  assert.equal(invalidDiagnostics.badge().props.tone, 'danger');
  assert.equal(invalidDiagnostics.toasts.at(-1)[0], 'warning');
  invalidDiagnostics.setStatusHandler(() => ({ ...mounted, provider_kind: 'unexpected\nmalicious' }));
  await invalidDiagnostics.diagnosticsButton().props.onClick();
  await invalidDiagnostics.settle();
  assert.match(invalidDiagnostics.copied.at(-1), /provider: unrecognized/);
  assert.doesNotMatch(invalidDiagnostics.copied.at(-1), /malicious/);
  invalidDiagnostics.setClipboardError(new Error('synthetic clipboard restriction'));
  await invalidDiagnostics.diagnosticsButton().props.onClick();
  await invalidDiagnostics.settle();
  assert.equal(invalidDiagnostics.toasts.at(-1)[0], 'error', 'failed diagnostics clipboard is actionable');
  invalidDiagnostics.cleanup();

  const closedDiagnostics = harness(mounted);
  await closedDiagnostics.settle();
  let resolveClosedDiagnostics;
  closedDiagnostics.setStatusHandler(() => new Promise(resolve => { resolveClosedDiagnostics = resolve; }));
  closedDiagnostics.diagnosticsButton().props.onClick();
  await closedDiagnostics.settle();
  closedDiagnostics.cleanup();
  resolveClosedDiagnostics(mounted);
  await closedDiagnostics.settle();
  assert.equal(closedDiagnostics.copied.length, 0, 'unmounted TopBar does not copy late diagnostics');
  assert.equal(closedDiagnostics.toasts.length, 0, 'late diagnostics cannot toast after unmount');

  // A separate successful toolbar Sync notifies the upload queue warning
  // without claiming remote publication for QA-local, dirty or bad results.
  const signalTopBar = harness(mounted);
  await signalTopBar.settle();
  let verifiedToolbarSignals = 0;
  const unsubscribeSync = syncSignalExports.subscribeToVerifiedDriveSync(() => {
    verifiedToolbarSignals++;
  });
  for (const [result, expected] of [
    [{ revision: 30, dirty: true, source: 'merged-published' }, 0],
    [{ revision: 31, dirty: false, source: 'qa-local' }, 0],
    [{ revision: 32, dirty: false, source: 'unexpected' }, 0],
    [{ revision: 33, dirty: false, source: 'current' }, 1],
    [{ revision: 34, dirty: false, source: 'merged-published' }, 2],
  ]) {
    signalTopBar.setSyncHandler(async () => result);
    signalTopBar.syncButton().props.onClick();
    await signalTopBar.settle();
    assert.equal(verifiedToolbarSignals, expected,
      'toolbar only publishes strictly verified clean metadata synchronization');
  }
  unsubscribeSync();
  signalTopBar.cleanup();

  const h = harness(mounted);
  await h.settle();
  assert.equal(h.badge().props.tone, 'success');
  assert.ok(h.openButton(), 'mounted Drive exposes one-click opener');
  assert.ok(h.syncButton(), 'mounted Drive exposes explicit metadata refresh');
  let finishSync;
  h.setSyncHandler(() => new Promise(resolve => { finishSync = resolve; }));
  h.syncButton().props.onClick();
  h.syncButton().props.onClick();
  await h.settle();
  assert.equal(h.syncCalls, 1, 'rapid refresh clicks invoke only one native sync');
  assert.equal(h.syncButton().props.disabled, true, 'refresh shows busy state');
  finishSync({ revision: 42, dirty: false, source: 'current' });
  await h.settle();
  assert.equal(h.toasts.at(-1)[0], 'success');
  assert.match(h.toasts.at(-1)[1], /up to date/, 'unchanged Drive should not claim remote import');
  assert.equal(h.syncButton().props.disabled, false, 'completed sync is retryable');
  h.setSyncHandler(async () => { throw new Error('synthetic offline metadata sync'); });
  h.syncButton().props.onClick();
  await h.settle();
  assert.equal(h.toasts.at(-1)[0], 'error', 'sync error is surfaced');
  assert.equal(h.syncButton().props.disabled, false, 'failed sync is retryable');
  // Backend source and dirty flags describe actual publication or import.
  // Unknown/QA/dirty outcomes must not be mistaken for a cloud success.
  for (const [result, expectedTone, textMatch] of [
    [{ revision: 9, dirty: false, source: 'remote' }, 'success', /updated from Telegram/],
    [{ revision: 9, dirty: false, source: 'remote-merged' }, 'success', /merged with updates/],
    [{ revision: 9, dirty: false, source: 'merged-published' }, 'success', /synced to Telegram/],
    [{ revision: 9, dirty: false, source: 'qa-local' }, 'warning', /QA mode/],
    [{ revision: 9, dirty: true, source: 'merged-published' }, 'warning', /waiting to sync/],
    [{ revision: 9, dirty: false, source: 'unknown-future-state' }, 'error', /unverified result/],
    [undefined, 'error', /did not return an outcome/],
  ]) {
    h.setSyncHandler(async () => result);
    h.syncButton().props.onClick();
    await h.settle();
    assert.equal(h.toasts.at(-1)[0], expectedTone, 'sync source/dirty outcome tone');
    assert.match(h.toasts.at(-1)[1], textMatch, 'sync source/dirty description');
  }
  // The explicit mounted-Drive action shares the startup IPC trust boundary.
  // An invalid native revision must never produce a sync success toast even
  // when the source pretends a Telegram upload was published.
  for (const [name, invalidResult] of [
    ['null result', null],
    ['array result', [{ revision: 5, dirty: false, source: 'merged-published' }]],
    ['missing revision', { dirty: false, source: 'merged-published' }],
    ['negative revision', { revision: -1, dirty: false, source: 'merged-published' }],
    ['fractional revision', { revision: 1.5, dirty: false, source: 'merged-published' }],
    ['unsafe revision', { revision: Number.MAX_SAFE_INTEGER + 1, dirty: false, source: 'merged-published' }],
    ['string revision', { revision: '8', dirty: false, source: 'merged-published' }],
    ['string dirty', { revision: 8, dirty: 'false', source: 'merged-published' }],
    ['missing dirty', { revision: 8, source: 'merged-published' }],
    ['nonstring source', { revision: 8, dirty: false, source: 123 }],
    ['unknown success source', { revision: 8, dirty: false, source: 'unexpected-published' }],
  ]) {
    const initialCalls = h.syncCalls;
    const initialToasts = h.toasts.length;
    h.setSyncHandler(async () => invalidResult);
    h.syncButton().props.onClick();
    await h.settle();
    assert.equal(h.syncCalls, initialCalls + 1, `${name}: one native sync attempted`);
    assert.equal(h.toasts.length, initialToasts + 1, `${name}: one user-visible failure`);
    assert.equal(h.toasts.at(-1)[0], 'error', `${name}: never report success`);
    assert.match(h.toasts.at(-1)[1], /Drive metadata sync returned|did not return an outcome/,
      `${name}: explain invalid native result`);
    assert.equal(h.syncButton().props.disabled, false, `${name}: user can retry`);
    assert.equal(h.badge().props.tone, 'success', `${name}: sync error does not falsify mount health`);
  }
  h.setSyncHandler(async () => ({ revision: 10, dirty: false, source: 'merged-published' }));
  h.syncButton().props.onClick();
  await h.settle();
  assert.equal(h.toasts.at(-1)[0], 'success',
    'later valid sync result recovers after malformed response');

  await h.openButton().props.onClick();
  await h.settle();
  assert.equal(h.openCount, 1);

  h.setOpenerError(new Error('synthetic open restriction'));
  await h.openButton().props.onClick();
  await h.settle();
  assert.equal(h.copied[0], mounted.mount_point, 'failure copies exact mount path');
  assert.equal(h.toasts.at(-1)[0], 'warning');

  h.setClipboardError(new Error('synthetic clipboard restriction'));
  await h.openButton().props.onClick();
  await h.settle();
  assert.equal(h.toasts.at(-1)[0], 'error', 'both failures surfaced non-fatally');

  h.setStatus({ ...mounted, mounted: false, native_recovery: true, mount_point: null });
  await h.focus();
  assert.equal(h.badge().props.tone, 'warning');
  assert.ok(JSON.stringify(h.badge().props.children).includes('Recovering'));
  assert.equal(h.openButton(), undefined, 'unmounted Drive is never opened');
  assert.equal(h.syncButton(), undefined, 'unmounted Drive cannot request manual sync');
  h.setStatus({ ...mounted, supported: false, mounted: false, mount_point: null });
  await h.refresh();
  assert.ok(JSON.stringify(h.badge().props.children).includes('Unsupported'));
  assert.equal(h.retryButton(), undefined, 'unsupported providers cannot be mounted');

  const unmounted = { ...mounted, mounted: false, native_recovery: false, mount_point: null };
  h.setStatus(unmounted);
  await h.refresh();
  const retry = h.retryButton();
  assert.ok(retry, 'an unmounted supported Drive exposes an explicit retry action');
  let finishMount;
  h.setMountHandler(() => new Promise(resolve => { finishMount = resolve; }));
  retry.props.onClick();
  retry.props.onClick();
  await h.settle();
  assert.equal(h.mountCalls, 1, 'rapid double click mounts only once');
  assert.equal(h.retryButton().props.disabled, true);
  assert.ok(JSON.stringify(h.badge().props.children).includes('Mounting'));
  h.setStatus(mounted);
  finishMount();
  await h.settle();
  assert.equal(h.retryButton(), undefined);
  assert.ok(h.openButton(), 'mounted retry becomes open Drive control');
  assert.equal(h.toasts.at(-1)[0], 'success');

  h.setStatus(unmounted);
  await h.refresh();
  h.setMountHandler(async () => { throw new Error('synthetic mount error'); });
  h.retryButton().props.onClick();
  await h.settle();
  assert.equal(h.toasts.at(-1)[0], 'error');
  assert.ok(h.retryButton(), 'failed manual mount remains retryable');
  h.setStatus({ ...unmounted, native_recovery: true });
  await h.refresh();
  assert.equal(h.retryButton(), undefined, 'native recovery must not be raced');

  h.setError(new Error('synthetic status unavailable'));
  await h.refresh();
  assert.equal(h.badge().props.tone, 'danger');
  assert.ok(JSON.stringify(h.badge().props.children).includes('Status unavailable'));
  const old = h.requests;
  h.cleanup();
  await h.refresh();
  // Even a stale timer callback cannot fetch after the component unmounts.
  assert.equal(h.requests, old, 'cleanup prevents further native status requests');

  // Native work may finish after the desktop dashboard is closed. In that case
  // neither a new status probe nor a success/failure toast is allowed.
  const lateSuccess = harness(unmounted);
  await lateSuccess.settle();
  let resolveLateMount;
  lateSuccess.setMountHandler(() => new Promise(resolve => { resolveLateMount = resolve; }));
  lateSuccess.retryButton().props.onClick();
  await lateSuccess.settle();
  const successRequests = lateSuccess.requests;
  const successStates = lateSuccess.states;
  lateSuccess.cleanup();
  resolveLateMount();
  await lateSuccess.settle();
  assert.equal(lateSuccess.requests, successRequests, 'late mount success skips status request');
  assert.deepEqual(lateSuccess.toasts, [], 'late mount success skips toast');
  assert.deepEqual(lateSuccess.states, successStates, 'late mount success skips UI updates');

  const lateFailure = harness(unmounted);
  await lateFailure.settle();
  let rejectLateMount;
  lateFailure.setMountHandler(() => new Promise((_resolve, reject) => { rejectLateMount = reject; }));
  lateFailure.retryButton().props.onClick();
  await lateFailure.settle();
  const failureStates = lateFailure.states;
  lateFailure.cleanup();
  rejectLateMount(new Error('synthetic late mount failure'));
  await lateFailure.settle();
  assert.equal(lateFailure.requests, 2, 'late mount rejection skips post-mount status request');
  assert.deepEqual(lateFailure.toasts, [], 'late mount rejection skips toast');
  assert.deepEqual(lateFailure.states, failureStates, 'late mount rejection skips UI updates');

  // A status call already issued by a completed mount can also be late.
  const lateStatus = harness(unmounted);
  await lateStatus.settle();
  let resolveLateStatus;
  let lateStatusChecks = 0;
  lateStatus.setStatusHandler(() => ++lateStatusChecks === 1
    ? Promise.resolve(unmounted)
    : new Promise(resolve => { resolveLateStatus = resolve; }));
  lateStatus.retryButton().props.onClick();
  await lateStatus.settle();
  assert.equal(lateStatus.requests, 3, 'mount completion begins post-mount status probe');
  const statusStates = lateStatus.states;
  lateStatus.cleanup();
  resolveLateStatus(mounted);
  await lateStatus.settle();
  assert.deepEqual(lateStatus.toasts, [], 'late status skips success toast');
  assert.deepEqual(lateStatus.states, statusStates, 'late status skips UI update');

  // A slow periodic status fetched BEFORE retry must not replace the mounted
  // response fetched AFTER retry, even if the old response arrives last.
  const lateOldPoll = harness(unmounted);
  await lateOldPoll.settle();
  let resolveOldPoll;
  let oldPollRequests = 0;
  lateOldPoll.setStatusHandler(() => ++oldPollRequests === 1
    ? new Promise(resolve => { resolveOldPoll = resolve; })
    : Promise.resolve(mounted));
  await lateOldPoll.focus();
  lateOldPoll.retryButton().props.onClick();
  await lateOldPoll.settle();
  assert.ok(lateOldPoll.openButton(), 'newer successful retry exposes mounted Drive');
  resolveOldPoll(unmounted);
  await lateOldPoll.settle();
  assert.ok(lateOldPoll.openButton(), 'older unmounted poll cannot revert mounted Drive');
  assert.equal(lateOldPoll.badge().props.tone, 'success');
  assert.equal(lateOldPoll.toasts.at(-1)[0], 'success');

  const lateOldError = harness(unmounted);
  await lateOldError.settle();
  let rejectOldPoll;
  let errorPollRequests = 0;
  lateOldError.setStatusHandler(() => ++errorPollRequests === 1
    ? new Promise((_resolve, reject) => { rejectOldPoll = reject; })
    : Promise.resolve(mounted));
  await lateOldError.focus();
  lateOldError.retryButton().props.onClick();
  await lateOldError.settle();
  rejectOldPoll(new Error('synthetic old status error'));
  await lateOldError.settle();
  assert.equal(lateOldError.badge().props.tone, 'success', 'older poll error cannot mask mounted status');
  assert.ok(lateOldError.openButton());

  const pollingSuppression = harness(unmounted);
  await pollingSuppression.settle();
  let resolveMountWithPoll;
  pollingSuppression.setMountHandler(() => new Promise(resolve => { resolveMountWithPoll = resolve; }));
  pollingSuppression.retryButton().props.onClick();
  await pollingSuppression.settle();
  const priorRequests = pollingSuppression.requests;
  await pollingSuppression.focus();
  assert.equal(pollingSuppression.requests, priorRequests, 'periodic refresh skips pending manual mount');
  pollingSuppression.setStatus(mounted);
  resolveMountWithPoll();
  await pollingSuppression.settle();
  assert.ok(pollingSuppression.openButton());

  // A stale unmounted badge must not issue a redundant mount after another
  // window or the native recovery worker has changed the authoritative state.
  const alreadyMounted = harness(unmounted);
  await alreadyMounted.settle();
  alreadyMounted.setStatus(mounted);
  await alreadyMounted.retryButton().props.onClick();
  await alreadyMounted.settle();
  assert.equal(alreadyMounted.mountCalls, 0, 'live mounted preflight skips duplicate mount');
  assert.ok(alreadyMounted.openButton());
  assert.equal(alreadyMounted.toasts.at(-1)[0], 'success');

  const recoveryStarted = harness(unmounted);
  await recoveryStarted.settle();
  recoveryStarted.setStatus({ ...unmounted, native_recovery: true });
  await recoveryStarted.retryButton().props.onClick();
  await recoveryStarted.settle();
  assert.equal(recoveryStarted.mountCalls, 0, 'live native recovery preflight skips manual mount');
  assert.ok(JSON.stringify(recoveryStarted.badge().props.children).includes('Recovering'));
  assert.equal(recoveryStarted.toasts.at(-1)[0], 'warning');

  const becameUnsupported = harness(unmounted);
  await becameUnsupported.settle();
  becameUnsupported.setStatus({ ...unmounted, supported: false });
  await becameUnsupported.retryButton().props.onClick();
  await becameUnsupported.settle();
  assert.equal(becameUnsupported.mountCalls, 0, 'unsupported preflight must not mount');
  assert.ok(JSON.stringify(becameUnsupported.badge().props.children).includes('Unsupported'));

  const preflightError = harness(unmounted);
  await preflightError.settle();
  preflightError.setError(new Error('synthetic live status failure'));
  await preflightError.retryButton().props.onClick();
  await preflightError.settle();
  assert.equal(preflightError.mountCalls, 0, 'preflight error must not blindly try mounting');
  assert.equal(preflightError.badge().props.tone, 'danger');
  assert.equal(preflightError.toasts.at(-1)[0], 'error');

  const cancelledPreflight = harness(unmounted);
  await cancelledPreflight.settle();
  let resolveCancelledPreflight;
  cancelledPreflight.setStatusHandler(() => new Promise(resolve => { resolveCancelledPreflight = resolve; }));
  cancelledPreflight.retryButton().props.onClick();
  const cancelledStates = cancelledPreflight.states;
  cancelledPreflight.cleanup();
  resolveCancelledPreflight(unmounted);
  await cancelledPreflight.settle();
  assert.equal(cancelledPreflight.mountCalls, 0, 'closed dashboard skips mount after delayed preflight');
  assert.deepEqual(cancelledPreflight.states, cancelledStates, 'preflight cleanup skips state mutations');
  assert.deepEqual(cancelledPreflight.toasts, [], 'preflight cleanup skips notifications');

  // A metadata sync may finish after TopBar is destroyed; late completion
  // must neither toast nor update the previous React component instance.
  const lateSyncSuccess = harness(mounted);
  await lateSyncSuccess.settle();
  let resolveLateSync;
  lateSyncSuccess.setSyncHandler(() => new Promise(resolve => { resolveLateSync = resolve; }));
  lateSyncSuccess.syncButton().props.onClick();
  await lateSyncSuccess.settle();
  const syncStates = lateSyncSuccess.states;
  lateSyncSuccess.cleanup();
  resolveLateSync();
  await lateSyncSuccess.settle();
  assert.equal(lateSyncSuccess.syncCalls, 1);
  assert.deepEqual(lateSyncSuccess.toasts, [], 'stale successful sync skips notification');
  assert.deepEqual(lateSyncSuccess.states, syncStates, 'stale successful sync skips state updates');

  const lateSyncFailure = harness(mounted);
  await lateSyncFailure.settle();
  let rejectLateSync;
  lateSyncFailure.setSyncHandler(() => new Promise((_resolve, reject) => { rejectLateSync = reject; }));
  lateSyncFailure.syncButton().props.onClick();
  await lateSyncFailure.settle();
  const failureSyncStates = lateSyncFailure.states;
  lateSyncFailure.cleanup();
  rejectLateSync(new Error('synthetic late sync failure'));
  await lateSyncFailure.settle();
  assert.deepEqual(lateSyncFailure.toasts, [], 'stale failed sync skips notification');
  assert.deepEqual(lateSyncFailure.states, failureSyncStates, 'stale failed sync skips state updates');

  // The mounted status badge can be stale when a user clicks its folder path.
  // Opening the stale path could expose a different local directory after an
  // unmount or remount. Verify the live native status and identity first.
  const staleUnmount = harness(mounted);
  await staleUnmount.settle();
  staleUnmount.setStatus({ ...mounted, mounted: false, native_recovery: true, mount_point: null });
  await staleUnmount.openButton().props.onClick();
  await staleUnmount.settle();
  assert.equal(staleUnmount.openCount, 0, 'unmounted Drive never opens stale path');
  assert.equal(staleUnmount.copied.length, 0, 'unmounted path is not copied as fallback');
  assert.equal(staleUnmount.openButton(), undefined);
  assert.equal(staleUnmount.toasts.at(-1)[0], 'warning');

  const movedMount = harness(mounted);
  await movedMount.settle();
  movedMount.setStatus({ ...mounted, mount_point: '/synthetic/Changed Path' });
  await movedMount.openButton().props.onClick();
  await movedMount.settle();
  assert.equal(movedMount.openCount, 0, 'changed mount identity never opens cached path');
  assert.equal(movedMount.copied.length, 0);
  assert.equal(movedMount.toasts.at(-1)[0], 'warning');
  assert.ok(movedMount.openButton(), 'new path is available to inspect after refresh');

  const openProbeFailure = harness(mounted);
  await openProbeFailure.settle();
  openProbeFailure.setError(new Error('synthetic mounted status failure'));
  await openProbeFailure.openButton().props.onClick();
  await openProbeFailure.settle();
  assert.equal(openProbeFailure.openCount, 0, 'cannot open without live status');
  assert.equal(openProbeFailure.badge().props.tone, 'danger');
  assert.equal(openProbeFailure.toasts.at(-1)[0], 'error');

  const sameClick = harness(mounted);
  await sameClick.settle();
  let verifyClick;
  sameClick.setStatusHandler(() => new Promise(resolve => { verifyClick = resolve; }));
  sameClick.openButton().props.onClick();
  sameClick.openButton().props.onClick();
  assert.equal(sameClick.openButton().props.disabled, true, 'folder button disabled during open');
  assert.equal(sameClick.requests, 2, 'duplicate clicks issue one live preflight');
  verifyClick(mounted);
  await sameClick.settle();
  assert.equal(sameClick.openCount, 1, 'single-flight opening launches file manager once');
  assert.equal(sameClick.openButton().props.disabled, false);

  const closedClick = harness(mounted);
  await closedClick.settle();
  let finishClosedClick;
  closedClick.setStatusHandler(() => new Promise(resolve => { finishClosedClick = resolve; }));
  closedClick.openButton().props.onClick();
  const closedStates = closedClick.states;
  closedClick.cleanup();
  finishClosedClick(mounted);
  await closedClick.settle();
  assert.equal(closedClick.openCount, 0, 'closed dashboard never opens after delayed preflight');
  assert.deepEqual(closedClick.states, closedStates, 'late preflight does not mutate old UI');
  assert.deepEqual(closedClick.toasts, [], 'late preflight does not toast');

  // If the OS file-manager launch fails AFTER the dashboard closes, copying
  // the path or displaying its stale failure would be misleading.
  const closedShell = harness(mounted);
  await closedShell.settle();
  let rejectShell;
  closedShell.setOpenerHandler(() => new Promise((_resolve, reject) => { rejectShell = reject; }));
  closedShell.openButton().props.onClick();
  await closedShell.settle();
  assert.equal(closedShell.openCount, 1);
  const closedShellStates = closedShell.states;
  closedShell.cleanup();
  rejectShell(new Error('synthetic late shell rejection'));
  await closedShell.settle();
  assert.equal(closedShell.copied.length, 0, 'late shell error must not copy obsolete path');
  assert.deepEqual(closedShell.states, closedShellStates);
  assert.deepEqual(closedShell.toasts, [], 'late shell error must not notify closed dashboard');

  // Status failures must be manually recoverable without waiting for the
  // next periodic poll/focus event.
  const recovery = harness(unmounted);
  await recovery.settle();
  recovery.setError(new Error('synthetic offline native probe'));
  await recovery.refresh();
  assert.ok(recovery.recheckButton(), 'unavailable native status offers retry control');
  assert.equal(recovery.badge().props.tone, 'danger');
  recovery.setError(null);
  recovery.setStatus(mounted);
  await recovery.recheckButton().props.onClick();
  await recovery.settle();
  assert.equal(recovery.recheckButton(), undefined, 'successful retry restores usual actions');
  assert.ok(recovery.openButton(), 'successful recheck shows mounted Drive');

  const stillUnavailable = harness(mounted);
  await stillUnavailable.settle();
  stillUnavailable.setError(new Error('synthetic native status outage'));
  await stillUnavailable.refresh();
  stillUnavailable.recheckButton().props.onClick();
  await stillUnavailable.settle();
  assert.ok(stillUnavailable.recheckButton(), 'failed recheck stays retryable');
  assert.equal(stillUnavailable.toasts.at(-1)[0], 'error');
  assert.equal(stillUnavailable.badge().props.tone, 'danger');

  const waitingRetry = harness(mounted);
  await waitingRetry.settle();
  waitingRetry.setError(new Error('synthetic offline check'));
  await waitingRetry.refresh();
  waitingRetry.setError(null);
  let finishRetry;
  waitingRetry.setStatusHandler(() => new Promise(resolve => { finishRetry = resolve; }));
  const checksBefore = waitingRetry.requests;
  waitingRetry.recheckButton().props.onClick();
  waitingRetry.recheckButton().props.onClick();
  assert.equal(waitingRetry.requests, checksBefore + 1, 'double-click probes only once');
  assert.equal(waitingRetry.recheckButton().props.disabled, true);
  await waitingRetry.focus();
  assert.equal(waitingRetry.requests, checksBefore + 1, 'periodic polling skips a manual probe');
  finishRetry({ ...mounted, mounted: false, native_recovery: true, mount_point: null });
  await waitingRetry.settle();
  assert.ok(waitingRetry.recheckButton(), 'native Recovering state offers on-demand recheck');
  assert.ok(JSON.stringify(waitingRetry.badge().props.children).includes('Recovering'));

  const closedRetry = harness(mounted);
  await closedRetry.settle();
  closedRetry.setError(new Error('synthetic status outage'));
  await closedRetry.refresh();
  closedRetry.setError(null);
  let finishClosedRetry;
  closedRetry.setStatusHandler(() => new Promise(resolve => { finishClosedRetry = resolve; }));
  closedRetry.recheckButton().props.onClick();
  const closedRetryStates = closedRetry.states;
  closedRetry.cleanup();
  finishClosedRetry(mounted);
  await closedRetry.settle();
  assert.deepEqual(closedRetry.states, closedRetryStates, 'delayed retry cannot update unmounted UI');
  assert.deepEqual(closedRetry.toasts, [], 'delayed retry does not toast');

  const failedAfterClose = harness(mounted);
  await failedAfterClose.settle();
  failedAfterClose.setError(new Error('synthetic initial status outage'));
  await failedAfterClose.refresh();
  failedAfterClose.setError(null);
  let rejectClosedRetry;
  failedAfterClose.setStatusHandler(() => new Promise((_resolve, reject) => { rejectClosedRetry = reject; }));
  failedAfterClose.recheckButton().props.onClick();
  const failedAfterCloseStates = failedAfterClose.states;
  failedAfterClose.cleanup();
  rejectClosedRetry(new Error('synthetic late status rejection'));
  await failedAfterClose.settle();
  assert.deepEqual(failedAfterClose.states, failedAfterCloseStates);
  assert.deepEqual(failedAfterClose.toasts, [], 'late error cannot notify closed dashboard');

  // Native recovery can finish well before the next 15-second poll. The
  // Recovering badge provides a manual recheck without initiating a new mount.
  const recovering = { ...unmounted, native_recovery: true };
  const recoveryCompleted = harness(recovering);
  await recoveryCompleted.settle();
  assert.ok(recoveryCompleted.recheckButton(), 'Recovering exposes an immediate status recheck');
  assert.equal(recoveryCompleted.retryButton(), undefined, 'recovery never exposes duplicate Mount');
  assert.equal(recoveryCompleted.badge().props.tone, 'warning');
  assert.match(JSON.stringify(recoveryCompleted.badge().props.children), /Recovering/);
  recoveryCompleted.setStatus(mounted);
  await recoveryCompleted.recheckButton().props.onClick();
  await recoveryCompleted.settle();
  assert.equal(recoveryCompleted.recheckButton(), undefined, 'success returns to mounted badge');
  assert.ok(recoveryCompleted.openButton(), 'completed recovery can be opened immediately');
  assert.equal(recoveryCompleted.mountCalls, 0, 'rechecking recovery never invokes Mount');

  const recoveryStopped = harness(recovering);
  await recoveryStopped.settle();
  recoveryStopped.setStatus(unmounted);
  await recoveryStopped.recheckButton().props.onClick();
  await recoveryStopped.settle();
  assert.equal(recoveryStopped.recheckButton(), undefined);
  assert.ok(recoveryStopped.retryButton(), 'recovery ending unmounted enables manual Mount');
  assert.equal(recoveryStopped.mountCalls, 0);

  const recoveryStillRunning = harness(recovering);
  await recoveryStillRunning.settle();
  let finishRecoveryProbe;
  recoveryStillRunning.setStatusHandler(
    () => new Promise(resolve => { finishRecoveryProbe = resolve; }));
  const beforeRecoveryChecks = recoveryStillRunning.requests;
  recoveryStillRunning.recheckButton().props.onClick();
  recoveryStillRunning.recheckButton().props.onClick();
  assert.equal(recoveryStillRunning.recheckButton().props.disabled, true,
    'busy recovery recheck disables repeated clicks');
  await recoveryStillRunning.focus();
  assert.equal(recoveryStillRunning.requests, beforeRecoveryChecks + 1,
    'background polling cannot race manual recovery recheck');
  finishRecoveryProbe(recovering);
  await recoveryStillRunning.settle();
  assert.ok(recoveryStillRunning.recheckButton(), 'still recovering remains recheckable');
  assert.equal(recoveryStillRunning.recheckButton().props.disabled, false);

  const recoveryProbeError = harness(recovering);
  await recoveryProbeError.settle();
  recoveryProbeError.setError(new Error('synthetic interrupted provider'));
  await recoveryProbeError.recheckButton().props.onClick();
  await recoveryProbeError.settle();
  assert.equal(recoveryProbeError.toasts.at(-1)[0], 'error');
  assert.equal(recoveryProbeError.badge().props.tone, 'danger');
  assert.ok(recoveryProbeError.recheckButton(), 'error remains recheckable');

  const lateRecoveryProbe = harness(recovering);
  await lateRecoveryProbe.settle();
  let finishLateRecovery;
  lateRecoveryProbe.setStatusHandler(() => new Promise(resolve => { finishLateRecovery = resolve; }));
  lateRecoveryProbe.recheckButton().props.onClick();
  const beforeClosingRecovery = lateRecoveryProbe.states;
  lateRecoveryProbe.cleanup();
  finishLateRecovery(mounted);
  await lateRecoveryProbe.settle();
  assert.deepEqual(lateRecoveryProbe.states, beforeClosingRecovery,
    'closed dashboard ignores delayed recovery result');
  assert.deepEqual(lateRecoveryProbe.toasts, []);

  // Direct copy exposes the mount path without relying on shell.open. Every
  // copy verifies the current native mount identity before writing clipboard.
  const copy = harness(mounted);
  await copy.settle();
  assert.ok(copy.copyButton(), 'mounted Drive offers a copy-path action');
  await copy.copyButton().props.onClick();
  await copy.settle();
  assert.deepEqual(copy.copied, [mounted.mount_point]);
  assert.equal(copy.toasts.at(-1)[0], 'success');
  assert.equal(copy.openCount, 0, 'copy action must never invoke OS file manager');
  assert.equal(copy.copyButton().props.disabled, false);

  const unavailableCopy = harness(mounted);
  await unavailableCopy.settle();
  unavailableCopy.setStatus({ ...mounted, mounted: false, native_recovery: true, mount_point: null });
  await unavailableCopy.copyButton().props.onClick();
  await unavailableCopy.settle();
  assert.deepEqual(unavailableCopy.copied, [], 'unmounted Drive path cannot be copied');
  assert.equal(unavailableCopy.copyButton(), undefined, 'copy hidden after unmount');
  assert.equal(unavailableCopy.toasts.at(-1)[0], 'warning');

  const movedCopy = harness(mounted);
  await movedCopy.settle();
  movedCopy.setStatus({ ...mounted, mount_point: '/synthetic/Another Mount' });
  await movedCopy.copyButton().props.onClick();
  await movedCopy.settle();
  assert.deepEqual(movedCopy.copied, [], 'stale mount identity cannot be copied');
  assert.equal(movedCopy.toasts.at(-1)[0], 'warning');
  assert.ok(movedCopy.copyButton(), 'new mount path appears after status refresh');

  const deniedCopy = harness(mounted);
  await deniedCopy.settle();
  deniedCopy.setClipboardError(new Error('synthetic clipboard unavailable'));
  await deniedCopy.copyButton().props.onClick();
  await deniedCopy.settle();
  assert.equal(deniedCopy.toasts.at(-1)[0], 'error', 'clipboard failures are actionable');
  assert.deepEqual(deniedCopy.copied, []);
  assert.equal(deniedCopy.badge().props.tone, 'success', 'clipboard error is not a mount failure');

  const failedCopyProbe = harness(mounted);
  await failedCopyProbe.settle();
  failedCopyProbe.setError(new Error('synthetic live Drive probe failure'));
  await failedCopyProbe.copyButton().props.onClick();
  await failedCopyProbe.settle();
  assert.deepEqual(failedCopyProbe.copied, []);
  assert.equal(failedCopyProbe.badge().props.tone, 'danger', 'status probe failures surface status unavailable');
  assert.equal(failedCopyProbe.toasts.at(-1)[0], 'error');

  const copySingleFlight = harness(mounted);
  await copySingleFlight.settle();
  let resumeCopyProbe;
  copySingleFlight.setStatusHandler(() => new Promise(resolve => { resumeCopyProbe = resolve; }));
  const beforeCopyRequests = copySingleFlight.requests;
  copySingleFlight.copyButton().props.onClick();
  copySingleFlight.copyButton().props.onClick();
  assert.equal(copySingleFlight.requests, beforeCopyRequests + 1, 'rapid copy clicks cause one status probe');
  assert.equal(copySingleFlight.copyButton().props.disabled, true);
  await copySingleFlight.focus();
  assert.equal(copySingleFlight.requests, beforeCopyRequests + 1, 'periodic polling skips explicit copy preflight');
  resumeCopyProbe(mounted);
  await copySingleFlight.settle();
  assert.deepEqual(copySingleFlight.copied, [mounted.mount_point]);

  const abandonedCopy = harness(mounted);
  await abandonedCopy.settle();
  let resumeAbandonedCopy;
  abandonedCopy.setStatusHandler(() => new Promise(resolve => { resumeAbandonedCopy = resolve; }));
  abandonedCopy.copyButton().props.onClick();
  const beforeCloseCopyState = abandonedCopy.states;
  abandonedCopy.cleanup();
  resumeAbandonedCopy(mounted);
  await abandonedCopy.settle();
  assert.deepEqual(abandonedCopy.copied, [], 'late preflight cannot copy path after component closure');
  assert.deepEqual(abandonedCopy.toasts, []);
  assert.deepEqual(abandonedCopy.states, beforeCloseCopyState);

  const pendingClipboard = harness(mounted);
  await pendingClipboard.settle();
  let resumeClipboard;
  pendingClipboard.setClipboardHandler(() => new Promise(resolve => { resumeClipboard = resolve; }));
  pendingClipboard.copyButton().props.onClick();
  await pendingClipboard.settle();
  const clipboardStates = pendingClipboard.states;
  pendingClipboard.cleanup();
  resumeClipboard();
  await pendingClipboard.settle();
  assert.deepEqual(pendingClipboard.toasts, [], 'already-issued clipboard completion cannot toast after close');
  assert.deepEqual(pendingClipboard.states, clipboardStates);

  // Copy and Open target the same physical mount. They must share a
  // synchronous lock so one deferred status check cannot authorize stale work
  // while the other action is updating native mount identity.
  const openThenCopy = harness(mounted);
  await openThenCopy.settle();
  let finishOpenProbe;
  openThenCopy.setStatusHandler(() => new Promise(resolve => { finishOpenProbe = resolve; }));
  const priorOpenChecks = openThenCopy.requests;
  openThenCopy.openButton().props.onClick();
  assert.equal(openThenCopy.copyButton().props.disabled, true, 'copy disabled during open');
  openThenCopy.copyButton().props.onClick();
  await openThenCopy.focus();
  assert.equal(openThenCopy.requests, priorOpenChecks + 1,
    'copy and background polling cannot issue competing probes while opening');
  finishOpenProbe(mounted);
  await openThenCopy.settle();
  assert.equal(openThenCopy.openCount, 1);
  assert.deepEqual(openThenCopy.copied, []);
  assert.equal(openThenCopy.copyButton().props.disabled, false);
  openThenCopy.setStatusHandler(null);
  await openThenCopy.copyButton().props.onClick();
  await openThenCopy.settle();
  assert.deepEqual(openThenCopy.copied, [mounted.mount_point],
    'copy is available after open finishes');

  const copyThenOpen = harness(mounted);
  await copyThenOpen.settle();
  let finishCopyProbe;
  copyThenOpen.setStatusHandler(() => new Promise(resolve => { finishCopyProbe = resolve; }));
  const priorCopyChecks = copyThenOpen.requests;
  copyThenOpen.copyButton().props.onClick();
  assert.equal(copyThenOpen.openButton().props.disabled, true, 'open disabled during copy');
  copyThenOpen.openButton().props.onClick();
  await copyThenOpen.focus();
  assert.equal(copyThenOpen.requests, priorCopyChecks + 1,
    'open and background polling cannot race a pending copy');
  finishCopyProbe(mounted);
  await copyThenOpen.settle();
  assert.deepEqual(copyThenOpen.copied, [mounted.mount_point]);
  assert.equal(copyThenOpen.openCount, 0);
  assert.equal(copyThenOpen.openButton().props.disabled, false);
  copyThenOpen.setStatusHandler(null);
  await copyThenOpen.openButton().props.onClick();
  await copyThenOpen.settle();
  assert.equal(copyThenOpen.openCount, 1, 'open works after copy settles');

  // Ownership must extend through the OS operation, not just its live
  // metadata preflight. A slow clipboard operation cannot be overlapped by Open.
  const deferredClipboard = harness(mounted);
  await deferredClipboard.settle();
  let finishDeferredClipboard;
  deferredClipboard.setClipboardHandler(() => new Promise(resolve => { finishDeferredClipboard = resolve; }));
  deferredClipboard.copyButton().props.onClick();
  await deferredClipboard.settle();
  assert.equal(deferredClipboard.openButton().props.disabled, true);
  deferredClipboard.openButton().props.onClick();
  assert.equal(deferredClipboard.openCount, 0);
  finishDeferredClipboard();
  await deferredClipboard.settle();
  assert.equal(deferredClipboard.openButton().props.disabled, false);

  const deferredShell = harness(mounted);
  await deferredShell.settle();
  let finishDeferredShell;
  deferredShell.setOpenerHandler(() => new Promise(resolve => { finishDeferredShell = resolve; }));
  deferredShell.openButton().props.onClick();
  await deferredShell.settle();
  assert.equal(deferredShell.copyButton().props.disabled, true);
  deferredShell.copyButton().props.onClick();
  assert.deepEqual(deferredShell.copied, []);
  finishDeferredShell();
  await deferredShell.settle();
  assert.equal(deferredShell.copyButton().props.disabled, false);

  // Manual metadata sync should verify the current native mount before
  // causing any cloud-side metadata work, not trust the 15-second-old badge.
  const unmountedBeforeSync = harness(mounted);
  await unmountedBeforeSync.settle();
  unmountedBeforeSync.setStatus({ ...mounted, mounted: false, native_recovery: true, mount_point: null });
  await unmountedBeforeSync.syncButton().props.onClick();
  await unmountedBeforeSync.settle();
  assert.equal(unmountedBeforeSync.syncCalls, 0, 'native unmount skips cloud metadata sync');
  assert.equal(unmountedBeforeSync.syncButton(), undefined, 'unmounted Drive hides sync control');
  assert.equal(unmountedBeforeSync.toasts.at(-1)[0], 'warning');

  const movedBeforeSync = harness(mounted);
  await movedBeforeSync.settle();
  movedBeforeSync.setStatus({ ...mounted, mount_point: '/synthetic/Replacement Mount' });
  await movedBeforeSync.syncButton().props.onClick();
  await movedBeforeSync.settle();
  assert.equal(movedBeforeSync.syncCalls, 0, 'changed native mount location skips stale sync');
  assert.equal(movedBeforeSync.toasts.at(-1)[0], 'warning');
  assert.ok(movedBeforeSync.syncButton(), 'new mount location remains visible after recheck');

  const unsupportedBeforeSync = harness(mounted);
  await unsupportedBeforeSync.settle();
  unsupportedBeforeSync.setStatus({ ...mounted, supported: false });
  await unsupportedBeforeSync.syncButton().props.onClick();
  await unsupportedBeforeSync.settle();
  assert.equal(unsupportedBeforeSync.syncCalls, 0, 'unsupported native provider skips sync');
  assert.equal(unsupportedBeforeSync.toasts.at(-1)[0], 'error',
    'contradictory unsupported+mounted native payload is not a valid status');

  const nativeSyncProbeError = harness(mounted);
  await nativeSyncProbeError.settle();
  nativeSyncProbeError.setError(new Error('synthetic Drive provider offline'));
  await nativeSyncProbeError.syncButton().props.onClick();
  await nativeSyncProbeError.settle();
  assert.equal(nativeSyncProbeError.syncCalls, 0, 'failed preflight never starts cloud sync');
  assert.equal(nativeSyncProbeError.badge().props.tone, 'danger');
  assert.equal(nativeSyncProbeError.toasts.at(-1)[0], 'error');
  assert.match(nativeSyncProbeError.toasts.at(-1)[1], /verify.*mount before syncing/);

  const pendingSyncProbe = harness(mounted);
  await pendingSyncProbe.settle();
  let finishSyncProbe;
  pendingSyncProbe.setStatusHandler(() => new Promise(resolve => { finishSyncProbe = resolve; }));
  const beforeSyncProbe = pendingSyncProbe.requests;
  pendingSyncProbe.syncButton().props.onClick();
  pendingSyncProbe.syncButton().props.onClick();
  assert.equal(pendingSyncProbe.syncButton().props.disabled, true, 'manual sync disabled while checking native mount');
  assert.equal(pendingSyncProbe.openButton().props.disabled, true, 'folder open blocked while verifying sync');
  assert.equal(pendingSyncProbe.copyButton().props.disabled, true, 'path copy blocked while verifying sync');
  pendingSyncProbe.openButton().props.onClick();
  pendingSyncProbe.copyButton().props.onClick();
  await pendingSyncProbe.focus();
  assert.equal(pendingSyncProbe.requests, beforeSyncProbe + 1,
    'duplicate sync, open, copy and focus do not compete with the mount verification');
  assert.equal(pendingSyncProbe.openCount, 0);
  assert.deepEqual(pendingSyncProbe.copied, []);
  assert.equal(pendingSyncProbe.syncCalls, 0, 'cloud sync waits for verified native mount');
  let completePendingSync;
  pendingSyncProbe.setSyncHandler(() => new Promise(resolve => { completePendingSync = resolve; }));
  finishSyncProbe(mounted);
  await pendingSyncProbe.settle();
  assert.equal(pendingSyncProbe.syncCalls, 1);
  assert.equal(pendingSyncProbe.openButton().props.disabled, true,
    'native folder operations remain disabled through cloud sync');
  pendingSyncProbe.openButton().props.onClick();
  pendingSyncProbe.copyButton().props.onClick();
  await pendingSyncProbe.focus();
  assert.equal(pendingSyncProbe.requests, beforeSyncProbe + 1,
    'background status polling still suppressed during active metadata publication');
  assert.equal(pendingSyncProbe.openCount, 0);
  completePendingSync({ revision: 10, dirty: false, source: 'current' });
  await pendingSyncProbe.settle();
  assert.equal(pendingSyncProbe.openButton().props.disabled, false);
  assert.equal(pendingSyncProbe.copyButton().props.disabled, false);
  assert.equal(pendingSyncProbe.syncButton().props.disabled, false);
  pendingSyncProbe.setStatusHandler(null);
  await pendingSyncProbe.copyButton().props.onClick();
  await pendingSyncProbe.settle();
  assert.deepEqual(pendingSyncProbe.copied, [mounted.mount_point],
    'path actions become available after metadata sync settles');

  const abandonedSyncProbe = harness(mounted);
  await abandonedSyncProbe.settle();
  let finishAbandonedSyncProbe;
  abandonedSyncProbe.setStatusHandler(
    () => new Promise(resolve => { finishAbandonedSyncProbe = resolve; }));
  abandonedSyncProbe.syncButton().props.onClick();
  const beforeAbandonedStates = abandonedSyncProbe.states;
  abandonedSyncProbe.cleanup();
  finishAbandonedSyncProbe(mounted);
  await abandonedSyncProbe.settle();
  assert.equal(abandonedSyncProbe.syncCalls, 0, 'closed dashboard skips cloud sync after late preflight');
  assert.deepEqual(abandonedSyncProbe.states, beforeAbandonedStates);
  assert.deepEqual(abandonedSyncProbe.toasts, []);

  // A running path action also fences metadata sync, rather than allowing a
  // second native status request to race folder opening.
  const openBeforeSync = harness(mounted);
  await openBeforeSync.settle();
  let finishOpenBeforeSync;
  openBeforeSync.setOpenerHandler(() => new Promise(resolve => { finishOpenBeforeSync = resolve; }));
  openBeforeSync.openButton().props.onClick();
  await openBeforeSync.settle();
  assert.equal(openBeforeSync.syncButton().props.disabled, true);
  const requestsBeforeBlockedSync = openBeforeSync.requests;
  openBeforeSync.syncButton().props.onClick();
  assert.equal(openBeforeSync.requests, requestsBeforeBlockedSync);
  assert.equal(openBeforeSync.syncCalls, 0);
  finishOpenBeforeSync();
  await openBeforeSync.settle();
  assert.equal(openBeforeSync.syncButton().props.disabled, false);

  // IPC typings never check runtime payloads. A malformed mounted status
  // cannot authorize shell.open, clipboard, metadata sync, or Mount.
  const validate = statusExports.validateNativeDriveMountStatus;
  for (const invalid of [
    null, {}, [], { ...mounted, supported: 'true' },
    { ...mounted, mounted: 'yes' }, { ...mounted, native_recovery: 'yes' },
    { ...mounted, mount_point: null }, { ...mounted, mount_point: '' },
    { ...mounted, mount_point: '    ' }, { ...mounted, mount_point: 999 },
    { ...mounted, supported: false },
  ]) {
    assert.throws(() => validate(invalid), /Invalid native Drive mount status/,
      'malformed native mount metadata must be rejected');
  }
  assert.equal(validate(mounted).mount_point, mounted.mount_point);
  assert.equal(validate({ ...mounted, mounted: false, mount_point: null }).mounted, false);
  assert.equal(validate({ ...mounted, supported: false, mounted: false, mount_point: null }).supported,
    false, 'unsupported valid provider retains existing unsupported badge');

  const malformedInitial = harness({ ...mounted, mount_point: null });
  await malformedInitial.settle();
  assert.equal(malformedInitial.badge().props.tone, 'danger');
  assert.equal(malformedInitial.openButton(), undefined);
  assert.equal(malformedInitial.copyButton(), undefined);
  assert.equal(malformedInitial.syncButton(), undefined);
  assert.ok(malformedInitial.recheckButton(), 'invalid initial status offers explicit recovery');
  malformedInitial.setStatus(mounted);
  await malformedInitial.recheckButton().props.onClick();
  await malformedInitial.settle();
  assert.equal(malformedInitial.badge().props.tone, 'success',
    'valid fresh status recovers without duplicate mount');
  assert.ok(malformedInitial.openButton());
  assert.equal(malformedInitial.mountCalls, 0);

  for (const [name, button] of [
    ['open', 'openButton'],
    ['copy', 'copyButton'],
    ['sync', 'syncButton'],
  ]) {
    const faultyPreflight = harness(mounted);
    await faultyPreflight.settle();
    faultyPreflight.setStatus({ ...mounted, mount_point: null });
    await faultyPreflight[button]().props.onClick();
    await faultyPreflight.settle();
    assert.equal(faultyPreflight.badge().props.tone, 'danger', name + ' must reject invalid live status');
    assert.equal(faultyPreflight.openCount, 0, name + ' cannot open unknown OS path');
    assert.deepEqual(faultyPreflight.copied, [], name + ' cannot copy unknown mount path');
    assert.equal(faultyPreflight.syncCalls, 0, name + ' cannot sync unverified mounted Drive');
    assert.equal(faultyPreflight.toasts.at(-1)[0], 'error');
    assert.ok(faultyPreflight.recheckButton());
    faultyPreflight.setStatus(mounted);
    await faultyPreflight.recheckButton().props.onClick();
    await faultyPreflight.settle();
    assert.equal(faultyPreflight.badge().props.tone, 'success',
      'status recheck recovers after invalid ' + name + ' preflight');
  }

  const malformedRetry = harness(unmounted);
  await malformedRetry.settle();
  malformedRetry.setStatus({ ...mounted, mount_point: null });
  await malformedRetry.retryButton().props.onClick();
  await malformedRetry.settle();
  assert.equal(malformedRetry.mountCalls, 0, 'invalid status cannot start a native mount');
  assert.equal(malformedRetry.badge().props.tone, 'danger');
  assert.equal(malformedRetry.toasts.at(-1)[0], 'error');

  const malformedRecheck = harness({ ...mounted, mount_point: null });
  await malformedRecheck.settle();
  await malformedRecheck.recheckButton().props.onClick();
  await malformedRecheck.settle();
  assert.equal(malformedRecheck.badge().props.tone, 'danger');
  assert.equal(malformedRecheck.toasts.at(-1)[0], 'error');
  assert.ok(malformedRecheck.recheckButton(),
    'malformed recheck remains actionable once native provider recovers');

  // macOS File Provider does not publish an OS mount path. The Rust native
  // implementation explicitly calls mark_provider_connected(None); it still
  // reports mounted=true and provider_kind='macos-file-provider'. Only that
  // exact provider may be mounted without a path.
  const macosProvider = {
    supported: true, mounted: true, native_recovery: false,
    mount_point: null, provider_kind: 'macos-file-provider',
  };
  assert.equal(validate(macosProvider).mounted, true);
  assert.equal(validate(macosProvider).mount_point, null);
  assert.throws(() => validate({ ...macosProvider, provider_kind: 'fuse' }),
    /Invalid native Drive mount status/, 'Linux FUSE mount cannot omit mount location');
  assert.throws(() => validate({ ...macosProvider, provider_kind: 'windows-cloud-files' }),
    /Invalid native Drive mount status/, 'Windows cloud-root mount cannot omit location');
  assert.throws(() => validate({ ...macosProvider, provider_kind: 'MacOS-file-provider' }),
    /Invalid native Drive mount status/, 'only exact native macOS provider label is allowed');
  assert.throws(() => validate({ ...macosProvider, supported: false }),
    /Invalid native Drive mount status/, 'unsupported macOS provider cannot claim mounted');

  const macos = harness(macosProvider);
  await macos.settle();
  assert.equal(macos.badge().props.tone, 'success', 'valid macOS provider reports Mounted');
  assert.match(JSON.stringify(macos.badge().props.children), /Mounted/);
  assert.equal(macos.openButton(), undefined, 'Finder File Provider has no OS mount path to open');
  assert.equal(macos.copyButton(), undefined, 'Finder File Provider has no location to copy');
  assert.ok(macos.syncButton(), 'macOS File Provider still permits metadata synchronization');
  macos.setSyncHandler(async () => ({ source: 'current', revision: 10, dirty: false }));
  macos.syncButton().props.onClick();
  await macos.settle();
  assert.equal(macos.syncCalls, 1, 'pathless Finder provider verifies status and syncs metadata');
  assert.equal(macos.toasts.at(-1)[0], 'success');
  assert.equal(macos.openCount, 0);
  assert.deepEqual(macos.copied, []);
  assert.equal(macos.mountCalls, 0, 'status polling must not mount a healthy File Provider');

  const changedMacos = harness(macosProvider);
  await changedMacos.settle();
  changedMacos.setStatus({ ...macosProvider, provider_kind: 'fuse' });
  changedMacos.syncButton().props.onClick();
  await changedMacos.settle();
  assert.equal(changedMacos.syncCalls, 0, 'invalid provider swap blocks cloud metadata sync');
  assert.equal(changedMacos.badge().props.tone, 'danger');
  assert.equal(changedMacos.toasts.at(-1)[0], 'error');

  const macosProviderSwap = harness(macosProvider);
  await macosProviderSwap.settle();
  macosProviderSwap.setStatus({ ...mounted, provider_kind: 'windows-cloud-files' });
  macosProviderSwap.syncButton().props.onClick();
  await macosProviderSwap.settle();
  assert.equal(macosProviderSwap.syncCalls, 0, 'changed provider with OS path cannot sync as old macOS mount');
  assert.equal(macosProviderSwap.toasts.at(-1)[0], 'warning');

  const macosUnmounted = harness({ ...macosProvider, mounted: false });
  await macosUnmounted.settle();
  assert.ok(macosUnmounted.retryButton(), 'disconnected macOS File Provider remains mount-retryable');
  macosUnmounted.setStatus(macosProvider);
  macosUnmounted.retryButton().props.onClick();
  await macosUnmounted.settle();
  assert.equal(macosUnmounted.mountCalls, 0, 'already-connected pathless macOS provider avoids duplicate mount');
  assert.equal(macosUnmounted.toasts.at(-1)[0], 'success');
  assert.match(macosUnmounted.toasts.at(-1)[1], /Finder/,
    'mount success message does not misleadingly claim path can be opened');

  console.log('PASS desktop Drive status: macOS pathless File Provider, validated IPC, mounted preflights and recovery');
})().catch(error => { console.error(error); process.exitCode = 1; });
