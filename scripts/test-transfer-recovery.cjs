const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const vm = require('node:vm');
const { createRequire } = require('node:module');
const { execFileSync } = require('node:child_process');
const assert = require('node:assert/strict');

const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
const fixtures = fs.mkdtempSync(path.join(os.tmpdir(), 'tera-transfer-recovery-'));
const sql = `
import json, sqlite3, sys
db, mode, kind = sys.argv[1:4]
conn = sqlite3.connect(db)
conn.execute("CREATE TABLE IF NOT EXISTS transfer_queues(kind TEXT PRIMARY KEY, items_json TEXT NOT NULL)")
if mode == "save":
    with conn:
        conn.execute("INSERT INTO transfer_queues(kind, items_json) VALUES (?, ?) ON CONFLICT(kind) DO UPDATE SET items_json=excluded.items_json", (kind, sys.argv[4]))
    print("null")
else:
    row = conn.execute("SELECT items_json FROM transfer_queues WHERE kind=?", (kind,)).fetchone()
    print(row[0] if row else "null")
conn.close()
`;
let serial = 0;
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function database(db, mode, kind, items) {
  const args = ['-c', sql, db, mode, kind];
  if (mode === 'save') args.push(JSON.stringify(items));
  return JSON.parse(execFileSync('python3', args, { encoding: 'utf8' }));
}
function makeBackend(saved = {}) {
  const db = path.join(fixtures, 'queue-' + ++serial + '.sqlite');
  for (const [kind, items] of Object.entries(saved)) database(db, 'save', kind, items);
  const backend = {
    db, saves: [], jobs: [], deleted: [], listeners: new Map(), saveHolds: [], failures: 0, authReady: true,
    saveInFlight: 0, maxSaveInFlight: 0, dialogs: [], filename: path.join(fixtures, 'folder-' + serial + '.zip'),
    read: kind => database(db, 'load', kind),
    holdSave(predicate = () => true) { const gate = deferred(); backend.saveHolds.push({ predicate, gate }); return gate; },
    complete(id, error) {
      const job = backend.jobs.find(j => j.id === id && !j.finished);
      assert.ok(job, 'Fixture transfer was not started: ' + id);
      job.finished = true;
      if (error) job.gate.reject(new Error(error)); else job.gate.resolve(null);
    },
    emit(name, payload) { for (const callback of backend.listeners.get(name) || []) callback({ payload }); },
    async invoke(command, args) {
      if (command === 'cmd_load_transfer_queue') return backend.read(args.kind);
      if (command === 'cmd_save_transfer_queue') {
        assert.ok(args.kind === 'upload' || args.kind === 'download');
        assert.ok(Array.isArray(args.items), 'Queue writes must contain a complete snapshot');
        const record = { kind: args.kind, items: structuredClone(args.items) };
        backend.saves.push(record);
        backend.saveInFlight++;
        backend.maxSaveInFlight = Math.max(backend.maxSaveInFlight, backend.saveInFlight);
        try {
          const index = backend.saveHolds.findIndex(h => h.predicate(record));
          if (index >= 0) {
            const { gate } = backend.saveHolds.splice(index, 1)[0];
            await gate.promise;
          }
          if (backend.failures > 0) { backend.failures--; throw new Error('fixture queue disk write failed'); }
          return database(db, 'save', args.kind, args.items);
        } finally { backend.saveInFlight--; }
      }
      if (command === 'cmd_upload_file' || command === 'cmd_upload_from_url' || command === 'cmd_download_file') {
        const id = command === 'cmd_download_file' ? args.req.transfer_id : args.transferId;
        const gate = deferred();
        backend.jobs.push({ id, command, args, gate, finished: false });
        return gate.promise;
      }
      if (command === 'cmd_cancel_transfer') {
        const job = backend.jobs.find(j => j.id === args.transferId && !j.finished);
        if (job) backend.complete(job.id, 'Transfer cancelled');
        return null;
      }
      if (command === 'cmd_zip_folder') { fs.writeFileSync(backend.filename, 'folder fixture'); return backend.filename; }
      if (command === 'cmd_delete_temp_zip') {
        backend.deleted.push(args.path);
        fs.rmSync(args.path, { force: true });
        return null;
      }
      if (command === 'cmd_start_foreground_service' || command === 'cmd_stop_foreground_service') return null;
      throw new Error('Unexpected IPC command: ' + command);
    },
  };
  return backend;
}
function legacyStore(saved = {}) {
  const data = { settings: { untouched: 'fixture preference' }, ...saved };
  return {
    writes: [], data,
    async get(key) { return structuredClone(data[key]); },
    async set(key, items) { this.writes.push(key); data[key] = structuredClone(items); },
    async save() {},
  };
}
function makeHarness(kind, backend, store = legacyStore(), concurrency = 1) {
  const cells = [];
  const effects = [];
  const timers = new Map();
  const toasts = [];
  let cursor = 0, dirty = true, result, mounted = true, now = 0, timerId = 0;
  const oldWindow = global.window;
  global.window = {
    setTimeout(fn, ms) { const id = ++timerId; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  function changed(a, b) { return !a || !b || a.length !== b.length || a.some((value, i) => !Object.is(value, b[i])); }
  const react = {
    useState(initial) {
      const i = cursor++;
      if (!cells[i]) cells[i] = { value: typeof initial === 'function' ? initial() : initial };
      return [cells[i].value, next => {
        const value = typeof next === 'function' ? next(cells[i].value) : next;
        if (!Object.is(value, cells[i].value)) { cells[i].value = value; dirty = true; }
      }];
    },
    useRef(initial) { const i = cursor++; return cells[i] ??= { current: initial }; },
    useEffect(fn, deps) {
      const i = cursor++;
      if (!cells[i] || changed(cells[i].deps, deps)) {
        const previous = cells[i];
        cells[i] = { deps, cleanup: previous?.cleanup };
        effects.push(() => { previous?.cleanup?.(); cells[i].cleanup = fn(); });
      }
    },
    useMemo(fn, deps) {
      const i = cursor++;
      if (!cells[i] || changed(cells[i].deps, deps)) cells[i] = { deps, value: fn() };
      return cells[i].value;
    },
    useCallback(fn, deps) { return react.useMemo(() => fn, deps); },
  };
  const overrides = {
    react,
    '@tauri-apps/api/core': { invoke: backend.invoke },
    '@tauri-apps/api/event': { listen: async (name, callback) => {
      const list = backend.listeners.get(name) || [];
      list.push(callback); backend.listeners.set(name, list);
      return () => backend.listeners.set(name, list.filter(fn => fn !== callback));
    } },
    '@tauri-apps/plugin-dialog': {
      save: async () => backend.dialogs.length ? backend.dialogs.shift() : path.join(fixtures, 'selected-' + serial + '.bin'),
      open: async options => options.title === 'Select Folder to Upload' ? '/fixture/folder' : null,
    },
    '@tanstack/react-query': { useQueryClient: () => ({ invalidateQueries() {} }) },
    sonner: { toast: Object.fromEntries(['info', 'success', 'error'].map(level => [level, text => toasts.push({ level, text })])) },
    '../utils': {
      isAndroidPlatform: false, isIOSPlatform: false, sanitizeFilename: value => value,
      pickWithFallback: async fn => fn(), showFileDialogFallback: async () => [],
    },
    '../context/SettingsContext': { useSettings: () => ({ settings: { maxConcurrentUploads: concurrency, maxConcurrentDownloads: concurrency } }) },
    '../context/FastTransferAuthContext': { useFastTransferAuth: () => ({ ensureFastTransferReady: async () => backend.authReady }) },
  };
  const modules = new Map();
  function load(file) {
    file = path.resolve(file);
    if (modules.has(file)) return modules.get(file).exports;
    const module = { exports: {} }; modules.set(file, module);
    const source = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
      compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
    }).outputText;
    function req(name) {
      if (name in overrides) return overrides[name];
      if (name.startsWith('.')) {
        const base = path.resolve(path.dirname(file), name);
        const target = [base, base + '.ts', base + '.tsx'].find(p => fs.existsSync(p) && fs.statSync(p).isFile());
        assert.ok(target, 'Missing production module: ' + name);
        return load(target);
      }
      return appRequire(name);
    }
    vm.runInThisContext('(function(require,module,exports){' + source + '\n})', { filename: file })(req, module, module.exports);
    return module.exports;
  }
  const hook = load(path.join(root, 'app/src/hooks/useFile' + (kind === 'upload' ? 'Upload' : 'Download') + '.ts'))[kind === 'upload' ? 'useFileUpload' : 'useFileDownload'];
  const harness = {
    backend, store, toasts, load,
    get state() { return result; },
    get queue() { return result[kind + 'Queue']; },
    async flush() {
      let quiet = 0;
      for (let turn = 0; turn < 80; turn++) {
        if (dirty && mounted) {
          dirty = false; cursor = 0;
          result = kind === 'upload' ? hook(null, store) : hook(store);
          while (effects.length) effects.shift()();
          quiet = 0;
        }
        await new Promise(resolve => setImmediate(resolve));
        if (!dirty && !effects.length) {
          if (++quiet >= 4) return;
        } else quiet = 0;
      }
      throw new Error('Hook did not settle; scheduling/persistence hot loop');
    },
    async advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) {
        if (timer.at <= now) { timers.delete(id); timer.fn(); }
      }
      await harness.flush();
    },
    close() {
      mounted = false;
      for (const cell of cells) cell?.cleanup?.();
      global.window = oldWindow;
    },
  };
  return harness;
}
async function withHarness(kind, saved, fn, concurrency = 1, legacy = {}) {
  const backend = makeBackend(saved);
  const harness = makeHarness(kind, backend, legacyStore(legacy), concurrency);
  try { await harness.flush(); await fn(harness, backend); } finally { harness.close(); }
}
const cases = [];
function test(name, fn) { cases.push({ name, fn }); }
const up = (id, status = 'pending', extra = {}) => ({ id, path: '/fixture/' + id + '.bin', folderId: null, status, ...extra });
const down = (id, status = 'pending', extra = {}) => ({ id, messageId: 7, filename: id + '.bin', folderId: null, status, ...extra });

