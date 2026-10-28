'use strict';
// Synthetic regression for the real Drive encryption Settings handlers.
// Passphrases here are disposable fixtures. Never touches Telegram or key storage.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require(require.resolve('typescript', {
  paths: [path.resolve(__dirname, '../app')],
}));
const filename = path.resolve(__dirname, '../app/src/components/desktop/dashboard/SettingsModal.tsx');
const source = fs.readFileSync(filename, 'utf8');
const continuityPath = path.resolve(__dirname, '../app/src/deviceContinuity.ts');
const continuityExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(continuityPath, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText, { exports: continuityExports });
const ast = ts.createSourceFile(filename, source, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const component = ast.statements.find(node => ts.isFunctionDeclaration(node)
  && node.name?.text === 'SettingsModal');
assert.ok(component, 'SettingsModal must exist');

const getHandler = name => {
  const declaration = component.body.statements
    .filter(ts.isVariableStatement)
    .flatMap(node => Array.from(node.declarationList.declarations))
    .find(node => ts.isIdentifier(node.name) && node.name.text === name);
  assert.ok(declaration, 'Missing SettingsModal handler: ' + name);
  return 'const ' + declaration.getText(ast) + ';';
};
const cryptoCloseEffect = component.body.statements.find(node =>
  ts.isExpressionStatement(node) && ts.isCallExpression(node.expression)
  && node.expression.expression.getText(ast) === 'useEffect'
  && node.expression.arguments[0]?.getText(ast).includes('driveCryptoSessionRef.current += 1'));
assert.ok(cryptoCloseEffect, 'closing Settings must invalidate Drive crypto UI session');

const realHandlers = [
  'clearDrivePassphrases', 'isCurrentDriveCryptoSession',
  'fetchDriveCryptoStatus', 'handleDriveCryptoSetup', 'handleDriveCryptoUnlock',
  'handleDriveCryptoChangePassphrase', 'handleDriveCryptoLock',
];
const script = ts.transpileModule([
  ...realHandlers.map(getHandler),
  'globalThis.handlers = { ' + realHandlers.join(', ') + ' };',
  'globalThis.closeEffect = ' + cryptoCloseEffect.expression.arguments[0].getText(ast) + ';',
].join('\n'), {
  compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 },
}).outputText;
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { resolve, reject, promise };
}
function environment({ confirmMatchesNew = false } = {}) {
  const fixturePassphrase = 'synthetic-passphrase-123';
  const state = {
    drivePassphrase: fixturePassphrase,
    driveCurrentPassphrase: fixturePassphrase,
    driveNewPassphrase: fixturePassphrase + '-new',
    driveConfirmPassphrase: confirmMatchesNew ? fixturePassphrase + '-new' : fixturePassphrase,
    busy: false,
    status: { configured: false, unlocked: false },
    clearCount: 0,
  };
  const refs = {
    driveCryptoSessionRef: { current: 0 },
    driveCryptoStatusEpochRef: { current: 0 },
    driveCryptoActionInFlightRef: { current: false },
    driveSettingsOpenRef: { current: true },
  };
  const pending = [];
  const confirmations = [];
  const calls = [];
  const toasts = [];
  const setPassphrase = key => value => {
    state[key] = value;
    if (value === '') state.clearCount += 1;
  };
  const context = {
    ...refs,
    isOpen: true,
    drivePassphrase: state.drivePassphrase,
    driveCurrentPassphrase: state.driveCurrentPassphrase,
    driveNewPassphrase: state.driveNewPassphrase,
    driveConfirmPassphrase: state.driveConfirmPassphrase,
    useCallback: fn => fn,
    validateDriveCryptoStatus: continuityExports.validateDriveCryptoStatus,
    setDrivePassphrase: setPassphrase('drivePassphrase'),
    setDriveCurrentPassphrase: setPassphrase('driveCurrentPassphrase'),
    setDriveNewPassphrase: setPassphrase('driveNewPassphrase'),
    setDriveConfirmPassphrase: setPassphrase('driveConfirmPassphrase'),
    setDriveCryptoBusy: value => { state.busy = value; },
    setDriveCryptoStatus: value => { state.status = value; },
    invoke(command, args) {
      calls.push({ command, args });
      const item = deferred();
      pending.push({ command, ...item });
      return item.promise;
    },
    confirm() {
      const item = deferred();
      confirmations.push(item);
      return item.promise;
    },
    toast: {
      success: message => toasts.push(['success', message]),
      error: message => toasts.push(['error', message]),
      info: message => toasts.push(['info', message]),
    },
  };
  vm.runInNewContext(script, context, { filename });
  const settle = async () => { for (let i = 0; i < 4; i++) await new Promise(done => setImmediate(done)); };
  return {
    ...context.handlers, refs, state, calls, pending, confirmations, toasts, settle,
    close() {
      context.isOpen = false;
      refs.driveSettingsOpenRef.current = false;
      context.closeEffect();
    },
    reopen() {
      context.isOpen = true;
      refs.driveSettingsOpenRef.current = true;
      context.closeEffect();
    },
  };
}

