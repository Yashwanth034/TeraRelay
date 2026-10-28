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
    authCalls: 0, transferChoice: 'tdlib',
    driveSnapshot: { pending: [] }, driveFinalized: [], driveSyncs: 0, driveSyncResults: [], preflightResult: null,
    saveInFlight: 0, maxSaveInFlight: 0, dialogs: [], filename: path.join(fixtures, 'folder-' + serial + '.zip'),
    read: kind => database(db, 'load', kind),
    holdSave(predicate = () => true) { const gate = deferred(); backend.saveHolds.push({ predicate, gate }); return gate; },
    complete(id, error, result = null) {
      const job = backend.jobs.find(j => j.id === id && !j.finished);
      assert.ok(job, 'Fixture transfer was not started: ' + id);
      job.finished = true;
      if (error) job.gate.reject(new Error(error)); else job.gate.resolve(result);
    },
    emit(name, payload) { for (const callback of backend.listeners.get(name) || []) callback({ payload }); },
    async invoke(command, args) {
      if (command === 'cmd_load_transfer_queue') return backend.read(args.kind);
      if (command === 'cmd_drive_get_snapshot') return structuredClone(backend.driveSnapshot);
      if (command === 'cmd_drive_sync_metadata') {
        backend.driveSyncs++;
        if (backend.driveSyncResults.length) {
          const next = backend.driveSyncResults.shift();
          if (next instanceof Error) throw next;
          return next;
        }
        return { revision: 1, dirty: false, source: 'current' };
      }
      if (command === 'cmd_drive_finalize_upload') {
        backend.driveFinalized.push(structuredClone(args));
        backend.driveSnapshot.pending = backend.driveSnapshot.pending.filter(item => item.id !== args.pendingId);
        return null;
      }
      if (command === 'cmd_preflight_upload_candidate') {
        return backend.preflightResult || {
          file_name: path.basename(args.path),
          size: fs.existsSync(args.path) ? fs.statSync(args.path).size : 1,
          source_sha256: 'a'.repeat(64),
          source_identity: 'fixture-source-identity',
          exact_duplicate: null,
          similar_files: [],
        };
      }
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
    sonner: { toast: Object.fromEntries(['info', 'success', 'error', 'warning'].map(level => [level, text => toasts.push({ level, text })])) },
    '../utils': {
      isAndroidPlatform: false, isIOSPlatform: false, sanitizeFilename: value => value,
      pickWithFallback: async fn => fn(), showFileDialogFallback: async () => [],
    },
    '../context/SettingsContext': { useSettings: () => ({ settings: { maxConcurrentUploads: concurrency, maxConcurrentDownloads: concurrency } }) },
    '../context/FastTransferAuthContext': {
      useFastTransferAuth: () => ({
        ensureFastTransferReady: async () => { backend.authCalls++; return backend.authReady; },
      }),
    },
    '../context/TransferMethodContext': { useTransferMethod: () => ({ chooseTransferMethod: async () => backend.transferChoice }) },
    '../context/UploadSuggestionContext': {
      useUploadSuggestion: () => ({ reviewUploadSuggestion: async () => 'keep_separate' }),
    },
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
  ['upload', 'invalid transfer engine', [up('bad', 'pending', { transferEngine: 'automatic' })]],
  ['download', 'invalid transfer engine', [down('bad', 'pending', { transferEngine: 'automatic' })]],
  ['upload', 'incomplete mounted-drive identity', [up('bad', 'pending', { drivePendingId: '0'.repeat(32) })]],
  ['upload', 'invalid mounted-drive identity', [up('bad', 'pending', { drivePendingId: 'not-an-id', driveFileName: 'file.txt' })]],
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

// Unassigned synthetic Drive writes have no Telegram storage or readable remote
// object. Keep their local staging intact across close events and app recovery.
for (const recovery of [false, true]) test(
  'QA unassigned Drive write stays local after ' + (recovery ? 'snapshot recovery' : 'native close'),
  async () => {
    const priorQa = globalThis.__TERARELAY_QA_FEATURE_A__;
    globalThis.__TERARELAY_QA_FEATURE_A__ = true;
    const backend = makeBackend({ upload: [] });
    const source = path.join(fixtures, 'qa-local-' + recovery + '.js');
    fs.writeFileSync(source, "console.log('local staging');\n");
    const pendingId = 'e'.repeat(32);
    const pending = {
      id: pendingId, staging_path: source, backing_channel_id: 0,
      display_name: 'local.js',
    };
    backend.driveSnapshot.pending = recovery ? [pending] : [];
    const harness = makeHarness('upload', backend);
    try {
      await harness.flush();
      if (!recovery) {
        backend.driveSnapshot.pending = [pending];
        backend.emit('drive-upload-staged', {
          pending_id: pendingId, path: source, folder_id: 0, file_name: 'local.js',
        });
        await harness.flush();
      }
      assert.equal(backend.jobs.length, 0, 'Unassigned QA write started a fake remote upload');
      assert.equal(harness.queue.length, 0, 'Local staging leaked into the upload queue');
      assert.deepEqual(backend.driveSnapshot.pending, [pending], 'Pending bytes were finalized or removed');
      assert.equal(fs.readFileSync(source, 'utf8'), "console.log('local staging');\n");
      assert.deepEqual(backend.driveFinalized, []);
    } finally {
      harness.close();
      if (priorQa === undefined) delete globalThis.__TERARELAY_QA_FEATURE_A__;
      else globalThis.__TERARELAY_QA_FEATURE_A__ = priorQa;
    }
  }
);

test('mounted-drive close enters the durable Boost upload queue and finalizes the virtual entry', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-staged.txt');
    fs.writeFileSync(source, 'mounted drive payload');
    const pendingId = '0'.repeat(32);
    backend.emit('drive-upload-staged', {
      pending_id: pendingId,
      path: source,
      folder_id: 9100001,
      file_name: 'Drive staged.txt',
    });
    await h.flush();

    const saved = backend.read('upload');
    assert.equal(saved.length, 1);
    assert.equal(saved[0].drivePendingId, pendingId);
    assert.equal(saved[0].driveFileName, 'Drive staged.txt');
    assert.equal(saved[0].transferEngine, 'boost');
    assert.equal(backend.authCalls, 0, 'Mounted-drive upload unexpectedly required TDLib setup');
    assert.equal(backend.jobs.length, 1);
    assert.equal(backend.jobs[0].args.folderId, 9100001);

    const logicalId = 'b'.repeat(32);
    backend.complete(backend.jobs[0].id, null, {
      message: 'fixture upload complete',
      logical_file_id: logicalId,
    });
    await h.flush();

    assert.deepEqual(backend.driveFinalized, [{
      pendingId,
      logicalFileId: logicalId,
    }]);
    assert.ok(backend.driveSyncs >= 1, 'Drive metadata publication was not requested after finalization');
  })
);

