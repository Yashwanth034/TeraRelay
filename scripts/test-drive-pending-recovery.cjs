'use strict';
// Synthetic native pending Drive write recovery test. This invokes the actual
// SettingsModal handler without touching Telegram, files, or login credentials.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const filename = path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx');
const source = fs.readFileSync(filename, 'utf8');
const ast = ts.createSourceFile(filename, source, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const component = ast.statements.find(node => ts.isFunctionDeclaration(node)
  && node.name?.text === 'SettingsModal');
assert.ok(component, 'SettingsModal component exists');
const declarations = component.body.statements.filter(ts.isVariableStatement)
  .flatMap(node => Array.from(node.declarationList.declarations));
const recover = declarations.find(node => ts.isIdentifier(node.name)
  && node.name.text === 'recoverPendingDriveWrites');
assert.ok(recover?.initializer, 'manual pending write recovery handler exists');
const lifecycleEffect = component.body.statements.find(node =>
  ts.isExpressionStatement(node) && ts.isCallExpression(node.expression)
  && node.expression.expression.getText(ast) === 'useEffect'
  && node.expression.arguments[0]?.getText(ast).includes('drivePendingRecoverySessionRef.current += 1'));
assert.ok(lifecycleEffect, 'Settings close invalidates in-flight recovery UI session');
const code = ts.transpileModule([
  'globalThis.recoverPendingDriveWrites = ' + recover.initializer.getText(ast) + ';',
  'globalThis.lifecycleEffect = ' + lifecycleEffect.expression.arguments[0].getText(ast) + ';',
].join('\n'), {
  compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 },
}).outputText;
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { resolve, reject, promise };
};

function harness() {
  const state = { busy: false, result: null, error: null };
  const refs = {
    drivePendingRecoveryInFlightRef: { current: false },
    drivePendingRecoverySessionRef: { current: 0 },
    driveSettingsOpenRef: { current: true },
    driveRestoreSyncInFlightRef: { current: false },
    driveTrashRequestRef: { current: 0 },
    driveFinderRequestRef: { current: 0 },
    driveFinderFetchInFlightRef: { current: false },
    driveFinderCopyInFlightRef: { current: false },
    driveFinderCsvCopyInFlightRef: { current: false },
    driveFinderCsvCopyGenerationRef: { current: 0 },
    driveTrashSessionRef: { current: 0 },
    driveTrashBatchStopRequestedRef: { current: false },
  };
  const calls = [], pending = [], toasts = [];
  const context = {
    ...refs,
    isOpen: true,
    setDrivePendingRecoveryBusy: value => { state.busy = value; },
    setDrivePendingRecoveryResult: value => { state.result = value; },
    setDrivePendingRecoveryError: value => { state.error = value; },
    setDriveFinderOpen: () => {},
    setDriveFinderEntries: () => {},
    setDriveFinderSearch: () => {},
    setDriveFinderKind: () => {},
    setDriveFinderSort: () => {},
    setDriveFinderSizeFilter: () => {},
    setDriveFinderFolderScope: () => {},
    setDriveFinderVisibleLimit: () => {},
    setDriveFinderBusy: () => {},
    setDriveFinderCopyBusy: () => {},
    setDriveFinderCsvCopyBusy: () => {},
    setDriveFinderError: () => {},
    setDriveFinderSkipped: () => {},
    setDriveFinderTruncated: () => {},
    setDriveTrashVisibleLimit: () => {},
    setSelectedDriveTrashKeys: () => {},
    setDriveTrashBatchProgress: () => {},
    setDriveTrashBatchError: () => {},
    setDriveTrashBatchStopRequested: () => {},
    setDriveTrashInventoryCopyBusy: () => {},
    setDriveTrashBusy: () => {},
    setDriveRestoreSyncBusy: () => {},
    toast: {
      success: msg => toasts.push(['success', msg]),
      warning: msg => toasts.push(['warning', msg]),
      error: msg => toasts.push(['error', msg]),
    },
    invoke(command) {
      calls.push(command);
      const d = deferred();
      pending.push(d);
      return d.promise;
    },
  };
  vm.runInNewContext(code, context, { filename });
  const settle = async () => { for (let i = 0; i < 4; ++i) await new Promise(ok => setImmediate(ok)); };
  return {
    state, refs, calls, pending, toasts, settle,
    recover: () => context.recoverPendingDriveWrites(),
    close() {
      context.isOpen = false;
      refs.driveSettingsOpenRef.current = false;
      context.lifecycleEffect();
    },
    reopen() {
      context.isOpen = true;
      refs.driveSettingsOpenRef.current = true;
      context.lifecycleEffect();
    },
  };
}

