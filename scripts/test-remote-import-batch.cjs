const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { createRequire } = require('node:module');
const assert = require('node:assert/strict');

const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
let failures = 0;
const asyncTests = [];

function test(name, fn) {
  try { fn(); console.log('PASS ' + name); }
  catch (error) { failures++; console.error('FAIL ' + name + ': ' + error.message); }
}
function testAsync(name, fn) { asyncTests.push([name, fn]); }

function loadTs(file) {
  const source = fs.readFileSync(file, 'utf8');
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const mod = { exports: {} };
  vm.runInThisContext('(function(require,module,exports){' + output + '\n})', { filename: file })(require, mod, mod.exports);
  return mod.exports;
}

const batchPath = path.join(root, 'app/src/remoteImportBatch.ts');

test('batch parser preserves order, duplicates, and removes blank lines', () => {
  assert.ok(fs.existsSync(batchPath), 'remoteImportBatch.ts is missing');
  const model = loadTs(batchPath);
  assert.deepEqual(model.parseRemoteImportSources('  https://a.test/v  \n\nmagnet:?xt=abc\nhttps://a.test/v\n'), [
    'https://a.test/v', 'magnet:?xt=abc', 'https://a.test/v',
  ]);
});

test('batch parser rejects more than 50 sources without truncating', () => {
  const model = loadTs(batchPath);
  const input = Array.from({ length: 51 }, (_, i) => `https://example.test/${i}`).join('\n');
  assert.throws(() => model.parseRemoteImportSources(input), /50/);
  assert.equal(model.MAX_REMOTE_IMPORT_BATCH_SOURCES, 50);
});

test('batch captures immutable destination and distinct ordered entries', () => {
  const model = loadTs(batchPath);
  const batch = model.createRemoteImportBatch(77, 'https://a.test\nhttps://a.test\nmagnet:?xt=x');
  assert.equal(batch.folderId, 77);
  assert.equal(batch.entries.length, 3);
  assert.deepEqual(batch.entries.map(e => e.ordinal), [0, 1, 2]);
  assert.equal(new Set(batch.entries.map(e => e.id)).size, 3);
  assert.deepEqual(batch.entries.map(e => e.source), ['https://a.test', 'https://a.test', 'magnet:?xt=x']);
});

test('multi-link mode never chooses default qualities, torrent files, or direct file item', () => {
  const model = loadTs(batchPath);
  for (const analysis of [
    { source_kind: 'web_media', title: 'v', source_label: 'YouTube', qualities: [{ format_id: '1080', label: '1080p', recommended: true, hdr: false, has_audio: true }], items: [] },
    { source_kind: 'torrent', title: 't', source_label: 'Torrent', qualities: [], items: [{ id: '0', label: 'a.mkv', kind: 'file' }] },
    { source_kind: 'direct_file', title: 'f', source_label: 'Direct', qualities: [], items: [{ id: 'direct', label: 'a.mp4', kind: 'file' }] },
  ]) assert.deepEqual(model.multiLinkDefaultSelectionIds(analysis), []);
});


test('corrupt persisted batch/group records are ignored without throwing during recovery', () => {
  const model = loadTs(batchPath);
  const valid = model.createRemoteImportBatch(1, 'https://good.test');
  const corrupt = { ...valid, id: 'bad', stackGroups: [{ id: 'broken', primaryConfirmed: true }] };
  assert.doesNotThrow(() => model.normalizeRecoveredRemoteImportBatches([corrupt, valid]));
  const recovered = model.normalizeRecoveredRemoteImportBatches([corrupt, valid]);
  assert.equal(recovered.length, 1);
  assert.equal(recovered[0].id, valid.id);
});

test('recovery pauses interrupted batch work and preserves completed entries', () => {
  const model = loadTs(batchPath);
  const recovered = model.normalizeRecoveredRemoteImportBatches([{
    schema: 1, id: 'batch-1', folderId: 77, phase: 'importing', activeEntryIndex: 1,
    entries: [
      { id: 'e1', ordinal: 0, source: 'https://a', jobId: 'j1', selectedIds: ['1080'], phase: 'success' },
      { id: 'e2', ordinal: 1, source: 'https://b', jobId: 'j2', selectedIds: ['720'], phase: 'importing' },
    ],
    stackGroups: [], createdAt: 1, updatedAt: 2,
  }]);
  assert.equal(recovered.length, 1);
  assert.equal(recovered[0].id, 'batch-1');
  assert.equal(recovered[0].phase, 'paused');
  assert.equal(recovered[0].entries[0].phase, 'success');
  assert.equal(recovered[0].entries[1].phase, 'error');
  assert.match(recovered[0].entries[1].error, /interrupted|retry/i);
});