test('mounted-drive byte-identical copy reuses existing logical content without another upload', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-dedup.txt');
    fs.writeFileSync(source, 'same bytes');
    const pendingId = '1'.repeat(32);
    const logicalId = 'c'.repeat(32);
    backend.preflightResult = {
      file_name: 'Duplicate alias.txt',
      size: fs.statSync(source).size,
      source_sha256: 'd'.repeat(64),
      source_identity: 'fixture-dedup-identity',
      exact_duplicate: {
        logical_file_id: logicalId,
        message_id: 77,
        name: 'Original.txt',
        size: fs.statSync(source).size,
      },
      similar_files: [],
    };

    backend.emit('drive-upload-staged', {
      pending_id: pendingId,
      path: source,
      folder_id: 9100001,
      file_name: 'Duplicate alias.txt',
    });
    await h.flush();

    assert.equal(backend.jobs.length, 0, 'Identical bytes were uploaded again');
    assert.deepEqual(backend.driveFinalized, [{
      pendingId,
      logicalFileId: logicalId,
    }]);
    assert.equal(backend.read('upload').length, 0, 'De-duplicated Drive work remained stuck in the queue');
  })
);

test('mounted Drive local finalize reports unverified cross-device sync once and rearms after clean sync', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'sync-notice-dedup.txt');
    fs.writeFileSync(source, 'identical existing Drive data');
    backend.preflightResult = {
      file_name: 'Drive sync notice.txt', size: 28,
      source_sha256: 'f'.repeat(64), source_identity: 'fixture-sync-notice',
      exact_duplicate: { logical_file_id: '9'.repeat(32), message_id: 44,
        name: 'same-remote-file.txt', size: 28 }, similar_files: [],
    };
    backend.driveSyncResults = [
      new Error('synthetic Telegram offline'),
      { revision: 2, dirty: true, source: 'current' },
      { revision: 3, dirty: false, source: 'unknown-fake-publication' },
      { revision: 4, dirty: false, source: 'current' },
      { revision: 5, dirty: false, source: 'qa-local' },
      { revision: 6, dirty: false, source: 'remote-merged' },
      new Error('synthetic second offline'),
    ];
    const queueDrive = async index => {
      backend.emit('drive-upload-staged', {
        pending_id: String(index).padStart(32, 'a'), path: source,
        folder_id: 9100001, file_name: 'Drive sync notice.txt',
      });
      await h.flush();
    };
    await queueDrive(1);
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 1,
      'offline metadata sync warns user even though local finalize succeeded');
    assert.match(h.toasts.find(t => t.level === 'warning').text, /saved locally|sync is unverified/i);
    await queueDrive(2);
    await queueDrive(3);
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 1,
      'more dirty or malformed outcomes do not flood the user with warning toasts');
    await queueDrive(4);
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 1,
      'a verified clean result clears the warning latch without extra success noise');
    await queueDrive(5);
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 2,
      'QA-local response cannot masquerade as successful cloud publication');
    assert.match(h.toasts.filter(t => t.level === 'warning')[1].text, /QA mode/);
    await queueDrive(6);
    await queueDrive(7);
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 3,
      'another offline episode after verified publication is surfaced once');
    assert.equal(backend.driveFinalized.length, 7, 'local finalized entries remain durable');
    assert.equal(backend.driveSyncs, 7, 'each finalize still requests metadata publication');
    assert.equal(backend.jobs.length, 0, 'duplicate Drive content never retransmits bytes');
  })
);

