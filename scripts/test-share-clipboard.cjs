const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');

const root = path.join(__dirname, '../app/src');
const utils = fs.readFileSync(path.join(root, 'utils.ts'), 'utf8');
const start = utils.indexOf('export async function nativeShareOrCopy(');
const end = utils.indexOf('\nexport function showFileDialogFallback(', start);
assert.ok(start >= 0 && end > start, 'Extract the real nativeShareOrCopy implementation');
const compiled = ts.transpileModule(utils.slice(start, end), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
}).outputText;

function fixture({ share, clipboard } = {}) {
  const successes = [];
  const errors = [];
  const writes = [];
  const nav = {};
  if (share) nav.share = share;
  const write = clipboard || (async link => { writes.push(link); });
  const exports = {};
  vm.runInNewContext(compiled, {
    exports,
    navigator: nav,
    copyToClipboard: write,
    toast: { success: msg => successes.push(msg), error: msg => errors.push(msg) },
  }, { timeout: 1200 });
  return { invoke: exports.nativeShareOrCopy, successes, errors, writes };
}

test('native share success does not copy a link or show copy feedback', async () => {
  const f = fixture({ share: async () => {} });
  await f.invoke('test.txt', '1 KB', 'https://example.test/link');
  assert.equal(f.successes.length, 0);
  assert.equal(f.errors.length, 0);
  assert.deepEqual(f.writes, []);
});

test('native share cancellation never secretly copies the link', async () => {
  const f = fixture({ share: async () => { throw { name: 'AbortError' }; } });
  await f.invoke('test.txt', '1 KB', 'https://example.test/link');
  assert.deepEqual(f.writes, []);
  assert.equal(f.successes.length, 0);
  assert.equal(f.errors.length, 0);
});

test('share failure copies only after successful fallback', async () => {
  const f = fixture({ share: async () => { throw new Error('share unavailable'); } });
  await f.invoke('test.txt', '1 KB', 'https://example.test/link');
  assert.deepEqual(f.writes, ['https://example.test/link']);
  assert.equal(f.successes.length, 1);
  assert.equal(f.errors.length, 0);
});

test('delayed copy callback cannot trigger premature copied feedback', async () => {
  let resolve;
  const pending = new Promise(r => { resolve = r; });
  const f = fixture();
  let value;
  const task = f.invoke('test.txt', '1 KB', 'https://example.test/link', async link => {
    value = link;
    await pending;
  });
  await Promise.resolve();
  assert.equal(value, 'https://example.test/link');
  assert.equal(f.successes.length, 0);
  resolve();
  await task;
  assert.equal(f.successes.length, 1);
});

test('rejected copy callback reports failure and never copied success', async () => {
  const f = fixture();
  await f.invoke('test.txt', '1 KB', 'https://example.test/link', async () => {
    throw new Error('Permission denied');
  });
  assert.equal(f.successes.length, 0);
  assert.equal(f.errors.length, 1);
});

test('clipboard write rejection reports failure with no copied toast', async () => {
  const f = fixture({ clipboard: async () => { throw new Error('blocked'); } });
  await f.invoke('test.txt', '1 KB', 'https://example.test/link');
  assert.equal(f.successes.length, 0);
  assert.equal(f.errors.length, 1);
});

test('source guards label copy buttons and share link inputs', () => {
  const share = fs.readFileSync(path.join(root, 'components/desktop/dashboard/ShareDialog.tsx'), 'utf8');
  const desktop = fs.readFileSync(path.join(root, 'components/desktop/DesktopDashboard.tsx'), 'utf8');
  assert.ok(share.includes('htmlFor="tr-share-link-value"'));
  assert.ok(share.includes('id="tr-share-link-value"'));
  assert.ok(share.includes("aria-label={copied ? 'Copied share link' : t('common.copy_share_link')}"));
  assert.ok(share.includes('await copyToClipboard(link)'));
  assert.ok(share.includes('role="alert"'));
  assert.ok(desktop.includes('await copyToClipboard(link)'));
  assert.ok(desktop.includes('aria-label={isCopied ?'));
  assert.ok(desktop.includes('Copy share link for'));
});

test('negative: replacing awaited clipboard with fire-and-forget fails contract', () => {
  const share = fs.readFileSync(path.join(root, 'components/desktop/dashboard/ShareDialog.tsx'), 'utf8');
  const altered = share.replace('await copyToClipboard(link);', 'void copyToClipboard(link);');
  assert.notEqual(share, altered);
  assert.ok(!altered.includes('await copyToClipboard(link);'));
});

