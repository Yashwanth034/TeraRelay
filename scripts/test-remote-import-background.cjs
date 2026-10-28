const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
const modalFile = path.join(root, 'app/src/components/desktop/dashboard/RemoteUploadModal.tsx');
const queueFile = path.join(root, 'app/src/components/desktop/dashboard/RemoteImportQueue.tsx');
const cases = [];
function test(name, fn) { cases.push({ name, fn }); }
function defer() { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; }
function harness(file, initialProps, options = {}) {
  let props = initialProps, tree, cursor = 0, dirty = true;
  const cells = [], pending = [], toasts = [];
  const same = (a, b) => a && b && a.length === b.length && a.every((v, i) => Object.is(v, b[i]));
  const react = {
    useState(initial) { const i = cursor++; if (!cells[i]) cells[i] = { value: typeof initial === 'function' ? initial() : initial }; return [cells[i].value, value => { cells[i].value = typeof value === 'function' ? value(cells[i].value) : value; dirty = true; }]; },
    useRef(initial) { const i = cursor++; return cells[i] ??= { current: initial }; },
    useMemo(fn, deps) { const i = cursor++; if (!cells[i] || !same(cells[i].deps, deps)) cells[i] = { value: fn(), deps }; return cells[i].value; },
    useCallback(fn, deps) { return react.useMemo(() => fn, deps); },
    useEffect(fn, deps) { const i = cursor++; if (!cells[i] || !same(cells[i].deps, deps)) { const old = cells[i]; cells[i] = { deps }; pending.push(() => { old?.cleanup?.(); cells[i].cleanup = fn(); }); } },
  };
  const cache = new Map();
  function load(file) {
    if (cache.has(file)) return cache.get(file).exports;
    const mod = { exports: {} }; cache.set(file, mod);
    const output = ts.transpileModule(fs.readFileSync(file, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.ReactJSX, esModuleInterop: true } }).outputText;
    function req(name) {
      if (options.overrides && name in options.overrides) return options.overrides[name];
      if (name === 'react') return react;
      if (name === 'react/jsx-runtime') return { jsx: (type, props) => ({ type, props }), jsxs: (type, props) => ({ type, props }), Fragment: 'fragment' };
      if (name === 'lucide-react') return new Proxy({}, { get: (_, key) => key });
      if (name === 'sonner') return { toast: new Proxy({}, { get: (_, level) => message => toasts.push({ level, message }) }) };
      if (name.endsWith('useEscapeToClose')) return { useEscapeToClose() {} };
      if (name.endsWith('/utils')) return { formatBytes: n => n + ' B' };
      if (name.startsWith('.')) {
        const base = path.resolve(path.dirname(file), name);
        const actual = [base, base + '.ts', base + '.tsx'].find(p => fs.existsSync(p) && fs.statSync(p).isFile());
        assert.ok(actual, 'Missing production import ' + name); return load(actual);
      }
      return appRequire(name);
    }
    vm.runInThisContext('(function(require,module,exports){' + output + '\n})', { filename: file })(req, mod, mod.exports);
    return mod.exports;
  }
  const exports = load(file);
  const component = options.exportName ? exports[options.exportName] : Object.values(exports).find(v => typeof v === 'function');
  function render() { for (let i = 0; dirty && i < 20; i++) { dirty = false; cursor = 0; tree = options.args ? component(...options.args) : component(props); while (pending.length) pending.shift()(); } assert.equal(dirty, false, 'Render loop'); return tree; }
  function walk(node) { if (Array.isArray(node)) return node.flatMap(walk); if (!node || typeof node !== 'object') return []; return [node, ...walk(node.props?.children)]; }
  function stringify(node) { if (Array.isArray(node)) return node.map(stringify).join(' '); if (node === null || node === undefined || typeof node === 'boolean') return ''; if (typeof node !== 'object') return String(node); return stringify(node.props?.children); }
  function nodes(node = tree) { return walk(node); }
  function text(node = tree) { return stringify(node); }
  render();
  return { render, nodes, text, toasts, async flush() { for (let i = 0; i < 15; i++) { render(); await new Promise(resolve => setImmediate(resolve)); } return tree; }, get tree() { return tree; }, update(next) { props = { ...props, ...next }; dirty = true; render(); } };
}
function job(overrides = {}) {
  return { id: 'job', source: 'https://example.test/public.mp4', folderId: 7, phase: 'waiting-for-selection', selectedIds: ['direct'], outputs: [], uploadQueueIds: [], createdAt: 1, updatedAt: 2, analysis: { source_kind: 'direct_file', title: 'Public video', source_label: 'Direct file', qualities: [], items: [{ id: 'direct', label: 'public.mp4', kind: 'file', size: 100 }] }, ...overrides };
}
function props(overrides = {}) {
  return { isOpen: true, onClose() {}, activeFolderId: 7, currentChannelName: 'test', jobs: [], onAnalyze: async () => job(), onImport: async () => job({ phase: 'success' }), remoteImportBatch: { batches: [], skipEntry: async () => {}, cancelBatch: async () => {} }, ...overrides };
}
test('running batch does not replace the fresh link editor', () => {
  const batch = { id: 'batch', folderId: 7, phase: 'importing', entries: [], stackGroups: [], activeEntryIndex: null, createdAt: 1, updatedAt: 2, startedAt: 1 };
  const h = harness(modalFile, props({ remoteImportBatch: { batches: [batch] } }));
  assert.ok(h.nodes().find(n => n.type === 'textarea'), 'Active batch hijacked Add links');
});
test('link input is multiline before the first URL is entered', () => {
  const h = harness(modalFile, props());
  const textarea = h.nodes().find(n => n.type === 'textarea');
  assert.ok(textarea.props.rows >= 4, 'Initial link editor is only one line');
});
test('new progress events preserve typed links instead of resetting modal state', () => {
  const h = harness(modalFile, props());
  h.nodes().find(n => n.type === 'textarea').props.onChange({ target: { value: 'https://example.test/new.mp4' } }); h.render();
  h.update({ remoteImportBatch: { batches: [{ id: 'other', folderId: 7, phase: 'importing', entries: [], stackGroups: [], activeEntryIndex: null, createdAt: 1, updatedAt: 3 }] } });
  assert.equal(h.nodes().find(n => n.type === 'textarea')?.props.value, 'https://example.test/new.mp4');
});
test('single source starts in background and closes without awaiting acquisition', async () => {
  const gate = defer(); let closed = 0, started = 0;
  const h = harness(modalFile, props({ onClose: () => { closed++; }, onImport: () => { started++; return gate.promise; } }));
  h.nodes().find(n => n.type === 'textarea').props.onChange({ target: { value: 'https://example.test/public.mp4' } }); h.render();
  await h.nodes().find(n => n.type === 'form').props.onSubmit({ preventDefault() {} }); h.update({ jobs: [job()] });
  const button = h.nodes().find(n => n.type === 'button' && h.text(n).trim() === 'Import');
  assert.ok(button); const result = button.props.onClick();
  await Promise.resolve();
  assert.equal(started, 1); assert.equal(closed, 1, 'Acquisition kept modal open');
  gate.resolve(job({ phase: 'success' })); await result;
});
test('queue uses native torrent phase and bytes rather than stale importing label', () => {
  assert.ok(fs.existsSync(queueFile), 'Background import queue is missing');
  const h = harness(queueFile, { jobs: [job({ phase: 'downloading', progress: { job_id: 'job', phase: 'waiting_for_peers', downloaded_bytes: 25, total_bytes: 100, speed_bytes_per_sec: 0 } })], batches: [], uploadItems: [], onReview() {}, onCancel() {}, onDismiss() {}, onRetry() {} });
  assert.match(h.text(), /Waiting for torrent data/i); assert.match(h.text(), /25.*100/);
});
test('terminal error replaces active spinner even when the batch entry remains importing', () => {
  assert.ok(fs.existsSync(queueFile), 'Background import queue is missing');
  const j = job({ phase: 'error', error: 'fixture helper failed' });
  const batch = { id: 'batch', phase: 'importing', entries: [{ id: 'entry', ordinal: 0, source: j.source, jobId: j.id, selectedIds: ['direct'], phase: 'importing' }], stackGroups: [], activeEntryIndex: 0, folderId: 7, createdAt: 1, updatedAt: 2 };
  const h = harness(queueFile, { jobs: [j], batches: [batch], uploadItems: [], onReview() {}, onCancel() {}, onDismiss() {}, onRetry() {} });
  assert.match(h.text(), /fixture helper failed/);
  assert.ok(!h.nodes().some(n => n.type === 'Loader2'), 'Terminal error still spins');
});
test('queue cancel and dismiss actions refer to the correct source', () => {
  assert.ok(fs.existsSync(queueFile), 'Background import queue is missing');
  const actions = [];
  const h = harness(queueFile, { jobs: [job({ phase: 'downloading' })], batches: [], uploadItems: [], onReview() {}, onCancel: (id, batch) => actions.push(['cancel', id, batch]), onDismiss: id => actions.push(['dismiss', id]), onRetry() {} });
  h.nodes().find(n => n.type === 'button' && /Cancel import/i.test(n.props['aria-label'] ?? '')).props.onClick();
  assert.deepEqual(actions, [['cancel', 'job', undefined]]);
  h.update({ jobs: [job({ phase: 'success' })] });
  h.nodes().find(n => n.type === 'button' && /Dismiss import/i.test(n.props['aria-label'] ?? '')).props.onClick();
  assert.deepEqual(actions.at(-1), ['dismiss', 'job']);
});

