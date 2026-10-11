const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { createRequire } = require('node:module');
const assert = require('node:assert/strict');

const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
let failures = 0;

function test(name, fn) {
  try {
    fn();
    console.log('PASS ' + name);
  } catch (error) {
    failures++;
    console.error('FAIL ' + name + ': ' + error.message);
  }
}

function loadTs(file) {
  const source = fs.readFileSync(file, 'utf8');
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const mod = { exports: {} };
  vm.runInThisContext('(function(require,module,exports){' + output + '\n})', { filename: file })(require, mod, mod.exports);
  return mod.exports;
}

const remoteImportPath = path.join(root, 'app/src/remoteImport.ts');
const hookPath = path.join(root, 'app/src/hooks/useRemoteImport.ts');
const uploadPath = path.join(root, 'app/src/hooks/useFileUpload.ts');
const typesPath = path.join(root, 'app/src/types.ts');

test('remote import pure model selects recommended quality and orders quality height', () => {
  assert.ok(fs.existsSync(remoteImportPath), 'remoteImport.ts is missing');
  const model = loadTs(remoteImportPath);
  const qualities = [
    { format_id: '720', label: '720p', height: 720, fps: 30, hdr: false, recommended: false, has_audio: true },
    { format_id: '4k', label: '4K', height: 2160, fps: 30, hdr: false, recommended: false, has_audio: false },
    { format_id: '1080', label: '1080p', height: 1080, fps: 30, hdr: false, recommended: true, has_audio: false },
  ];
  assert.equal(model.pickRecommendedSelection(qualities), '1080');
  assert.deepEqual(model.sortQualitiesHighestFirst(qualities).map(q => q.format_id), ['4k', '1080', '720']);
});

test('recovery keeps the same job id/destination and converts interrupted acquisition to retryable error', () => {
  assert.ok(fs.existsSync(remoteImportPath), 'remoteImport.ts is missing');
  const model = loadTs(remoteImportPath);
  const jobs = model.normalizeRecoveredRemoteImportJobs([{ id: 'job-1', source: 'https://example.test/v', folderId: 77, phase: 'downloading', selectedIds: ['1080'], outputs: [], uploadQueueIds: [], createdAt: 1, updatedAt: 2 }]);
  assert.equal(jobs.length, 1);
  assert.equal(jobs[0].id, 'job-1');
  assert.equal(jobs[0].folderId, 77);
  assert.equal(jobs[0].phase, 'error');
  assert.match(jobs[0].error, /resume|retry|interrupted/i);
});

test('prepared remote outputs enter the normal upload queue with durable ownership metadata', () => {
  const upload = fs.readFileSync(uploadPath, 'utf8');
  const types = fs.readFileSync(typesPath, 'utf8');
  assert.ok(upload.includes('enqueuePreparedImports'), 'prepared-file enqueue entry point is missing');
  assert.ok(upload.includes('uploadName: workingItem.uploadName'), 'prepared display filename is not passed to cmd_upload_file');
  assert.ok(upload.includes('remoteImportJobId'), 'upload queue does not preserve remote import ownership');
  assert.ok(types.includes('remoteImportJobId?: string'), 'QueueItem lacks remote import ownership metadata');
  assert.ok(types.includes('uploadName?: string'), 'QueueItem lacks explicit upload name');
  const start = upload.indexOf('const enqueuePreparedImports');
  const end = upload.indexOf('\n    const ', start + 10);
  assert.ok(start >= 0, 'enqueuePreparedImports function is missing');
  const body = upload.slice(start, end > start ? end : undefined);
  assert.ok(!body.includes('cmd_upload_from_url'), 'prepared imports must not use legacy URL upload');
});

test('remote import hook persists jobs, captures destination, listens for progress, and acquires by job id', () => {
  assert.ok(fs.existsSync(hookPath), 'useRemoteImport.ts is missing');
  const hook = fs.readFileSync(hookPath, 'utf8');
  assert.ok(hook.includes('REMOTE_IMPORT_STORE_KEY'), 'durable store key is missing');
  assert.ok(hook.includes("cmd_remote_import_analyze"), 'analysis backend call is missing');
  assert.ok(hook.includes("cmd_remote_import_acquire"), 'acquisition backend call is missing');
  assert.ok(hook.includes("remote-import-progress"), 'progress listener is missing');
  assert.match(hook, /folderId:\s*activeFolderId/, 'destination is not captured from the current channel');
  assert.ok(hook.includes('job_id: job.id'), 'retry/acquisition does not reuse the durable job id');
  assert.ok(hook.includes('enqueuePreparedImports'), 'acquired outputs are not handed to the normal upload queue');
});