// Execute the current ShareDialog handleGenerate TypeScript body, not a
// parallel reimplementation, without starting a Tauri app or using credentials.
const shareSource = fs.readFileSync(path.join(root, 'components/desktop/dashboard/ShareDialog.tsx'), 'utf8');
const generateStart = shareSource.indexOf('    const handleGenerate = async () => {');
const generateEnd = shareSource.indexOf('\n    };', generateStart);
assert.ok(generateStart >= 0 && generateEnd > generateStart, 'Share link generation handler must exist');
const generateBody = shareSource.slice(generateStart, generateEnd + '\n    };'.length);
const generateCompiled = ts.transpileModule(
  'function makeGenerate(expiryType, customHours, requirePassword, password, generatePendingRef, setLoading, setError, invoke, folderId, file, setShareInfo) {\n' +
  generateBody + '\nreturn handleGenerate;\n}\nexports.makeGenerate = makeGenerate;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } },
);
const generateModule = {};
vm.runInNewContext(generateCompiled.outputText, { exports: generateModule }, { timeout: 1000 });

function shareGenerateFixture({
  expiryType = '1d', customHours = '24', requirePassword = false,
  password = '', invoke = async () => ({ id: 'qa-only-link' }),
} = {}) {
  const pending = { current: false };
  const loads = [], errors = [], shares = [];
  const calls = [];
  const run = generateModule.makeGenerate(
    expiryType, customHours, requirePassword, password, pending,
    value => loads.push(value),
    value => errors.push(value),
    async (...args) => {
      calls.push(args);
      return invoke(...args);
    },
    null, { id: 71, name: 'synthetic-qa.txt', size: 1234 },
    value => shares.push(value),
  );
  return { run, pending, loads, errors, shares, calls };
}

test('requested password protection never creates an unprotected share', async () => {
  for (const password of ['', '   ', '\t']) {
    const f = shareGenerateFixture({ requirePassword: true, password });
    await f.run();
    assert.equal(f.calls.length, 0, 'no command must execute for blank password');
    assert.equal(f.shares.length, 0);
    assert.match(f.errors.at(-1), /Enter a password/);
    assert.equal(f.pending.current, false);
    assert.deepEqual(f.loads, [true, false]);
  }
});

test('protected share forwards its exact nonblank password to the backend', async () => {
  const f = shareGenerateFixture({ requirePassword: true, password: ' secure phrase ' });
  await f.run();
  assert.equal(f.errors.at(-1), null);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0][0], 'cmd_create_share');
  assert.equal(f.calls[0][1].password, ' secure phrase ');
  assert.equal(f.calls[0][1].expiryHours, 24);
  assert.equal(f.shares.length, 1);
});

test('deliberately unprotected share sends null rather than stale password', async () => {
  const f = shareGenerateFixture({ requirePassword: false, password: 'previous text' });
  await f.run();
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0][1].password, null);
});

test('custom expiry accepts positive integer hours and refuses malformed values before IPC', async () => {
  for (const input of ['0', '-1', '1.5', '1e3', '1abc', '', '9007199254740992']) {
    const f = shareGenerateFixture({ expiryType: 'custom', customHours: input });
    await f.run();
    assert.equal(f.calls.length, 0, 'invalid hours: ' + input);
    assert.match(f.errors.at(-1), /positive whole number/);
  }
  const valid = shareGenerateFixture({ expiryType: 'custom', customHours: ' 0024 ' });
  await valid.run();
  assert.equal(valid.calls[0][1].expiryHours, 24);
});

test('ShareDialog ref prevents two same-tick share link creations', async () => {
  let release;
  const waiting = new Promise(resolve => { release = resolve; });
  const f = shareGenerateFixture({ invoke: async () => { await waiting; return { id: 'qa-only-link' }; } });
  const first = f.run();
  const second = f.run();
  assert.equal(f.calls.length, 1);
  assert.equal(f.pending.current, true);
  release();
  await Promise.all([first, second]);
  assert.equal(f.calls.length, 1);
  assert.equal(f.pending.current, false);
  assert.deepEqual(f.loads, [true, false]);
  assert.equal(f.shares.length, 1);
});

test('ShareDialog error leaves generation retryable and never displays a created link', async () => {
  let attempts = 0;
  const f = shareGenerateFixture({ invoke: async () => {
    if (++attempts === 1) throw new Error('synthetic write refused');
    return { id: 'qa-only-link' };
  } });
  await f.run();
  assert.equal(f.shares.length, 0);
  assert.match(f.errors.at(-1), /synthetic write refused/);
  assert.equal(f.pending.current, false);
  await f.run();
  assert.equal(attempts, 2);
  assert.equal(f.shares.length, 1);
  assert.equal(f.pending.current, false);
});

test('share creation uses a protected dismiss guard and an accessible error alert', () => {
  const required = [
    'if (generatePendingRef.current) return;',
    'generatePendingRef.current = true;',
    'generatePendingRef.current = false;',
    'useEscapeToClose(true, closeIfIdle, loading)',
    'role="alert"',
    'aria-busy={loading}',
    'onClick={closeIfIdle}',
  ];
  for (const marker of required) assert.ok(shareSource.includes(marker), marker);
  assert.ok(!shareSource.replace('generatePendingRef.current = true;', '').includes('generatePendingRef.current = true;'));
});