async function importingHook() {
  const gate = defer(), listeners = new Map(), calls = [];
  let saved = [job()];
  const store = { async get() { return structuredClone(saved); }, async set(_, value) { saved = structuredClone(value); }, async save() {} };
  const h = harness(path.join(root, 'app/src/hooks/useRemoteImport.ts'), {}, {
    exportName: 'useRemoteImport', args: [7, store, async () => ['upload']],
    overrides: {
      '@tauri-apps/api/core': { invoke: async (name, args) => {
        calls.push([name, args]);
        if (name === 'cmd_remote_import_acquire') return gate.promise;
        if (name === 'cmd_remote_import_cancel' || name === 'cmd_remote_import_cleanup') return true;
        throw new Error('Unexpected command ' + name);
      } },
      '@tauri-apps/api/event': { listen: async (name, callback) => { listeners.set(name, callback); return () => listeners.delete(name); } },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    },
  });
  await h.flush();
  const acquiring = h.tree.startImport('job', ['direct']);
  await h.flush();
  assert.equal(h.tree.getRemoteImportJob('job').phase, 'downloading');
  await h.tree.cancelJob('job'); await h.flush();
  return { h, gate, acquiring, calls, emit: payload => listeners.get('remote-import-progress')({ payload }) };
}
test('late native progress cannot restart a cancelled acquisition', async () => {
  const { h, gate, acquiring, emit } = await importingHook();
  emit({ job_id: 'job', phase: 'downloading', downloaded_bytes: 10, total_bytes: 100, speed_bytes_per_sec: 10 });
  await h.flush();
  gate.resolve({ job_id: 'job', outputs: [] }); await acquiring.catch(() => {});
  await h.flush();
  assert.equal(h.tree.getRemoteImportJob('job').phase, 'cancelled');
});
test('a delayed acquisition failure preserves explicit cancellation', async () => {
  const { h, gate, acquiring } = await importingHook();
  gate.resolve(Promise.reject(new Error('fixture cancelled acquisition')));
  await acquiring.catch(() => {}); await h.flush();
  assert.equal(h.tree.getRemoteImportJob('job').phase, 'cancelled');
  assert.equal(h.tree.getRemoteImportJob('job').uploadQueueIds.length, 0);
});