// Unknown statuses or malformed identities must never be normalized into a destructive empty save.
const corruptedQueues = [
  ['upload', 'unknown status', [up('bad', 'pendng')]],
  ['download', 'wrong kind status', [down('bad', 'uploading')]],
  ['upload', 'duplicate identity', [up('same'), up('same')]],
  ['download', 'duplicate identity', [down('same'), down('same')]],
  ['upload', 'missing source', [up('bad', 'pending', { path: '' })]],
  ['download', 'invalid message identity', [down('bad', 'pending', { messageId: '7' })]],
  ['upload', 'invalid temporary source', [up('bad', 'pending', { tempZipPath: 7 })]],
  ['download', 'invalid destination', [down('bad', 'pending', { savePath: { path: '/bad' } })]],
  ['upload', 'invalid folder identity', [up('bad', 'pending', { folderId: false })]],
  ['download', 'missing filename', [down('bad', 'pending', { filename: '' })]],
];
for (const [kind, reason, items] of corruptedQueues) test(kind + ' preserves corrupted saved records: ' + reason, () => withHarness(kind, { [kind]: items }, async (h, backend) => {
  const helper = h.load(path.join(root, 'app/src/transferQueue.ts'));
  const normalizer = kind === 'upload' ? helper.normalizeUploadQueue : helper.normalizeDownloadQueue;
  const writer = helper.createTransferQueuePersistence(kind, normalizer);
  await assert.rejects(() => writer.load(async () => []), /queue/i);
  assert.deepEqual(backend.read(kind), items, 'Corrupted saved work was overwritten');
  assert.equal(backend.saves.length, 0, 'Malformed work reached the durable write boundary');
  assert.equal(backend.jobs.length, 0);
}));
test('malformed legacy records are preserved without committing migration', () => withHarness('upload', {}, async (h, backend) => {
  assert.equal(backend.read('upload'), null, 'Invalid legacy records were replaced by an empty migrated queue');
  assert.equal(backend.saves.length, 0);
  assert.equal(h.store.writes.length, 0);
  assert.equal(backend.jobs.length, 0);
  assert.equal(h.store.data.uploadQueue[0].status, 'pendng');
}, 1, { uploadQueue: [up('legacy', 'pendng')] }));

