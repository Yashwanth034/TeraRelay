'use strict';
// Deterministic Drive passphrase modal tests. Synthetic strings only: no login,
// actual credentials, native OS keyring, filesystem or Telegram access.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const file = path.join(root, 'app/src/components/shared/DriveContinuityUnlock.tsx');
const compiled = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
  compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const continuityPath = path.join(root, 'app/src/deviceContinuity.ts');
const continuityExports = {};
vm.runInNewContext(ts.transpileModule(fs.readFileSync(continuityPath, 'utf8'), {
  compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
}).outputText, { exports: continuityExports });

const jsx = (type, props) => ({ type, props: props || {} });
const find = (node, match) => {
  if (Array.isArray(node)) return node.map(child => find(child, match)).find(Boolean);
  if (!node || typeof node !== 'object') return undefined;
  if (match(node)) return node;
  return find(node.props?.children, match);
};

function harness() {
  const state = [];
  const refs = [];
  let stateIndex = 0;
  let refIndex = 0;
  let effect;
  let cleanup;
  let previousOpen;
  let requests = [];
  let unlocked = 0;
  let dismissed = 0;
  const successes = [];
  const useState = value => {
    const index = stateIndex++;
    if (!(index in state)) state[index] = value;
    return [state[index], value => { state[index] = typeof value === 'function' ? value(state[index]) : value; }];
  };
  const useRef = initial => {
    const index = refIndex++;
    if (!(index in refs)) refs[index] = { current: initial };
    return refs[index];
  };
  const exports = {};
  const stubs = {
    react: { useState, useRef, useEffect: fn => { effect = fn; } },
    'react/jsx-runtime': { jsx, jsxs: jsx, Fragment: 'fragment' },
    '../../deviceContinuity': continuityExports,
    '@tauri-apps/api/core': { invoke: (command, args) => {
      assert.equal(command, 'cmd_drive_crypto_unlock');
      assert.equal(args.passphrase, 'synthetic-secret-only');
      return new Promise((resolve, reject) => { requests.push({ resolve, reject }); });
    } },
    'lucide-react': new Proxy({}, { get: (_t, key) => String(key) }),
    sonner: { toast: { success: message => successes.push(message) } },
  };
  vm.runInNewContext(compiled, {
    exports, require: name => {
      if (!(name in stubs)) throw new Error('Unexpected dependency: ' + name);
      return stubs[name];
    },
  }, { filename: file });

  let tree;
  let open;
  const render = nextOpen => {
    stateIndex = 0;
    refIndex = 0;
    tree = exports.DriveContinuityUnlock({
      open: nextOpen,
      restoredFromRemote: false,
      directoryCount: 0,
      fileCount: 0,
      onUnlocked: () => { unlocked++; },
      onDismiss: () => { dismissed++; },
    });
    if (previousOpen !== nextOpen) {
      if (cleanup) cleanup();
      cleanup = effect();
    }
    previousOpen = nextOpen;
    open = nextOpen;
    return tree;
  };
  const typePassphrase = () => {
    const input = find(tree, node => node.type === 'input' && node.props.type === 'password');
    assert.ok(input && open);
    input.props.onChange({ target: { value: 'synthetic-secret-only' } });
    render(open);
  };
  const submit = () => {
    const form = find(tree, node => node.type === 'form');
    assert.ok(form);
    return form.props.onSubmit({ preventDefault() {} });
  };
  const dismiss = () => {
    const button = find(tree, node => node.type === 'button'
      && node.props?.children === 'Later');
    assert.ok(button, 'Later must be available');
    assert.equal(button.props.disabled, undefined, 'Later must remain usable while unlocking');
    button.props.onClick();
  };
  const escape = () => {
    const dialog = find(tree, node => node.props?.role === 'dialog');
    assert.ok(dialog?.props.onKeyDown);
    let prevented = false;
    dialog.props.onKeyDown({ key: 'Escape', preventDefault: () => { prevented = true; } });
    assert.ok(prevented, 'Escape prevents background handler');
  };
  const settle = async () => { for (let i = 0; i < 4; i++) await new Promise(resolve => setImmediate(resolve)); };
  const hasError = () => Boolean(find(tree, node => node.props?.role === 'alert'));
  render(true);
  return {
    render, submit, typePassphrase, dismiss, escape, settle, hasError,
    get requests() { return requests; },
    get successes() { return successes; },
    get unlocked() { return unlocked; },
    get dismissed() { return dismissed; },
    get state() { return state; },
  };
}

