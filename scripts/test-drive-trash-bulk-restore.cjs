'use strict';
// Synthetic execution of the actual SettingsModal batch restore handler.
// No Telegram account, native mount, or personal files are accessed.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require(require.resolve('typescript', {
  paths: [path.resolve(__dirname, '../app')],
}));
const file = path.resolve(__dirname, '../app/src/components/desktop/dashboard/SettingsModal.tsx');
const source = fs.readFileSync(file, 'utf8');
const syncHelperExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(path.resolve(__dirname, '../app/src/deviceContinuity.ts'), 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: syncHelperExports });
const ast = ts.createSourceFile(file, source, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const settings = ast.statements.find(node => ts.isFunctionDeclaration(node)
  && node.name?.text === 'SettingsModal');
assert.ok(settings, 'real SettingsModal must exist');
function handler(name) {
  const declaration = settings.body.statements.filter(ts.isVariableStatement)
    .flatMap(node => [...node.declarationList.declarations])
    .find(node => ts.isIdentifier(node.name) && node.name.text === name);
  assert.ok(declaration, 'missing real handler ' + name);
  return 'const ' + declaration.getText(ast) + ';';
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}
const flush = async () => {
  for (let i = 0; i < 4; i++) await new Promise(done => setImmediate(done));
};
const item = id => ({
  id, name: id + '.txt', kind: 'file', restorable: true, deleted_at: 100,
  original_directory_id: null, size: 2,
});
function harness(ids) {
  const entries = ids.map(item);
  const state = {
    items: [...entries], selected: entries.map(row => row.kind + ':' + row.id),
    progress: null, error: null, restoring: null, syncProblem: null,
  };
  const refs = {
    driveTrashRestoreInFlightRef: { current: false },
    driveTrashBatchStopRequestedRef: { current: false },
    driveRestoreSyncInFlightRef: { current: false },
    driveTrashSessionRef: { current: 1 },
    driveSettingsOpenRef: { current: true },
    driveTrashRequestRef: { current: 0 },
  };
  const pending = [];
  const calls = [];
  const toasts = [];
  let refreshes = 0;
  const ctx = {
    ...refs,
    selectedRestorableDriveTrash: entries,
    driveTrashBusy: false,
    driveTrashError: null,
    validateDriveSyncResult: syncHelperExports.validateDriveSyncResult,
    invoke: (command, args) => {
      calls.push({ command, args });
      const request = deferred();
      pending.push({ command, args, ...request });
      return request.promise;
    },
    fetchDriveTrash: () => { refreshes++; return Promise.resolve(); },
    setRestoringDriveItem: value => { state.restoring = value; },
    setDriveTrashBatchProgress: value => { state.progress = value; },
    setDriveTrashBatchError: value => { state.error = value; },
    setDriveTrashBatchStopRequested: value => { state.stopRequested = value; },
    setDriveTrash: change => {
      state.items = typeof change === 'function' ? change(state.items) : change;
    },
    setSelectedDriveTrashKeys: change => {
      state.selected = typeof change === 'function' ? change(state.selected) : change;
    },
    setDriveRestoreSyncProblem: value => { state.syncProblem = value; },
    toast: {
      success: text => toasts.push(['success', text]),
      warning: text => toasts.push(['warning', text]),
      error: text => toasts.push(['error', text]),
    },
  };
  const script = ts.transpileModule(
    handler('classifyDriveRestoreSync') + '\n'
    + handler('restoreSelectedDriveTrash') + '\n'
    + 'globalThis.runBulk = restoreSelectedDriveTrash;',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText;
  vm.runInNewContext(script, ctx);
  return {
    run: ctx.runBulk, state, refs, pending, calls, toasts,
    stop() { refs.driveTrashBatchStopRequestedRef.current = true; state.stopRequested = true; },
    rerender() { ctx.selectedRestorableDriveTrash = state.items.filter(entry =>
      state.selected.includes(entry.kind + ':' + entry.id)); },
    get refreshes() { return refreshes; },
    close() {
      refs.driveTrashSessionRef.current++;
      refs.driveSettingsOpenRef.current = false;
    },
    reopen() { refs.driveSettingsOpenRef.current = true; },
  };
}
(async () => {
  const success = harness(['one', 'two']);
  const job = success.run();
  await success.run();
  assert.equal(success.calls.length, 1, 'double click cannot overlap batch');
  assert.equal(success.pending[0].command, 'cmd_drive_restore_trash_item');
  assert.equal(success.pending[0].args.id, 'one');
  assert.equal(success.state.restoring, 'bulk');
  assert.equal(success.state.progress.total, 2);
  assert.equal(success.pending.filter(p => p.command === 'cmd_drive_sync_metadata').length, 0);
  success.pending[0].resolve('one.txt');
  await flush();
  assert.equal(success.pending[1].args.id, 'two', 'second restore begins only after first finishes');
  assert.deepEqual(success.state.selected, ['file:two'], 'completed identity removed from selection');
  assert.equal(success.state.progress.completed, 1);
  success.pending[1].resolve('two.txt');
  await flush();
  assert.equal(success.pending[2].command, 'cmd_drive_sync_metadata', 'one sync after all restores');
  assert.equal(success.calls.filter(c => c.command === 'cmd_drive_sync_metadata').length, 1);
  success.pending[2].resolve({ revision: 30, dirty: false, source: 'merged-published' });
  await job;
  assert.equal(success.state.restoring, null);
  assert.equal(success.refs.driveTrashRestoreInFlightRef.current, false);
  assert.deepEqual(success.state.selected, []);
  assert.deepEqual(success.state.items, []);
  assert.equal(success.state.progress.completed, 2);
  assert.equal(success.refreshes, 1, 'refresh only after metadata sync');
  assert.equal(success.toasts.at(-1)[0], 'success');

  const stopped = harness(['first', 'second', 'third']);
  const stopJob = stopped.run();
  assert.equal(stopped.pending[0].command, 'cmd_drive_restore_trash_item');
  stopped.stop();
  assert.equal(stopped.state.stopRequested, true);
  assert.equal(stopped.calls.length, 1, 'stop cannot cancel the existing native restore');
  stopped.pending[0].resolve('first.txt');
  await flush();
  assert.equal(stopped.pending[1].command, 'cmd_drive_sync_metadata',
    'stop must sync already completed local restore before returning');
  assert.equal(stopped.calls.filter(c => c.command === 'cmd_drive_restore_trash_item').length, 1,
    'stop before next item prevents new restore calls');
  stopped.pending[1].resolve({ revision: 9, dirty: false, source: 'current' });
  await stopJob;
  assert.deepEqual(stopped.state.selected, ['file:second', 'file:third']);
  assert.equal(stopped.state.progress.completed, 1);
  assert.equal(stopped.toasts.at(-1)[0], 'warning');
  assert.match(stopped.toasts.at(-1)[1], /Stopped as requested/);
  assert.equal(stopped.refs.driveTrashRestoreInFlightRef.current, false);
  stopped.rerender();
  const retryStopped = stopped.run();
  assert.equal(stopped.pending[2].args.id, 'second', 'retry begins at first remaining selection');
  assert.equal(stopped.state.stopRequested, false, 'retry resets cancellation state');
  stopped.pending[2].resolve('second.txt');
  await flush();
  assert.equal(stopped.pending[3].args.id, 'third');
  stopped.pending[3].resolve('third.txt');
  await flush();
  assert.equal(stopped.pending[4].command, 'cmd_drive_sync_metadata');
  stopped.pending[4].resolve({ revision: 10, dirty: false, source: 'merged-published' });
  await retryStopped;
  assert.equal(stopped.toasts.at(-1)[0], 'success');
  assert.deepEqual(stopped.state.selected, []);

  const partial = harness(['first', 'failed', 'unattempted']);
  const partialJob = partial.run();
  partial.pending[0].resolve('first.txt');
  await flush();
  assert.equal(partial.pending[1].args.id, 'failed');
  partial.pending[1].reject(new Error('synthetic permission error'));
  await flush();
  assert.equal(partial.pending[2].command, 'cmd_drive_sync_metadata',
    'completed subset must still sync even when next restore fails');
  assert.equal(partial.calls.filter(c => c.command === 'cmd_drive_restore_trash_item').length, 2,
    'stop after first failure instead of trying every item');
  partial.pending[2].resolve({ revision: 32, dirty: true, source: 'current' });
  await partialJob;
  assert.deepEqual(partial.state.selected, ['file:failed', 'file:unattempted']);
  assert.equal(partial.state.progress.completed, 1);
  assert.match(partial.state.error, /Could not restore failed.txt/);
  assert.equal(partial.state.syncProblem, 'pending');
  assert.equal(partial.toasts.at(-1)[0], 'warning');
  assert.equal(partial.refreshes, 1);

  const malformedSync = harness(['restored-but-unverified']);
  const malformedJob = malformedSync.run();
  malformedSync.pending[0].resolve('restored-but-unverified.txt');
  await flush();
  assert.equal(malformedSync.pending[1].command, 'cmd_drive_sync_metadata');
  malformedSync.pending[1].resolve({ revision: 'not-a-revision', dirty: false, source: 'merged-published' });
  await malformedJob;
  assert.equal(malformedSync.state.syncProblem, 'unverified',
    'malformed native sync revision must never claim cloud publication');
  assert.equal(malformedSync.toasts.at(-1)[0], 'warning');
  assert.equal(malformedSync.state.progress.completed, 1,
    'local restore success remains accurate even when sync is unverified');

  const failed = harness(['bad']);
  const failedJob = failed.run();
  failed.pending[0].reject(new Error('synthetic file unavailable'));
  await failedJob;
  assert.equal(failed.calls.length, 1, 'nothing synchronized if no restore succeeded');
  assert.equal(failed.refs.driveTrashRestoreInFlightRef.current, false);
  assert.equal(failed.toasts.at(-1)[0], 'error');
  assert.deepEqual(failed.state.selected, ['file:bad']);

  const closed = harness(['slow', 'must-not-start']);
  const closedJob = closed.run();
  closed.close();
  closed.pending[0].resolve('slow.txt');
  await flush();
  assert.equal(closed.calls.filter(c => c.command === 'cmd_drive_restore_trash_item').length, 1,
    'closing settings prevents the next destructive restore');
  assert.equal(closed.pending[1].command, 'cmd_drive_sync_metadata',
    'native restore already completed, so metadata must still sync');
  closed.reopen();
  closed.pending[1].resolve({ revision: 33, dirty: false, source: 'current' });
  await closedJob;
  assert.equal(closed.refreshes, 0, 'old session cannot refresh a reopened Settings view');
  assert.equal(closed.toasts.length, 0, 'old session cannot publish stale toasts');
  assert.equal(closed.refs.driveTrashRestoreInFlightRef.current, false);

  // Exercise the actual select-shown handler: merge without duplicates,
  // preserve prior choices and never select more than the safe batch cap.
  const selections = { selected: ['file:already'] };
  const selectCtx = {
    shownRestorableDriveTrash: Array.from({ length: 120 }, (_, i) => item('pick-' + i)),
    setSelectedDriveTrashKeys: change => {
      selections.selected = typeof change === 'function' ? change(selections.selected) : change;
    },
  };
  vm.runInNewContext(ts.transpileModule(
    handler('selectShownDriveTrash') + '\n' + 'globalThis.selectShown = selectShownDriveTrash;',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText, selectCtx);
  selectCtx.selectShown();
  selectCtx.selectShown();
  assert.equal(selections.selected.length, 100);
  assert.equal(new Set(selections.selected).size, 100);
  assert.equal(selections.selected[0], 'file:already');

  const cap = harness(Array.from({ length: 103 }, (_, i) => 'batch-' + i));
  const capJob = cap.run();
  assert.equal(cap.state.progress.total, 100, 'batch cannot silently restore more than 100');
  cap.close();
  cap.pending[0].resolve('batch-0.txt');
  await flush();
  assert.equal(cap.pending[1].command, 'cmd_drive_sync_metadata');
  cap.pending[1].resolve({ revision: 1, dirty: false, source: 'current' });
  await capJob;
  assert.equal(cap.calls.filter(c => c.command === 'cmd_drive_restore_trash_item').length, 1);

  const busy = harness(['one']);
  busy.refs.driveRestoreSyncInFlightRef.current = true;
  await busy.run();
  assert.equal(busy.calls.length, 0, 'cannot restore during an existing metadata sync');

  assert.match(source, /aria-label="Restore selected deleted Drive items"/);
  assert.match(source, /Select shown restorable Drive items/);
  assert.match(source, /Bulk restore progress:/);
  assert.match(source, /checked=\{selectedDriveTrashKeys.includes/);
  console.log('PASS Drive bulk restore: selection, ordered restores, one sync, partial failure and close safety');
})().catch(error => { console.error(error); process.exitCode = 1; });