(async () => {
  assert.match(source, /aria-label="Retry pending TeraRelay Drive writes"/);
  assert.match(source, /onClick=\{\(\) => \{ void recoverPendingDriveWrites\(\); \}\}/);
  assert.match(source, /drivePendingRecoveryResult && \([\s\S]*?role="status"/);
  assert.match(source, /drivePendingRecoveryError && \([\s\S]*?role="alert"/);
  assert.match(source, /disabled=\{drivePendingRecoveryBusy\}/);
  assert.doesNotMatch(recover.initializer.getText(ast), /cmd_drive_restore_trash_item/,
    'recovery must not restore already-deleted items');

  const success = harness();
  const completed = success.recover();
  assert.deepEqual(success.calls, ['cmd_drive_recover_pending'],
    'user action issues exactly one native recovery');
  assert.equal(success.state.busy, true);
  const duplicate = success.recover();
  await duplicate;
  assert.deepEqual(success.calls, ['cmd_drive_recover_pending'],
    'rapid repeated Retry clicks cannot start overlapping recovery');
  success.pending[0].resolve({ recovered: 2, remaining: 0, failures: 0 });
  await completed;
  assert.equal(success.state.result.recovered, 2);
  assert.equal(success.toasts.at(-1)[0], 'success');
  assert.equal(success.state.busy, false);
  assert.equal(success.refs.drivePendingRecoveryInFlightRef.current, false);
  assert.deepEqual(success.calls, ['cmd_drive_recover_pending'],
    'UI must never issue an unrequested cloud metadata sync');

  const noWork = harness();
  const empty = noWork.recover();
  noWork.pending[0].resolve({ recovered: 0, remaining: 0, failures: 0 });
  await empty;
  assert.equal(noWork.toasts.at(-1)[0], 'success');
  assert.match(noWork.toasts.at(-1)[1], /No closed pending Drive writes/);

  const partial = harness();
  const pendingPartial = partial.recover();
  partial.pending[0].resolve({ recovered: 3, remaining: 2, failures: 1 });
  await pendingPartial;
  assert.equal(partial.toasts.at(-1)[0], 'warning');
  assert.match(partial.toasts.at(-1)[1], /2 still pending, 1 failed/);
  assert.equal(partial.state.result.remaining, 2);
  assert.equal(partial.state.result.failures, 1);
  assert.equal(partial.state.error, null);

  for (const invalid of [
    null, undefined, [], { recovered: -1, remaining: 0, failures: 0 },
    { recovered: 1.5, remaining: 0, failures: 0 },
    { recovered: 2, remaining: '0', failures: 0 },
    { recovered: 1, remaining: 0, failures: Number.NaN },
    { recovered: 4294967296, remaining: 0, failures: 0 },
    { recovered: 1, remaining: 0 },
  ]) {
    const bad = harness();
    const attempt = bad.recover();
    bad.pending[0].resolve(invalid);
    await attempt;
    assert.equal(bad.toasts.at(-1)[0], 'error',
      'malformed native result may not show successful write recovery');
    assert.match(bad.state.error, /invalid/i);
    assert.equal(bad.state.result, null);
    assert.equal(bad.state.busy, false);
    assert.equal(bad.refs.drivePendingRecoveryInFlightRef.current, false);
  }

  const offline = harness();
  const failed = offline.recover();
  offline.pending[0].reject(new Error('synthetic Telegram unavailable'));
  await failed;
  assert.equal(offline.toasts.at(-1)[0], 'error');
  assert.match(offline.state.error, /Telegram unavailable/);
  const retry = offline.recover();
  assert.equal(offline.calls.length, 2, 'failed recovery is retryable');
  offline.pending[1].resolve({ recovered: 1, remaining: 0, failures: 0 });
  await retry;
  assert.equal(offline.state.error, null, 'successful retry clears previous error');
  assert.equal(offline.toasts.at(-1)[0], 'success');

  // Native command continues even when Settings closes; no stale notification
  // or success state may leak into a fresh modal session.
  const reopened = harness();
  const first = reopened.recover();
  reopened.close();
  assert.equal(reopened.state.busy, false, 'closing clears presentation-only busy state');
  reopened.reopen();
  assert.equal(reopened.state.busy, true, 'reopened dialog shows outstanding native request');
  await reopened.recover();
  assert.equal(reopened.calls.length, 1, 'reopen cannot issue duplicate native recovery');
  reopened.pending[0].resolve({ recovered: 3, remaining: 0, failures: 0 });
  await first;
  assert.deepEqual(reopened.toasts, [], 'old session result must not toast');
  assert.equal(reopened.state.result, null, 'old result must not update reopened session');
  assert.equal(reopened.state.busy, false);
  const next = reopened.recover();
  assert.equal(reopened.calls.length, 2, 'fresh retry works after old native command settles');
  reopened.pending[1].resolve({ recovered: 0, remaining: 0, failures: 0 });
  await next;
  assert.equal(reopened.toasts.at(-1)[0], 'success');
  console.log('PASS Drive pending writes: manual recovery, truthful counts, single-flight, invalid IPC and session cancellation');
})().catch(e => { console.error(e); process.exitCode = 1; });