test('manual groups are explicit, cross-source, and reject overlapping membership', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  batch.entries[0].selectedIds = ['1080', '720'];
  batch.entries[1].selectedIds = ['4k'];
  assert.equal(batch.stackGroups.length, 0, 'cross-source groups must never be inferred');
  batch = model.groupSelectedMembers(batch, [
    { entryId: batch.entries[0].id, selectionId: '1080' },
    { entryId: batch.entries[1].id, selectionId: '4k' },
  ], { name: 'Movie', primary: { entryId: batch.entries[1].id, selectionId: '4k' } });
  assert.equal(batch.stackGroups.length, 1);
  assert.equal(batch.stackGroups[0].primaryConfirmed, true);
  assert.throws(() => model.groupSelectedMembers(batch, [
    { entryId: batch.entries[0].id, selectionId: '1080' },
    { entryId: batch.entries[1].id, selectionId: '4k' },
  ]), /already|group/i);
  assert.throws(() => model.groupSelectedMembers(batch, [
    { entryId: batch.entries[0].id, selectionId: 'missing' },
    { entryId: batch.entries[1].id, selectionId: '4k' },
  ]), /selected|missing/i);
});

test('manual cross-source involvement disables overlapping same-source auto-stack', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  const e1 = batch.entries[0], e2 = batch.entries[1];
  e1.selectedIds = ['720', '1080', '4k'];
  e2.selectedIds = ['2160'];
  const analysis = { source_kind: 'web_media', title: 'Video', source_label: 'YouTube', items: [], qualities: [
    { format_id: '720', label: '720p', height: 720, hdr: false, recommended: false, has_audio: true },
    { format_id: '1080', label: '1080p', height: 1080, hdr: false, recommended: true, has_audio: true },
    { format_id: '4k', label: '4K', height: 2160, hdr: false, recommended: false, has_audio: true },
  ] };
  const job = { id: 'j1', source: e1.source, folderId: 1, phase: 'waiting-for-selection', analysis, selectedIds: e1.selectedIds, outputs: [], uploadQueueIds: [], createdAt: 1, updatedAt: 1 };
  assert.equal(model.planEntryFinalization(batch, e1, job).automaticSameSourceStack, true);
  batch = model.groupSelectedMembers(batch, [
    { entryId: e1.id, selectionId: '4k' },
    { entryId: e2.id, selectionId: '2160' },
  ]);
  const plan = model.planEntryFinalization(batch, e1, job);
  assert.equal(plan.automaticSameSourceStack, false);
  assert.deepEqual(plan.standaloneSelectionIds.sort(), ['1080', '720'].sort());
  assert.equal(model.sourceUsesManualStack(batch, e1.id), true);
});

test('torrent outputs from different links resolve into a manual version group only after uploads exist', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'magnet:?xt=a\nmagnet:?xt=b');
  const [a, b] = batch.entries;
  a.jobId = 'ja'; b.jobId = 'jb';
  a.selectedIds = ['file-a']; b.selectedIds = ['file-b'];
  batch = model.groupSelectedMembers(batch, [
    { entryId: a.id, selectionId: 'file-a' },
    { entryId: b.id, selectionId: 'file-b' },
  ], { primary: { entryId: b.id, selectionId: 'file-b' } });
  const base = { folderId: 1, phase: 'uploading', outputs: [], createdAt: 1, updatedAt: 1 };
  const jobs = new Map([
    ['ja', { ...base, id: 'ja', source: a.source, selectedIds: ['file-a'], uploadQueueIds: ['qa'], uploadOutcomes: { qa: { queueId: 'qa', selectionId: 'file-a', logicalFileId: 'fa', status: 'success', label: '1080p', height: 1080 } } }],
    ['jb', { ...base, id: 'jb', source: b.source, selectedIds: ['file-b'], uploadQueueIds: ['qb'], uploadOutcomes: { qb: { queueId: 'qb', selectionId: 'file-b', logicalFileId: 'fb', status: 'success', label: '4K', height: 2160 } } }],
  ]);
  const plan = model.planCrossSourceStackFinalization(batch, batch.stackGroups[0], jobs);
  assert.equal(plan.ready, true);
  assert.deepEqual(plan.members.map(x => x.logicalFileId), ['fa', 'fb']);
  assert.equal(plan.primary.logicalFileId, 'fb');
});