test('removed batch sources do not reappear as standalone imports', () => {
  const j = job();
  const h = harness(queueFile, { jobs: [j], batches: [{ id: 'batch', entries: [{ id: 'entry', jobId: j.id, phase: 'skipped' }] }], uploadItems: [], onReview() {}, onCancel() {}, onDismiss() {}, onRetry() {} });
  assert.equal(h.tree, null);
});
test('late analysis from a closed editor cannot replace newly typed links', async () => {
  const gate = defer();
  const h = harness(modalFile, props({ onAnalyze: () => gate.promise }));
  h.nodes().find(n => n.type === 'textarea').props.onChange({ target: { value: 'https://example.test/old.mp4' } }); h.render();
  const pending = h.nodes().find(n => n.type === 'form').props.onSubmit({ preventDefault() {} });
  h.update({ isOpen: false }); h.update({ isOpen: true });
  h.nodes().find(n => n.type === 'textarea').props.onChange({ target: { value: 'https://example.test/new.mp4' } }); h.render();
  gate.resolve(job({ id: 'old-analysis' })); await pending; h.update({ jobs: [job({ id: 'old-analysis' })] });
  assert.equal(h.nodes().find(n => n.type === 'textarea').props.value, 'https://example.test/new.mp4');
  assert.ok(!h.nodes().find(n => n.type === 'button' && h.text(n).trim() === 'Import'));
});
test('analysis completion preserves cancellation before the native acknowledgement', async () => {
  const analysis = defer(), cancel = defer(), calls = [];
  let saved = [];
  const store = { async get() { return structuredClone(saved); }, async set(_, value) { saved = structuredClone(value); }, async save() {} };
  const h = harness(path.join(root, 'app/src/hooks/useRemoteImport.ts'), {}, {
    exportName: 'useRemoteImport', args: [7, store, async () => { throw new Error('Cancelled job reached upload'); }],
    overrides: {
      '@tauri-apps/api/core': { invoke: async (name) => { calls.push(name); if (name === 'cmd_remote_import_analyze') return analysis.promise; if (name === 'cmd_remote_import_cancel') return cancel.promise; if (name === 'cmd_remote_import_cleanup') return true; throw new Error(name); } },
      '@tauri-apps/api/event': { listen: async () => () => {} },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    },
  });
  await h.flush(); const pending = h.tree.analyzeSource('https://example.test/public.mp4'); await h.flush();
  const id = h.tree.jobs[0].id;
  const cancelling = h.tree.cancelJob(id); await h.flush();
  analysis.resolve(job().analysis); await pending; await h.flush();
  assert.equal(h.tree.getRemoteImportJob(id).phase, 'cancelled');
  cancel.resolve(true); await cancelling; await h.flush();
  assert.equal(h.tree.getRemoteImportJob(id).phase, 'cancelled');
});