test('finalization plan keeps one quality normal and stacks multiple qualities with highest primary', () => {
  const model = loadTs(remoteImportPath);
  const analysis = {
    source_kind: 'web_media', title: 'Video', source_label: 'YouTube', items: [],
    qualities: [
      { format_id: '720', label: '720p', height: 720, fps: 30, hdr: false, recommended: false, has_audio: true },
      { format_id: '1080', label: '1080p', height: 1080, fps: 30, hdr: false, recommended: true, has_audio: false },
      { format_id: '4k', label: '4K', height: 2160, fps: 30, hdr: false, recommended: false, has_audio: false },
    ],
  };
  const base = { id: 'job', source: 'x', folderId: 1, analysis, outputs: [], createdAt: 1, updatedAt: 1 };
  const one = model.planRemoteImportFinalization({ ...base, phase: 'uploading', selectedIds: ['1080'], uploadQueueIds: ['q1'], uploadOutcomes: { q1: { queueId: 'q1', selectionId: '1080', logicalFileId: 'f1080', status: 'success', label: '1080p', height: 1080 } } });
  assert.equal(one.ready, true);
  assert.equal(one.shouldStack, false);
  assert.equal(one.terminalPhase, 'success');

  const three = model.planRemoteImportFinalization({ ...base, phase: 'uploading', selectedIds: ['720', '1080', '4k'], uploadQueueIds: ['q720', 'q1080', 'q4k'], uploadOutcomes: {
    q720: { queueId: 'q720', selectionId: '720', logicalFileId: 'f720', status: 'success', label: '720p', height: 720 },
    q1080: { queueId: 'q1080', selectionId: '1080', logicalFileId: 'f1080', status: 'success', label: '1080p', height: 1080 },
    q4k: { queueId: 'q4k', selectionId: '4k', logicalFileId: 'f4k', status: 'success', label: '4K', height: 2160 },
  } });
  assert.equal(three.ready, true);
  assert.equal(three.shouldStack, true);
  assert.equal(three.primary?.logicalFileId, 'f4k');
  assert.deepEqual(three.available.map(x => x.logicalFileId), ['f4k', 'f1080', 'f720']);
  assert.equal(three.terminalPhase, 'success');
});

test('partial multi-quality upload is safe and only stacks successful members', () => {
  const model = loadTs(remoteImportPath);
  const analysis = {
    source_kind: 'web_media', title: 'Video', source_label: 'YouTube', items: [],
    qualities: [
      { format_id: '720', label: '720p', height: 720, fps: 30, hdr: false, recommended: false, has_audio: true },
      { format_id: '1080', label: '1080p', height: 1080, fps: 30, hdr: false, recommended: true, has_audio: false },
      { format_id: '4k', label: '4K', height: 2160, fps: 30, hdr: false, recommended: false, has_audio: false },
    ],
  };
  const plan = model.planRemoteImportFinalization({ id: 'job', source: 'x', folderId: 1, phase: 'uploading', analysis, selectedIds: ['720', '1080', '4k'], outputs: [], uploadQueueIds: ['q720', 'q1080', 'q4k'], createdAt: 1, updatedAt: 1, uploadOutcomes: {
    q720: { queueId: 'q720', selectionId: '720', logicalFileId: 'f720', status: 'success', label: '720p', height: 720 },
    q1080: { queueId: 'q1080', selectionId: '1080', logicalFileId: 'f1080', status: 'success', label: '1080p', height: 1080 },
    q4k: { queueId: 'q4k', selectionId: '4k', status: 'error', error: 'network' },
  } });
  assert.equal(plan.ready, true);
  assert.equal(plan.shouldStack, true);
  assert.equal(plan.available.length, 2);
  assert.equal(plan.terminalPhase, 'partial-error');
  assert.equal(plan.failures.length, 1);
});

test('upload worker records durable remote import outcomes before normal queue cleanup', () => {
  const upload = fs.readFileSync(uploadPath, 'utf8');
  assert.ok(upload.includes('recordRemoteImportUploadOutcome'), 'upload worker does not persist remote import outcomes');
  assert.ok(upload.includes("status: 'success'"), 'successful prepared upload outcome is missing');
  assert.ok(upload.includes("status: 'skipped'"), 'exact duplicate skip outcome is missing');
  assert.ok(upload.includes('logicalFileId: uploadResult.logical_file_id'), 'logical file ID is not persisted for finalization');
});