test('cross-source stack retry planning is idempotent and seeds creation with the confirmed primary', () => {
  const model = loadTs(batchPath);
  const primary = { ref: { entryId: 'e3', selectionId: '4k' }, logicalFileId: 'f4k', label: '4K', height: 2160 };
  const plan = {
    ready: true,
    members: [
      { ref: { entryId: 'e1', selectionId: '720' }, logicalFileId: 'f720', label: '720p', height: 720 },
      { ref: { entryId: 'e2', selectionId: '1080' }, logicalFileId: 'f1080', label: '1080p', height: 1080 },
      primary,
    ],
    primary,
    missing: [],
  };
  const seed = model.selectCrossSourceStackSeed(plan);
  assert.deepEqual(seed.map(x => x.logicalFileId), ['f4k', 'f720']);
  const group = { id: 'g', members: [], primaryConfirmed: true, stackId: 'stack-1', stackedFileIds: ['f4k', 'f720'] };
  assert.deepEqual(model.missingCrossSourceStackMembers(group, plan).map(x => x.logicalFileId), ['f1080']);
  group.stackedFileIds.push('f1080');
  assert.deepEqual(model.missingCrossSourceStackMembers(group, plan), []);
});

test('failed manual group member keeps group unready and successful logical files safe', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'magnet:?xt=a\nmagnet:?xt=b');
  const [a, b] = batch.entries;
  a.jobId = 'ja'; b.jobId = 'jb'; a.selectedIds = ['a']; b.selectedIds = ['b'];
  batch = model.groupSelectedMembers(batch, [
    { entryId: a.id, selectionId: 'a' }, { entryId: b.id, selectionId: 'b' },
  ], { primary: { entryId: a.id, selectionId: 'a' } });
  const base = { folderId: 1, phase: 'uploading', outputs: [], createdAt: 1, updatedAt: 1 };
  const jobs = new Map([
    ['ja', { ...base, id: 'ja', source: a.source, selectedIds: ['a'], uploadQueueIds: ['qa'], uploadOutcomes: { qa: { queueId: 'qa', selectionId: 'a', logicalFileId: 'fa', status: 'success' } } }],
    ['jb', { ...base, id: 'jb', source: b.source, selectedIds: ['b'], uploadQueueIds: ['qb'], uploadOutcomes: { qb: { queueId: 'qb', selectionId: 'b', status: 'error', error: 'network' } } }],
  ]);
  const plan = model.planCrossSourceStackFinalization(batch, batch.stackGroups[0], jobs);
  assert.equal(plan.ready, false);
  assert.deepEqual(plan.members.map(x => x.logicalFileId), ['fa']);
  assert.deepEqual(plan.missing, [{ entryId: b.id, selectionId: 'b' }]);
});

testAsync('sequential batch runner preserves order, never overlaps, resumes after completed entries, and stops on failure', async () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b\nhttps://c\nhttps://d');
  batch.entries[0].phase = 'success';
  const order = [];
  let inFlight = 0;
  let maxInFlight = 0;
  batch = await model.runSequentialBatchEntries(batch, async entry => {
    order.push(entry.ordinal);
    inFlight++;
    maxInFlight = Math.max(maxInFlight, inFlight);
    await new Promise(resolve => setTimeout(resolve, 5));
    inFlight--;
    if (entry.ordinal === 2) return { ...entry, phase: 'error', error: 'boom' };
    return { ...entry, phase: 'success' };
  }, async () => {});
  assert.deepEqual(order, [1, 2]);
  assert.equal(maxInFlight, 1);
  assert.equal(batch.entries[0].phase, 'success');
  assert.equal(batch.entries[1].phase, 'success');
  assert.equal(batch.entries[2].phase, 'error');
  assert.equal(batch.entries[3].phase, 'pending');
  assert.equal(batch.phase, 'paused');
});