test('cancellation while upload setup waits cannot queue or restart the import', async () => {
  const gate = defer(); let queued = 0, saved = [job({ phase: 'error', outputs: [{ staging_path: '/owned/public.mp4', display_name: 'public.mp4', selection_id: 'direct' }] })];
  const store = { async get() { return structuredClone(saved); }, async set(_, value) { saved = structuredClone(value); }, async save() {} };
  const h = harness(path.join(root, 'app/src/hooks/useRemoteImport.ts'), {}, {
    exportName: 'useRemoteImport', args: [7, store, async (_, options) => { await gate.promise; if (options?.isCancelled()) return []; queued++; return ['queued']; }],
    overrides: {
      '@tauri-apps/api/core': { invoke: async name => { if (name === 'cmd_remote_import_cancel' || name === 'cmd_remote_import_cleanup') return true; throw new Error(name); } },
      '@tauri-apps/api/event': { listen: async () => () => {} },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    },
  });
  await h.flush(); const pending = h.tree.retryJob('job'); await h.flush();
  await h.tree.cancelJob('job'); gate.resolve(); await pending.catch(() => {}); await h.flush();
  assert.equal(queued, 0); assert.equal(h.tree.getRemoteImportJob('job').phase, 'cancelled');
});


test('actual upload chooser cancellation prevents durable queue insertion', async () => {
  const gate = defer(), ref = { current: [] }; let cancelled = false, saves = 0;
  const h = harness(path.join(root, 'app/src/hooks/useFileUpload.ts'), {}, {
    exportName: 'useFileUpload', args: [7, null, []],
    overrides: {
      '@tauri-apps/api/core': { invoke: async () => true },
      '@tauri-apps/api/event': { listen: async () => () => {} },
      '@tauri-apps/plugin-dialog': { open: async () => null },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
      '../utils': { isAndroidPlatform: false, isIOSPlatform: false },
      '../transferQueue': { normalizeUploadQueue: x => x, useRecoverableTransferQueue: () => ({ queue: ref.current, queueRef: ref, initialized: false, durable: true, setQueue: next => { ref.current = typeof next === 'function' ? next(ref.current) : next; }, persist: async () => { saves++; } }) },
      '../context/SettingsContext': { useSettings: () => ({ settings: {} }) },
      '../context/FastTransferAuthContext': { useFastTransferAuth: () => ({ ensureFastTransferReady: async () => true }) },
      '../context/TransferMethodContext': { useTransferMethod: () => ({ chooseTransferMethod: () => gate.promise }) },
      '../context/UploadSuggestionContext': { useUploadSuggestion: () => ({ reviewUploadSuggestion: async () => ({}) }) },
    },
  });
  const pending = h.tree.enqueuePreparedImports([{ path: '/owned/public.mp4', folderId: 7 }], { isCancelled: () => cancelled });
  cancelled = true; gate.resolve('boost'); const ids = await pending;
  assert.deepEqual(ids, []); assert.deepEqual(ref.current, []); assert.equal(saves, 0);
});
test('cancelling queued imports marks the job cancelled and cancels every upload', async () => {
  const cancelled = []; let saved = [job({ phase: 'uploading', uploadQueueIds: ['one', 'two'] })];
  const store = { async get() { return structuredClone(saved); }, async set(_, value) { saved = structuredClone(value); }, async save() {} };
  const h = harness(path.join(root, 'app/src/hooks/useRemoteImport.ts'), {}, {
    exportName: 'useRemoteImport', args: [7, store, async () => [], id => cancelled.push(id)],
    overrides: {
      '@tauri-apps/api/core': { invoke: async () => true },
      '@tauri-apps/api/event': { listen: async () => () => {} },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    },
  });
  await h.flush(); await h.tree.cancelJob('job'); await h.flush();
  assert.equal(h.tree.getRemoteImportJob('job').phase, 'cancelled'); assert.deepEqual(cancelled, ['one', 'two']);
});