test('older delayed Drive metadata result cannot undo a newer verified upload sync', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'sync-epoch-dedup.txt');
    fs.writeFileSync(source, 'identical bytes');
    backend.preflightResult = {
      file_name: 'Drive delayed sync.txt', size: 15,
      source_sha256: 'f'.repeat(64), source_identity: 'fixture-sync-epoch',
      exact_duplicate: { logical_file_id: '8'.repeat(32), message_id: 45,
        name: 'same.txt', size: 15 }, similar_files: [],
    };
    const delayed = deferred();
    backend.driveSyncResults = [delayed.promise, { revision: 8, dirty: false, source: 'current' }];
    for (let i = 1; i <= 2; i++) {
      backend.emit('drive-upload-staged', {
        pending_id: String(i).padStart(32, 'b'), path: source,
        folder_id: 9100001, file_name: 'Drive delayed sync.txt',
      });
      await h.flush();
    }
    assert.equal(backend.driveSyncs, 2);
    delayed.resolve({ revision: 6, dirty: true, source: 'current' });
    await h.flush();
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 0,
      'stale delayed dirty response may not override newer clean metadata result');
  })
);

test('uploaded Drive bytes stay successful even if later Telegram metadata sync fails', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-real-upload-sync.txt');
    fs.writeFileSync(source, 'new upload bytes');
    backend.driveSyncResults = [new Error('synthetic Telegram unavailable')];
    backend.emit('drive-upload-staged', {
      pending_id: '7'.repeat(32), path: source, folder_id: 9100001,
      file_name: 'Drive real upload.txt',
    });
    await h.flush();
    assert.equal(backend.jobs.length, 1, 'new bytes really enter the upload worker');
    backend.complete(backend.jobs[0].id, null, {
      message: 'synthetic transfer complete', logical_file_id: 'a'.repeat(32),
    });
    await h.flush();
    assert.equal(backend.driveFinalized.length, 1, 'local native finalize must still complete');
    assert.equal(backend.driveSyncs, 1);
    assert.equal(h.queue[0]?.status, 'success',
      'metadata publication failure must not mark uploaded bytes as failed');
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 1,
      'new uploaded bytes also surface the cross-device publication warning');
  })
);