test('valid signed legacy download identities and nullable folders remain recoverable', () => withHarness('download', {}, async (h, backend) => {
  const saved = backend.read('download');
  assert.equal(saved[0].messageId, -7);
  assert.equal(saved[0].folderId, -1000000000123);
  assert.equal(saved[1].folderId, null);
  assert.equal(backend.jobs[0].args.req.message_id, -7);
}, 1, { downloadQueue: [
  down('signed', 'pending', { messageId: -7, folderId: -1000000000123, savePath: '/fixture/signed.bin' }),
  down('nullable', 'paused'),
] }));

test('legacy URL identity can recover without a separate local source path', () => withHarness('upload', {}, async (h, backend) => {
  assert.equal(backend.read('upload')[0].url, 'https://fixture.invalid/large.mkv');
  assert.ok(backend.read('upload')[0].path);
  assert.equal(backend.jobs[0].command, 'cmd_upload_from_url');
}, 1, { uploadQueue: [{ id: 'url-only', url: 'https://fixture.invalid/large.mkv', folderId: null, status: 'pending' }] }));

for (const kind of ['upload', 'download']) test(kind + ' preserves setup-cancelled work and its source/destination on reopening', () => withHarness(kind, { [kind]: [] }, async (h, backend) => {
  backend.authReady = false;
  if (kind === 'upload') await h.state.handleFolderUpload();
  else h.state.queueDownload(7, 'setup-movie.mkv', null);
  await h.flush();
  assert.equal(backend.jobs.length, 0);
  const saved = backend.read(kind);
  assert.equal(saved.length, 1, 'Cancelled setup forgot retryable work');
  assert.equal(saved[0].status, 'paused');
  assert.match(saved[0].error, /setup.*cancel|cancel.*setup/i);
  if (kind === 'upload') {
    assert.ok(fs.existsSync(backend.filename));
    assert.equal(saved[0].tempZipPath, backend.filename);
  } else assert.ok(saved[0].savePath);
  const id = saved[0].id;
  h.close();
  backend.authReady = true;
  const reopened = makeHarness(kind, backend);
  try {
    await reopened.flush();
    assert.equal(reopened.queue[0].status, 'paused');
    assert.equal(backend.jobs.length, 0, 'Cancelled setup automatically restarted');
    reopened.state.resumeItem(id); await reopened.flush();
    assert.equal(backend.jobs.length, 1);
    if (kind === 'download') assert.equal(backend.jobs[0].args.req.save_path, saved[0].savePath);
  } finally { reopened.close(); }
}));