async function dismissalFixture(initialJob) {
  let saved = [initialJob], pending = saved, failSave = false;
  const calls = [];
  const store = {
    async get() { return structuredClone(saved); },
    async set(_, value) { pending = structuredClone(value); },
    async save() {
      if (failSave) throw new Error('fixture disk full');
      saved = structuredClone(pending);
    },
  };
  const mount = () => harness(path.join(root, 'app/src/hooks/useRemoteImport.ts'), {}, {
    exportName: 'useRemoteImport', args: [7, store, async () => ['retried-upload']],
    overrides: {
      '@tauri-apps/api/core': { invoke: async name => { calls.push(name); return true; } },
      '@tauri-apps/api/event': { listen: async () => () => {} },
      '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    },
  });
  const h = mount(); await h.flush();
  return { h, mount, calls, get saved() { return saved; }, failNextSave() { failSave = true; } };
}
function dashboardDismiss(h, batches) {
  const file = path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx');
  const source = ts.createSourceFile(file, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let callback;
  function visit(node) {
    if (ts.isJsxAttribute(node) && node.name.getText(source) === 'onDismiss'
        && node.parent.parent.tagName?.getText(source) === 'RemoteImportQueue') {
      callback = node.initializer.expression.getText(source);
    }
    ts.forEachChild(node, visit);
  }
  visit(source); assert.ok(callback, 'Dashboard import dismissal callback is missing');
  const output = ts.transpileModule('export const dismiss = ' + callback, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const mod = { exports: {} };
  vm.runInThisContext('(function(module,exports,remoteImport,remoteImportBatch){' + output + '\n})', { filename: file })(
    mod, mod.exports, h.tree, { batches });
  return mod.exports.dismiss;
}
function queueFor(h, batches = []) {
  return harness(queueFile, {
    jobs: h.tree.jobs, batches, uploadItems: [], onReview() {}, onCancel() {}, onRetry() {},
    onDismiss: dashboardDismiss(h, batches),
  });
}
test('dismissing a completed batch stays dismissed after restart and retains its journal', async () => {
  const completed = job({ phase: 'success', deferAutomaticFinalization: true,
    uploadQueueIds: ['uploaded'], uploadOutcomes: { uploaded: { queueId: 'uploaded', status: 'success', logicalFileId: 'cloud-file' } },
    stackId: 'cloud-stack', stackedFileIds: ['cloud-file'] });
  const fixture = await dismissalFixture(completed);
  const batches = [{ id: 'batch', phase: 'success', entries: [{ id: 'entry', jobId: completed.id, phase: 'success' }] }];
  const queue = queueFor(fixture.h, batches);
  queue.nodes().find(n => n.type === 'button' && n.props['aria-label'] === 'Dismiss import').props.onClick();
  await queue.flush(); await fixture.h.flush(); queue.update({ jobs: fixture.h.tree.jobs });
  assert.equal(queue.tree, null, 'Completed row remained on screen');
  assert.equal(fixture.saved.length, 1, 'Batch recovery journal was deleted');
  assert.ok(fixture.saved[0].dismissedAt, 'Dismissal was not saved');
  assert.deepEqual(fixture.saved[0].uploadOutcomes, completed.uploadOutcomes);
  assert.equal(fixture.saved[0].stackId, completed.stackId);
  assert.deepEqual(fixture.saved[0].stackedFileIds, completed.stackedFileIds);
  assert.deepEqual(fixture.calls, [], 'Dismissing a batch cleaned recovery files');
  const restarted = fixture.mount(); await restarted.flush();
  assert.equal(queueFor(restarted, batches).tree, null, 'Dismissed batch returned after restart');
});
test('a failed dismissal save leaves the import visible with an error', async () => {
  const fixture = await dismissalFixture(job({ phase: 'success', deferAutomaticFinalization: true }));
  fixture.failNextSave();
  const queue = queueFor(fixture.h, [{ id: 'batch', entries: [{ jobId: 'job', phase: 'success' }] }]);
  queue.nodes().find(n => n.type === 'button' && n.props['aria-label'] === 'Dismiss import').props.onClick();
  await queue.flush(); await fixture.h.flush(); queue.update({ jobs: fixture.h.tree.jobs });
  assert.ok(queue.tree, 'Failed dismissal hid the row');
  assert.match(queue.toasts.at(-1)?.message ?? '', /disk full/);
  assert.equal(fixture.saved[0].dismissedAt, undefined);
});
test('single imports still dismiss durably without retaining an unnecessary journal', async () => {
  const fixture = await dismissalFixture(job({ phase: 'success', uploadQueueIds: ['uploaded'] }));
  await fixture.h.tree.dismissJob('job'); await fixture.h.flush();
  assert.deepEqual(fixture.saved, []);
  const restarted = fixture.mount(); await restarted.flush();
  assert.equal(queueFor(restarted).tree, null);
});
test('closing an unstarted source in a cancelled batch stays closed after restart', async () => {
  const fixture = await dismissalFixture(job());
  const batches = [{ id: 'batch', phase: 'cancelled', entries: [{ jobId: 'job', phase: 'waiting-for-selection' }] }];
  const queue = queueFor(fixture.h, batches);
  queue.nodes().find(n => n.type === 'button' && n.props['aria-label'] === 'Dismiss import').props.onClick();
  await queue.flush(); await fixture.h.flush(); queue.update({ jobs: fixture.h.tree.jobs });
  assert.equal(queue.toasts.length, 0, 'Cancelled batch row could not be dismissed');
  assert.equal(queue.tree, null);
  const restarted = fixture.mount(); await restarted.flush();
  assert.equal(queueFor(restarted, batches).tree, null);
});
test('active imports cannot be dismissed', async () => {
  const fixture = await dismissalFixture(job());
  await assert.rejects(fixture.h.tree.dismissJob('job', { retainJournal: true }), /still active/);
  assert.equal(fixture.saved[0].dismissedAt, undefined);
});
test('retrying retained failed uploads clears their persisted dismissal', async () => {
  const fixture = await dismissalFixture(job({ phase: 'error', deferAutomaticFinalization: true,
    dismissedAt: 123, uploadQueueIds: ['failed'], uploadOutcomes: { failed: { queueId: 'failed', status: 'error' } } }));
  assert.equal(queueFor(fixture.h).tree, null);
  await fixture.h.tree.resetRemoteImportUploadOutcomes('job', ['failed']); await fixture.h.flush();
  assert.equal(fixture.h.tree.jobs[0].phase, 'uploading');
  assert.equal(fixture.h.tree.jobs[0].dismissedAt, undefined);
  assert.ok(queueFor(fixture.h).tree, 'Retried import stayed hidden');
});

(async () => { let failures = 0; for (const { name, fn } of cases) { try { await fn(); console.log('PASS ' + name); } catch (e) { failures++; console.error('FAIL ' + name + ': ' + e.message); } } console.log(cases.length + ' background import cases; ' + failures + ' failures'); process.exitCode = failures ? 1 : 0; })();