test('multi-quality finalizer uses only existing Version Stack commands and quality labels', () => {
  const hook = fs.readFileSync(hookPath, 'utf8');
  for (const command of ['cmd_create_file_stack', 'cmd_add_file_to_stack', 'cmd_update_file_stack_label', 'cmd_set_file_stack_primary']) {
    assert.ok(hook.includes(command), command + ' is missing from Remote Import finalization');
  }
  assert.ok(hook.includes('stackedFileIds'), 'finalizer cannot resume partial stack creation safely');
  assert.ok(hook.includes('cmd_remote_import_cleanup'), 'successful import staging cleanup is missing');
});

test('remote import modal has no destination selector and uses direct multi-select qualities/items', () => {
  const modalPath = path.join(root, 'app/src/components/desktop/dashboard/RemoteUploadModal.tsx');
  const modal = fs.readFileSync(modalPath, 'utf8');
  assert.ok(!modal.includes('<select'), 'destination channel selector still exists');
  assert.ok(!modal.includes('folders:'), 'modal still accepts a folders list');
  assert.ok(!modal.includes('defaultFolderId'), 'modal still accepts a destination override');
  assert.ok(modal.includes('type="checkbox"'), 'quality/item multi-select checkboxes are missing');
  assert.ok(modal.includes('Analyze'), 'Analyze action is missing');
  assert.ok(modal.includes('Import'), 'single Import action is missing');
  assert.ok(modal.includes('selectedIds'), 'modal does not track direct selections');
  assert.ok(!modal.toLowerCase().includes('version stack'), 'modal exposes an unnecessary Version Stack mode');
});

test('desktop dashboard wires Remote Import to the current channel only', () => {
  const dashboard = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  const topbar = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/TopBar.tsx'), 'utf8');
  assert.ok(dashboard.includes('useRemoteImport'), 'Remote Import hook is not wired into desktop dashboard');
  assert.ok(dashboard.includes('enqueuePreparedImports'), 'dashboard does not hand staged imports into normal upload queue');
  assert.ok(!dashboard.includes('folders={folders}\n                        defaultFolderId={activeFolderId}'), 'old destination dropdown props remain');
  assert.ok(dashboard.includes('activeFolderId={activeFolderId}'), 'modal does not receive current channel context');
  assert.ok(topbar.includes('channelMode && !readOnly'), 'Remote Import action is not limited to writable channels');
});

test('successful batch finalization refreshes the open channel so new Version Stacks collapse immediately', () => {
  const modal = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/RemoteUploadModal.tsx'), 'utf8');
  const dashboard = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.ok(modal.includes('onBatchSettled'), 'batch modal has no completion callback for post-finalization refresh');
  assert.match(modal, /startBatch\(batch\.id\)[\s\S]*onBatchSettled/, 'batch completion does not notify the dashboard after finalization');
  assert.ok(dashboard.includes('onBatchSettled'), 'dashboard does not receive the batch completion signal');
  assert.match(dashboard, /invalidateQueries\(\{ queryKey: \['files', activeFolderId\] \}\)/, 'batch completion does not invalidate the active channel file list');
});



test('Remote Import job mutations adopt the merged durable store snapshot and serialize state writes', () => {
  const hook = fs.readFileSync(hookPath, 'utf8');
  assert.ok(hook.includes('jobMutationTailRef'), 'Remote Import job state mutations are not serialized');
  assert.ok(hook.includes('const persisted = await persistSnapshot(next)'), 'job state save does not read back the merged durable snapshot');
  assert.ok(hook.includes('jobsRef.current = persisted'), 'in-memory jobs can remain stale after durable upload outcome merging');
});

test('single-job Remote Import exposes batch-safe no-default and deferred-finalization controls', () => {
  const hook = fs.readFileSync(hookPath, 'utf8');
  const model = fs.readFileSync(remoteImportPath, 'utf8');
  assert.ok(hook.includes('defaultSelection?: boolean'), 'analyzeSource lacks explicit default-selection control');
  assert.ok(hook.includes('options?.defaultSelection !== false'), 'single-link recommended selection is not preserved by default');
  assert.ok(hook.includes('deferAutomaticFinalization?: boolean'), 'startImport lacks batch finalization deferral');
  assert.ok(model.includes('deferAutomaticFinalization?: boolean'), 'durable job model does not preserve finalization ownership');
  assert.ok(hook.includes('getRemoteImportJob'), 'batch cannot read authoritative durable job state');
  assert.ok(hook.includes('finalizeRemoteImportJob'), 'batch cannot explicitly finalize a deferred job');
});