// Removing the durable scheduling barrier would start a transfer before this gate commits.
for (const kind of ['upload', 'download']) test(kind + ' waits for its complete queue snapshot before starting work', () => withHarness(kind, { [kind]: [] }, async (h, backend) => {
  const gate = backend.holdSave(record => record.items.length > 0);
  if (kind === 'upload') h.state.handleDropUpload(['/fixture/large.bin']);
  else h.state.queueDownload(7, 'large.bin', null);
  await h.flush();
  assert.equal(backend.jobs.length, 0, 'Transfer began before durable queue save');
  assert.deepEqual(backend.read(kind), [], 'Uncommitted queue became visible');
  gate.resolve();
  await h.flush();
  assert.equal(backend.jobs.length, 1);
  assert.equal(backend.read(kind).length, 1);
  assert.equal(h.store.writes.length, 0, 'Hook still modifies shared settings Store');
}));

// Restoring only pending/paused silently drops interrupted and actionable failed jobs.
for (const kind of ['upload', 'download']) test(kind + ' reopens active jobs as pending and preserves pause/error metadata', () => {
  const build = kind === 'upload' ? up : down;
  const active = kind === 'upload' ? 'uploading' : 'downloading';
  const items = [build('running', active), build('paused', 'paused'), build('pausing', 'pausing'), build('failed', 'error', { error: 'fixture destination is full' })];
  return withHarness(kind, { [kind]: items }, async (h, backend) => {
    assert.equal(h.queue.length, 4, 'Recoverable queue entries disappeared on reload');
    assert.equal(h.queue.find(i => i.id === 'paused')?.status, 'paused');
    assert.equal(h.queue.find(i => i.id === 'pausing')?.status, 'paused');
    assert.equal(h.queue.find(i => i.id === 'failed')?.error, 'fixture destination is full');
    const disk = backend.read(kind);
    assert.equal(disk.find(i => i.id === 'running')?.status, 'pending');
    assert.equal(disk.find(i => i.id === 'failed')?.status, 'error');
    assert.equal(backend.jobs.length, 1, 'Paused or errored work was resumed automatically');
  }, 1, { [kind + 'Queue']: items });
});