(async () => {
  const h = harness();
  h.typePassphrase();
  const first = h.submit();
  const duplicate = h.submit();
  assert.equal(h.requests.length, 1, 'rapid repeated submission must not invoke twice');
  await duplicate;
  assert.equal(h.state[0], '', 'passphrase input clears as native unlock begins');
  h.render(false); // simulate logout / external closure during unlock
  h.render(true); // next session reopens before old result arrives
  assert.equal(h.state[1], true, 'reopened prompt reflects native unlock still running');
  await h.submit();
  assert.equal(h.requests.length, 1, 'reopen cannot dispatch duplicate native unlock');
  h.requests[0].resolve({ configured: true, unlocked: true });
  await first;
  await h.settle();
  h.render(true);
  assert.equal(h.state[1], false, 'previous native completion releases reopened busy state');
  assert.equal(h.unlocked, 0, 'late success cannot unlock the new session');
  assert.equal(h.successes.length, 0, 'late success cannot display a success toast');

  h.typePassphrase();
  const second = h.submit();
  h.requests[1].reject(new Error('synthetic invalid passphrase'));
  await second;
  h.render(true);
  assert.equal(h.hasError(), true, 'current failure shows an accessible inline warning');
  assert.equal(h.unlocked, 0);

  h.typePassphrase();
  const third = h.submit();
  h.requests[2].resolve({ configured: true, unlocked: true });
  await third;
  assert.equal(h.unlocked, 1, 'current successful request unlocks exactly once');
  assert.equal(h.successes.length, 1);

  const staleFailure = harness();
  staleFailure.typePassphrase();
  const pending = staleFailure.submit();
  staleFailure.render(false);
  staleFailure.requests[0].reject(new Error('synthetic late error'));
  await pending;
  staleFailure.render(true);
  assert.equal(staleFailure.hasError(), false, 'late failure must not pollute a reopened session');

  // A slow native unlock must never trap someone in the modal. Dismissal
  // immediately fences that request, leaving already-running native work alone.
  const closedWhileBusy = harness();
  closedWhileBusy.typePassphrase();
  const oldUnlock = closedWhileBusy.submit();
  closedWhileBusy.render(true);
  closedWhileBusy.dismiss();
  closedWhileBusy.dismiss();
  assert.equal(closedWhileBusy.dismissed, 1, 'repeated Later click dismisses once');
  assert.equal(closedWhileBusy.state[0], '', 'dismissal clears the passphrase');
  assert.equal(closedWhileBusy.state[1], false, 'dismissal resets busy state');
  closedWhileBusy.requests[0].resolve({ configured: true, unlocked: true });
  await oldUnlock;
  assert.equal(closedWhileBusy.unlocked, 0, 'dismissed native success must not unlock UI');
  assert.equal(closedWhileBusy.successes.length, 0, 'dismissed result must not toast');
  closedWhileBusy.render(false);
  closedWhileBusy.render(true);
  assert.equal(closedWhileBusy.state[1], false, 'completed dismissed request releases busy state');
  closedWhileBusy.typePassphrase();
  const newUnlock = closedWhileBusy.submit();
  assert.equal(closedWhileBusy.requests.length, 2, 'fresh session may retry after dismiss');
  closedWhileBusy.requests[1].resolve({ configured: true, unlocked: true });
  await newUnlock;
  assert.equal(closedWhileBusy.unlocked, 1, 'fresh session handles only fresh completion');

  const escapedWhileBusy = harness();
  escapedWhileBusy.typePassphrase();
  const pendingEscape = escapedWhileBusy.submit();
  escapedWhileBusy.render(true);
  escapedWhileBusy.escape();
  assert.equal(escapedWhileBusy.dismissed, 1, 'Escape dismisses during native unlock');
  escapedWhileBusy.requests[0].reject(new Error('synthetic late native failure'));
  await pendingEscape;
  escapedWhileBusy.render(false);
  escapedWhileBusy.render(true);
  assert.equal(escapedWhileBusy.hasError(), false, 'dismissed failure cannot pollute reopened prompt');
  assert.equal(escapedWhileBusy.successes.length, 0);

  // Closing and reopening while the dismissed request is *still* pending
  // must leave the new form disabled until native completion, without stale
  // alerts or an extra unlock command. A late rejection is also fenced.
  const reopenedPending = harness();
  reopenedPending.typePassphrase();
  const priorPending = reopenedPending.submit();
  reopenedPending.render(true);
  reopenedPending.dismiss();
  reopenedPending.render(false);
  reopenedPending.render(true);
  assert.equal(reopenedPending.state[1], true, 'reopened form remains busy during prior native invoke');
  const passwordInput = find(reopenedPending.render(true), node => node.type === 'input'
    && node.props?.type === 'password');
  assert.equal(passwordInput.props.disabled, true);
  await reopenedPending.submit();
  assert.equal(reopenedPending.requests.length, 1, 'no duplicate native unlock after reopening');
  reopenedPending.requests[0].reject(new Error('synthetic older native rejection'));
  await priorPending;
  reopenedPending.render(true);
  assert.equal(reopenedPending.state[1], false, 'old rejection releases current busy indicator');
  assert.equal(reopenedPending.hasError(), false, 'old rejection never shows stale error');
  reopenedPending.typePassphrase();
  const freshAfterError = reopenedPending.submit();
  assert.equal(reopenedPending.requests.length, 2, 'fresh retry allowed after old native command settles');
  reopenedPending.requests[1].resolve({ configured: true, unlocked: true });
  await freshAfterError;
  assert.equal(reopenedPending.unlocked, 1);
  assert.equal(reopenedPending.successes.length, 1);

  const escapedBeforeSubmit = harness();
  escapedBeforeSubmit.typePassphrase();
  escapedBeforeSubmit.escape();
  await escapedBeforeSubmit.submit();
  assert.equal(escapedBeforeSubmit.requests.length, 0,
    'stale form handler cannot dispatch unlock after prompt was dismissed');

  // The frontend must not treat a resolved native IPC promise as a valid
  // decrypted Drive. A truthy string or contradictory encryption state is
  // particularly dangerous here because the old UI would call onUnlocked.
  for (const [label, nativeResult] of [
    ['null', null],
    ['missing-configured', { unlocked: true }],
    ['unconfigured-but-unlocked', { configured: false, unlocked: true }],
    ['string-true', { configured: true, unlocked: 'true' }],
    ['string-false', { configured: true, unlocked: 'false' }],
    ['still-locked', { configured: true, unlocked: false }],
    ['invalid-version', { configured: true, unlocked: true, version: 'one' }],
  ]) {
    const corrupted = harness();
    corrupted.typePassphrase();
    const firstAttempt = corrupted.submit();
    assert.equal(corrupted.requests.length, 1, label);
    corrupted.requests[0].resolve(nativeResult);
    await firstAttempt;
    corrupted.render(true);
    assert.equal(corrupted.unlocked, 0, label + ': malformed response must not call onUnlocked');
    assert.equal(corrupted.successes.length, 0, label + ': malformed response must not toast');
    assert.equal(corrupted.hasError(), true, label + ': malformed response must show accessible error');
    assert.equal(corrupted.state[1], false, label + ': bad response must release busy state');
    assert.equal(corrupted.state[0], '', label + ': passphrase must not linger after request');

    corrupted.typePassphrase();
    const recovered = corrupted.submit();
    assert.equal(corrupted.requests.length, 2, label + ': fresh retry must be allowed');
    corrupted.requests[1].resolve({ configured: true, unlocked: true, version: 1 });
    await recovered;
    corrupted.render(true);
    assert.equal(corrupted.unlocked, 1, label + ': valid retry unlocks once');
    assert.equal(corrupted.successes.length, 1, label + ': success toast only after validated result');
    assert.equal(corrupted.hasError(), false, label + ': valid retry clears previous error');
  }

  console.log('PASS Drive unlock: verified crypto IPC postcondition, native request lease, dismiss/reopen and safe retry');
})().catch(error => { console.error(error); process.exitCode = 1; });