test('recovered draft batch becomes paused and clears stale active entry without inventing selections', () => {
  const model = loadTs(batchPath);
  const draft = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  draft.phase = 'draft';
  draft.activeEntryIndex = 0;
  const [recovered] = model.normalizeRecoveredRemoteImportBatches([draft]);
  assert.equal(recovered.phase, 'paused');
  assert.equal(recovered.activeEntryIndex, null);
  assert.deepEqual(recovered.entries.map(entry => entry.phase), ['pending', 'pending']);
  assert.deepEqual(recovered.entries.map(entry => entry.selectedIds), [[], []]);
});

test('completed source execution stays durable-finalizing until cross-link stack finalization finishes', () => {
  const model = loadTs(batchPath);
  const recovered = model.normalizeRecoveredRemoteImportBatches([{
    schema: 1, id: 'batch-finalizing', folderId: 1, phase: 'finalizing', activeEntryIndex: null,
    startedAt: 10, entries: [
      { id: 'e1', ordinal: 0, source: 'https://a', jobId: 'j1', selectedIds: ['1080'], phase: 'success' },
      { id: 'e2', ordinal: 1, source: 'https://b', jobId: 'j2', selectedIds: ['4k'], phase: 'success' },
    ], stackGroups: [], createdAt: 1, updatedAt: 2,
  }]);
  assert.equal(recovered[0].phase, 'paused');
  assert.equal(recovered[0].entries[0].phase, 'success');
  assert.equal(recovered[0].entries[1].phase, 'success');
  assert.equal(recovered[0].startedAt, 10);
});

test('manual Version Stack configuration is locked once batch execution starts', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  batch.entries[0].selectedIds = ['a'];
  batch.entries[1].selectedIds = ['b'];
  batch.startedAt = 123;
  assert.equal(model.batchConfigurationLocked(batch), true);
  assert.throws(() => model.groupSelectedMembers(batch, [
    { entryId: batch.entries[0].id, selectionId: 'a' },
    { entryId: batch.entries[1].id, selectionId: 'b' },
  ], { primary: { entryId: batch.entries[0].id, selectionId: 'a' } }), /started|locked|configuration/i);
});

test('skipping after execution starts preserves locked selections and requested manual Version Stack', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  const [a, b] = batch.entries;
  a.selectedIds = ['1080'];
  b.selectedIds = ['4k'];
  batch = model.groupSelectedMembers(batch, [
    { entryId: a.id, selectionId: '1080' },
    { entryId: b.id, selectionId: '4k' },
  ], { primary: { entryId: b.id, selectionId: '4k' } });
  batch.startedAt = 123;
  const skipped = model.skipRemoteImportBatchEntry(batch, a.id);
  assert.equal(skipped.entries[0].phase, 'skipped');
  assert.deepEqual(skipped.entries[0].selectedIds, ['1080'], 'locked selections must remain immutable');
  assert.equal(skipped.stackGroups.length, 1, 'requested manual stack must not disappear after execution starts');
  assert.equal(skipped.stackGroups[0].members.length, 2);
});

test('skipping before execution clears that source selection and removes now-invalid groups', () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  const [a, b] = batch.entries;
  a.selectedIds = ['1080'];
  b.selectedIds = ['4k'];
  batch = model.groupSelectedMembers(batch, [
    { entryId: a.id, selectionId: '1080' },
    { entryId: b.id, selectionId: '4k' },
  ], { primary: { entryId: b.id, selectionId: '4k' } });
  const skipped = model.skipRemoteImportBatchEntry(batch, a.id);
  assert.equal(skipped.entries[0].phase, 'skipped');
  assert.deepEqual(skipped.entries[0].selectedIds, []);
  assert.equal(skipped.stackGroups.length, 0);
});