test('legacy migration commits before workers run and does not overwrite other shared preferences', () => withHarness('upload', {}, async (h, backend) => {
  assert.equal(backend.read('upload')?.[0]?.path, '/fixture/legacy.bin', 'Legacy queue was not durably migrated');
  assert.equal(backend.jobs.length, 1);
  assert.equal(h.store.writes.length, 0);
  assert.deepEqual(h.store.data.settings, { untouched: 'fixture preference' });
}, 1, { uploadQueue: [up('legacy', 'uploading')] }));

test('a deliberately empty durable queue does not resurrect stale legacy work', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  assert.equal(h.queue.length, 0, 'Old shared-store entries were resurrected');
  assert.equal(backend.jobs.length, 0);
}, 1, { uploadQueue: [up('stale')] }));

// Starting concurrent snapshot saves lets a delayed older snapshot replace the newest queue.
test('ordered saves prevent delayed older metadata from losing later queued files', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  const first = backend.holdSave(record => record.items.length === 1);
  h.state.handleDropUpload(['/fixture/first.bin']); await h.flush();
  h.state.handleDropUpload(['/fixture/second.bin']); await h.flush();
  assert.ok(backend.maxSaveInFlight <= 1, 'Queue snapshots were written concurrently');
  first.resolve(); await h.flush();
  assert.deepEqual(backend.read('upload').map(i => i.path), ['/fixture/first.bin', '/fixture/second.bin']);
  assert.equal(backend.jobs.length, 1);
}));

test('failed saves remain undurable and retry before transfer start', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  backend.failures = 1;
  h.state.handleDropUpload(['/fixture/recover.bin']); await h.flush();
  assert.equal(backend.jobs.length, 0, 'Failed save allowed transfer start');
  assert.deepEqual(backend.read('upload'), []);
  await h.advance(1000);
  assert.equal(backend.read('upload')?.[0]?.path, '/fixture/recover.bin', 'Identical metadata was marked durable after a failed write');
  assert.equal(backend.jobs.length, 1);
}));

// Reserving an active ID before await protects configured concurrency across progress/renders.
for (const kind of ['upload', 'download']) test(kind + ' reserves unique worker slots across delayed persistence and rerenders', () => withHarness(kind, { [kind]: [] }, async (h, backend) => {
  const gate = backend.holdSave(record => record.items.length > 0);
  if (kind === 'upload') h.state.handleDropUpload(['/fixture/one.bin', '/fixture/two.bin', '/fixture/three.bin']);
  else await h.state.queueBulkDownload([]);
  if (kind === 'download') {
    h.state.queueDownload(1, 'one.bin', null);
    h.state.queueDownload(2, 'two.bin', null);
    h.state.queueDownload(3, 'three.bin', null);
  }
  await h.flush(); gate.resolve(); await h.flush();
  assert.equal(backend.jobs.length, 2, 'Configured worker slots were duplicated or underfilled');
  assert.equal(new Set(backend.jobs.map(j => j.id)).size, 2, 'Same transfer was started twice');
  backend.emit(kind + '-progress', { id: backend.jobs[0].id, percent: 1, uploaded_bytes: 3, total_bytes: 300, speed_bytes_per_sec: 27 });
  await h.flush();
  assert.equal(backend.jobs.length, 2);
  backend.complete(backend.jobs[0].id); await h.flush();
  assert.equal(backend.jobs.length, 3, 'Released worker slot did not schedule remaining work');
}, 2));

