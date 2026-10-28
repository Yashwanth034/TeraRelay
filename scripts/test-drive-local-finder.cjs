'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const tsc = text => ts.transpileModule(text, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText;
const continuity = {};
vm.runInNewContext(tsc(fs.readFileSync(path.join(root, 'app/src/deviceContinuity.ts'), 'utf8')),
  { exports: continuity });
const finder = {};
vm.runInNewContext(tsc(fs.readFileSync(path.join(root, 'app/src/driveLocalFinder.ts'), 'utf8')),
  { exports: finder, require: name => {
    assert.equal(name, './deviceContinuity'); return continuity;
  } });
const folder = (id, name, parent_id = null) => ({ id, name, parent_id });
const file = (entry_id, display_name, directory_id = null, total_size = 10) =>
  ({ entry_id, display_name, directory_id, total_size, plaintext_size: null });
const pending = (id, display_name, directory_id = null, size = 11) =>
  ({ id, display_name, directory_id, size });
const snapshot = (directories = [], files = [], pending = []) =>
  ({ revision: 3, dirty: false, directories, files, pending });
const data = snapshot(
  [folder('docs', 'Documents'), folder('photos', 'Photos', 'docs'),
    folder('cycle-a', 'LoopA', 'cycle-b'), folder('cycle-b', 'LoopB', 'cycle-a'),
    folder('missing-parent', 'Lost', 'unknown-dir'), folder('bad-name', 'a/b'),
    folder('docs', 'Duplicate'), { ...folder('trashed', 'Removed'), trashed_at: 123 }],
  [file('photo1', 'Beach.JPG', 'photos', 524288), file('root', 'Readme.md'),
    file('orphan', 'CannotResolve.txt', 'missing-parent'),
    file('bad', '../injected.txt'), file('unsafe', 'err\nnewline'),
    file('neg', 'UnknownSize.bin', null, -5)],
  [pending('write-1', 'unfinished.bin', 'docs', 9),
    pending('bad-write', 'orphan.part', 'cycle-a')],
);
// Reviewer-found trust-boundary regression: malformed internal identities
// must never be accepted as valid parent references or leaf identities.
for (const [i, invalidId] of [
  '/', '\\', '.', '..', 'folder/../escape', 'folder\\child', 'a\nb', 'a\tb',
  'a\0b', ' a ', '\u007f', '\u0085', '\u2028', '\u2029',
].entries()) {
  const invalid = finder.normalizeDriveFinderSnapshot(snapshot(
    [folder('stable', 'Stable'), folder(invalidId, 'Unsafe'),
      folder('child-' + i, 'Child', invalidId)],
    [file(invalidId, 'unsafe.txt'), file('reference-' + i, 'via-unsafe-parent.txt', invalidId)],
    [pending(invalidId, 'bad-write'), pending('pending-' + i, 'pending-parent.txt', invalidId)],
  ));
  assert.equal(invalid.entries.length, 1, 'malformed IDs cannot project a path: ' + JSON.stringify(invalidId));
  assert.equal(invalid.entries[0].path, '/Stable');
  assert.ok(invalid.skipped >= 6, 'reject malformed internal IDs and their children');
}
const normalized = finder.normalizeDriveFinderSnapshot(data);
const paths = Array.from(normalized.entries, e => e.path);
assert.ok(paths.includes('/Documents'));
assert.ok(paths.includes('/Documents/Photos/Beach.JPG'));
assert.ok(paths.includes('/Documents/unfinished.bin'));
assert.ok(paths.includes('/Readme.md'));
assert.ok(paths.includes('/UnknownSize.bin'));
assert.equal(normalized.entries.find(e => e.id === 'neg').size, null);
assert.ok(!paths.some(p => /Loop|Lost|orphan|injected|newline|Duplicate|Removed/.test(p)));
assert.ok(normalized.skipped >= 9, 'invalid names, ancestry and duplicate identities rejected');
assert.equal(normalized.truncated, 0);
assert.equal(finder.filterDriveFinderEntries(normalized.entries, 'DOCUMENTS/photos', 'all').length, 2,
  'path matching finds both folder and a nested file case-insensitively');