test('multi-link Remote Import keeps one modal, no destination list, explicit selections, and manual version grouping', () => {
  const modal = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/RemoteUploadModal.tsx'), 'utf8');
  const dashboard = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.ok(modal.includes('<textarea'), 'multi-link input is not multiline');
  assert.ok(modal.includes('multiLinkMode'), 'modal does not derive multi-link mode from input count');
  assert.ok(modal.includes('Group as versions'), 'manual cross-link grouping action is missing');
  assert.ok(modal.includes('Primary'), 'manual Version Stack primary choice is missing');
  assert.ok(modal.includes('batch.entries'), 'multi-link cards are not driven by ordered batch entries');
  assert.ok(modal.includes('entry.selectedIds'), 'per-source independent selections are missing');
  assert.ok(modal.includes('selectionReady'), 'batch Import is not gated on explicit per-link selections');
  assert.ok(modal.includes('current entry') || modal.includes('activeEntryIndex'), 'batch progress does not expose current entry');
  assert.ok(!modal.includes('<select'), 'Remote Import reintroduced a selector/dropdown');
  assert.ok(dashboard.includes('useRemoteImportBatch'), 'desktop dashboard does not wire the batch orchestrator');
  assert.ok(dashboard.includes('remoteImportBatch'), 'batch controller is not passed to the existing modal');
});




test('batch analysis reserves and persists a job id before extractor work and pins the immutable destination', () => {
  const singleHook = fs.readFileSync(hookPath, 'utf8');
  const batchHook = fs.readFileSync(path.join(root, 'app/src/hooks/useRemoteImportBatch.ts'), 'utf8');
  assert.ok(singleHook.includes('prepareSource'), 'single-job hook cannot reserve a durable analysis job');
  assert.ok(batchHook.includes('rememberJobId(prepared.id)'), 'batch does not persist the reserved job id before extraction');
  assert.ok(batchHook.includes('folderId: initial.folderId') || batchHook.includes('folderId: batch.folderId'), 'batch analysis is not pinned to its captured destination');
});

test('paused multi-link analysis can resume only unfinished sources before Import is allowed', () => {
  const modal = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/RemoteUploadModal.tsx'), 'utf8');
  const hook = fs.readFileSync(path.join(root, 'app/src/hooks/useRemoteImportBatch.ts'), 'utf8');
  assert.ok(hook.includes('resumeBatchAnalysis'), 'batch hook has no analysis-resume API');
  assert.ok(modal.includes('Resume analysis'), 'modal has no Resume analysis action');
  assert.ok(modal.includes('pendingAnalysis'), 'modal does not distinguish unfinished analysis from import-ready state');
});

test('retrying an unanalyzed batch source reserves a job pinned to the batch destination', () => {
  const hook = fs.readFileSync(path.join(root, 'app/src/hooks/useRemoteImportBatch.ts'), 'utf8');
  const start = hook.indexOf('const retryEntryAnalysis');
  const end = hook.indexOf('const groupMembers', start);
  assert.ok(start >= 0 && end > start, 'retryEntryAnalysis implementation is missing');
  const retryBody = hook.slice(start, end);
  assert.ok(retryBody.includes('remoteImport.prepareSource(entry.source, { folderId: batch.folderId })'), 'retry analysis can bind a new job to the current channel instead of the immutable batch destination');
  assert.ok(!retryBody.includes('remoteImport.analyzeSource(entry.source'), 'retry analysis still creates an unpinned job through analyzeSource');
});

test('cross-link Version Stack finalizer can recover a remotely-created stack after a crash before local stackId persistence', () => {
  const hook = fs.readFileSync(path.join(root, 'app/src/hooks/useRemoteImportBatch.ts'), 'utf8');
  assert.ok(hook.includes("cmd_get_files"), 'crash recovery does not inspect existing stack-backed files');
  assert.ok(hook.includes("cmd_get_file_stack"), 'crash recovery does not verify existing stack members');
  assert.ok(hook.includes('recoverExistingCrossSourceStackId'), 'cross-source stack recovery helper is missing');
  const recoverPos = hook.indexOf('recoverExistingCrossSourceStackId(');
  const createPos = hook.indexOf("cmd_create_file_stack", recoverPos);
  assert.ok(recoverPos >= 0 && createPos > recoverPos, 'existing stack recovery must run before creating another stack');
});

if (failures) process.exit(1);
console.log('Remote Import UI/model checks passed');