test('newly selected download destination commits before download invoke', () => withHarness('download', { download: [] }, async (h, backend) => {
  const destination = path.join(fixtures, 'selected-movie.mkv');
  backend.dialogs.push(destination);
  const gate = backend.holdSave(record => record.items.some(i => i.savePath === destination));
  h.state.queueDownload(7, 'movie.mkv', 4); await h.flush();
  assert.equal(backend.jobs.length, 0, 'Download started before destination was durable');
  assert.ok(!backend.read('download')[0].savePath);
  gate.resolve(); await h.flush();
  assert.equal(backend.read('download')[0].savePath, destination);
  assert.equal(backend.jobs[0].args.req.save_path, destination);
  assert.equal(backend.jobs[0].args.req.folder_id, 4);
}));

test('save-dialog cancellation releases exactly one slot', () => withHarness('download', { download: [] }, async (h, backend) => {
  backend.dialogs.push(null, path.join(fixtures, 'two.bin'), path.join(fixtures, 'three.bin'));
  h.state.queueDownload(1, 'one.bin', null);
  h.state.queueDownload(2, 'two.bin', null);
  h.state.queueDownload(3, 'three.bin', null);
  await h.flush();
  assert.equal(backend.jobs.filter(j => !j.finished).length, 1, 'Canceling a save dialog bypassed the concurrency limit');
  assert.equal(h.queue.filter(i => i.status === 'pending').length, 1);
}));

test('folder ZIP survives upload failure and error metadata survives reopening', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  await h.state.handleFolderUpload(); await h.flush();
  const id = backend.jobs[0].id;
  backend.complete(id, 'fixture permanent upload failure'); await h.flush();
  assert.ok(fs.existsSync(backend.filename), 'Failed upload destroyed the only retryable ZIP source');
  const saved = backend.read('upload');
  assert.equal(saved[0].tempZipPath, backend.filename);
  assert.equal(saved[0].status, 'error');
  assert.ok(saved[0].error.includes('fixture permanent upload failure'));
  h.close();
  const reopened = makeHarness('upload', backend);
  try {
    await reopened.flush();
    assert.equal(reopened.queue[0].status, 'error');
    reopened.state.retryItem(id); await reopened.flush();
    assert.equal(backend.jobs.length, 2, 'Recovered error was not retryable');
    assert.ok(fs.existsSync(backend.filename));
  } finally { reopened.close(); }
}));

test('successful folder upload waits for durable completion before ZIP cleanup', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  await h.state.handleFolderUpload(); await h.flush();
  const gate = backend.holdSave(record => record.items.length === 0);
  backend.complete(backend.jobs[0].id); await h.flush();
  assert.ok(fs.existsSync(backend.filename), 'ZIP was deleted before completion snapshot committed');
  gate.resolve(); await h.flush();
  assert.deepEqual(backend.read('upload'), []);
  assert.ok(!fs.existsSync(backend.filename));
}));

test('explicit cancellation cleans a queued folder ZIP after cancellation is durable', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  await h.state.handleFolderUpload(); await h.flush();
  const id = backend.jobs[0].id;
  h.state.pauseItem(id); await h.flush();
  const gate = backend.holdSave(record => record.items.length === 0);
  h.state.cancelItem(id); await h.flush();
  assert.ok(fs.existsSync(backend.filename));
  gate.resolve(); await h.flush();
  assert.ok(!fs.existsSync(backend.filename), 'Explicit canceled ZIP remained orphaned');
  assert.deepEqual(backend.read('upload'), []);
}));

// A late URL phase event must not turn a durable pause back into resumable work.
test('late remote progress cannot unpause or resurrect a cancelled upload', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  h.state.handleUrlUpload('https://fixture.invalid/movie.mkv', null); await h.flush();
  const id = backend.jobs[0].id;
  h.state.pauseItem(id); await h.flush();
  backend.emit('remote-upload-progress', { id, phase: 'uploading', percent: 50, speed: 123, uploaded_bytes: 50, total_bytes: 100 });
  await h.flush();
  assert.equal(h.queue[0].status, 'paused', 'Late progress resumed paused work');
  assert.equal(backend.read('upload')[0].status, 'paused');
  h.state.cancelItem(id); await h.flush();
  backend.emit('remote-upload-progress', { id, phase: 'downloading', percent: 5, speed: 123, uploaded_bytes: 5, total_bytes: 100 });
  await h.flush();
  assert.equal(h.queue[0].status, 'cancelled', 'Late progress resurrected canceled work');
  assert.deepEqual(backend.read('upload'), []);
}));