test('batch import validation rejects a job bound to a different destination channel', () => {
  const model = loadTs(batchPath);
  const batch = model.createRemoteImportBatch(77, 'https://a');
  batch.entries[0].jobId = 'j1';
  batch.entries[0].selectedIds = ['1080'];
  batch.entries[0].phase = 'waiting-for-selection';
  const jobs = new Map([['j1', {
    id: 'j1', source: 'https://a', folderId: 88, phase: 'waiting-for-selection', selectedIds: ['1080'],
    analysis: { source_kind: 'web_media', title: 'A', source_label: 'Test', qualities: [{ format_id: '1080', label: '1080p', recommended: true, hdr: false, has_audio: true }], items: [] },
    outputs: [], uploadQueueIds: [], createdAt: 1, updatedAt: 1,
  }]]);
  const errors = model.validateBatchReadyForImport(batch, jobs);
  assert.ok(errors.some(error => /destination|channel/i.test(error)), errors.join('\n'));
});

testAsync('analysis persists a reserved job id before extractor work so a crash cannot orphan it', async () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a');
  const persisted = [];
  batch = await model.analyzeBatchEntriesSequentially(batch, async (entry, rememberJobId) => {
    await rememberJobId('reserved-job');
    throw new Error('extractor interrupted');
  }, async snapshot => { persisted.push(JSON.parse(JSON.stringify(snapshot))); });
  assert.equal(batch.entries[0].jobId, 'reserved-job');
  assert.equal(batch.entries[0].phase, 'error');
  const reservation = persisted.find(snapshot => snapshot.entries[0].jobId === 'reserved-job' && snapshot.entries[0].phase === 'analyzing');
  assert.ok(reservation, 'reserved job id was not made durable before extractor failure');
});

testAsync('resumed analysis skips already analyzed entries and continues only pending links in order', async () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b\nhttps://c');
  batch.phase = 'paused';
  batch.entries[0].jobId = 'job-0';
  batch.entries[0].phase = 'waiting-for-selection';
  batch.entries[1].phase = 'pending';
  batch.entries[2].phase = 'pending';
  const calls = [];
  batch = await model.analyzeBatchEntriesSequentially(batch, async entry => {
    calls.push(entry.ordinal);
    return { id: `job-${entry.ordinal}` };
  }, async () => {});
  assert.deepEqual(calls, [1, 2]);
  assert.equal(batch.entries[0].jobId, 'job-0');
  assert.equal(batch.entries[1].jobId, 'job-1');
  assert.equal(batch.entries[2].jobId, 'job-2');
  assert.equal(batch.phase, 'waiting-for-selection');
});

testAsync('fully completed sequential source execution ends in finalizing, not success', async () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b');
  batch.entries.forEach(entry => { entry.phase = 'waiting-for-selection'; entry.selectedIds = ['x']; });
  batch = await model.runSequentialBatchEntries(batch, async entry => ({ ...entry, phase: 'success' }), async () => {});
  assert.equal(batch.phase, 'finalizing');
  assert.ok(batch.startedAt, 'execution start was not persisted');
  assert.ok(batch.entries.every(entry => entry.phase === 'success'));
});

testAsync('sequential analysis creates no defaults and continues after one failed source', async () => {
  const model = loadTs(batchPath);
  let batch = model.createRemoteImportBatch(1, 'https://a\nhttps://b\nhttps://c');
  const calls = [];
  batch = await model.analyzeBatchEntriesSequentially(batch, async entry => {
    calls.push(entry.ordinal);
    if (entry.ordinal === 1) throw Object.assign(new Error('bad source'), { remoteImportJobId: 'job-1' });
    return { id: `job-${entry.ordinal}`, selectedIds: ['should-not-leak'] };
  }, async () => {});
  assert.deepEqual(calls, [0, 1, 2]);
  assert.equal(batch.entries[0].jobId, 'job-0');
  assert.deepEqual(batch.entries[0].selectedIds, []);
  assert.equal(batch.entries[1].phase, 'error');
  assert.equal(batch.entries[1].jobId, 'job-1');
  assert.equal(batch.entries[2].jobId, 'job-2');
  assert.equal(batch.phase, 'paused');
});


(async () => {
  for (const [name, fn] of asyncTests) {
    try { await fn(); console.log('PASS ' + name); }
    catch (error) { failures++; console.error('FAIL ' + name + ': ' + error.message); }
  }
  if (failures) process.exit(1);
  console.log('Remote Import batch model checks passed');
})();