test('Drive metadata warning persists after uploads drain and Retry synchronizes once', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-visible-warning.txt');
    fs.writeFileSync(source, 'identical bytes');
    backend.preflightResult = {
      file_name: 'Visible Drive warning.txt', size: 15,
      source_sha256: 'f'.repeat(64), source_identity: 'fixture-visible-warning',
      exact_duplicate: { logical_file_id: 'e'.repeat(32), message_id: 77,
        name: 'same.txt', size: 15 }, similar_files: [],
    };
    const retry = deferred();
    backend.driveSyncResults = [new Error('offline'), retry.promise];
    backend.emit('drive-upload-staged', {
      pending_id: 'd'.repeat(32), path: source, folder_id: 9100001,
      file_name: 'Visible Drive warning.txt',
    });
    await h.flush();
    assert.equal(h.queue.length, 0, 'duplicate content finalizes and drains queue');
    assert.match(h.state.driveSyncWarning, /metadata synchronization is unverified/,
      'warning persists even after all file transfers disappear');
    assert.equal(h.state.driveSyncRetryBusy, false);
    h.state.retryDriveMetadataSync();
    h.state.retryDriveMetadataSync();
    await h.flush();
    assert.equal(backend.driveSyncs, 2, 'rapid Retry clicks start only one native metadata request');
    assert.equal(h.state.driveSyncRetryBusy, true);
    assert.ok(h.state.driveSyncWarning, 'warning remains while native retry is unresolved');
    retry.resolve({ revision: 2, dirty: false, source: 'current' });
    await h.flush();
    assert.equal(h.state.driveSyncWarning, null, 'verified clean retry removes persistent warning');
    assert.equal(h.state.driveSyncRetryBusy, false, 'Retry button becomes usable after native result');
    assert.ok(h.toasts.some(t => t.level === 'success' && /synchronization verified/.test(t.text)));
    assert.equal(backend.driveFinalized.length, 1, 'manual Retry never re-uploads existing bytes');
  })
);

test('clean toolbar Drive Sync clears stale upload warning but QA, dirty and delayed old outcomes do not', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-toolbar-sync-signal.txt');
    fs.writeFileSync(source, 'already stored');
    backend.preflightResult = {
      file_name: 'Toolbar sync.txt', size: 14,
      source_sha256: 'f'.repeat(64), source_identity: 'fixture-toolbar-sync',
      exact_duplicate: { logical_file_id: '7'.repeat(32), message_id: 46,
        name: 'duplicate.txt', size: 14 }, similar_files: [],
    };
    const signal = h.load(path.join(root, 'app/src/driveSyncSignal.ts'));
    const send = id => backend.emit('drive-upload-staged', {
      pending_id: String(id).padStart(32, 'c'), path: source, folder_id: 9100001,
      file_name: 'Toolbar sync.txt',
    });
    backend.driveSyncResults = [new Error('offline metadata sync')];
    send(1);
    await h.flush();
    assert.ok(h.state.driveSyncWarning, 'upload warning is visible before toolbar action');
    assert.equal(signal.publishVerifiedDriveSync({ revision: 2, dirty: true, source: 'current' }), false);
    assert.equal(signal.publishVerifiedDriveSync({ revision: 3, dirty: false, source: 'qa-local' }), false);
    await h.flush();
    assert.ok(h.state.driveSyncWarning, 'dirty and local QA do not dismiss real warning');
    assert.equal(signal.publishVerifiedDriveSync({ revision: 4, dirty: false, source: 'merged-published' }), true);
    await h.flush();
    assert.equal(h.state.driveSyncWarning, null, 'verified toolbar publication clears outdated upload warning');
    const slow = deferred();
    backend.driveSyncResults = [slow.promise];
    send(2);
    await h.flush();
    assert.equal(backend.driveSyncs, 2);
    signal.publishVerifiedDriveSync({ revision: 5, dirty: false, source: 'current' });
    slow.reject(new Error('older sync completed late'));
    await h.flush();
    assert.equal(h.state.driveSyncWarning, null,
      'older asynchronous failed upload sync cannot resurrect a warning after newer toolbar success');
    assert.equal(backend.driveFinalized.length, 2, 'signal never retransmits or cancels durable uploads');
    assert.equal(backend.driveSyncs, 2, 'signal never launches an extra Telegram sync');
  })
);