// Completing an older cancellation must not delete the source of a newer manual Retry.
test('retry during delayed cancellation keeps the folder ZIP and starts one replacement worker', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  await h.state.handleFolderUpload(); await h.flush();
  const id = backend.jobs[0].id;
  h.state.pauseItem(id); await h.flush();
  const gate = backend.holdSave(record => record.items.length === 0);
  h.state.cancelItem(id); await h.flush();
  h.state.retryItem(id); await h.flush();
  gate.resolve(); await h.flush();
  assert.ok(fs.existsSync(backend.filename), 'Stale cancellation destroyed the Retry source');
  assert.equal(backend.jobs.length, 2);
  assert.equal(backend.jobs.filter(job => !job.finished).length, 1);
}));

test('selected destination save failure retains a retryable download without starting transport', () => withHarness('download', { download: [] }, async (h, backend) => {
  const destination = path.join(fixtures, 'retry-destination.mkv');
  backend.dialogs.push(destination);
  const gate = backend.holdSave(record => record.items.some(i => i.savePath === destination));
  h.state.queueDownload(7, 'movie.mkv', null); await h.flush();
  backend.failures = 1;
  gate.resolve(); await h.flush();
  assert.equal(backend.jobs.length, 0);
  assert.equal(h.queue[0].status, 'error');
  assert.equal(h.queue[0].savePath, destination);
  assert.equal(backend.read('download')[0].status, 'error');
  h.state.retryItem(h.queue[0].id); await h.flush();
  assert.equal(backend.jobs.length, 1);
  assert.equal(backend.jobs[0].args.req.save_path, destination);
}));

test('persistent storage failure stops automatic retries and never starts a worker', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  backend.failures = 100;
  h.state.handleDropUpload(['/fixture/no-disk.bin']); await h.flush();
  await h.advance(1000); await h.advance(2000); await h.advance(4000);
  const writes = backend.saves.length;
  await h.advance(300000);
  assert.equal(backend.saves.length, writes, 'Permanent failure continued automatic disk retries');
  assert.equal(backend.jobs.length, 0);
  assert.deepEqual(backend.read('upload'), []);
  assert.equal(h.queue[0].path, '/fixture/no-disk.bin', 'Unsaved work was forgotten');
}));

for (const kind of ['upload', 'download']) test(kind + ' keeps rolling byte/speed UI counters without rewriting stable metadata', () => withHarness(kind, { [kind]: [] }, async (h, backend) => {
  if (kind === 'upload') h.state.handleDropUpload(['/fixture/speed.bin']);
  else h.state.queueDownload(7, 'speed.bin', null);
  await h.flush();
  const writes = backend.saves.length;
  const id = backend.jobs[0].id;
  backend.emit(kind + '-progress', { id, percent: 25, uploaded_bytes: 2 ** 40, total_bytes: 4 * 2 ** 40, speed_bytes_per_sec: 52428800 });
  await h.flush();
  assert.equal(h.queue[0][kind === 'upload' ? 'uploadedBytes' : 'downloadedBytes'], 2 ** 40);
  assert.equal(h.queue[0].speedBytesPerSec, 52428800);
  assert.equal(backend.saves.length, writes, 'Progress events rewrote durable queue metadata');
}));

(async () => {
  let failures = 0;
  for (const { name, fn } of cases) {
    try { await fn(); console.log('PASS ' + name); }
    catch (error) { failures++; console.error('FAIL ' + name + ': ' + error.message); }
  }
  console.log(cases.length + ' recovery cases; ' + failures + ' failures');
  fs.rmSync(fixtures, { recursive: true, force: true });
  process.exitCode = failures ? 1 : 0;
})();