(async () => {
  const close = environment();
  const oldSession = close.refs.driveCryptoSessionRef.current;
  close.close();
  assert.equal(close.refs.driveCryptoSessionRef.current, oldSession + 1);
  for (const key of ['drivePassphrase', 'driveCurrentPassphrase',
    'driveNewPassphrase', 'driveConfirmPassphrase']) {
    assert.equal(close.state[key], '', 'closing must clear ' + key);
  }
  assert.equal(close.state.busy, false);

  const unlock = environment();
  const unlocking = unlock.handleDriveCryptoUnlock();
  assert.equal(unlock.pending[0].command, 'cmd_drive_crypto_unlock');
  assert.equal(unlock.state.busy, true);
  unlock.close();
  const noticeCount = unlock.toasts.length;
  unlock.pending[0].resolve({ configured: true, unlocked: true });
  await unlocking;
  assert.equal(unlock.toasts.length, noticeCount, 'late unlock success must not toast');
  assert.equal(unlock.state.status.unlocked, false, 'late unlock does not update closed dialog');
  assert.equal(unlock.state.busy, false);

  const unlockError = environment();
  const failingUnlock = unlockError.handleDriveCryptoUnlock();
  unlockError.close();
  unlockError.pending[0].reject(new Error('synthetic late unlock error'));
  await failingUnlock;
  assert.deepEqual(unlockError.toasts, [], 'late unlock failure cannot toast');

  const change = environment();
  await change.handleDriveCryptoChangePassphrase();
  assert.equal(change.pending.length, 0, 'mismatched passphrase fails before native call');
  assert.equal(change.toasts.at(-1)[0], 'error');

  const changed = environment({ confirmMatchesNew: true });
  const changing = changed.handleDriveCryptoChangePassphrase();
  assert.equal(changed.pending[0].command, 'cmd_drive_crypto_change_passphrase');
  changed.close();
  changed.pending[0].reject(new Error('synthetic delayed passphrase-change failure'));
  await changing;
  assert.equal(changed.pending.length, 1, 'stale failure must not trigger crypto status refresh');
  assert.deepEqual(changed.toasts, [], 'stale passphrase-change failure cannot toast');
  assert.equal(changed.state.busy, false);

  const changedSuccess = environment({ confirmMatchesNew: true });
  const changingSuccess = changedSuccess.handleDriveCryptoChangePassphrase();
  changedSuccess.close();
  changedSuccess.reopen();
  changedSuccess.pending[0].resolve({ configured: true, unlocked: true });
  await changingSuccess;
  assert.equal(changedSuccess.toasts.length, 0, 'older change cannot toast in reopened dialog');
  assert.equal(changedSuccess.state.status.configured, false);

  const locking = environment();
  const lockPromise = locking.handleDriveCryptoLock();
  assert.equal(locking.pending[0].command, 'cmd_drive_crypto_lock');
  locking.close();
  locking.pending[0].resolve({ configured: true, unlocked: false });
  await lockPromise;
  assert.equal(locking.toasts.length, 0, 'late lock completion cannot toast');
  assert.equal(locking.state.status.configured, false, 'late lock status cannot change closed view');

  const closingSetup = environment();
  const setupPromise = closingSetup.handleDriveCryptoSetup();
  assert.equal(closingSetup.confirmations.length, 1);
  closingSetup.close();
  closingSetup.confirmations[0].resolve(true);
  await setupPromise;
  assert.equal(closingSetup.pending.length, 0,
    'confirming setup after close cannot start metadata sync or new native encryption');

  const syncingSetup = environment();
  const setupAfterConfirm = syncingSetup.handleDriveCryptoSetup();
  syncingSetup.confirmations[0].resolve(true);
  await syncingSetup.settle();
  assert.equal(syncingSetup.pending[0].command, 'cmd_drive_sync_metadata');
  syncingSetup.close();
  syncingSetup.pending[0].resolve({ source: 'current', dirty: false, revision: 1 });
  await setupAfterConfirm;
  assert.equal(syncingSetup.pending.length, 1,
    'closing during metadata sync must not call crypto status/setup afterward');
  assert.equal(syncingSetup.toasts.length, 0, 'stale crypto setup cannot notify');

  const cryptoStatus = environment();
  const requestingStatus = cryptoStatus.fetchDriveCryptoStatus();
  cryptoStatus.close();
  cryptoStatus.reopen();
  cryptoStatus.pending[0].resolve({ configured: true, unlocked: false });
  await requestingStatus;
  assert.equal(cryptoStatus.state.status.configured, false,
    'old status refresh cannot overwrite reopened Settings');

  // An immediate double-submit must remain single-flight even before a
  // React state update rerenders/disables the buttons. A second crypto action
  // cannot run while the first native command owns the in-flight ref.
  const duplicate = environment();
  const firstUnlock = duplicate.handleDriveCryptoUnlock();
  await duplicate.handleDriveCryptoUnlock();
  await duplicate.handleDriveCryptoLock();
  assert.equal(duplicate.pending.length, 1, 'duplicate unlock and concurrent lock dispatch only one native command');
  assert.equal(duplicate.refs.driveCryptoActionInFlightRef.current, true);
  duplicate.pending[0].reject(new Error('synthetic failed first unlock'));
  await firstUnlock;
  assert.equal(duplicate.refs.driveCryptoActionInFlightRef.current, false, 'native errors release crypto lock');
  const afterFailureLock = duplicate.handleDriveCryptoLock();
  assert.equal(duplicate.pending[1].command, 'cmd_drive_crypto_lock', 'new crypto action available after failure');
  duplicate.pending[1].resolve({ configured: true, unlocked: false });
  await afterFailureLock;

  const setupGate = environment();
  const firstSetup = setupGate.handleDriveCryptoSetup();
  await setupGate.handleDriveCryptoSetup();
  await setupGate.handleDriveCryptoLock();
  assert.equal(setupGate.confirmations.length, 1, 'double setup opens only one confirmation');
  assert.equal(setupGate.pending.length, 0, 'native crypto commands blocked while awaiting setup confirmation');
  assert.equal(setupGate.state.busy, true);
  setupGate.confirmations[0].resolve(false);
  await firstSetup;
  assert.equal(setupGate.refs.driveCryptoActionInFlightRef.current, false, 'cancelled setup releases lock');
  assert.equal(setupGate.state.busy, false, 'cancelled setup restores actions');
  const afterCancelledSetup = setupGate.handleDriveCryptoLock();
  assert.equal(setupGate.pending[0].command, 'cmd_drive_crypto_lock');
  setupGate.pending[0].resolve({ configured: false, unlocked: false });
  await afterCancelledSetup;

  const rejectedConfirm = environment();
  const badConfirmation = rejectedConfirm.handleDriveCryptoSetup();
  rejectedConfirm.confirmations[0].reject(new Error('synthetic confirmation unavailable'));
  await badConfirmation;
  assert.equal(rejectedConfirm.refs.driveCryptoActionInFlightRef.current, false,
    'failed setup confirmation releases synchronous action ownership');
  assert.equal(rejectedConfirm.state.busy, false);
  assert.equal(rejectedConfirm.toasts.at(-1)[0], 'error', 'confirmation failure is handled');

  const syncingGate = environment();
  const confirmedSetup = syncingGate.handleDriveCryptoSetup();
  syncingGate.confirmations[0].resolve(true);
  await syncingGate.settle();
  assert.equal(syncingGate.pending[0].command, 'cmd_drive_sync_metadata');
  await syncingGate.handleDriveCryptoUnlock();
  await syncingGate.handleDriveCryptoLock();
  assert.equal(syncingGate.pending.length, 1,
    'crypto actions remain serialized throughout setup cloud metadata preflight');
  syncingGate.pending[0].reject(new Error('synthetic offline setup preflight'));
  await confirmedSetup;
  assert.equal(syncingGate.refs.driveCryptoActionInFlightRef.current, false);
  assert.equal(syncingGate.state.busy, false);

  const reopenedInFlight = environment();
  const priorUnlock = reopenedInFlight.handleDriveCryptoUnlock();
  reopenedInFlight.close();
  assert.equal(reopenedInFlight.state.busy, false, 'close clears visible busy state');
  reopenedInFlight.reopen();
  assert.equal(reopenedInFlight.state.busy, true, 'reopening displays previously running crypto action');
  await reopenedInFlight.handleDriveCryptoLock();
  assert.equal(reopenedInFlight.pending.length, 1, 'reopened dialog cannot issue conflicting command');
  reopenedInFlight.pending[0].resolve({ configured: true, unlocked: true });
  await priorUnlock;
  assert.equal(reopenedInFlight.state.busy, false, 'old operation completion unlocks current dialog');
  assert.equal(reopenedInFlight.refs.driveCryptoActionInFlightRef.current, false);
  assert.deepEqual(reopenedInFlight.toasts, [], 'old crypto operation remains silent after reopening');
  const lockAfterOld = reopenedInFlight.handleDriveCryptoLock();
  assert.equal(reopenedInFlight.pending[1].command, 'cmd_drive_crypto_lock');
  reopenedInFlight.pending[1].resolve({ configured: true, unlocked: false });
  await lockAfterOld;

  const active = environment();
  const activeUnlock = active.handleDriveCryptoUnlock();
  active.pending[0].resolve({ configured: true, unlocked: true });
  await activeUnlock;
  assert.equal(active.toasts.at(-1)[0], 'success');
  assert.equal(active.state.status.unlocked, true);
  assert.equal(active.state.busy, false);
  assert.equal(active.state.clearCount, 4);

  // A resolved native command is not proof of its requested security state.
  // Do not clear the passphrase or claim success for contradictory statuses.
  const rejectedUnlockStatus = environment();
  const uncertainUnlock = rejectedUnlockStatus.handleDriveCryptoUnlock();
  rejectedUnlockStatus.pending[0].resolve({ configured: true, unlocked: false });
  await uncertainUnlock;
  assert.equal(rejectedUnlockStatus.toasts.at(-1)[0], 'error');
  assert.equal(rejectedUnlockStatus.toasts.some(([tone]) => tone === 'success'), false);
  assert.equal(rejectedUnlockStatus.state.clearCount, 0, 'failed unlock retains input for retry');
  assert.equal(rejectedUnlockStatus.refs.driveCryptoActionInFlightRef.current, false);

  const rejectedLockStatus = environment();
  const uncertainLock = rejectedLockStatus.handleDriveCryptoLock();
  rejectedLockStatus.pending[0].resolve({ configured: true, unlocked: true });
  await uncertainLock;
  assert.equal(rejectedLockStatus.toasts.at(-1)[0], 'error',
    'lock cannot succeed while native status still says unlocked');
  assert.equal(rejectedLockStatus.toasts.some(([tone]) => tone === 'success'), false);
  assert.equal(rejectedLockStatus.refs.driveCryptoActionInFlightRef.current, false);

  const rejectedChangeStatus = environment({ confirmMatchesNew: true });
  const uncertainChange = rejectedChangeStatus.handleDriveCryptoChangePassphrase();
  rejectedChangeStatus.pending[0].resolve({ configured: true, unlocked: false });
  await uncertainChange;
  assert.equal(rejectedChangeStatus.toasts.at(-1)[0], 'error',
    'passphrase update cannot claim success when the resulting Drive is locked');
  assert.equal(rejectedChangeStatus.state.clearCount, 0);
  assert.equal(rejectedChangeStatus.pending[1].command, 'cmd_drive_crypto_status',
    'failed state verification initiates conservative status recheck');
  rejectedChangeStatus.pending[1].resolve({ configured: true, unlocked: false });
  await rejectedChangeStatus.settle();

  const rejectedSetupStatus = environment();
  const uncertainSetup = rejectedSetupStatus.handleDriveCryptoSetup();
  rejectedSetupStatus.confirmations[0].resolve(true);
  await rejectedSetupStatus.settle();
  assert.equal(rejectedSetupStatus.pending[0].command, 'cmd_drive_sync_metadata');
  rejectedSetupStatus.pending[0].resolve({ revision: 5, dirty: false, source: 'current' });
  await rejectedSetupStatus.settle();
  assert.equal(rejectedSetupStatus.pending[1].command, 'cmd_drive_crypto_status');
  rejectedSetupStatus.pending[1].resolve({ configured: false, unlocked: false });
  await rejectedSetupStatus.settle();
  assert.equal(rejectedSetupStatus.pending[2].command, 'cmd_drive_crypto_setup');
  rejectedSetupStatus.pending[2].resolve({ configured: false, unlocked: false });
  await uncertainSetup;
  assert.equal(rejectedSetupStatus.toasts.at(-1)[0], 'error',
    'setup cannot claim encryption was enabled with unconfigured status');
  assert.equal(rejectedSetupStatus.toasts.some(([tone]) => tone === 'success'), false);
  assert.equal(rejectedSetupStatus.state.clearCount, 0);
  assert.equal(rejectedSetupStatus.refs.driveCryptoActionInFlightRef.current, false);
  assert.equal(rejectedSetupStatus.pending[3].command, 'cmd_drive_crypto_status');
  rejectedSetupStatus.pending[3].resolve({ configured: false, unlocked: false });
  await rejectedSetupStatus.settle();

  // Valid native postconditions still follow normal success and cleanup paths.
  const goodLock = environment();
  const finishedLock = goodLock.handleDriveCryptoLock();
  goodLock.pending[0].resolve({ configured: true, unlocked: false });
  await finishedLock;
  assert.equal(goodLock.toasts.at(-1)[0], 'success');
  assert.equal(goodLock.state.clearCount, 4);

  const goodChange = environment({ confirmMatchesNew: true });
  const finishedChange = goodChange.handleDriveCryptoChangePassphrase();
  goodChange.pending[0].resolve({ configured: true, unlocked: true });
  await finishedChange;
  assert.equal(goodChange.toasts.at(-1)[0], 'success');
  assert.equal(goodChange.state.clearCount, 4);

  // A stale status request from BEFORE a mutating command cannot reverse the
  // visible result after the native unlock/lock has already succeeded.
  const staleBeforeUnlock = environment();
  const oldStatus = staleBeforeUnlock.fetchDriveCryptoStatus();
  const newUnlock = staleBeforeUnlock.handleDriveCryptoUnlock();
  staleBeforeUnlock.pending[1].resolve({ configured: true, unlocked: true });
  await newUnlock;
  staleBeforeUnlock.pending[0].resolve({ configured: true, unlocked: false });
  await oldStatus;
  assert.equal(staleBeforeUnlock.state.status.unlocked, true,
    'old locked-status reply cannot override a newer successful unlock');

  const staleBeforeLock = environment();
  const oldUnlockedStatus = staleBeforeLock.fetchDriveCryptoStatus();
  const newLock = staleBeforeLock.handleDriveCryptoLock();
  staleBeforeLock.pending[1].resolve({ configured: true, unlocked: false });
  await newLock;
  staleBeforeLock.pending[0].resolve({ configured: true, unlocked: true });
  await oldUnlockedStatus;
  assert.equal(staleBeforeLock.state.status.unlocked, false,
    'old unlocked-status reply cannot override a newer successful lock');

  const overlappingQueries = environment();
  const firstStatus = overlappingQueries.fetchDriveCryptoStatus();
  const secondStatus = overlappingQueries.fetchDriveCryptoStatus();
  overlappingQueries.pending[1].resolve({ configured: true, unlocked: false });
  await secondStatus;
  overlappingQueries.pending[0].resolve({ configured: false, unlocked: false });
  await firstStatus;
  assert.equal(overlappingQueries.state.status.configured, true,
    'only latest crypto status request may update live Settings');

  const lateError = environment();
  const oldFailedStatus = lateError.fetchDriveCryptoStatus();
  const newerStatus = lateError.fetchDriveCryptoStatus();
  lateError.pending[1].resolve({ configured: true, unlocked: true });
  await newerStatus;
  lateError.pending[0].reject(new Error('synthetic stale status error'));
  await oldFailedStatus;
  assert.equal(lateError.state.status.unlocked, true,
    'stale error cannot overwrite newer status with unavailable state');

  const duringUnlock = environment();
  const pendingUnlock = duringUnlock.handleDriveCryptoUnlock();
  const concurrentPoll = duringUnlock.fetchDriveCryptoStatus();
  duringUnlock.pending[1].resolve({ configured: true, unlocked: false });
  await concurrentPoll;
  assert.equal(duringUnlock.state.status.unlocked, false, 'poll may finish before unlock');
  duringUnlock.pending[0].resolve({ configured: true, unlocked: true });
  await pendingUnlock;
  assert.equal(duringUnlock.state.status.unlocked, true,
    'mutation postcondition overrides status sampled during operation');

  const afterActionQuery = duringUnlock.fetchDriveCryptoStatus();
  duringUnlock.pending[2].resolve({ configured: true, unlocked: true });
  await afterActionQuery;
  assert.equal(duringUnlock.state.status.unlocked, true,
    'new status queries continue to work after action completion');

  // TypeScript invoke<T>() is only a compile-time contract. A truthy
  // string "false" must not claim decrypted content or clear inputs.
  for (const [label, invalidStatus] of [
    ['null', null],
    ['missing-configured', { unlocked: true }],
    ['string-configured', { configured: 'false', unlocked: true }],
    ['truthy-unlocked', { configured: true, unlocked: 'false' }],
    ['contradictory-security', { configured: false, unlocked: true }],
    ['invalid-version', { configured: true, unlocked: true, version: '1' }],
  ]) {
    const corruptUnlock = environment();
    const attempt = corruptUnlock.handleDriveCryptoUnlock();
    corruptUnlock.pending[0].resolve(invalidStatus);
    await attempt;
    assert.equal(corruptUnlock.toasts.at(-1)[0], 'error', label + ': no false unlock success');
    assert.equal(corruptUnlock.toasts.some(([tone]) => tone === 'success'), false);
    assert.equal(corruptUnlock.state.clearCount, 0,
      label + ': passphrase remains available for corrective retry');
    assert.equal(corruptUnlock.refs.driveCryptoActionInFlightRef.current, false,
      label + ': native ownership releases after invalid response');
    const followUp = corruptUnlock.handleDriveCryptoUnlock();
    assert.equal(corruptUnlock.pending[1].command, 'cmd_drive_crypto_unlock',
      label + ': correct native response can be retried');
    corruptUnlock.pending[1].resolve({ configured: true, unlocked: true });
    await followUp;
    assert.equal(corruptUnlock.toasts.at(-1)[0], 'success',
      label + ': recovery only after valid security state');
  }

  for (const [label, value] of [
    ['string-locked', { configured: true, unlocked: 'false' }],
    ['missing-unlocked', { configured: true }],
    ['wrong-configuration', { configured: 'true', unlocked: false }],
  ]) {
    const corruptLock = environment();
    const request = corruptLock.handleDriveCryptoLock();
    corruptLock.pending[0].resolve(value);
    await request;
    assert.equal(corruptLock.toasts.at(-1)[0], 'error', label + ': lock status is verified');
    assert.equal(corruptLock.toasts.some(([tone]) => tone === 'success'), false);
    assert.equal(corruptLock.state.clearCount, 0);
    assert.equal(corruptLock.refs.driveCryptoActionInFlightRef.current, false);
  }

  const invalidLiveStatus = environment();
  const firstCryptoStatus = invalidLiveStatus.fetchDriveCryptoStatus();
  invalidLiveStatus.pending[0].resolve({ configured: 'false', unlocked: 'false' });
  await firstCryptoStatus;
  assert.equal(invalidLiveStatus.state.status, null,
    'malformed native crypto refresh cannot display a forged status');
  const recoveredCryptoStatus = invalidLiveStatus.fetchDriveCryptoStatus();
  invalidLiveStatus.pending[1].resolve({ configured: true, unlocked: false });
  await recoveredCryptoStatus;
  assert.equal(invalidLiveStatus.state.status.configured, true);
  assert.equal(invalidLiveStatus.state.status.unlocked, false);

  const invalidSetupPreflight = environment();
  const invalidPreflightRequest = invalidSetupPreflight.handleDriveCryptoSetup();
  invalidSetupPreflight.confirmations[0].resolve(true);
  await invalidSetupPreflight.settle();
  invalidSetupPreflight.pending[0].resolve({ revision: 1, source: 'current', dirty: false });
  await invalidSetupPreflight.settle();
  assert.equal(invalidSetupPreflight.pending[1].command, 'cmd_drive_crypto_status');
  invalidSetupPreflight.pending[1].resolve({ configured: 'false', unlocked: false });
  await invalidPreflightRequest;
  assert.equal(invalidSetupPreflight.toasts.at(-1)[0], 'error',
    'invalid preflight cannot authorize a new master encryption key');
  assert.equal(invalidSetupPreflight.calls.some(x => x.command === 'cmd_drive_crypto_setup'), false,
    'setup is never dispatched for malformed preflight status');
  assert.equal(invalidSetupPreflight.refs.driveCryptoActionInFlightRef.current, false);
  assert.equal(invalidSetupPreflight.pending[2].command, 'cmd_drive_crypto_status');
  invalidSetupPreflight.pending[2].resolve({ configured: false, unlocked: false });
  await invalidSetupPreflight.settle();

  const invalidSetupResult = environment();
  const setupAttempt = invalidSetupResult.handleDriveCryptoSetup();
  invalidSetupResult.confirmations[0].resolve(true);
  await invalidSetupResult.settle();
  invalidSetupResult.pending[0].resolve({ revision: 2, source: 'current', dirty: false });
  await invalidSetupResult.settle();
  invalidSetupResult.pending[1].resolve({ configured: false, unlocked: false });
  await invalidSetupResult.settle();
  assert.equal(invalidSetupResult.pending[2].command, 'cmd_drive_crypto_setup');
  invalidSetupResult.pending[2].resolve({ configured: true, unlocked: 'false' });
  await setupAttempt;
  assert.equal(invalidSetupResult.toasts.at(-1)[0], 'error');
  assert.equal(invalidSetupResult.toasts.some(([tone]) => tone === 'success'), false);
  assert.equal(invalidSetupResult.state.clearCount, 0);
  assert.equal(invalidSetupResult.refs.driveCryptoActionInFlightRef.current, false);
  invalidSetupResult.pending[3].resolve({ configured: true, unlocked: false });
  await invalidSetupResult.settle();

  const invalidChangeResult = environment({ confirmMatchesNew: true });
  const changeAttempt = invalidChangeResult.handleDriveCryptoChangePassphrase();
  assert.equal(invalidChangeResult.pending[0].command, 'cmd_drive_crypto_change_passphrase');
  invalidChangeResult.pending[0].resolve({ configured: true, unlocked: 'false' });
  await changeAttempt;
  assert.equal(invalidChangeResult.toasts.at(-1)[0], 'error');
  assert.equal(invalidChangeResult.toasts.some(([tone]) => tone === 'success'), false);
  assert.equal(invalidChangeResult.state.clearCount, 0);
  assert.equal(invalidChangeResult.refs.driveCryptoActionInFlightRef.current, false);
  invalidChangeResult.pending[1].resolve({ configured: true, unlocked: false });
  await invalidChangeResult.settle();

  console.log('PASS Drive crypto Settings: strict native IPC security fields, postconditions, ordering and single-flight');
})().catch(error => { console.error(error); process.exitCode = 1; });