test('Drive upload sync warning ignores native reply after frontend unmount', () =>
  withHarness('upload', { upload: [] }, async (h, backend) => {
    const source = path.join(fixtures, 'drive-closed-sync.txt');
    fs.writeFileSync(source, 'already stored bytes');
    backend.preflightResult = {
      file_name: 'Closed Drive sync.txt', size: 20,
      source_sha256: 'f'.repeat(64), source_identity: 'fixture-closed-sync',
      exact_duplicate: { logical_file_id: '8'.repeat(32), message_id: 46,
        name: 'duplicate.txt', size: 20 }, similar_files: [],
    };
    const pendingSync = deferred();
    backend.driveSyncResults = [pendingSync.promise];
    backend.emit('drive-upload-staged', {
      pending_id: 'c'.repeat(32), path: source, folder_id: 9100001,
      file_name: 'Closed Drive sync.txt',
    });
    await h.flush();
    assert.equal(backend.driveSyncs, 1);
    h.close();
    pendingSync.reject(new Error('offline after view closed'));
    for (let turn = 0; turn < 4; turn++) await new Promise(resolve => setImmediate(resolve));
    assert.equal(h.toasts.filter(t => t.level === 'warning').length, 0,
      'unmounted hook must not surface a late upload metadata warning');
  })
);

test('Drive upload warning panel remains usable with no queue rows and handles busy Retry', () => {
  const dashboard = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.match(dashboard, /driveSyncWarning !== null\) && \(/,
    'desktop transfer dock must stay visible after upload queue drains');
  assert.match(dashboard, /driveSyncWarning=\{driveSyncWarning\}/);
  assert.match(dashboard, /driveSyncRetryBusy=\{driveSyncRetryBusy\}/);
  assert.match(dashboard, /onRetryDriveSync=\{retryDriveMetadataSync\}/);
  const file = path.join(root, 'app/src/components/desktop/dashboard/UploadQueue.tsx');
  const compiled = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020,
      jsx: ts.JsxEmit.ReactJSX },
  }).outputText;
  const exported = {};
  const jsx = (type, props) => ({ type, props: props || {} });
  const icons = new Proxy({}, { get: (_target, name) => String(name) });
  vm.runInNewContext(compiled, {
    exports: exported,
    require(name) {
      if (name === 'react/jsx-runtime') return { jsx, jsxs: jsx, Fragment: 'fragment' };
      if (name === 'lucide-react') return icons;
      if (name === '../../../utils') return { formatBytes: n => `${n} bytes` };
      throw new Error('unexpected UI import: ' + name);
    },
  });
  const emptyProps = {
    items: [], onCancelAll() {}, onCancelItem() {}, onPauseItem() {},
    onResumeItem() {}, onRetryItem() {}, onDismissItem() {},
  };
  assert.equal(exported.UploadQueue(emptyProps), null, 'idle dock remains hidden without warning');
  let clicks = 0;
  const collect = (node, out = []) => {
    if (!node || typeof node !== 'object') return out;
    if (Array.isArray(node)) { node.forEach(part => collect(part, out)); return out; }
    out.push(node);
    collect(node.props?.children, out);
    return out;
  };
  const render = busy => collect(exported.UploadQueue({ ...emptyProps,
    driveSyncWarning: 'Telegram metadata publication not verified',
    driveSyncRetryBusy: busy,
    onRetryDriveSync: () => { clicks++; },
  }));
  const nodes = render(false);
  assert.ok(nodes.find(n => n.props?.role === 'alert'), 'persistent warning has alert semantics');
  assert.ok(!nodes.find(n => n.props?.role === 'list'), 'empty upload list is not fabricated');
  const retryButton = nodes.find(n => n.props?.['aria-label'] === 'Retry TeraRelay Drive metadata sync');
  assert.ok(retryButton, 'one-click Retry remains reachable with zero queued uploads');
  assert.equal(retryButton.props.disabled, false);
  retryButton.props.onClick();
  assert.equal(clicks, 1, 'Retry click invokes the actual wired callback');
  assert.equal(render(true).find(n => n.props?.['aria-label'] === 'Retry TeraRelay Drive metadata sync').props.disabled,
    true, 'Retry is disabled during an in-progress native metadata synchronization');
});