assert.equal(finder.filterDriveFinderEntries(normalized.entries, 'JpG', 'file').length, 1);
assert.equal(finder.filterDriveFinderEntries(normalized.entries, 'documents', 'pending').length, 1);
assert.equal(finder.filterDriveFinderEntries(normalized.entries, '', 'file').length, 3);
assert.deepEqual(Array.from(finder.filterDriveFinderEntries(normalized.entries, 'not-a-match', 'all')), []);
assert.throws(() => finder.normalizeDriveFinderSnapshot(null), /snapshot/i);
assert.throws(() => finder.normalizeDriveFinderSnapshot({ ...data, revision: -1 }), /snapshot/i);
assert.throws(() => finder.normalizeDriveFinderSnapshot({ ...data, files: 'not an array' }), /snapshot/i);
const giant = snapshot([], Array.from({ length: 25007 }, (_, i) => file('f' + i, 'File ' + String(i).padStart(5, '0'))));
const bounded = finder.normalizeDriveFinderSnapshot(giant);
assert.equal(bounded.entries.length, finder.DRIVE_FINDER_MAX_ROWS);
assert.equal(bounded.truncated, 7);
assert.equal(bounded.skipped, 0);
assert.equal(giant.files.length, 25007, 'native snapshot remains unchanged');
const srcPath = path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx');
const source = fs.readFileSync(srcPath, 'utf8');
const ast = ts.createSourceFile(srcPath, source, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const component = ast.statements.find(n => ts.isFunctionDeclaration(n) && n.name?.text === 'SettingsModal');
assert.ok(component, 'real SettingsModal is present');
const declaration = component.body.statements.filter(ts.isVariableStatement)
  .flatMap(n => Array.from(n.declarationList.declarations))
  .find(n => ts.isIdentifier(n.name) && n.name.text === 'refreshLocalDriveFinder');
assert.ok(declaration?.initializer, 'real finder refresh handler exists');
const js = ts.transpileModule(
  'globalThis.refreshFinder = ' + declaration.initializer.getText(ast) + ';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function harness() {
  const state = { entries: [], busy: false, error: null, skipped: null, truncated: null, limit: 50 };
  const calls = [];
  const refs = {
    driveSettingsOpenRef: { current: true },
    driveFinderRequestRef: { current: 0 },
    driveFinderFetchInFlightRef: { current: false },
  };
  const ctx = {
    ...refs,
    normalizeDriveFinderSnapshot: finder.normalizeDriveFinderSnapshot,
    invoke(command) {
      calls.push(command);
      assert.equal(command, 'cmd_drive_get_snapshot', 'only read local DB');
      const next = deferred();
      pending.push(next);
      return next.promise;
    },
    setDriveFinderBusy: value => { state.busy = value; },
    setDriveFinderError: value => { state.error = value; },
    setDriveFinderEntries: value => { state.entries = value; },
    setDriveFinderSkipped: value => { state.skipped = value; },
    setDriveFinderTruncated: value => { state.truncated = value; },
    setDriveFinderVisibleLimit: value => { state.limit = value; },
  };
  const pending = [];
  vm.runInNewContext(js, ctx);
  const settle = async () => { for (let i = 0; i < 4; i++) await new Promise(done => setImmediate(done)); };
  return {
    state, calls, pending, refs, settle, run: () => ctx.refreshFinder(),
    close() {
      refs.driveSettingsOpenRef.current = false;
      refs.driveFinderRequestRef.current++;
      refs.driveFinderFetchInFlightRef.current = false;
    },
    reopen() { refs.driveSettingsOpenRef.current = true; },
  };
}
(async () => {
  const success = harness();
  const job = success.run();
  await success.run();
  assert.deepEqual(success.calls, ['cmd_drive_get_snapshot'], 'single-flight refresh');
  assert.equal(success.state.busy, true);
  success.pending[0].resolve(data);
  await job;
  assert.equal(success.state.busy, false);
  assert.equal(success.state.error, null);
  assert.ok(success.state.entries.some(e => e.path === '/Documents/Photos/Beach.JPG'));
  assert.equal(success.state.skipped, normalized.skipped);

  const malformed = harness();
  const bad = malformed.run();
  malformed.pending[0].resolve({ ...data, dirty: 'not-a-boolean' });
  await bad;
  assert.match(malformed.state.error, /verify local Drive metadata/);
  assert.equal(malformed.state.entries.length, 0);
  const retry = malformed.run();
  malformed.pending[1].resolve(snapshot([folder('good', 'Recovered')]));
  await retry;
  assert.equal(malformed.state.error, null);
  assert.equal(malformed.state.entries[0].path, '/Recovered');

  const stale = harness();
  const old = stale.run();
  stale.close();
  stale.reopen();
  const latest = stale.run();
  stale.pending[1].resolve(snapshot([folder('new', 'New')]));
  await latest;
  stale.pending[0].resolve(snapshot([folder('old', 'Old')]));
  await old;
  assert.equal(stale.state.entries[0].name, 'New', 'stale former Settings session cannot overwrite new results');
  assert.equal(stale.calls.length, 2);

  const copyDeclaration = component.body.statements.filter(ts.isVariableStatement)
    .flatMap(n => Array.from(n.declarationList.declarations))
    .find(n => ts.isIdentifier(n.name) && n.name.text === 'copyLocalDriveRelativePath');
  assert.ok(copyDeclaration?.initializer, 'real Copy Drive relative path action exists');
  const copyJs = ts.transpileModule(
    'globalThis.copyRelativePath = ' + copyDeclaration.initializer.getText(ast) + ';',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText;
  let resolveClipboard;
  let rejectClipboard;
  const copied = [], notifications = [], busy = [];
  const copyRefs = { driveSettingsOpenRef: { current: true },
    driveFinderRequestRef: { current: 2 }, driveFinderCopyInFlightRef: { current: false } };
  const copyContext = {
    ...copyRefs, driveFinderOpen: true,
    navigator: { clipboard: { writeText: value => {
      copied.push(value);
      return new Promise((resolve, reject) => { resolveClipboard = resolve; rejectClipboard = reject; });
    } } },
    setDriveFinderCopyBusy: value => busy.push(value),
    toast: { success: message => notifications.push(['success', message]),
      error: message => notifications.push(['error', message]) },
  };
  vm.runInNewContext(copyJs, copyContext);
  const validCopy = copyContext.copyRelativePath('/Documents/Photos/Beach.JPG');
  await copyContext.copyRelativePath('/Different.txt');
  assert.equal(copied.length, 1, 'repeated copy while clipboard pending remains single-flight');
  assert.equal(copied[0], '/Documents/Photos/Beach.JPG');
  resolveClipboard();
  await validCopy;
  assert.deepEqual(busy, [true, false]);
  assert.equal(notifications[0][0], 'success');
  assert.match(notifications[0][1], /not an absolute OS mount path/);
  const invalidCopy = copyContext.copyRelativePath('/retry.txt');
  rejectClipboard(new Error('synthetic clipboard denied'));
  await invalidCopy;
  assert.equal(notifications.at(-1)[0], 'error');
  const lateCopy = copyContext.copyRelativePath('/closed.txt');
  copyRefs.driveSettingsOpenRef.current = false;
  copyRefs.driveFinderRequestRef.current++;
  resolveClipboard();
  await lateCopy;
  assert.equal(notifications.length, 2, 'closed dialog cannot show late clipboard toasts');

  assert.match(source, /aria-label="Open local Drive file finder"/);
  assert.match(source, /Copy Drive-relative path for \$\{entry\.name\}/);
  assert.match(source, /aria-label="Search local Drive filenames or folder paths"/);
  assert.match(source, /aria-label="Filter local Drive finder by type"/);
  assert.match(source, /aria-label="Show more local Drive search results"/);
  assert.match(source, /driveFinderMatches\.slice\(0, driveFinderVisibleLimit\)/);
  assert.match(source, /normalizeDriveFinderSnapshot\(await invoke<unknown>\('cmd_drive_get_snapshot'\)\)/);
  console.log('PASS Drive local finder: safe paths, ancestry cycles, 25k cap, full-text filters, real IPC handler, stale sessions');
})().catch(error => { console.error(error); process.exitCode = 1; });
