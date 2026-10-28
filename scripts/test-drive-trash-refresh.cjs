'use strict';
// Isolated native-command regression for the real SettingsModal Drive-trash
// handlers. Extracts TypeScript AST nodes; does not access accounts or files.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require(require.resolve('typescript', {
  paths: [path.resolve(__dirname, '../app')],
}));
const file = path.resolve(__dirname, '../app/src/components/desktop/dashboard/SettingsModal.tsx');
const source = fs.readFileSync(file, 'utf8');
const helperPath = path.resolve(__dirname, '../app/src/driveTrashBrowse.ts');
const helperExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(helperPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: helperExports });
const syncHelperExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(path.resolve(__dirname, '../app/src/deviceContinuity.ts'), 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: syncHelperExports });
const ast = ts.createSourceFile(file, source, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const settings = ast.statements.find(node => ts.isFunctionDeclaration(node)
  && node.name?.text === 'SettingsModal');
assert.ok(settings, 'SettingsModal component must exist');
const liveTrashEffect = settings.body.statements.find(node => ts.isExpressionStatement(node)
  && ts.isCallExpression(node.expression)
  && node.expression.expression.getText(ast) === 'useEffect'
  && node.expression.arguments[1]?.getText(ast).includes('driveTrashOpen')
  && node.expression.arguments[0]?.getText(ast).includes('fetchDriveTrash()'));
assert.ok(liveTrashEffect, 'Recently Deleted must refresh on panel open and Settings reopen');
assert.doesNotMatch(source, /if \(!driveTrashOpen\) void fetchDriveTrash\(\);/,
  'panel toggle must not duplicate the effect-driven list refresh');
const getHandler = name => {
  const declaration = settings.body.statements
    .filter(ts.isVariableStatement)
    .flatMap(node => Array.from(node.declarationList.declarations))
    .find(node => ts.isIdentifier(node.name) && node.name.text === name);
  assert.ok(declaration, 'Missing real SettingsModal handler: ' + name);
  return 'const ' + declaration.getText(ast) + ';';
};

function deferred() {
  let resolve, reject;
  const promise = new Promise((good, bad) => { resolve = good; reject = bad; });
  return { promise, resolve, reject };
}
function environment() {
  let open = true;
  const refs = {
    driveTrashRequestRef: { current: 0 },
    driveTrashSessionRef: { current: 0 },
    driveTrashRestoreInFlightRef: { current: false },
    driveRestoreSyncInFlightRef: { current: false },
    driveSettingsOpenRef: { current: true },
  };
  const state = { busy: false, items: [], restoring: null, error: null, warning: null, syncProblem: null, syncBusy: false };
  // Earlier synthetic tests intentionally omit optional row fields. Keep
  // their original fixture behavior, and explicitly enable the real native
  // payload validation for the new end-to-end normalization cases below.
  let normalizeEntries = entries => ({ entries, skipped: 0 });
  const pending = [];
  const calls = [];
  const messages = [];
  const invoke = (command, args) => {
    calls.push({ command, args });
    const item = deferred();
    pending.push({ ...item, command });
    return item.promise;
  };
  const context = {
    ...refs,
    useCallback: fn => fn,
    driveRestoreSyncProblem: null,
    invoke,
    normalizeDriveTrashEntries: value => normalizeEntries(value),
    validateDriveSyncResult: syncHelperExports.validateDriveSyncResult,
    setDriveTrashWarning: value => { state.warning = value; },
    setDriveTrashBusy: value => { state.busy = value; },
    setDriveTrash: value => { state.items = typeof value === 'function' ? value(state.items) : value; },
    setSelectedDriveTrashKeys: value => {
      state.selected = typeof value === 'function' ? value(state.selected || []) : value;
    },
    setDriveTrashError: value => { state.error = value; },
    setRestoringDriveItem: value => { state.restoring = value; },
    setDriveRestoreSyncProblem: value => {
      state.syncProblem = value;
      context.driveRestoreSyncProblem = value; // model next React render for handler closure
    },
    setDriveRestoreSyncBusy: value => { state.syncBusy = value; },
    toast: {
      error: text => messages.push(['error', text]),
      success: text => messages.push(['success', text]),
      warning: text => messages.push(['warning', text]),
    },
  };
  const script = ts.transpileModule(
    getHandler('fetchDriveTrash') + '\n'
      + getHandler('classifyDriveRestoreSync') + '\n'
      + getHandler('retryDriveRestoreMetadataSync') + '\n'
      + getHandler('restoreDriveTrashItem') + '\n'
      + 'globalThis.resultHandlers = { fetchDriveTrash, retryDriveRestoreMetadataSync, restoreDriveTrashItem };',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText;
  vm.runInNewContext(script, context);
  return {
    ...context.resultHandlers,
    state, pending, calls, messages, refs,
    useRealNormalizer() { normalizeEntries = helperExports.normalizeDriveTrashEntries; },
    close() {
      open = false;
      refs.driveSettingsOpenRef.current = false;
      refs.driveTrashRequestRef.current++;
      refs.driveTrashSessionRef.current++;
      state.busy = false;
      state.syncBusy = false;
    },
    reopen() { open = true; refs.driveSettingsOpenRef.current = true; },
  };
}

(async () => {
  const s = environment();
  const a = s.fetchDriveTrash();
  const b = s.fetchDriveTrash();
  assert.equal(s.pending.length, 2);
  s.pending[1].resolve([{ id: 'newer' }]);
  await b;
  assert.equal(s.state.items[0].id, 'newer');
  s.pending[0].resolve([{ id: 'older' }]);
  await a;
  assert.equal(s.state.items[0].id, 'newer', 'late old response must not replace new list');

  const list = s.fetchDriveTrash();
  assert.equal(s.state.busy, true);
  s.close();
  s.pending[2].reject(new Error('synthetic stale close error'));
  await list;
  assert.equal(s.state.busy, false);
  assert.equal(s.messages.length, 0, 'closed settings cannot show stale list errors');

  s.reopen();
  const current = s.fetchDriveTrash();
  s.pending[3].reject(new Error('synthetic active list error'));
  await current;
  assert.equal(s.state.busy, false);
  assert.equal(s.messages.length, 0, 'list errors use inline status instead of toasts');
  assert.match(s.state.error, /Could not refresh recently deleted/,
    'current list failure must be surfaced inline');

  const item = { kind: 'file', id: 'fixture', name: 'fixture.txt' };
  const restoring = s.restoreDriveTrashItem(item);
  const duplicate = s.restoreDriveTrashItem(item);
  await duplicate;
  assert.equal(s.calls.filter(c => c.command === 'cmd_drive_restore_trash_item').length, 1,
    'double-click cannot request restore twice');
  assert.equal(s.state.restoring, 'file:fixture', 'busy row uses type-qualified native identity');
  s.pending[4].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(s.pending[5].command, 'cmd_drive_sync_metadata',
    'essential Drive sync must start without waiting for optional trash refresh');
  assert.equal(s.pending.length, 6, 'trash refresh does not block metadata sync');
  s.pending[5].resolve({ revision: 42, dirty: false, source: 'merged-published' });
  await restoring;
  assert.equal(s.pending[6].command, 'cmd_drive_list_trash',
    'list refresh runs after sync and cannot delay restore completion');
  assert.equal(s.state.restoring, null);
  assert.equal(s.refs.driveTrashRestoreInFlightRef.current, false);
  assert.equal(s.messages.at(-1)[0], 'success');
  assert.match(s.messages.at(-1)[1], /metadata is synchronized/,
    'successful native publication is described accurately');
  s.pending[6].resolve([]);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(s.state.error, null, 'successful refresh clears previous inline error');

  const failed = s.restoreDriveTrashItem(item);
  assert.equal(s.pending[7].command, 'cmd_drive_restore_trash_item');
  s.pending[7].reject(new Error('synthetic restore failure'));
  await failed;
  assert.equal(s.refs.driveTrashRestoreInFlightRef.current, false);
  assert.equal(s.state.restoring, null);
  assert.equal(s.messages.at(-1)[0], 'error');
  const retry = s.restoreDriveTrashItem(item);
  assert.equal(s.pending[8].command, 'cmd_drive_restore_trash_item',
    'restore is retryable after failure');
  s.pending[8].reject(new Error('synthetic retry'));
  await retry;

  // A closed dialog cannot delay essential sync or surface stale toast UI.
  const noticeCount = s.messages.length;
  const closedSuccess = s.restoreDriveTrashItem(item);
  assert.equal(s.pending[9].command, 'cmd_drive_restore_trash_item');
  s.close();
  s.pending[9].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(s.pending[10].command, 'cmd_drive_sync_metadata', 'sync continues after close');
  s.pending[10].resolve(undefined);
  await closedSuccess;
  assert.equal(s.pending.length, 11, 'closed dialog does not refetch trash');
  assert.equal(s.messages.length, noticeCount, 'closed dialog cannot show stale restore success');

  s.reopen();
  const closedError = s.restoreDriveTrashItem(item);
  s.close();
  s.pending[11].reject(new Error('synthetic late restore error'));
  await closedError;
  assert.equal(s.messages.length, noticeCount, 'closed dialog cannot show stale restore failure');

  // Reopening while an old restore finishes refreshes the current view but
  // never shows a notification belonging to the earlier dialog session.
  s.reopen();
  const reopenedOld = s.restoreDriveTrashItem(item);
  s.close();
  s.reopen();
  s.pending[12].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(s.pending[13].command, 'cmd_drive_sync_metadata');
  s.pending[13].resolve(undefined);
  await reopenedOld;
  assert.equal(s.pending[14].command, 'cmd_drive_list_trash');
  assert.equal(s.messages.length, noticeCount, 'older session cannot toast in reopened dialog');
  s.pending[14].resolve([]);
  await new Promise(resolve => setImmediate(resolve));

  const syncFailed = s.restoreDriveTrashItem(item);
  s.pending[15].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(s.pending[16].command, 'cmd_drive_sync_metadata');
  s.pending[16].reject(new Error('synthetic offline sync'));
  await syncFailed;
  assert.equal(s.messages.at(-1)[0], 'warning', 'offline sync reports local restore');
  s.pending[17].resolve([]);
  await new Promise(resolve => setImmediate(resolve));

  // Native restoration and cloud metadata publication are different events.
  // A returned QA-local, pending, or malformed sync result must not claim
  // that Telegram already received the recovered layout.
  for (const [result, expectedTone, expectedText] of [
    [{ source: 'current', revision: 10, dirty: false }, 'success', /synchronized/],
    [{ source: 'remote', revision: 11, dirty: false }, 'success', /synchronized/],
    [{ source: 'remote-merged', revision: 12, dirty: false }, 'success', /synchronized/],
    [{ source: 'merged-published', revision: 13, dirty: false }, 'success', /synchronized/],
    [{ source: 'qa-local', revision: 14, dirty: false }, 'warning', /QA mode/],
    [{ source: 'merged-published', revision: 15, dirty: true }, 'warning', /pending changes/],
    [{ source: 'qa-local', revision: 16, dirty: true }, 'warning', /QA mode/],
    [{ source: 'unrecognized', revision: 17, dirty: false }, 'warning', /not be verified/],
    [{ source: 'remote', revision: 18, dirty: 'false' }, 'warning', /not be verified/],
    [undefined, 'warning', /not be verified/],
  ]) {
    const caseFixture = environment();
    const restore = caseFixture.restoreDriveTrashItem(item);
    assert.equal(caseFixture.pending[0].command, 'cmd_drive_restore_trash_item');
    caseFixture.pending[0].resolve('fixture.txt');
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(caseFixture.pending[1].command, 'cmd_drive_sync_metadata');
    caseFixture.pending[1].resolve(result);
    await restore;
    assert.equal(caseFixture.messages.length, 1, 'one truthful message per successful restore');
    assert.equal(caseFixture.messages[0][0], expectedTone, 'cloud sync outcome tone');
    assert.match(caseFixture.messages[0][1], expectedText, 'cloud sync outcome description');
    assert.equal(caseFixture.pending[2].command, 'cmd_drive_list_trash');
    assert.equal(caseFixture.state.restoring, null, 'native restore is complete even if listing is pending');
    caseFixture.pending[2].resolve([]);
    await new Promise(resolve => setImmediate(resolve));
  }
  assert.match(s.messages.at(-1)[1], /retry when Telegram is connected/,
    'native sync rejection reports offline recovery, not remote publication');

  // A successful native restore must disappear immediately, before the
  // optional remote metadata sync or list refresh. Even an older pending
  // refresh returning deleted entries cannot resurrect the restored row.
  const race = environment();
  const target = { kind: 'file', id: 'shared-id', name: 'old.txt' };
  const unrelatedKind = { kind: 'folder', id: 'shared-id', name: 'keep-folder' };
  const unrelatedFile = { kind: 'file', id: 'keep-id', name: 'keep.txt' };
  const initial = race.fetchDriveTrash();
  race.pending[0].resolve([target, unrelatedKind, unrelatedFile]);
  await initial;
  assert.equal(race.state.items.length, 3);
  const olderList = race.fetchDriveTrash();
  const restored = race.restoreDriveTrashItem(target);
  assert.equal(race.pending[2].command, 'cmd_drive_restore_trash_item');
  race.pending[2].resolve('old.txt');
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(race.state.items.map(item => item.name),
    ['keep-folder', 'keep.txt'], 'native successful restore immediately removes only exact kind/id');
  assert.equal(race.state.busy, false, 'invalidated stale refresh must not leave loader spinning');
  assert.equal(race.pending[3].command, 'cmd_drive_sync_metadata',
    'essential remote metadata sync proceeds even before trash list completes');
  race.pending[1].resolve([target, unrelatedKind, unrelatedFile]);
  await olderList;
  assert.deepEqual(race.state.items.map(item => item.name), ['keep-folder', 'keep.txt'],
    'pre-restore trash response cannot revive successfully restored row');
  race.pending[3].resolve(undefined);
  await restored;
  assert.equal(race.pending[4].command, 'cmd_drive_list_trash');
  assert.equal(race.state.restoring, null);
  assert.equal(race.refs.driveTrashRestoreInFlightRef.current, false);
  assert.deepEqual(race.state.items.map(item => item.name), ['keep-folder', 'keep.txt'],
    'successful restore remains absent while post-restore refresh is still pending');
  race.pending[4].resolve([unrelatedKind, unrelatedFile]);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(race.state.items.map(item => item.name), ['keep-folder', 'keep.txt']);

  const failure = environment();
  const listForFailed = failure.fetchDriveTrash();
  failure.pending[0].resolve([target]);
  await listForFailed;
  const denied = failure.restoreDriveTrashItem(target);
  failure.pending[1].reject(new Error('synthetic native restore failure'));
  await denied;
  assert.equal(failure.state.items.length, 1,
    'failed native restore preserves deleted row for retry');
  const retryDenied = failure.restoreDriveTrashItem(target);
  assert.equal(failure.pending[2].command, 'cmd_drive_restore_trash_item');
  failure.pending[2].reject(new Error('synthetic second restore failure'));
  await retryDenied;

  // Local restore can succeed while Telegram sync fails or stays dirty.
  // The panel must retain an actionable retry without restoring twice.
  const pendingRemote = environment();
  const restoreOnlyOnce = pendingRemote.restoreDriveTrashItem(item);
  pendingRemote.pending[0].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  pendingRemote.pending[1].reject(new Error('synthetic Telegram disconnected'));
  await restoreOnlyOnce;
  assert.equal(pendingRemote.state.syncProblem, 'offline', 'local-only restore retains sync warning');
  assert.equal(pendingRemote.pending[2].command, 'cmd_drive_list_trash');
  pendingRemote.pending[2].resolve([]);
  await new Promise(resolve => setImmediate(resolve));
  const retrySync = pendingRemote.retryDriveRestoreMetadataSync();
  const duplicateSync = pendingRemote.retryDriveRestoreMetadataSync();
  await duplicateSync;
  assert.equal(pendingRemote.pending[3].command, 'cmd_drive_sync_metadata',
    'recovery button only retries cloud metadata sync');
  assert.equal(pendingRemote.state.syncBusy, true, 'retry exposes progress while native sync is pending');
  assert.equal(pendingRemote.refs.driveRestoreSyncInFlightRef.current, true);
  assert.equal(pendingRemote.calls.filter(call => call.command === 'cmd_drive_restore_trash_item').length, 1,
    'retrying remote sync must never restore the item twice');
  assert.equal(pendingRemote.pending.length, 4, 'rapid Retry clicks remain single-flight');
  const blockedRestoreDuringSync = pendingRemote.restoreDriveTrashItem(item);
  await blockedRestoreDuringSync;
  assert.equal(pendingRemote.pending.length, 4, 'native restore waits for ongoing metadata retry');
  pendingRemote.pending[3].resolve({ revision: 20, dirty: false, source: 'merged-published' });
  await retrySync;
  assert.equal(pendingRemote.state.syncProblem, null, 'verified remote success clears persistent warning');
  assert.equal(pendingRemote.state.syncBusy, false);
  assert.equal(pendingRemote.refs.driveRestoreSyncInFlightRef.current, false);
  assert.equal(pendingRemote.messages.at(-1)[0], 'success');
  assert.equal(pendingRemote.pending.length, 4, 'metadata retry does not implicitly refetch or restore trash');

  const stillPending = environment();
  const pendingRestore = stillPending.restoreDriveTrashItem(item);
  stillPending.pending[0].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  stillPending.pending[1].resolve({ revision: 21, dirty: true, source: 'merged-published' });
  await pendingRestore;
  assert.equal(stillPending.state.syncProblem, 'pending', 'dirty native sync stays actionable');
  stillPending.pending[2].resolve([]);
  await new Promise(resolve => setImmediate(resolve));
  const retryStillDirty = stillPending.retryDriveRestoreMetadataSync();
  stillPending.pending[3].resolve({ revision: 21, dirty: false, source: 'qa-local' });
  await retryStillDirty;
  assert.equal(stillPending.state.syncProblem, 'qa-local',
    'QA mode must never be represented as published Telegram metadata');
  assert.equal(stillPending.messages.at(-1)[0], 'warning');
  const retryMalformed = stillPending.retryDriveRestoreMetadataSync();
  stillPending.pending[4].resolve({ source: 'unknown', dirty: false });
  await retryMalformed;
  assert.equal(stillPending.state.syncProblem, 'unverified');
  const retryOffline = stillPending.retryDriveRestoreMetadataSync();
  stillPending.pending[5].reject(new Error('synthetic retry outage'));
  await retryOffline;
  assert.equal(stillPending.state.syncProblem, 'offline');
  assert.equal(stillPending.refs.driveRestoreSyncInFlightRef.current, false,
    'failed retry releases native sync lease');

  const reopenedRetry = environment();
  const local = reopenedRetry.restoreDriveTrashItem(item);
  reopenedRetry.pending[0].resolve('fixture.txt');
  await new Promise(resolve => setImmediate(resolve));
  reopenedRetry.pending[1].reject(new Error('synthetic initial offline'));
  await local;
  reopenedRetry.pending[2].resolve([]);
  await new Promise(resolve => setImmediate(resolve));
  const retryBeforeClose = reopenedRetry.retryDriveRestoreMetadataSync();
  reopenedRetry.close();
  reopenedRetry.reopen();
  const staleToastCount = reopenedRetry.messages.length;
  const staleProblem = reopenedRetry.state.syncProblem;
  const overlapping = reopenedRetry.retryDriveRestoreMetadataSync();
  await overlapping;
  assert.equal(reopenedRetry.pending.length, 4, 'reopened dialog cannot duplicate ongoing retry');
  reopenedRetry.pending[3].resolve({ revision: 23, dirty: false, source: 'remote' });
  await retryBeforeClose;
  assert.equal(reopenedRetry.messages.length, staleToastCount, 'old dialog session cannot toast');
  assert.equal(reopenedRetry.state.syncProblem, staleProblem,
    'old dialog session cannot silently erase persistent pending-sync warning');
  assert.equal(reopenedRetry.state.syncBusy, false, 'native settlement releases reopened busy state');
  assert.equal(reopenedRetry.refs.driveRestoreSyncInFlightRef.current, false);
  const retryAfterReopen = reopenedRetry.retryDriveRestoreMetadataSync();
  assert.equal(reopenedRetry.pending[4].command, 'cmd_drive_sync_metadata',
    'fresh session may retry after stale native sync settled');
  reopenedRetry.pending[4].resolve({ revision: 24, dirty: false, source: 'current' });
  await retryAfterReopen;
  assert.equal(reopenedRetry.state.syncProblem, null);

  assert.match(source, /aria-label="Retry Drive metadata sync after restore"/,
    'persistent local recovery status provides accessible retry action');
  assert.match(source, /role="status"[\s\S]*?driveRestoreSyncProblem/,
    'local-only sync result has persistent visible status');

  // Drive's native-command type declarations do not validate runtime values.
  // A partially malformed response must keep the healthy recovery entries,
  // while a wholly corrupt response preserves the last known usable list.
  const validated = environment();
  validated.useRealNormalizer();
  const healthyRow = {
    kind: 'file', id: 'real-id', name: 'recovery.txt', deleted_at: 1792000000000,
    restorable: true, original_directory_id: null, size: 64,
  };
  const sameIdFolder = {
    kind: 'folder', id: 'real-id', name: 'recovery-folder', deleted_at: 1792000000100,
    restorable: false,
  };
  const parsed = validated.fetchDriveTrash();
  validated.pending[0].resolve([
    healthyRow, { ...healthyRow, name: 'duplicate name.txt' },
    sameIdFolder, { ...healthyRow, id: 'unsafe', restorable: 'yes' },
  ]);
  await parsed;
  assert.deepEqual(Array.from(validated.state.items, row => [row.kind, row.id].join(':')),
    ['file:real-id', 'folder:real-id'],
    'normalization retains only distinct, valid restore identities');
  assert.equal(validated.state.items[0].restorable, true);
  assert.equal(validated.state.items[1].restorable, false);
  assert.match(validated.state.warning, /Skipped 2 malformed or duplicate/,
    'partially corrupt native response reports skipped rows');
  assert.equal(validated.state.error, null);

  const wrongType = validated.fetchDriveTrash();
  validated.pending[1].resolve({ not: 'a list' });
  await wrongType;
  assert.equal(validated.state.items.length, 2, 'wrong native payload cannot erase recovery list');
  assert.match(validated.state.error, /previous list may be out of date/);
  assert.match(validated.state.warning, /Skipped 2/,
    'old partial-data warning remains visible after refresh failure');

  const allCorrupt = validated.fetchDriveTrash();
  validated.pending[2].resolve([null, { name: 'missing identity' }]);
  await allCorrupt;
  assert.equal(validated.state.items.length, 2,
    'fully corrupt native payload cannot silently report empty trash');
  assert.match(validated.state.error, /Could not refresh recently deleted items/);

  const cleanRefresh = validated.fetchDriveTrash();
  validated.pending[3].resolve([healthyRow]);
  await cleanRefresh;
  assert.equal(validated.state.items.length, 1);
  assert.equal(validated.state.warning, null, 'clean native listing clears skipped-item warning');
  assert.equal(validated.state.error, null);

  const emptyRefresh = validated.fetchDriveTrash();
  validated.pending[4].resolve([]);
  await emptyRefresh;
  assert.equal(validated.state.items.length, 0, 'valid empty listing remains empty');
  assert.equal(validated.state.warning, null);
  assert.match(source, /\{driveTrashWarning && \(/,
    'visible Settings UI must expose skipped native rows');
  assert.match(source, /role="alert">[\s\S]*?\{driveTrashWarning\}/,
    'partial corruption is announced accessibly');

  // Legacy file and folder opaque IDs can coincide. Their DOM keys and
  // active restore row must use the same kind:id identity as the native command.
  const sameIds = environment();
  const sameFile = { kind: 'file', id: 'shared', name: 'file.txt', deleted_at: 1000 };
  const sameFolder = { kind: 'folder', id: 'shared', name: 'folder', deleted_at: 2000 };
  const sameIdList = sameIds.fetchDriveTrash();
  sameIds.pending[0].resolve([sameFile, sameFolder]);
  await sameIdList;
  const activeSharedFile = sameIds.restoreDriveTrashItem(sameFile);
  assert.equal(sameIds.state.restoring, 'file:shared', 'only file:shared is in progress');
  assert.notEqual(sameIds.state.restoring, sameFolder.kind + ':' + sameFolder.id,
    'folder with same opaque id must not falsely display Restoring');
  assert.equal(sameIds.calls[1].args.kind, 'file');
  assert.equal(sameIds.calls[1].args.id, 'shared');
  const blockedFolder = sameIds.restoreDriveTrashItem(sameFolder);
  await blockedFolder;
  assert.equal(sameIds.pending.length, 2, 'single-flight blocks second native restore');
  sameIds.pending[1].reject(new Error('synthetic shared-id failure'));
  await activeSharedFile;
  assert.equal(sameIds.state.restoring, null);
  const followingFolder = sameIds.restoreDriveTrashItem(sameFolder);
  assert.equal(sameIds.state.restoring, 'folder:shared');
  assert.equal(sameIds.calls[2].args.kind, 'folder',
    'folder with same opaque id is retryable after first operation ends');
  sameIds.pending[2].reject(new Error('synthetic folder restore failure'));
  await followingFolder;
  assert.match(source, /restoringDriveItem === entry\.kind \+ ':' \+ entry\.id/,
    'actual JSX progress state must match qualified handler key');
  assert.match(source, /aria-label=\{restoringDriveItem === entry\.kind \+ ':' \+ entry\.id/,
    'exact active row exposes accessible restoring status');
  assert.match(source, /new Date\(entry\.deleted_at\)\.toLocaleString\(\)/,
    'deleted item row displays precise local deletion time for duplicate names');

  // Execute the actual effect callback in isolation: first expansion and
  // reopening with the panel already expanded both refresh exactly once.
  const effectScript = ts.transpileModule(
    'globalThis.runDriveTrashEffect = ' + liveTrashEffect.expression.arguments[0].getText(ast) + ';',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText;
  const visibility = { isOpen: false, driveTrashOpen: false, refreshes: 0 };
  visibility.fetchDriveTrash = () => { visibility.refreshes++; };
  vm.runInNewContext(effectScript, visibility);
  visibility.runDriveTrashEffect();
  assert.equal(visibility.refreshes, 0, 'closed modal never fetches trash');
  visibility.isOpen = true;
  visibility.runDriveTrashEffect();
  assert.equal(visibility.refreshes, 0, 'collapsed panel skips list request');
  visibility.driveTrashOpen = true;
  visibility.runDriveTrashEffect();
  assert.equal(visibility.refreshes, 1, 'expanding trash panel triggers one fetch');
  visibility.isOpen = false;
  visibility.runDriveTrashEffect();
  assert.equal(visibility.refreshes, 1, 'closing prevents additional fetch');
  visibility.isOpen = true;
  visibility.runDriveTrashEffect();
  assert.equal(visibility.refreshes, 2, 'reopening expanded panel refreshes stale data');

  console.log('PASS Drive trash: precise per-row restore progress, truthful remote sync results, stale-list suppression and sync-first');
})().catch(error => { console.error(error); process.exitCode = 1; });
