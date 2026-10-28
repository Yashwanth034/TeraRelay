const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const base = path.join(__dirname, '../app/src/components/desktop/dashboard');
const modals = {
  'Rename File': { file: 'RenameFileModal.tsx', id: 'tr-rename-file-title' },
  'Rename Folder': { file: 'RenameFolderModal.tsx', id: 'tr-rename-folder-title' },
  'Move to Folder': { file: 'MoveToFolderModal.tsx', id: 'tr-move-folder-title' },
  'Join Channel': { file: 'JoinChannelModal.tsx', id: 'tr-join-channel-title' },
  'Share': { file: 'ShareDialog.tsx', id: 'tr-share-dialog-title' },
  'Archive Viewer': { file: 'ArchiveViewerModal.tsx', id: 'tr-archive-viewer-title' },
  'Version Stack': { file: 'VersionStackModal.tsx', id: 'tr-version-stack-title' },
  'Remote Upload': { file: 'RemoteUploadModal.tsx', id: 'tr-remote-upload-title' },
};

function read(file) { return fs.readFileSync(path.join(base, file), 'utf8'); }
function issues(source, id) {
  const result = [];
  if (!/role="dialog"/.test(source)) result.push('role');
  if (!/aria-modal="true"/.test(source)) result.push('modal');
  if (!source.includes('aria-labelledby="' + id + '"')) result.push('association');
  if (!source.includes('id="' + id + '"')) result.push('title');
  return result;
}

for (const [name, config] of Object.entries(modals)) {
  test(name + ' exposes a named modal dialog', () => {
    assert.deepEqual(issues(read(config.file), config.id), []);
  });
}

test('both rename dialogs label their text field and icon-only Close control', () => {
  for (const [file, label] of [
    ['RenameFileModal.tsx', 'files.file_name'],
    ['RenameFolderModal.tsx', 'files.folder_name'],
  ]) {
    const source = read(file);
    assert.ok(source.includes("aria-label={t('" + label + "')}"), file);
    assert.ok(source.includes("aria-label={t('common.close')}"), file);
  }
});

test('Join Channel associates the invite textarea with its label', () => {
  const source = read('JoinChannelModal.tsx');
  assert.match(source, /<label htmlFor="tr-channel-invite"/);
  assert.match(source, /<textarea\s+id="tr-channel-invite"\s+aria-label="Channel invite"/);
});

test('Share protects the password toggle accessible name and pressed state', () => {
  const source = read('ShareDialog.tsx');
  assert.match(source, /aria-label=\{t\('share\.password_protection'\)\}/);
  assert.match(source, /aria-pressed=\{requirePassword\}/);
  assert.match(source, /aria-label=\{t\('share\.enter_password'\)\}/);
});

