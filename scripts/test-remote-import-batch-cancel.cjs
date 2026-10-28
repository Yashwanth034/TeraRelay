const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
const cases = [];
const test = (name, fn) => cases.push({ name, fn });
function defer() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return { promise, resolve, reject }; }
function hookHarness(initialBatch, controller) {
  let cursor = 0, dirty = true, result, saved = [structuredClone(initialBatch)];
  const cells = [], effects = [], cache = new Map();
  const same = (a, b) => a && b && a.length === b.length && a.every((v, i) => Object.is(v, b[i]));
  const react = {
    useState(initial) { const i = cursor++; if (!cells[i]) cells[i] = { value: initial }; return [cells[i].value, value => { cells[i].value = typeof value === 'function' ? value(cells[i].value) : value; dirty = true; }]; },
    useRef(initial) { const i = cursor++; return cells[i] ??= { current: initial }; },
    useMemo(fn, deps) { const i = cursor++; if (!cells[i] || !same(cells[i].deps, deps)) cells[i] = { value: fn(), deps }; return cells[i].value; },
    useCallback(fn, deps) { return react.useMemo(() => fn, deps); },
    useEffect(fn, deps) { const i = cursor++; if (!cells[i] || !same(cells[i].deps, deps)) { cells[i] = { deps }; effects.push(fn); } },
  };
  function load(file) {
    if (cache.has(file)) return cache.get(file).exports;
    const mod = { exports: {} }; cache.set(file, mod);
    const source = ts.transpileModule(fs.readFileSync(file, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText;
    const req = name => {
      if (name === 'react') return react;
      if (name === '@tauri-apps/api/core') return { invoke: async () => { throw new Error('Unexpected native command'); } };
      if (name.startsWith('.')) return load(path.resolve(path.dirname(file), name + '.ts'));
      return appRequire(name);
    };
    vm.runInThisContext('(function(require,module,exports){' + source + '\n})', { filename: file })(req, mod, mod.exports);
    return mod.exports;
  }
  const store = { async get() { return structuredClone(saved); }, async set(_, value) { saved = structuredClone(value); }, async save() {} };
  const hook = load(path.join(root, 'app/src/hooks/useRemoteImportBatch.ts')).useRemoteImportBatch;
  function render() { for (let i = 0; dirty && i < 20; i++) { dirty = false; cursor = 0; result = hook(7, store, controller); while (effects.length) effects.shift()(); } assert.equal(dirty, false); }
  async function flush() { for (let i = 0; i < 12; i++) { render(); await new Promise(resolve => setImmediate(resolve)); } }
  render();
  return { flush, get hook() { return result; }, get saved() { return structuredClone(saved[0]); }, model: load(path.join(root, 'app/src/remoteImportBatch.ts')) };
}
function batch() {
  return { schema: 1, id: 'batch', folderId: 7, phase: 'draft', activeEntryIndex: null, entries: [
    { id: 'first', ordinal: 0, source: 'https://example.test/a', phase: 'pending', selectedIds: [] },
    { id: 'second', ordinal: 1, source: 'https://example.test/b', phase: 'pending', selectedIds: [] },
  ], stackGroups: [], createdAt: 1, updatedAt: 1 };
}
function controller(gate) {
  const jobs = new Map(), analyzed = [], acquired = [];
  return {
    jobs: [], analyzed, acquired,
    prepareSource: async source => { const id = source.endsWith('/a') ? 'job-a' : 'job-b'; const job = { id, source, folderId: 7, phase: 'analyzing', outputs: [], selectedIds: [], uploadQueueIds: [] }; jobs.set(id, job); return job; },
    reanalyzeSource: async id => { analyzed.push(id); if (id === 'job-a') await gate.promise; const job = { ...jobs.get(id), phase: 'waiting-for-selection', analysis: { source_kind: 'direct_file', items: [{ id: 'file' }], qualities: [] } }; jobs.set(id, job); return job; },
    cancelJob: async id => { const job = jobs.get(id); if (job) jobs.set(id, { ...job, phase: 'cancelled' }); return true; },
    getRemoteImportJob: id => jobs.get(id),
    startImport: async id => { acquired.push(id); return jobs.get(id); },
    retryJob: async id => { acquired.push(id); return jobs.get(id); },
    resetRemoteImportUploadOutcomes: async () => {},
    finalizeRemoteImportJob: async id => jobs.get(id),
  };
}
for (const outcome of ['resolve', 'reject']) {
  test('cancelled batch stays durable after first analysis ' + outcome, async () => {
    const gate = defer(), remote = controller(gate), h = hookHarness(batch(), remote);
    await h.flush();
    const pending = h.hook.resumeBatchAnalysis('batch');
    await h.flush(); assert.deepEqual(remote.analyzed, ['job-a']);
    await h.hook.cancelBatch('batch'); await h.flush();
    gate[outcome](outcome === 'reject' ? new Error('late analysis failure') : undefined);
    await pending; await h.flush();
    assert.equal(h.saved.phase, 'cancelled', 'Late analysis resurrected the cancelled batch');
    assert.deepEqual(remote.analyzed, ['job-a'], 'Cancellation analyzed the next source');
    assert.equal(h.saved.entries[0].jobId, 'job-a');
  });
}
test('removing the next source while analysis waits prevents its analysis', async () => {
  const gate = defer(), remote = controller(gate), h = hookHarness(batch(), remote);
  await h.flush(); const pending = h.hook.resumeBatchAnalysis('batch');
  await h.flush(); await h.hook.skipEntry('batch', 'second'); await h.flush();
  gate.resolve(); await pending; await h.flush();
  assert.equal(h.saved.entries[1].phase, 'skipped', 'Late analysis restored a removed source');
  assert.deepEqual(remote.analyzed, ['job-a'], 'Removed source was analyzed');
});
test('late analysis preserves current selections and manual groups', async () => {
  const gate = defer(), remote = controller(gate), h = hookHarness(batch(), remote);
  await h.flush(); const pending = h.hook.resumeBatchAnalysis('batch');
  await h.flush();
  await h.hook.setEntrySelections('batch', 'first', ['file']);
  await h.hook.setEntrySelections('batch', 'second', ['file']);
  const members = [{ entryId: 'first', selectionId: 'file' }, { entryId: 'second', selectionId: 'file' }];
  await h.hook.groupMembers('batch', members, members[0]);
  const groups = h.saved.stackGroups;
  gate.resolve(); await pending; await h.flush();
  assert.deepEqual(h.saved.entries[0].selectedIds, ['file']);
  assert.deepEqual(h.saved.entries[1].selectedIds, ['file']);
  assert.deepEqual(h.saved.stackGroups, groups, 'Late analysis removed explicit grouping');
});
test('cancellation between sequential sources prevents the next acquisition', async () => {
  const gate = defer(), remote = controller(gate), h = hookHarness(batch(), remote);
  await h.flush();
  let latest = { ...batch(), phase: 'waiting-for-selection', entries: batch().entries.map(entry => ({ ...entry, phase: 'waiting-for-selection', jobId: 'job-' + entry.id, selectedIds: ['file'] })) };
  const started = [];
  const pending = h.model.runSequentialBatchEntries(latest, async entry => { started.push(entry.id); if (entry.id === 'first') await gate.promise; return { ...entry, phase: 'success' }; }, async snapshot => { if (latest.phase !== 'cancelled') latest = snapshot; }, () => latest);
  await h.flush();
  latest = { ...latest, phase: 'cancelled', activeEntryIndex: null };
  gate.resolve(); const result = await pending;
  assert.deepEqual(started, ['first'], 'Cancelled batch acquired the next source');
  assert.equal(result.phase, 'cancelled');
});
(async () => { let failures = 0; for (const { name, fn } of cases) { try { await fn(); console.log('PASS ' + name); } catch (error) { failures++; console.error('FAIL ' + name + ': ' + error.message); } } console.log(cases.length + ' batch cancellation cases; ' + failures + ' failures'); process.exitCode = failures ? 1 : 0; })();