for (const kind of ['upload', 'download']) test('TeraRelay Boost ' + kind + ' persists its engine and bypasses TDLib setup', () =>
  withHarness(kind, { [kind]: [] }, async (h, backend) => {
    backend.authReady = false;
    backend.transferChoice = 'boost';
    if (kind === 'upload') {
      h.state.handleDropUpload(['/fixture/boost.bin']);
    } else {
      await h.state.queueDownload(7, 'boost.bin', null);
    }
    await h.flush();
    const saved = backend.read(kind);
    assert.equal(saved.length, 1);
    assert.equal(saved[0].transferEngine, 'boost');
    assert.equal(backend.jobs.length, 1, 'Boost was blocked by TDLib setup');
    if (kind === 'upload') {
      assert.equal(backend.jobs[0].args.transferEngine, 'boost');
    } else {
      assert.equal(backend.jobs[0].args.req.transfer_engine, 'boost');
    }
  })
);

for (const kind of ['upload', 'download']) test('reopened TeraRelay Boost ' + kind + ' keeps the chosen engine', () => {
  const item = kind === 'upload'
    ? up('boost-reopen', 'pending', { transferEngine: 'boost' })
    : down('boost-reopen', 'pending', { transferEngine: 'boost', savePath: '/fixture/boost-reopen.bin' });
  return withHarness(kind, { [kind]: [item] }, async (h, backend) => {
    backend.authReady = false;
    await h.flush();
    assert.equal(backend.jobs.length, 1);
    if (kind === 'upload') {
      assert.equal(backend.jobs[0].args.transferEngine, 'boost');
    } else {
      assert.equal(backend.jobs[0].args.req.transfer_engine, 'boost');
    }
  });
});

for (const kind of ['upload', 'download']) test('legacy ' + kind + ' queue keeps historical implicit engine selection', () => {
  const item = kind === 'upload'
    ? up('legacy-engine', 'pending')
    : down('legacy-engine', 'pending', { savePath: '/fixture/legacy-engine.bin' });
  return withHarness(kind, { [kind]: [item] }, async (h, backend) => {
    await h.flush();
    assert.equal(backend.jobs.length, 1);
    if (kind === 'upload') {
      assert.equal(backend.jobs[0].args.transferEngine, undefined);
    } else {
      assert.equal(backend.jobs[0].args.req.transfer_engine, undefined);
    }
  });
});

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

test('Boost choice is durable and bypasses TDLib readiness', () => withHarness('upload', { upload: [] }, async (h, backend) => {
  backend.transferChoice = 'boost';
  backend.authReady = false;
  h.state.handleDropUpload(['/fixture/boost.bin']);
  await h.flush();
  assert.equal(backend.authCalls, 0, 'Boost unexpectedly entered TDLib readiness');
  assert.equal(backend.jobs.length, 1, 'Boost upload did not start');
  assert.equal(backend.jobs[0].args.transferEngine, 'boost');
  assert.equal(backend.read('upload')[0].transferEngine, 'boost', 'Boost engine was not durable');

  h.close();
  const reopened = makeHarness('upload', backend);
  try {
    await reopened.flush();
    assert.equal(reopened.queue[0].transferEngine, 'boost', 'Recovered upload lost Boost selection');
  } finally { reopened.close(); }
}));

test('TDLib choice still uses the existing readiness flow and reaches backend unchanged', () => withHarness('download', { download: [] }, async (h, backend) => {
  backend.transferChoice = 'tdlib';
  backend.authReady = true;
  await h.state.queueDownload(77, 'tdlib.bin', 9);
  await h.flush();
  assert.equal(backend.authCalls, 1, 'TDLib choice did not use the existing readiness flow');
  assert.equal(backend.jobs.length, 1, 'TDLib download did not start after readiness');
  assert.equal(backend.jobs[0].args.req.transfer_engine, 'tdlib');
  assert.equal(backend.read('download')[0].transferEngine, 'tdlib', 'TDLib engine was not durable');
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