test('Preview exposes a named dialog and named navigation/Close controls', () => {
  const source = read('PreviewModal.tsx');
  assert.match(source, /role="dialog" aria-modal="true" aria-label=\{\x60Preview: /);
  for (const label of ['Previous file', 'Next file', 'Close preview']) {
    assert.ok(source.includes('aria-label="' + label + '"'), label);
  }
});

test('Archive Viewer exposes a named icon-only Close control', () => {
  assert.match(read('ArchiveViewerModal.tsx'), /aria-label="Close archive"/);
});

test('Version Stack search field has a stable accessible name', () => {
  const source = read('VersionStackModal.tsx');
  const expected = 'aria-label="Search existing files"';
  assert.ok(source.includes(expected));
  const withoutLabel = source.replace(expected, '');
  assert.ok(!withoutLabel.includes(expected), 'Removing the explicit search label must be detectable');
});

const versionStackRequiredNames = [
  'aria-label="Stack name"',
  'aria-label="Save stack name"',
  'aria-label={`Version label for ${displayFileName(memberFile.name)}`}',
  'aria-label={`Preview ${displayFileName(memberFile.name)}`}',
  'aria-label={`Download ${displayFileName(memberFile.name)}`}',
];

function missingVersionStackNames(source) {
  return versionStackRequiredNames.filter(name => !source.includes(name));
}

test('Version Stack name and per-version controls expose explicit names', () => {
  assert.deepEqual(missingVersionStackNames(read('VersionStackModal.tsx')), []);
});

test('negative: removing the Stack name label fails the explicit-name contract', () => {
  const source = read('VersionStackModal.tsx').replace('aria-label="Stack name"', '');
  assert.deepEqual(missingVersionStackNames(source), [versionStackRequiredNames[0]]);
});

test('negative: removing a member label fails the explicit-name contract', () => {
  const source = read('VersionStackModal.tsx').replace(versionStackRequiredNames[2], '');
  assert.deepEqual(missingVersionStackNames(source), [versionStackRequiredNames[2]]);
});

test('Remote Upload link field has an explicit AT-SPI name as well as its visible label', () => {
  const source = read('RemoteUploadModal.tsx');
  assert.match(source, /<label htmlFor="tr-remote-upload-links"/);
  assert.match(source, /<textarea\s+id="tr-remote-upload-links"\s+aria-label="Links to import"/);
});

test('negative: dropping the Remote Upload native field name is detected', () => {
  const source = read('RemoteUploadModal.tsx');
  const without = source.replace('aria-label="Links to import"', '');
  assert.doesNotMatch(without, /<textarea\s+id="tr-remote-upload-links"\s+aria-label="Links to import"/);
});

test('negative: removing a modal role is detected', () => {
  const config = modals['Rename File'];
  const changed = read(config.file).replace('role="dialog"', '');
  assert.ok(issues(changed, config.id).includes('role'));
});

test('negative: mismatching aria-labelledby is detected', () => {
  const config = modals['Share'];
  const changed = read(config.file).replace('aria-labelledby="' + config.id + '"',
    'aria-labelledby="nonexistent-heading"');
  assert.ok(issues(changed, config.id).includes('association'));
});

function channelInfoIssues(source) {
  const required = [
    ['dialog', /role="dialog"\s+aria-modal="true"\s+aria-labelledby="tr-channel-info-title"/],
    ['channel title', /<h2 id="tr-channel-info-title"/],
    ['named focus target', /aria-label="Close channel info"\s+autoFocus/],
    ['keyboard trap', /onKeyDown=\{trapPanelTab\}/],
    ['panel focus root', /ref=\{panelRef\}\s+tabIndex=\{-1\}/],
  ];
  return required.filter(([, re]) => !re.test(source)).map(([name]) => name);
}

test('Channel Information side panel exposes a named dialog and initial keyboard focus', () => {
  assert.deepEqual(channelInfoIssues(read('ChannelInfoPanel.tsx')), []);
});

test('negative: Channel Information loses the dialog role', () => {
  const modified = read('ChannelInfoPanel.tsx').replace('role="dialog"', '');
  assert.ok(channelInfoIssues(modified).includes('dialog'));
});

test('negative: Channel Information loses its local Tab trap', () => {
  const modified = read('ChannelInfoPanel.tsx').replace('onKeyDown={trapPanelTab}', '');
  assert.ok(channelInfoIssues(modified).includes('keyboard trap'));
});

test('Channel Info opener has an explicit action and channel accessible name', () => {
  const source = read('TopBar.tsx');
  const name = 'aria-label={`Channel info for ${currentFolderName}`}';
  assert.ok(source.includes(name));
  assert.ok(source.includes('onClick={onChannelInfo}'));
  assert.ok(source.includes('channelMode && onChannelInfo ? ('));
  assert.ok(!source.replace(name, '').includes(name),
    'Removing Channel Info accessible name must fail the contract');
});

function currentChannelInfoCloseHandler() {
  const source = fs.readFileSync(path.join(__dirname, '../app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  const start = source.indexOf('    const closeChannelInfo = () => {');
  const end = source.indexOf('\n    };', start);
  assert.ok(start >= 0 && end > start, 'The current channel info close handler must exist');
  return source.slice(start, end + '\n    };'.length);
}

test('Channel Information opener is saved and restored after closing', () => {
  const source = fs.readFileSync(path.join(__dirname, '../app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.match(source, /channelInfoReturnFocusRef\.current = document\.activeElement instanceof HTMLElement/);
  assert.match(source, /onChannelInfo=\{activeFolderId !== null \? openChannelInfo : undefined\}/);
  assert.match(source, /onClose=\{closeChannelInfo\}/);
  assert.match(currentChannelInfoCloseHandler(), /previousFocus\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/);
});

test('negative: dropping Channel Information focus restoration is detected', () => {
  const altered = currentChannelInfoCloseHandler().replace('previousFocus.focus({ preventScroll: true })', '');
  assert.doesNotMatch(altered, /previousFocus\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/);
});

// Compile and execute the actual panel keydown handler. This deterministic
// contract is not a substitute for keyboard input in a live Tauri window.
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');
const channelSource = read('ChannelInfoPanel.tsx');
const trapStart = channelSource.indexOf('    const trapPanelTab = (event: React.KeyboardEvent<HTMLElement>) => {');
const trapEnd = channelSource.indexOf('\n    };', trapStart);
assert.ok(trapStart >= 0 && trapEnd > trapStart, 'Current channel panel keyboard handler must exist');
const trapBody = channelSource.slice(trapStart, trapEnd + '\n    };'.length);
const compiledTrap = ts.transpileModule(
  'function makeTrap(panelRef, document) {\n' + trapBody +
  '\nreturn trapPanelTab;\n}\nexports.makeTrap = makeTrap;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const trapModule = {};
vm.runInNewContext(compiledTrap.outputText, { exports: trapModule }, { timeout: 1000 });

function panelFixture({ initial = 'first', visible = [true, true, true], factory = trapModule.makeTrap } = {}) {
  const doc = { activeElement: null };
  function control(name, shown) {
    return {
      name,
      getClientRects: () => shown ? [{}] : [],
      focus() { doc.activeElement = this; },
    };
  }
  const controls = [
    control('first', visible[0]),
    control('middle', visible[1]),
    control('last', visible[2]),
  ];
  const outside = control('outside', true);
  const panel = {
    contains(node) { return controls.includes(node); },
    querySelectorAll(query) {
      assert.match(query, /button:not\(\[disabled\]\)/);
      return controls;
    },
    focus() { doc.activeElement = this; },
  };
  doc.activeElement = ({ first: controls[0], middle: controls[1], last: controls[2], outside })[initial];
  const trap = factory({ current: panel }, doc);
  const press = (key, shift = false) => {
    let prevented = 0;
    trap({ key, shiftKey: shift, preventDefault() { prevented++; } });
    return { focus: doc.activeElement, prevented };
  };
  return { press, controls, panel };
}

test('Channel panel Shift+Tab wraps from first to last', () => {
  const f = panelFixture();
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[2]);
});

test('Channel panel Tab wraps from last to first', () => {
  const f = panelFixture({ initial: 'last' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[0]);
});

test('Channel panel Tab from middle is left to native browser navigation', () => {
  const f = panelFixture({ initial: 'middle' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.controls[1]);
});

test('Channel panel wraps to the final visible control, not a hidden button', () => {
  const f = panelFixture({ visible: [true, true, false] });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[1]);
});

test('Channel panel without visible controls keeps focus inside the panel', () => {
  const f = panelFixture({ visible: [false, false, false] });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.panel);
});

// Reuse the same fixture for the *actual* Move to Folder keyboard handler.
// This is a source/handler regression, not a native AT-SPI keyboard receipt.
const moveSource = read('MoveToFolderModal.tsx');
function moveDialogIssues(source) {
  const required = [
    ['focus root', /ref=\{dialogRef\}\s+tabIndex=\{-1\}/],
    ['keyboard trap', /onKeyDown=\{trapMoveDialogTab\}/],
    ['named initial Close', /ref=\{closeButtonRef\}\s+type="button"[^>]*aria-label=\{t\('common\.close'\)\}/],
    ['initial focus', /requestAnimationFrame\(\(\) => closeButtonRef\.current\?\.focus\(\{ preventScroll: true \}\)\)/],
    ['connected opener return', /previousFocus\?\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/],
  ];
  return required.filter(([, re]) => !re.test(source)).map(([name]) => name);
}

test('Move to Folder supplies initial focus, dialog keyboard containment and safe opener restoration', () => {
  assert.deepEqual(moveDialogIssues(moveSource), []);
});

test('negative: removing Move to Folder Tab trap violates keyboard contract', () => {
  assert.ok(moveDialogIssues(moveSource.replace('onKeyDown={trapMoveDialogTab}', '')).includes('keyboard trap'));
});

test('negative: removing Move to Folder opener restoration violates focus contract', () => {
  assert.ok(moveDialogIssues(moveSource.replace('previousFocus.focus({ preventScroll: true })', '')).includes('connected opener return'));
});

const moveTrapStart = moveSource.indexOf('    const trapMoveDialogTab = (event: React.KeyboardEvent<HTMLDivElement>) => {');
const moveTrapEnd = moveSource.indexOf('\n    };', moveTrapStart);
assert.ok(moveTrapStart >= 0 && moveTrapEnd > moveTrapStart, 'Current Move to Folder handler must exist');
const moveTrapBody = moveSource.slice(moveTrapStart, moveTrapEnd + '\n    };'.length);
const compiledMoveTrap = ts.transpileModule(
  'function makeTrap(dialogRef, document) {\n' + moveTrapBody +
  '\nreturn trapMoveDialogTab;\n}\nexports.makeTrap = makeTrap;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const moveTrapModule = {};
vm.runInNewContext(compiledMoveTrap.outputText, { exports: moveTrapModule }, { timeout: 1000 });
function moveFixture(options = {}) { return panelFixture({ ...options, factory: moveTrapModule.makeTrap }); }

test('Move to Folder Shift+Tab wraps from Close to last visible destination', () => {
  const f = moveFixture();
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[2]);
});

test('Move to Folder Tab wraps from last destination to Close', () => {
  const f = moveFixture({ initial: 'last' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[0]);
});

test('Move to Folder leaves middle Tab navigation to the browser', () => {
  const f = moveFixture({ initial: 'middle' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.controls[1]);
});

test('Move to Folder excludes invisible destinations from keyboard wrap', () => {
  const f = moveFixture({ visible: [true, true, false] });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[1]);
});

test('Move to Folder contains keyboard focus when no destination is visible', () => {
  const f = moveFixture({ visible: [false, false, false] });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.panel);
});

test('Move to Folder regains keyboard containment if focus was outside', () => {
  const f = moveFixture({ initial: 'outside' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[0]);
});

test('Move to Folder never traps ordinary typing', () => {
  const f = moveFixture();
  const result = f.press('Enter');
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.controls[0]);
});

function moveSelectionIssues(source) {
  const required = [
    ['ref gate', /if \(selectingRef\.current\) return;/],
    ['immediate lock', /selectingRef\.current = true;/],
    ['awaited operation', /await onSelect\(id\)/],
    ['unlock in finally', /finally \{\s+selectingRef\.current = false;\s+setSelecting\(false\)/],
    ['Escape disabled in flight', /useEscapeToClose\(true, closeIfIdle, selecting\)/],
    ['same-tick dismissal guard', /if \(!selectingRef\.current\) onClose\(\)/],
    ['Close disabled in flight', /onClick=\{closeIfIdle\} disabled=\{selecting\}/],
    ['destinations disabled in flight', /disabled=\{selecting\}\s+onClick=\{\(\) => \{ void selectOnce\(f\.id\); \}\}/],
  ];
  return required.filter(([, re]) => !re.test(source)).map(([name]) => name);
}

test('Move to Folder blocks duplicate destinations and dismissal while a transfer is in flight', () => {
  assert.deepEqual(moveSelectionIssues(moveSource), []);
  const dashboard = fs.readFileSync(path.join(__dirname, '../app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.ok(dashboard.includes('await handleBulkMove(targetFolderId, () => setShowMoveModal(false))'),
    'bulk move promise must keep the dialog locked until the operation completes');
});

test('negative: removing Move to Folder immediate in-flight lock fails the contract', () => {
  assert.ok(moveSelectionIssues(moveSource.replace('selectingRef.current = true;', '')).includes('immediate lock'));
});

const selectStart = moveSource.indexOf('    const selectOnce = async (id: number | null) => {');
const selectEnd = moveSource.indexOf('\n    };', selectStart);
assert.ok(selectStart >= 0 && selectEnd > selectStart, 'Current Move to Folder select-once handler must exist');
const selectBody = moveSource.slice(selectStart, selectEnd + '\n    };'.length);
const compiledSelect = ts.transpileModule(
  'function makeSelect(selectingRef, setSelecting, onSelect) {\n' + selectBody +
  '\nreturn selectOnce;\n}\nexports.makeSelect = makeSelect;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const selectModule = {};
vm.runInNewContext(compiledSelect.outputText, { exports: selectModule }, { timeout: 1000 });

test('Move to Folder submits at most one overlapping asynchronous move', async () => {
  let release;
  const pending = new Promise(resolve => { release = resolve; });
  const calls = [];
  const updates = [];
  const selectingRef = { current: false };
  const select = selectModule.makeSelect(selectingRef, value => updates.push(value), async id => {
    calls.push(id);
    await pending;
  });
  const first = select(42);
  const duplicate = select(43);
  assert.deepEqual(calls, [42]);
  assert.equal(selectingRef.current, true);
  release();
  await Promise.all([first, duplicate]);
  assert.equal(selectingRef.current, false);
  assert.deepEqual(updates, [true, false]);
  await select(43);
  assert.deepEqual(calls, [42, 43]);
});

test('Move to Folder unlocks after a rejected move so the user can retry', async () => {
  const updates = [];
  const selectingRef = { current: false };
  let calls = 0;
  const select = selectModule.makeSelect(selectingRef, value => updates.push(value), async () => {
    if (++calls === 1) throw new Error('move failed');
  });
  await assert.rejects(select(null), /move failed/);
  assert.equal(selectingRef.current, false);
  await select(7);
  assert.equal(calls, 2);
  assert.deepEqual(updates, [true, false, true, false]);
});

test('Move to Folder same-tick close guard refuses dismissal during an active move', () => {
  const start = moveSource.indexOf('    const closeIfIdle = () => {');
  const end = moveSource.indexOf('\n    };', start);
  assert.ok(start >= 0 && end > start);
  const body = moveSource.slice(start, end + '\n    };'.length);
  const compiled = ts.transpileModule(
    'function makeClose(selectingRef, onClose) {\n' + body +
    '\nreturn closeIfIdle;\n}\nexports.makeClose = makeClose;',
    { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
  );
  const mod = {};
  vm.runInNewContext(compiled.outputText, { exports: mod }, { timeout: 1000 });
  const state = { current: false };
  let dismissed = 0;
  const close = mod.makeClose(state, () => dismissed++);
  state.current = true;
  close();
  assert.equal(dismissed, 0);
  state.current = false;
  close();
  assert.equal(dismissed, 1);
});

const joinSource = read('JoinChannelModal.tsx');

function joinDialogIssues(source) {
  const required = [
    ['dialog focus root', /ref=\{dialogRef\}\s+tabIndex=\{-1\}/],
    ['busy state exposed', /aria-busy=\{busy\}/],
    ['join error announced', /\{error && \(\s+<div role="alert"/],
    ['local Tab handler', /onKeyDown=\{trapJoinDialogTab\}/],
    ['initial invite field focus', /aria-label="Channel invite"\s+autoFocus/],
    ['busy invite disabled', /autoFocus\s+disabled=\{busy\}/],
    ['immediate join lock', /submitPendingRef\.current = true;/],
    ['duplicate join gate', /if \(!value \|\| submitPendingRef\.current\) return;/],
    ['join release in finally', /finally \{\s+submitPendingRef\.current = false;\s+setBusy\(false\)/],
    ['Escape ref guard', /useEscapeToClose\(open, closeIfIdle, busy\)/],
    ['close ref guard', /if \(!submitPendingRef\.current\) onClose\(\)/],
  ];
  return required.filter(([, regex]) => !regex.test(source)).map(([label]) => label);
}

test('Join Channel contains focus and blocks overlapping async joins', () => {
  assert.deepEqual(joinDialogIssues(joinSource), []);
});

test('negative: missing Join Channel immediate ref lock violates the contract', () => {
  const modified = joinSource.replace('submitPendingRef.current = true;', '');
  assert.ok(joinDialogIssues(modified).includes('immediate join lock'));
});

test('negative: removing Join Channel local keyboard trap violates the contract', () => {
  const modified = joinSource.replace('onKeyDown={trapJoinDialogTab}', '');
  assert.ok(joinDialogIssues(modified).includes('local Tab handler'));
});

test('negative: Join Channel error requires an accessible alert announcement', () => {
  const modified = joinSource.replace('<div role="alert" className="tr-modal-note tr-modal-note--danger break-words">', '<div className="tr-modal-note tr-modal-note--danger break-words">');
  assert.ok(joinDialogIssues(modified).includes('join error announced'));
});

test('Join Channel restores focus to its still-connected Sidebar opener', () => {
  const source = fs.readFileSync(path.join(__dirname, '../app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.match(source, /onJoinChannel=\{openJoinChannel\}/);
  assert.match(source, /onClose=\{closeJoinChannel\}/);
  assert.match(source, /joinChannelReturnFocusRef\.current = active instanceof HTMLElement/);
  assert.match(source, /if \(previousFocus\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/);
});

const joinTabStart = joinSource.indexOf('    const trapJoinDialogTab = (event: React.KeyboardEvent<HTMLFormElement>) => {');
const joinTabEnd = joinSource.indexOf('\n    };', joinTabStart);
assert.ok(joinTabStart >= 0 && joinTabEnd > joinTabStart, 'Actual Join Channel Tab handler required');
const joinTabBody = joinSource.slice(joinTabStart, joinTabEnd + '\n    };'.length);
const joinTabCompiled = ts.transpileModule(
  'function makeTrap(dialogRef, document) {\n' + joinTabBody +
  '\nreturn trapJoinDialogTab;\n}\nexports.makeTrap = makeTrap;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const joinTabModule = {};
vm.runInNewContext(joinTabCompiled.outputText, { exports: joinTabModule }, { timeout: 1000 });

test('Join Channel Tab wraps from last enabled control to first', () => {
  const f = panelFixture({ initial: 'last', factory: joinTabModule.makeTrap });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[0]);
});

test('Join Channel Shift+Tab wraps to last visible enabled control', () => {
  const f = panelFixture({ visible: [true, true, false], factory: joinTabModule.makeTrap });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.controls[1]);
});

test('Join Channel contains empty and outside-focused Tab without leaking', () => {
  const none = panelFixture({ visible: [false, false, false], factory: joinTabModule.makeTrap });
  assert.equal(none.press('Tab').focus, none.panel);
  const outside = panelFixture({ initial: 'outside', factory: joinTabModule.makeTrap });
  assert.equal(outside.press('Tab').focus, outside.controls[0]);
});

const joinSubmitStart = joinSource.indexOf('    const submit = async (event: React.FormEvent) => {');
const joinSubmitEnd = joinSource.indexOf('\n    };', joinSubmitStart);
assert.ok(joinSubmitStart >= 0 && joinSubmitEnd > joinSubmitStart, 'Actual Join Channel submit handler required');
const joinSubmitBody = joinSource.slice(joinSubmitStart, joinSubmitEnd + '\n    };'.length);
const joinSubmitCompiled = ts.transpileModule(
  'function makeSubmit(invite, submitPendingRef, setBusy, setError, onJoin, onClose) {\n' +
  joinSubmitBody + '\nreturn submit;\n}\nexports.makeSubmit = makeSubmit;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const joinSubmitModule = {};
vm.runInNewContext(joinSubmitCompiled.outputText, { exports: joinSubmitModule }, { timeout: 1000 });
function makeJoinFixture(invite, onJoin) {
  const lock = { current: false };
  const busyUpdates = [];
  const errorUpdates = [];
  let closed = 0;
  const submit = joinSubmitModule.makeSubmit(
    invite, lock,
    value => busyUpdates.push(value),
    value => errorUpdates.push(value),
    onJoin,
    () => { closed++; },
  );
  return {
    lock, busyUpdates, errorUpdates, submit,
    get closed() { return closed; },
    event: { preventDefault() {} },
  };
}

test('Join Channel allows exactly one pending join even for two same-tick submissions', async () => {
  let resolveJoin;
  const pending = new Promise(resolve => { resolveJoin = resolve; });
  const calls = [];
  const f = makeJoinFixture('  terarelay://join/test  ', async invite => {
    calls.push(invite);
    await pending;
  });
  const first = f.submit(f.event);
  const second = f.submit(f.event);
  assert.deepEqual(calls, ['terarelay://join/test']);
  assert.equal(f.lock.current, true);
  assert.equal(f.closed, 0);
  resolveJoin();
  await Promise.all([first, second]);
  assert.equal(f.lock.current, false);
  assert.equal(f.closed, 1);
  assert.deepEqual(f.busyUpdates, [true, false]);
});

test('Join Channel releases the lock after failed join and permits retry', async () => {
  let count = 0;
  const f = makeJoinFixture('test-invite', async () => {
    if (++count === 1) throw new Error('invalid invite');
  });
  await f.submit(f.event);
  assert.equal(f.closed, 0);
  assert.equal(f.lock.current, false);
  assert.ok(f.errorUpdates.includes('invalid invite'));
  await f.submit(f.event);
  assert.equal(count, 2);
  assert.equal(f.closed, 1);
});

test('Join Channel rejects empty input without starting a network operation', async () => {
  const calls = [];
  const f = makeJoinFixture('   ', async invite => calls.push(invite));
  await f.submit(f.event);
  assert.equal(f.lock.current, false);
  assert.deepEqual(calls, []);
  assert.deepEqual(f.busyUpdates, []);
});

test('Join Channel close guard refuses same-tick dismissal until operation completes', () => {
  const start = joinSource.indexOf('    const closeIfIdle = () => {');
  const end = joinSource.indexOf('\n    };', start);
  assert.ok(start >= 0 && end > start);
  const body = joinSource.slice(start, end + '\n    };'.length);
  const compiled = ts.transpileModule(
    'function makeClose(submitPendingRef, onClose) {\n' + body +
    '\nreturn closeIfIdle;\n}\nexports.makeClose = makeClose;',
    { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
  );
  const mod = {};
  vm.runInNewContext(compiled.outputText, { exports: mod }, { timeout: 1000 });
  const lock = { current: true };
  let closes = 0;
  const close = mod.makeClose(lock, () => closes++);
  close();
  assert.equal(closes, 0);
  lock.current = false;
  close();
  assert.equal(closes, 1);
});

// Keep the existing rename tests in this suite rather than creating two
// duplicate harnesses. Compile each dialog's real current async handler.
for (const config of [
  { file: 'RenameFileModal.tsx', oldName: 'before.txt', folder: false },
  { file: 'RenameFolderModal.tsx', oldName: 'Before Folder', folder: true },
]) {
  const source = read(config.file);
  const handlerStart = source.indexOf('    const handleSubmit = async () => {');
  const handlerEnd = source.indexOf('\n    };', handlerStart);
  assert.ok(handlerStart >= 0 && handlerEnd > handlerStart, 'Missing rename handler: ' + config.file);
  const handler = source.slice(handlerStart, handlerEnd + '\n    };'.length);
  const paramNames = config.folder
    ? 'name, currentName, folderId, submittingRef, setIsSubmitting, onRename, onClose, closeIfIdle'
    : 'name, fileName, submittingRef, setIsSubmitting, onRename, onClose, closeIfIdle';
  const compiled = ts.transpileModule(
    'function makeSubmit(' + paramNames + ') {\n' + handler +
    '\nreturn handleSubmit;\n}\nexports.makeSubmit = makeSubmit;',
    { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
  );
  const compiledModule = {};
  vm.runInNewContext(compiled.outputText, { exports: compiledModule }, { timeout: 1000 });

  function fixture({ name = '  After  ', onRename = async () => {} } = {}) {
    const lock = { current: false };
    const busyUpdates = [];
    let closed = 0;
    const close = () => { closed++; };
    const args = config.folder
      ? [name, config.oldName, 12, lock, busy => busyUpdates.push(busy), onRename, close, close]
      : [name, config.oldName, lock, busy => busyUpdates.push(busy), onRename, close, close];
    return {
      lock, busyUpdates,
      submit: compiledModule.makeSubmit(...args),
      get closed() { return closed; },
    };
  }

  test(config.file + ' blocks repeated async rename events before React can rerender', async () => {
    let release;
    const pending = new Promise(resolve => { release = resolve; });
    const calls = [];
    const f = fixture({ onRename: async (...args) => { calls.push(args); await pending; } });
    const first = f.submit();
    const duplicate = f.submit();
    assert.equal(calls.length, 1);
    assert.equal(f.lock.current, true);
    assert.equal(f.closed, 0);
    assert.equal(calls[0].at(-1), 'After');
    if (config.folder) assert.deepEqual(calls[0].slice(0, 2), [12, config.oldName]);
    release();
    await Promise.all([first, duplicate]);
    assert.equal(f.closed, 1);
    assert.equal(f.lock.current, false);
    assert.deepEqual(f.busyUpdates, [true, false]);
  });

  test(config.file + ' recovers from a failed rename and permits a new attempt', async () => {
    let calls = 0;
    const f = fixture({ onRename: async () => {
      if (++calls === 1) throw new Error('rename rejected');
    } });
    await f.submit();
    assert.equal(f.closed, 0);
    assert.equal(f.lock.current, false);
    await f.submit();
    assert.equal(calls, 2);
    assert.equal(f.closed, 1);
    assert.deepEqual(f.busyUpdates, [true, false, true, false]);
  });

  test(config.file + ' prevents dismissing the dialog during pending rename', () => {
    const closeStart = source.indexOf('    const closeIfIdle = () => {');
    const closeEnd = source.indexOf('\n    };', closeStart);
    assert.ok(closeStart >= 0 && closeEnd > closeStart);
    const body = source.slice(closeStart, closeEnd + '\n    };'.length);
    const compiledClose = ts.transpileModule(
      'function makeClose(submittingRef, onClose) {\n' + body +
      '\nreturn closeIfIdle;\n}\nexports.makeClose = makeClose;',
      { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
    );
    const mod = {};
    vm.runInNewContext(compiledClose.outputText, { exports: mod }, { timeout: 1000 });
    const lock = { current: true };
    let closes = 0;
    const close = mod.makeClose(lock, () => closes++);
    close();
    assert.equal(closes, 0);
    lock.current = false;
    close();
    assert.equal(closes, 1);
    assert.match(source, /onClick=\{closeIfIdle\}/);
    assert.match(source, /aria-busy=\{isSubmitting\}/);
  });

  test('negative: ' + config.file + ' requires the immediate async rename guard', () => {
    assert.match(source, /if \(submittingRef\.current\) return;/);
    assert.match(source, /submittingRef\.current = true;/);
    const modified = source.replace('submittingRef.current = true;', '');
    assert.doesNotMatch(modified, /submittingRef\.current = true;/);
  });
}


// Exercise the shared Tab implementation and the actual scoped rename handlers.
// No application instance, real Telegram account, or file rename is invoked.
const renameKeyboardSource = fs.readFileSync(path.join(__dirname, '../app/src/dialogKeyboard.ts'), 'utf8');
const renameKeyboardCompiled = ts.transpileModule(renameKeyboardSource,
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } });
const renameKeyboardExports = {};
vm.runInNewContext(renameKeyboardCompiled.outputText, { exports: renameKeyboardExports }, { timeout: 1000 });
const renameKeyboardFactory = (dialogRef, doc) =>
  event => renameKeyboardExports.containDialogTab(event, dialogRef.current, doc.activeElement);

for (const file of ['RenameFileModal.tsx', 'RenameFolderModal.tsx']) {
  const source = read(file);
  test(file + ' contains Tab and Escape in the dialog and restores its connected opener', () => {
    assert.match(source, /ref=\{dialogRef\}\s+tabIndex=\{-1\}\s+onKeyDown=\{handleDialogKeyDown\}/);
    assert.match(source, /containDialogTab\(event, dialogRef\.current, document\.activeElement\)/);
    assert.match(source, /previousFocus\?\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/);
  });

  test(file + ' exposes reverse and forward keyboard wrapping with the real shared helper', () => {
    const reverse = panelFixture({ factory: renameKeyboardFactory });
    assert.equal(reverse.press('Tab', true).focus, reverse.controls[2]);
    const forward = panelFixture({ initial: 'last', factory: renameKeyboardFactory });
    assert.equal(forward.press('Tab').focus, forward.controls[0]);
    const middle = panelFixture({ initial: 'middle', factory: renameKeyboardFactory });
    assert.equal(middle.press('Tab').prevented, 0);
  });

  test(file + ' ignores hidden buttons and contains Tab when all controls disappear', () => {
    const hidden = panelFixture({ visible: [true, true, false], factory: renameKeyboardFactory });
    assert.equal(hidden.press('Tab', true).focus, hidden.controls[1]);
    const none = panelFixture({ visible: [false, false, false], factory: renameKeyboardFactory });
    assert.equal(none.press('Tab').focus, none.panel);
    const outside = panelFixture({ initial: 'outside', factory: renameKeyboardFactory });
    assert.equal(outside.press('Tab').focus, outside.controls[0]);
  });

  test(file + ' actual dialog key handler prevents Escape and refuses action during pending rename', () => {
    const start = source.indexOf('    const handleDialogKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {');
    const end = source.indexOf('\n    };', start);
    assert.ok(start >= 0 && end > start);
    const body = source.slice(start, end + '\n    };'.length);
    const compiled = ts.transpileModule(
      'function makeHandler(closeIfIdle, containDialogTab, dialogRef, document) {\n' + body +
      '\nreturn handleDialogKeyDown;\n}\nexports.makeHandler = makeHandler;',
      { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
    );
    const mod = {};
    vm.runInNewContext(compiled.outputText, { exports: mod }, { timeout: 1000 });
    let closed = 0, prevent = 0, stop = 0, trapped = 0;
    const lock = { current: true };
    const closeIfIdle = () => { if (!lock.current) closed++; };
    const keydown = mod.makeHandler(closeIfIdle, () => { trapped++; }, { current: {} }, { activeElement: null });
    const escape = { key: 'Escape', preventDefault() { prevent++; }, stopPropagation() { stop++; } };
    keydown(escape);
    assert.equal(prevent, 1);
    assert.equal(stop, 1);
    assert.equal(trapped, 0);
    assert.equal(closed, 0);
    lock.current = false;
    keydown(escape);
    assert.equal(closed, 1);
    keydown({ key: 'Tab', preventDefault() {}, stopPropagation() {} });
    assert.equal(trapped, 1);
  });

  test(file + ' focus cleanup restores only a still-attached opener', () => {
    const start = source.indexOf('    useEffect(() => {');
    const end = source.indexOf('\n    }, []);', start);
    assert.ok(start >= 0 && end > start, 'Current rename mount focus effect is present');
    const body = source.slice(start, end + '\n    }, []);'.length);
    const compiled = ts.transpileModule(
      'function mountFocus(document, HTMLElement, inputRef, requestAnimationFrame) {\n' +
      'let cleanup; function useEffect(fn) { cleanup = fn(); }\n' + body +
      '\nreturn cleanup;\n}\nexports.mountFocus = mountFocus;',
      { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
    );
    const mod = {};
    vm.runInNewContext(compiled.outputText, { exports: mod }, { timeout: 1000 });
    class Element {
      constructor() { this.isConnected = true; this.focused = 0; }
      focus() { this.focused++; }
    }
    for (const attached of [true, false]) {
      const opener = new Element();
      const input = new Element();
      input.select = () => {};
      const frames = [];
      const cleanup = mod.mountFocus(
        { activeElement: opener, body: new Element() }, Element, { current: input },
        fn => { frames.push(fn); },
      );
      assert.equal(input.focused, 1);
      opener.isConnected = attached;
      cleanup();
      assert.equal(frames.length, 1);
      frames[0]();
      assert.equal(opener.focused, attached ? 1 : 0);
    }
  });

  test('negative: ' + file + ' requires Tab containment and safe opener return', () => {
    const withoutTrap = source.replace('onKeyDown={handleDialogKeyDown}', '');
    assert.doesNotMatch(withoutTrap, /onKeyDown=\{handleDialogKeyDown\}/);
    const withoutReturn = source.replace('previousFocus.focus({ preventScroll: true })', '');
    assert.doesNotMatch(withoutReturn, /previousFocus\?\.isConnected\) previousFocus\.focus\(\{ preventScroll: true \}\)/);
  });
}
