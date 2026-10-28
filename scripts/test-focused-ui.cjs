const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { createRequire } = require('node:module');
const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
const React = appRequire('react');
const { renderToStaticMarkup } = appRequire('react-dom/server');
const assert = require('node:assert/strict');
const ipc = {
  invoke: async () => null,
  getCurrentWindow: () => ({ listen: async () => () => {}, onResized: async () => () => {}, isFullscreen: async () => false }),
  type: () => 'linux',
  writeText: async () => {},
};
function loader(overrides = {}) {
  const cache = new Map();
  function load(file) {
    file = path.resolve(file);
    if (cache.has(file)) return cache.get(file).exports;
    const mod = { exports: {} };
    cache.set(file, mod);
    const source = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
      compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022 },
    }).outputText;
    function req(name) {
      if (name in overrides) return overrides[name];
      if (name.startsWith('@tauri-apps/')) return ipc;
      if (name.startsWith('.')) {
        const base = path.resolve(path.dirname(file), name);
        const target = [base, base + '.ts', base + '.tsx', path.join(base, 'index.ts')].find(p => fs.existsSync(p) && fs.statSync(p).isFile());
        return load(target);
      }
      return appRequire(name);
    }
    vm.runInThisContext('(function(require,module,exports){' + source + '\n})', { filename: file })(req, mod, mod.exports);
    return mod.exports;
  }
  return load;
}
const load = loader();
const utils = load(path.join(root, 'app/src/utils.ts'));
let failures = 0;
const pending = [];
function test(name, fn) {
  try {
    const result = fn();
    if (result?.then) pending.push(result.then(() => console.log('PASS ' + name), e => { failures++; console.error('FAIL ' + name + ': ' + e.message); }));
    else console.log('PASS ' + name);
  }
  catch (e) { failures++; console.error('FAIL ' + name + ': ' + e.message); }
}
const queueProps = {
  onCancelAll() {}, onCancelItem() {}, onPauseItem() {}, onResumeItem() {}, onRetryItem() {},
};

test('isolated real Telegram QA keeps API hash ephemeral when secure storage is unavailable', () => {
  const authUi = fs.readFileSync(path.join(root, 'app/src/components/shared/AuthWizard.tsx'), 'utf8');
  const authRust = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/auth.rs'), 'utf8');
  assert.ok(authUi.includes("VITE_TERA_REAL_E2E_QA"), 'QA-only real Telegram login gate is missing');
  assert.ok(authUi.includes("cmd_auth_request_code_ephemeral"), 'QA login does not use the ephemeral backend command');
  assert.ok(authUi.includes("!secureStoreAvailable && !qaEphemeralCredentialsAllowed"), 'Production keyring requirement was weakened instead of adding a QA-only exception');
  assert.ok(authRust.includes("cmd_auth_request_code_ephemeral"), 'Ephemeral backend command is missing');
  assert.match(authRust, /request_login_code_inner\([\s\S]*?false\)\.await/, 'Ephemeral login must not persist the API hash');
});

test('isolated real Telegram QA can prime TDLib credentials without logging out', () => {
  const fastUi = fs.readFileSync(path.join(root, 'app/src/context/FastTransferAuthContext.tsx'), 'utf8');
  const tdlib = fs.readFileSync(path.join(root, 'app/src-tauri/src/tdlib_fast.rs'), 'utf8');
  assert.ok(fastUi.includes("VITE_TERA_REAL_E2E_QA"), 'TDLib QA credential recovery is not gated to real-E2E dev mode');
  assert.ok(fastUi.includes("cmd_fast_transfer_prepare_qa_credentials"), 'TDLib cannot accept a one-time QA API hash from the active session');
  assert.ok(fastUi.includes("api_hash"), 'TDLib QA modal has no API-hash-only step');
  assert.ok(tdlib.includes("save_qa_kernel_api_credentials"), 'TDLib QA credentials are not persisted in the kernel keyring');
});

test('Telegram API_ID_INVALID returns the auth wizard to API credentials', () => {
  const authUi = fs.readFileSync(path.join(root, 'app/src/components/shared/AuthWizard.tsx'), 'utf8');
  assert.ok(authUi.includes("message.includes('API_ID_INVALID')"), 'API_ID_INVALID is not handled explicitly');
  assert.ok(authUi.includes("setStep('setup')"), 'Invalid API credentials do not return to setup');
  assert.ok(authUi.includes('Telegram rejected this API ID / API Hash pair.'), 'Invalid API credentials lack a clear user-facing message');
});
for (const [name, bytesKey, status] of [['UploadQueue', 'uploadedBytes', 'uploading'], ['DownloadQueue', 'downloadedBytes', 'downloading']]) {
  test(name + ' does not present missing throughput updates as stopped transfer', () => {
    const component = load(path.join(root, 'app/src/components/desktop/dashboard/' + name + '.tsx'))[name];
    const html = renderToStaticMarkup(React.createElement(component, {
      ...queueProps,
      items: [{ id: 'qa', path: 'fixture.bin', filename: 'fixture.bin', folderId: null, status, progress: 20, [bytesKey]: 200, totalBytes: 1000, speedBytesPerSec: 0 }],
    }));
    assert.ok(!html.includes('>0 B/s<'), 'Active transfer renders 0 B/s');
    assert.ok(html.includes('200 B / 1 KB'), 'Confirmed file bytes changed');
  });
}
test('transfer queues expose named actions, determinate progress and completion/error announcements', () => {
  for (const [name, verb, bytesKey, fileKey, activeStatus] of [
    ['UploadQueue', 'Upload', 'uploadedBytes', 'path', 'uploading'],
    ['DownloadQueue', 'Download', 'downloadedBytes', 'filename', 'downloading'],
  ]) {
    const Component = load(path.join(root, 'app/src/components/desktop/dashboard/' + name + '.tsx'))[name];
    const filename = 'qa-accessible-transfer.bin';
    const render = (status, bytes, progress = 20) => renderToStaticMarkup(React.createElement(Component, {
      ...queueProps,
      onDismissItem() {},
      items: [{
        id: 'qa-accessible', path: filename, filename, folderId: null, status,
        progress, [bytesKey]: bytes, totalBytes: 600, speedBytesPerSec: 1, error: status === 'error' ? 'QA transfer failed' : undefined,
      }],
    }));
    for (const status of [activeStatus, 'pending', 'paused', 'pausing', 'error', 'cancelled', 'success']) {
      const html = render(status, 120);
      assert.ok(html.includes('role="list"'), name + ' list lacks its landmark');
      assert.ok(html.includes('role="listitem"'), name + ' item lacks list identity');
      const buttons = [...html.matchAll(/<button\b[^>]*>[\s\S]*?<\/button>/g)].map(match => match[0]);
      assert.ok(buttons.length > 0, name + ' ' + status + ' had no controls');
      for (const button of buttons) {
        const plainText = button.replace(/<[^>]*>/g, '').trim();
        assert.ok(button.includes('aria-label=') || plainText.length > 0,
          name + ' ' + status + ' has an unnamed keyboard action');
      }
      if (status === activeStatus || status === 'paused' || status === 'pausing') {
        assert.ok(html.includes('role="progressbar"'), name + ' lacks screen-reader progress');
        assert.ok(html.includes('aria-valuenow="20"'), name + ' misreports 120/600 bytes');
        assert.ok(html.includes('aria-valuemin="0"') && html.includes('aria-valuemax="100"'));
        assert.ok(html.includes(verb + ' progress for ' + filename));
      }
      if (status === 'error') assert.ok(html.includes('role="alert"'), name + ' does not announce errors');
      if (status === 'success' || status === 'cancelled') {
        assert.ok(html.includes('role="status"'), name + ' does not announce its terminal state');
      }
    }
  }
});

test('adaptive video player exposes named modal controls and protects native widget keys', () => {
  const playerLoad = loader({
    '../../../hooks/useAdaptiveStreaming': {
      useAdaptiveStreaming: () => ({
        videoRef: { current: null }, phase: 'loading', error: null,
        tracks: [], loadProgress: 10, currentQuality: 'original', setQuality() {},
        adaptiveMode: false, setAdaptiveMode() {}, measuredKbps: 0,
        useFallback: true, fallbackUrl: 'http://localhost/fixture.mp4', abort() {},
      }),
    },
  });
  const player = playerLoad(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx')).AdaptiveMediaPlayer;
  const html = renderToStaticMarkup(React.createElement(player, {
    file: { id: 1, name: 'qa-accessible.mp4', size: 123, type: 'video' },
    activeFolderId: null, streamUrl: 'http://localhost/fixture.mp4', onClose() {},
  }));
  assert.ok(html.includes('role="dialog"') && html.includes('aria-modal="true"'));
  assert.ok(html.includes('Video player: qa-accessible.mp4'), 'Dialog has no file-scoped accessible name');
  assert.ok(html.includes('aria-label="Close video player"'));
  assert.ok(html.includes('aria-label="Enter fullscreen"'));
  assert.ok(html.includes('aria-label="Video: qa-accessible.mp4"'));
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx'), 'utf8');
  assert.ok(source.includes("target?.tagName === 'SELECT'"), 'Arrow navigation still steals select controls');
  assert.ok(source.includes("e.key === ' '"), 'Native button spacebar handling is unverified');
  assert.ok(source.includes("event.key !== 'Tab'"), 'Modal focus trap is missing');
  assert.ok(source.includes('previous.isConnected) previous.focus()'), 'Dialog does not restore prior focus');
});

test('multipart video opens the actual preview handler', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  const ast = ts.createSourceFile('Dashboard.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let initializer;
  function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(ast) === 'handlePreview') initializer = node.initializer.getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  const state = {};
  const file = { id: 7, name: 'fixture.mkv', size: 123, type: 'video', is_split: true };
  const context = {
    ...utils, displayedFiles: [file], toast: { info() {} },
    ...Object.fromEntries(['PreviewContextFiles','PreviewContextIndex','ArchiveViewFile','PreviewFile','PlayingFile','PdfFile'].map(name => ['set' + name, value => { state[name] = value; }])),
  };
  const js = ts.transpileModule('const preview = ' + initializer + '; preview;', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  vm.runInNewContext(js, context)(file);
  assert.equal(state.PlayingFile?.id, 7, 'Split video was blocked instead of opening the player');
});
test('native fallback is not obstructed by an inactive MP4 parser error', () => {
  const playerLoad = loader({
    '../../../hooks/useAdaptiveStreaming': {
      useAdaptiveStreaming: () => ({
        videoRef: { current: null }, phase: 'error', error: 'fixture parser failed',
        tracks: [], loadProgress: 0, currentQuality: 'original', setQuality() {},
        adaptiveMode: false, setAdaptiveMode() {}, measuredKbps: 0,
        useFallback: true, fallbackUrl: 'http://localhost/fixture.mp4', abort() {},
      }),
    },
  });
  const player = playerLoad(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx')).AdaptiveMediaPlayer;
  const html = renderToStaticMarkup(React.createElement(player, {
    file: { id: 1, name: 'fixture.mp4', size: 123, type: 'video' },
    activeFolderId: null, streamUrl: 'http://localhost/fixture.mp4', onClose() {},
  }));
  assert.ok(html.includes('<video'), 'Native playback element missing');
  assert.ok(!html.includes('Playback Error'), 'Parser error competes with the active native video');
});
test('video keys are owned by the adaptive player; audio navigation remains available', () => {
  for (const [name, expected] of [['fixture.mp4', 0], ['fixture.mp3', 1]]) {
    const effects = [], listeners = [];
    const fakeReact = {
      ...React,
      useState: value => [typeof value === 'function' ? value() : value, () => {}],
      useRef: value => ({ current: value }),
      useMemo: fn => fn(), useCallback: fn => fn, useEffect: fn => effects.push(fn),
    };
    global.window = { addEventListener: (event, fn) => { if (event === 'keydown') listeners.push(fn); }, removeEventListener() {} };
    const playerLoad = loader({ react: fakeReact });
    const player = playerLoad(path.join(root, 'app/src/components/desktop/dashboard/MediaPlayer.tsx')).MediaPlayer;
    player({ file: { id: 1, name, size: 1, type: 'file' }, onClose() {}, activeFolderId: null });
    for (const effect of effects) effect();
    assert.equal(listeners.length, expected, name + ' has an unexpected parent keyboard handler');
  }
  delete global.window;
});

test('stream source changes recompute native fallback without freezing the first container', () => {
  const cells = [];
  let cursor = 0;
  const fakeReact = { ...React,
    useRef: value => { const i = cursor++; return cells[i] ??= { current: value }; },
    useState: value => { const i = cursor++; if (!(i in cells)) cells[i] = typeof value === 'function' ? value() : value; return [cells[i], next => { cells[i] = typeof next === 'function' ? next(cells[i]) : next; }]; },
    useMemo: fn => fn(), useCallback: fn => fn, useEffect() {},
  };
  const previous = global.MediaSource;
  global.MediaSource = { isTypeSupported: () => true };
  try {
    const hookLoad = loader({ react: fakeReact, './useStreamingSettings': {
      useStreamingSettings: () => ({ settings: { quality: 'original', adaptiveMode: false }, setQuality() {}, setAdaptiveMode() {} }),
    } });
    const hook = hookLoad(path.join(root, 'app/src/hooks/useAdaptiveStreaming.ts')).useAdaptiveStreaming;
    assert.equal(hook('http://localhost/stream/1/7', 'fixture.mkv').useFallback, true);
    cursor = 0;
    assert.equal(hook('http://localhost/stream/1/8', 'fixture.mp4').useFallback, false);
    cursor = 0;
    assert.equal(hook('http://localhost/fmp4/8/output.mp4', 'fixture.mp4', undefined, true).useFallback, true, 'Growing remux must use native streaming');
  } finally { global.MediaSource = previous; }
});
test('progressive MP4 cache warming skips small files rather than racing the native parser', () => {
  const hook = loader()(path.join(root, 'app/src/hooks/useAdaptiveStreaming.ts'));
  const select = hook.progressiveMoovWarmRange;
  assert.equal(typeof select, 'function', 'progressive cache warm selector was not exported');
  assert.equal(select(3_667_074), null,
    'Generated 3.6MB MP4 should not spawn a second almost-full HTTP download');
  assert.equal(select(6_291_456), null,
    'A file no larger than 2x cache warm bytes does not benefit from prefetch');
  assert.equal(select(0), null);
  assert.equal(select(Number.NaN), null);
  assert.equal(select(60_000_000), 'bytes=56854272-');
});

test('ordinary progressive MP4 gets native playback before remux', () => {
  const effects = [], cleanups = [];
  const parser = { stop() {}, flush() {} };
  const previous = global.MediaSource;
  global.MediaSource = { isTypeSupported: () => true };
  let remuxRequests = 0;
  try {
    const fakeReact = { ...React,
      useState: value => [typeof value === 'function' ? value() : value, () => {}],
      useRef: value => ({ current: value }), useMemo: fn => fn(), useCallback: fn => fn,
      useEffect: fn => effects.push(fn),
    };
    const hookLoad = loader({
      react: fakeReact,
      mp4box: { createFile: () => parser },
      './useStreamingSettings': { useStreamingSettings: () => ({ settings: { quality: 'original', adaptiveMode: false }, setQuality() {}, setAdaptiveMode() {} }) },
      './moovCache': { extractCacheKey: () => 'qa', getCachedMoov: () => new Promise(() => {}), setCachedMoov: async () => {} },
    });
    const hook = hookLoad(path.join(root, 'app/src/hooks/useAdaptiveStreaming.ts')).useAdaptiveStreaming;
    hook('http://localhost/stream/1/7', 'fixture.mp4', () => { remuxRequests++; });
    for (const effect of effects) { const cleanup = effect(); if (cleanup) cleanups.push(cleanup); }
    parser.onReady({ tracks: [], duration: 10, timescale: 1, isFragmented: false });
    assert.equal(remuxRequests, 0, 'A supported progressive file started remux before native playback');
  } finally {
    for (const cleanup of cleanups) cleanup();
    global.MediaSource = previous;
  }
});


function remuxHarness(options = {}) {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx'), 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let callback;
  function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(ast) === 'handleProgressiveDetected') callback = node.initializer.arguments[0].getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  const state = { polls: 0, reloads: 0, url: null, error: null, autoPlay: true };
  const context = {
    file: { id: 7 }, activeFolderId: 1, fmp4RemuxingRef: { current: false },
    clearMemoryMedia() { state.memoryCleared = true; },
    transcodeCapsRef: { current: { available: true } }, remuxGenerationRef: { current: 0 },
    selectedAudioStreamRef: { current: 2 }, streamBaseRef: { current: 'http://localhost' },
    streamAuthRef: { current: 'qa' }, abortMseRef: { current() {} }, logRef: { current() {} },
    savedTimeRef: { current: 0 }, remuxWindowRef: { current: { start: 0, complete: false, key: null } },
    pendingSeekRef: { current: null }, nativePlaybackStartedRef: { current: true },
    nativeUserPauseRef: { current: !!options.paused },
    nativeCompletedRetryRef: { current: 0 }, setMovieTime() {},
    setNativeDecoderRevision: () => {},
    fallbackVideoRef: { current: {
      currentTime: 3.5, paused: !!options.paused,
      pause() { state.oldPaused = true; },
      removeAttribute(name) { if (name === 'src') state.oldSrcDetached = true; },
      load() { state.oldDecoderUnloaded = true; },
    } },
    setFmp4Remuxing() {}, setFmp4RemuxError: value => { state.error = value; },
    setNativeAutoPlay: value => { state.autoPlay = value; },
    setFmp4StreamUrl: value => { state.url = value; }, setRestartNonce: () => { state.reloads++; },
    shouldResumeAfterRemuxFinish: load(path.join(root,
      'app/src/components/desktop/dashboard/playbackTimeline.tsx')).shouldResumeAfterRemuxFinish,
    buildAuthenticatedRemuxUrl: load(path.join(root,
      'app/src/components/desktop/dashboard/playbackTimeline.tsx')).buildAuthenticatedRemuxUrl,
    setTimeout: callback => { callback(); return 1; },
    invoke: async name => {
      if (name === 'cmd_prepare_fmp4_stream') return { status: 'processing', url: '/fmp4/qa/output.mp4', output_file_key: 'qa' };
      state.polls++;
      return options.status ? options.status(state.polls, context) : { status: 'ready', error: null, complete: state.polls > 1 };
    },
  };
  const js = ts.transpileModule('const prepare = ' + callback + '; prepare;', { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  const start = vm.runInNewContext(js, context);
  return { state, context, start };
}
test('early remux playback keeps polling and switches to final seekable output', async () => {
  const { state, context, start } = remuxHarness();
  start();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(state.polls, 2, 'First playable fragment was confused with final completion');
  assert.ok(state.url.includes('/fmp4/qa/output.mp4'));
  assert.equal(state.reloads, 2, 'Final ranged source did not replace the growing source');
  assert.equal(state.oldPaused, true, 'Growing source was not paused before completed source startup');
  assert.equal(state.oldSrcDetached, true, 'Growing source URI was not detached before replacement');
  assert.equal(state.oldDecoderUnloaded, true, 'Growing decoder was left active alongside completed decoder');
  assert.equal(context.savedTimeRef.current, 3.5, 'Completion reload lost playback position');
  assert.equal(context.fmp4RemuxingRef.current, false);
});
test('healthy partial playback waits beyond the startup timeout for the final file', async () => {
  const { state, start } = remuxHarness({ status: poll =>
    poll === 1 || poll === 605 ? { status: 'ready', complete: poll === 605, error: null }
      : { status: 'processing', complete: false, error: null },
  });
  start();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(state.polls, 605, 'Completion polling stopped during healthy playback');
  assert.equal(state.error, null);
  assert.equal(state.reloads, 2);
});
test('a source change during a remux status request discards the stale result', async () => {
  let resolveStatus;
  const { state, context, start } = remuxHarness({ status: () => new Promise(resolve => { resolveStatus = resolve; }) });
  start();
  await new Promise(resolve => setImmediate(resolve));
  context.remuxGenerationRef.current++;
  context.fmp4RemuxingRef.current = true; // The new source owns its active job.
  resolveStatus({ status: 'ready', complete: true, error: null });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(state.url, null, 'Stale status replaced the newly selected source');
  assert.equal(state.reloads, 0);
  assert.equal(context.fmp4RemuxingRef.current, true, 'Stale job cleared the new job guard');
});
test('the final ranged-file transition preserves a user-paused video', async () => {
  const { state, start } = remuxHarness({ paused: true });
  start();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(state.reloads, 2);
  assert.equal(state.autoPlay, false, 'Completion automatically resumed a paused video');
});


test('download progress shows confirmed bytes without an independent moving overlay', () => {
  const { DownloadQueue } = load(path.join(root, 'app/src/components/desktop/dashboard/DownloadQueue.tsx'));
  const html = renderToStaticMarkup(React.createElement(DownloadQueue, {
    ...queueProps, items: [{ id: 'qa', filename: 'fixture.mkv', status: 'downloading',
      progress: 90, downloadedBytes: 100, totalBytes: 1000, speedBytesPerSec: 200 }],
  }));
  assert.ok(html.includes('width:10%'), 'Bar percentage disagrees with confirmed byte count');
  assert.ok(!html.includes('animate-progress-indeterminate'), 'Decorative animation advances independently of downloaded bytes');
});
test('unsupported EAC3 audio starts the compatibility fallback once FFmpeg is available', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx'), 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let effect;
  function visit(node) {
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect'
      && node.arguments[0].getText(ast).includes('audio/mp4; codecs="ec-3"')) effect = node.arguments[0].getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(effect, 'Known unsupported EAC3 audio can stay on a black native player');
  const js = ts.transpileModule('const check = ' + effect + '; check;', { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  for (const [codec, canPlay, available, expected] of [
    ['eac3', '', true, 1], ['eac3', 'probably', true, 0], ['aac', '', true, 0], ['eac3', '', false, 0],
  ]) {
    let attempts = 0;
    const check = vm.runInNewContext(js, {
      mediaTracks: { audio_tracks: [{ index: 1, codec }] }, selectedAudioStream: 1,
      transcodeCapabilities: { available }, fallbackVideoRef: { current: { canPlayType: () => canPlay } },
      fmp4StreamUrl: null, fmp4RemuxError: null, fmp4RemuxingRef: { current: false },
      nativePhase: 'loading', playbackMode: 'original', useFallback: true,
      handleProgressiveDetected: () => { attempts++; },
    });
    check();
    assert.equal(attempts, expected, codec + ' compatibility fallback chose the wrong path');
  }
});


test('native conversion failures are visible while the decoder is still loading', () => {
  let stateIndex = 0;
  const playerLoad = loader({
    react: {
      ...React,
      useState(initial) {
        // Two memory-recovery useState slots precede fMP4 error state.
        if (stateIndex++ === 5) return ['fixture conversion failed', () => {}];
        return React.useState(initial);
      },
    },
    '../../../hooks/useAdaptiveStreaming': {
      useAdaptiveStreaming: () => ({
        videoRef: { current: null }, phase: 'error', error: 'inactive parser failure',
        tracks: [], loadProgress: 0, currentQuality: 'original', setQuality() {},
        adaptiveMode: false, setAdaptiveMode() {}, measuredKbps: 0,
        useFallback: true, fallbackUrl: 'http://localhost/fixture.mkv', abort() {},
      }),
    },
  });
  const player = playerLoad(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx')).AdaptiveMediaPlayer;
  const html = renderToStaticMarkup(React.createElement(player, {
    file: { id: 7, name: 'fixture.mkv', size: 100, type: 'video' },
    activeFolderId: 1, streamUrl: 'http://localhost/stream/1/7?token=qa', onClose() {},
  }));
  assert.ok(html.includes('Playback Error'), 'Failed conversion left an indefinitely black native player');
  assert.ok(html.includes('fixture conversion failed'));
  assert.ok(!html.includes('inactive parser failure'));
});
test('silent native startup recovers while playable and stale sources are left alone', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx'), 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let effect;
  function visit(node) {
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect') {
      const body = node.arguments[0].getText(ast);
      if (body.includes('setTimeout') && body.includes('readyState') && body.includes('fallbackVideoRef'))
        effect = body;
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(effect, 'Native startup has no recovery when a decoder never fires an error');
  const js = ts.transpileModule('const check = ' + effect + '; check;', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  for (const scenario of [
    { name: 'silent stalled decoder', expected: 1 },
    { name: 'already has a frame', readyState: 2, expected: 0 },
    { name: 'already playing', phase: 'playing', expected: 0 },
    { name: 'buffering after playback began', played: true, expected: 0 },
    { name: 'source changed', stale: true, expected: 0 },
    { name: 'HLS owns playback', mode: 'hls', expected: 0 },
    { name: 'conversion active', converting: true, expected: 0 },
    { name: 'FFmpeg unavailable', available: false, expected: 0, error: true },
  ]) {
    let timer, attempts = 0, error = null;
    const generation = { current: 0 };
    const context = {
      useFallback: true, playbackMode: scenario.mode || 'original',
      nativePhase: scenario.phase || 'loading', fmp4StreamUrl: null, fmp4RemuxError: null,
      fmp4RemuxingRef: { current: !!scenario.converting }, remuxGenerationRef: generation,
      transcodeCapabilities: { available: scenario.available !== false },
      transcodeCapsRef: { current: { available: scenario.available !== false } },
      fallbackVideoRef: { current: { readyState: scenario.readyState || 0 } },
      nativePlaybackStartedRef: { current: !!scenario.played },
      HTMLMediaElement: { HAVE_CURRENT_DATA: 2 },
      handleProgressiveDetected() { attempts++; },
      logRef: { current() {} }, setNativePhase() {}, setNativeError(value) { error = value; },
      setTimeout(callback) { timer = callback; return 1; }, clearTimeout() {},
    };
    vm.runInNewContext(js, context)();
    if (scenario.stale) generation.current++;
    timer?.();
    assert.equal(attempts, scenario.expected, scenario.name);
    assert.equal(!!error, !!scenario.error, scenario.name + ' error');
  }
});

test('native startup recovery remembers successful playback before later buffering', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx'), 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let handler;
  function visit(node) {
    if (ts.isJsxAttribute(node) && node.name.getText(ast) === 'onPlaying')
      handler = node.initializer.expression.getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  const nativePlaybackStartedRef = { current: false };
  const js = ts.transpileModule('const played = ' + handler + '; played;', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  vm.runInNewContext(js, { nativePlaybackStartedRef, setNativePhase() {} })();
  assert.ok(nativePlaybackStartedRef.current, 'Later buffering is confused with never having started');
});

test('compatibility conversion releases the original network source before starting', async () => {
  const { context, state, start } = remuxHarness();
  let paused = false, removed = false, loaded = false;
  context.fallbackVideoRef.current = {
    currentTime: 0, paused: true,
    pause() { paused = true; },
    removeAttribute(name) { if (name === 'src') removed = true; },
    load() { loaded = true; },
  };
  context.invoke = async () => {
    assert.ok(paused && removed && loaded, 'Original video still competes for network while conversion starts');
    throw new Error('fixture backend conversion failure');
  };
  start();
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(state.error?.includes('fixture backend conversion failure'));
});

test('version files expose explicit Add version and Manage versions actions', () => {
  const menuLoad = loader({
    'react-i18next': { useTranslation: () => ({ t: key => key }) },
  });
  const { ContextMenu } = menuLoad(path.join(root, 'app/src/components/desktop/dashboard/ContextMenu.tsx'));
  const noop = () => {};
  const base = {
    x: 10, y: 10, onClose: noop, onDownload: noop, onDelete: noop,
    onPreview: noop, onRename: noop, onMove: noop, onAddVersion: noop,
    onManageVersions: noop, folders: [], activeFolderId: 7,
  };
  const plain = renderToStaticMarkup(React.createElement(ContextMenu, {
    ...base,
    file: {
      id: 1, name: 'movie-1080p.mkv', size: 100, sizeStr: '100 B',
      type: 'file', folder_id: 7, logical_file_id: '1'.repeat(32),
    },
  }));
  assert.ok(plain.includes('Add version'), 'Unstacked logical file is missing Add version');
  assert.ok(!plain.includes('Manage versions'), 'Unstacked file unexpectedly shows Manage versions');

  const stacked = renderToStaticMarkup(React.createElement(ContextMenu, {
    ...base,
    file: {
      id: 2, name: 'movie-4k.mkv', size: 200, sizeStr: '200 B',
      type: 'file', folder_id: 7, logical_file_id: '2'.repeat(32),
      stack_id: 'a'.repeat(32), stack_name: 'Movie', stack_version_count: 2,
    },
  }));
  for (const label of ['Add version', 'Manage versions', 'Move stack', 'Rename stack', 'Delete / unstack']) {
    assert.ok(stacked.includes(label), 'Stack context menu is missing: ' + label);
  }
});

test('Add version UI offers upload-new and existing-file flows without drag stacking', () => {
  const modalLoad = loader({
    '../../../context/ConfirmContext': { useConfirm: () => ({ confirm: async () => true }) },
    '../../../hooks/useEscapeToClose': { useEscapeToClose() {} },
  });
  const { VersionStackModal } = modalLoad(path.join(root, 'app/src/components/desktop/dashboard/VersionStackModal.tsx'));
  const noop = () => {};
  const html = renderToStaticMarkup(React.createElement(VersionStackModal, {
    file: {
      id: 1, name: 'movie-1080p.mkv', size: 100, sizeStr: '100 B',
      type: 'file', folder_id: 7, logical_file_id: '1'.repeat(32),
    },
    allFiles: [{
      id: 1, name: 'movie-1080p.mkv', size: 100, sizeStr: '100 B',
      type: 'file', folder_id: 7, logical_file_id: '1'.repeat(32),
    }],
    folderId: 7, initialMode: 'add', readOnly: false,
    onClose: noop, onChanged: noop, onUploadNew: noop, onPreview: noop,
    onDownload: noop, onDownloadAll: noop,
  }));
  assert.ok(html.includes('Upload new version'), 'Upload-new version action missing');
  assert.ok(html.includes('Use existing TeraRelay file'), 'Existing-file version action missing');
  assert.ok(html.includes('Make added file primary'), 'Explicit primary choice missing');
  assert.ok(!html.includes('Drag one file'), 'Version creation regressed to drag-first UX');
});

test('version stacks stay expanded unless the user explicitly collapses them', () => {
  const feed = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/ChannelFeed.tsx'), 'utf8');
  const grouped = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/StackedFileRow.tsx'), 'utf8');

  assert.ok(feed.includes('collapsedStackIds'), 'Channel feed does not remember only explicit user collapses');
  assert.ok(feed.includes('expanded={!collapsedStackIds.has(file.stack_id)}'), 'Stack rows do not default to expanded');
  assert.ok(feed.includes('if (nextExpanded) next.delete(file.stack_id!)'), 'Explicit expand does not clear the user-collapse marker');
  assert.ok(feed.includes('else next.add(file.stack_id!)'), 'Explicit collapse does not persist the user-collapse marker');
  assert.ok(feed.includes('onExpandedChange='), 'Stack row cannot report an explicit user expand/collapse choice');
  assert.ok(grouped.includes('if (expanded && !stack && !loading)'), 'Initially expanded stacks do not load their members automatically');
  assert.ok(!grouped.includes('const [expanded, setExpanded] = useState(false)'), 'Stack row still resets itself to collapsed whenever it remounts');
});

test('stacked files use one expandable group without decorative version badges', () => {
  const feed = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/ChannelFeed.tsx'), 'utf8');
  const grouped = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/StackedFileRow.tsx'), 'utf8');
  const grid = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/FileCard.tsx'), 'utf8');
  const list = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/FileListItem.tsx'), 'utf8');
  const mobile = fs.readFileSync(path.join(root, 'app/src/components/mobile/TouchFileList.tsx'), 'utf8');
  const modal = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/VersionStackModal.tsx'), 'utf8');

  assert.ok(feed.includes('<StackedFileRow'), 'Channel feed does not route stacked files into the grouped card');
  assert.ok(feed.includes('file.stack_id && (file.stack_version_count ?? 0) > 1'), 'Grouped-card routing lost the stable stack guard');
  assert.ok(grouped.includes("invoke<FileStackView>('cmd_get_file_stack'"), 'Grouped card does not load the existing stack manifest');
  assert.ok(grouped.includes('tr-version-group__body'), 'Grouped card has no contained expanded body');
  assert.ok(grouped.includes('ChevronDown') && grouped.includes('ChevronRight'), 'Grouped card lost its quiet expand/collapse control');
  const parentMarkup = grouped.split('{expanded && (')[0];
  assert.ok(!parentMarkup.includes('<PremiumFileThumbnail'), 'Group parent must not duplicate a member thumbnail');
  assert.ok(!parentMarkup.includes('formatBytes(file.size)'), 'Group parent must not duplicate member size metadata');
  assert.ok(!parentMarkup.includes('getPremiumFileMeta(file)'), 'Group parent must not duplicate member quality metadata');
  assert.ok(parentMarkup.includes('onDownloadAll(current.members.map(member => member.file))'), 'Group download must reuse the existing bulk-download path');
  assert.ok(parentMarkup.includes('Download all files in'), 'Group download action should be explicit without adding visible labels');

  for (const [name, source] of [['grid', grid], ['list', list], ['mobile', mobile]]) {
    assert.ok(!source.includes('stack_version_count'), name + ' view reintroduced a rendered stack-count badge');
  }
  assert.ok(!modal.includes('<Crown'), 'Stack manager reintroduced the crown decoration');
  assert.ok(!/>\s*Primary\s*</.test(modal), 'Stack manager reintroduced a visible PRIMARY badge');
  assert.ok(!modal.includes('Timeline anchor'), 'Stack manager exposed the internal timeline-anchor label');
});

test('auto-created technical stack names are presented as clean content titles', () => {
  const { getPremiumFileTitle } = load(path.join(root, 'app/src/filePresentation.ts'));
  assert.equal(
    getPremiumFileTitle({
      id: 1,
      name: 'Interstellar.1080p.BluRay.mkv',
      stack_name: 'Interstellar.1080p.BluRay.mkv',
      size: 1,
      sizeStr: '1 B',
      type: 'file',
    }),
    'Interstellar',
  );
  assert.equal(
    getPremiumFileTitle({
      id: 2,
      name: 'Interstellar.1080p.BluRay.mkv',
      stack_name: 'Director Cut',
      size: 1,
      sizeStr: '1 B',
      type: 'file',
    }),
    'Director Cut',
    'Deliberate custom stack names must stay untouched',
  );
});

test('rich media metadata stays on-demand and enriches cached file rows', () => {
  const details = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/MediaDetailsPanel.tsx'), 'utf8');
  const hook = fs.readFileSync(path.join(root, 'app/src/hooks/useRichMediaMetadata.ts'), 'utf8');
  const cachedRow = fs.readFileSync(path.join(root, 'app/src/components/shared/RichFileMetaText.tsx'), 'utf8');
  const feed = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/ChannelFeed.tsx'), 'utf8');
  const grouped = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/StackedFileRow.tsx'), 'utf8');
  const menu = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/ContextMenu.tsx'), 'utf8');

  assert.ok(details.includes("useRichMediaMetadata("), 'File-details panel does not load rich media metadata');
  for (const label of ['Resolution', 'Dynamic range', 'Codec', 'Container', 'Duration', 'Audio', 'Subtitles', 'Channel']) {
    assert.ok(details.includes(label), 'File-details panel is missing ' + label);
  }
  assert.ok(hook.includes("enabled: eligible && enabled"), 'Metadata hook no longer has an explicit on-demand gate');
  assert.ok(cachedRow.includes("useRichMediaMetadata(file.id, folderId, file.name, false)"), 'File rows can trigger media probing instead of reading cache only');
  assert.ok(feed.includes('<RichFileMetaText'), 'Channel rows do not consume cached rich metadata');
  assert.ok(grouped.includes('<RichFileMetaText'), 'Stack member rows do not consume cached rich metadata');
  assert.ok(menu.includes('File details'), 'Context menu is missing File details');
  assert.ok(menu.includes("!file.stack_id && onDetails"), 'Group parent incorrectly exposes member-level file details');
});

test('rich media presentation keeps the main list compact', () => {
  const { getRichFileMeta, formatCompactMediaDuration, formatVideoCodec } = load(path.join(root, 'app/src/filePresentation.ts'));
  const file = {
    id: 1,
    name: 'Interstellar.2160p.HDR.mkv',
    size: 4300000000,
    sizeStr: '4.3 GB',
    type: 'file',
  };
  const metadata = {
    duration_secs: 10140,
    width: 3840,
    height: 2160,
    video_codec: 'hevc',
    video_profile: 'Main 10',
    pixel_format: 'yuv420p10le',
    dynamic_range: 'HDR10',
    container: 'MKV',
    audio_tracks: [],
    subtitle_tracks: [],
  };
  assert.equal(formatCompactMediaDuration(metadata.duration_secs), '2h 49m');
  assert.equal(formatVideoCodec(metadata.video_codec), 'HEVC');
  assert.equal(getRichFileMeta(file, metadata), '4K · HDR · HEVC · 2h 49m');
  assert.ok(getRichFileMeta(file, metadata).split(' · ').length <= 4, 'Main-list metadata became cluttered');
});

test('duplicate detection is byte-verified and filename-only matches cannot become duplicates', () => {
  const backend = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/fs.rs'), 'utf8');
  const transfers = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/transfers.rs'), 'utf8');
  const upload = fs.readFileSync(path.join(root, 'app/src/hooks/useFileUpload.ts'), 'utf8');
  const dialog = fs.readFileSync(path.join(root, 'app/src/context/UploadSuggestionContext.tsx'), 'utf8');

  assert.ok(transfers.includes('hash_file_range(path, 0, before.size)'), 'Preflight does not hash the complete source file');
  assert.ok(transfers.includes('if before != after'), 'Preflight does not reject a source that changed while hashing');
  assert.ok(backend.includes('existing_size == size') && backend.includes('whole_sha256.as_deref() == Some(source_sha256.as_str())'), 'Exact duplicate classification is not guarded by size + whole-file SHA-256');
  assert.ok(backend.includes('whole_sha256.as_deref() != Some(source_sha256.as_str())'), 'Version suggestions do not require different verified content hashes');
  assert.ok(backend.includes('whole_sha256: whole_sha256.clone()'), 'Uploaded logical manifests do not persist the whole-file hash');
  assert.ok(upload.includes("cmd_preflight_upload_candidate"), 'Normal upload path does not run duplicate/version preflight');
  assert.ok(upload.includes("sourceSha256: preflight.source_sha256"), 'Verified preflight hash is not persisted with the recoverable upload');
  assert.ok(dialog.includes('Skip duplicate') && dialog.includes('Upload anyway'), 'Exact-duplicate decision UI is incomplete');
  assert.ok(dialog.includes('Add as version') && dialog.includes('Keep separate') && dialog.includes('Cancel'), 'Possible-version decision UI is incomplete');
  assert.ok(dialog.includes('calculated from every byte'), 'UI does not explain the byte-verified duplicate check');
});

test('TDLib saved-session restore waits up to one minute before giving up', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/context/FastTransferAuthContext.tsx'), 'utf8');
  assert.ok(source.includes('60_000'), 'TDLib restore window is shorter than the real saved-session settle time');
  assert.ok(source.includes('1_000'), 'TDLib restore polling is not paced at about one second');
  assert.ok(source.includes('Date.now() < restoreDeadline'), 'TDLib restore is not bounded by a real deadline');
});

test('global error toasts expose a manual dismiss button', () => {
  const app = fs.readFileSync(path.join(root, 'app/src/App.tsx'), 'utf8');
  assert.ok(/<Toaster[^>]*closeButton/.test(app), 'Sonner toasts do not expose the global close button');
});

test('failed and completed transfer cards can be dismissed manually', () => {
  const upload = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/UploadQueue.tsx'), 'utf8');
  const download = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/DownloadQueue.tsx'), 'utf8');
  const dashboard = fs.readFileSync(path.join(root, 'app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
  const uploadHook = fs.readFileSync(path.join(root, 'app/src/hooks/useFileUpload.ts'), 'utf8');
  const downloadHook = fs.readFileSync(path.join(root, 'app/src/hooks/useFileDownload.ts'), 'utf8');
  for (const [name, source] of [['upload queue', upload], ['download queue', download]]) {
    assert.ok(source.includes('onDismissItem'), name + ' has no dismiss callback');
    assert.ok(source.includes('title="Dismiss"'), name + ' has no visible dismiss button');
    assert.ok(source.includes("item.status === 'success'"), name + ' cannot dismiss completed items');
  }
  assert.ok(dashboard.includes('onDismissItem={dismissUploadItem}'), 'Upload dismiss is not wired to the dashboard');
  assert.ok(dashboard.includes('onDismissItem={dismissDownloadItem}'), 'Download dismiss is not wired to the dashboard');
  assert.ok(uploadHook.includes('const dismissItem ='), 'Upload queue cannot remove terminal items durably');
  assert.ok(downloadHook.includes('const dismissItem ='), 'Download queue cannot remove terminal items durably');
});

test('channel feed keeps compact rows while grouping uploads by day', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/ChannelFeed.tsx'), 'utf8');
  assert.ok(source.includes('formatChannelDay'), 'Channel feed has no upload-day formatter');
  assert.ok(source.includes('tr-channel-day-divider'), 'Channel feed has no visible day grouping divider');
  assert.ok(source.includes("return 'Today'") && source.includes("return 'Yesterday'"), 'Recent upload dates are not human-friendly');
  assert.ok(!source.includes('flex justify-center py-2'), 'Bulky centered date chips were reintroduced');
});

test('transfer chooser exposes only TeraRelay Boost and TDLib C++', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/context/TransferMethodContext.tsx'), 'utf8');
  assert.ok(source.includes('TeraRelay Boost'), 'Boost choice missing');
  assert.ok(source.includes('TDLib / C++'), 'TDLib choice missing');
  assert.ok(!source.includes('Automatic — Recommended') && !source.includes("finish('automatic')"), 'Automatic engine must not exist in V1');
});

test('Feature A version upload remains independent from Feature B chooser', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useFileUpload.ts'), 'utf8');
  const ast = ts.createSourceFile('useFileUpload.ts', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  let body = '';
  function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(ast) === 'handleVersionUpload') {
      body = node.initializer?.getText(ast) || '';
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(body, 'handleVersionUpload was not found');
  assert.ok(!body.includes('chooseTransferMethod'), 'Feature B chooser leaked into Feature A version upload');
  assert.ok(!body.includes('transferEngine'), 'Feature A version upload changed its transfer contract');
});

if (process.argv.includes('--fixture')) {
  const layoutFile = process.env.TERA_QA_LAYOUT_SOURCE || 'app/src/components/desktop/DesktopDashboard.tsx';
  const source = fs.readFileSync(path.join(root, layoutFile), 'utf8');
  const dockClass = source.slice(source.indexOf('{(uploadQueue.length > 0')).match(/className="([^"]+)"/)[1];
  const { ChannelFeed } = load(path.join(root, 'app/src/components/desktop/dashboard/ChannelFeed.tsx'));
  const { UploadQueue } = load(path.join(root, 'app/src/components/desktop/dashboard/UploadQueue.tsx'));
  const noop = () => {};
  const feed = React.createElement(ChannelFeed, {
    channelName: 'QA', files: [], loading: false, error: null, selectedIds: [], activeFolderId: 1,
    searchTerm: '', totalFileCount: 0, showFolderUpload: true,
    onFileClick: noop, onToggleSelection: noop, onDownload: noop, onPreview: noop,
    onDelete: noop, onRename: noop, onFileMove: noop, onManualUpload: noop, onFolderUpload: noop,
    onChannelInfo: noop, onSearchChange: noop,
  });
  const dock = React.createElement('div', { className: dockClass }, React.createElement(UploadQueue, {
    ...queueProps, items: [{ id: 'qa', path: 'fixture.bin', status: 'uploading', progress: 20, uploadedBytes: 200, totalBytes: 1000, speedBytesPerSec: 0 }],
  }));
  const css = fs.readdirSync(path.join(root, 'app/dist/assets')).filter(n => n.endsWith('.css')).map(n => fs.readFileSync(path.join(root, 'app/dist/assets', n), 'utf8')).join('\n');
  const body = renderToStaticMarkup(React.createElement('div', { style: { height: '100vh', display: 'flex', flexDirection: 'column' } }, feed, dock));
  const html = '<!doctype html><html><head><meta charset="utf-8"><style>' + css + '</style></head><body>' + body + '<script>window.addEventListener("load",()=>{const b=[...document.querySelectorAll("button")].find(e=>e.textContent.trim()==="Add folder");const r=b.getBoundingClientRect();const h=document.elementFromPoint(r.x+r.width/2,r.y+r.height/2);window.__qaReport={pass:b===h||b.contains(h),button:[r.x,r.y,r.width,r.height],hit:h?.outerHTML.slice(0,180)};});</script></body></html>';
  assert.ok(process.env.TERA_QA_UI_OUTPUT, '--fixture requires TERA_QA_UI_OUTPUT');
  fs.writeFileSync(process.env.TERA_QA_UI_OUTPUT, html);
}
test('device continuity preserves reconciliation independently from desktop mount', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  const snapshot = source.indexOf("validateDriveSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'))");
  const sync = source.indexOf("validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'))");
  const crypto = source.indexOf("validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_status'))");
  const mount = source.indexOf("invoke('cmd_drive_mount')");
  const continuityStart = source.indexOf('const runInitialContinuity');
  assert.ok(snapshot >= 0 && sync > snapshot, 'Continuity must snapshot local Drive before remote reconciliation');
  assert.ok(crypto > sync, 'Continuity must inspect encryption after remote metadata is reconciled');
  assert.ok(mount >= 0 && mount < continuityStart, 'Independent provider recovery must own the initial mount');
  assert.ok(!source.slice(continuityStart).includes("invoke('cmd_drive_mount')"), 'Delayed metadata sync must not request a second mount');
  assert.ok(source.includes("invoke('cmd_drive_recover_pending')"), 'Existing pending-write recovery was dropped');
  assert.ok(source.includes('15_000'), 'Existing 15-second metadata retry interval was dropped');
  assert.ok(source.includes('if (!isMobile)'), 'Linux mount/recovery is not kept desktop-only');
  assert.ok(!source.includes('cmd_drive_crypto_setup'), 'Continuity must never create a new Drive key');
});

test('device continuity only classifies pristine remote state as a new-device restore', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/deviceContinuity.ts'), 'utf8');
  assert.ok(source.includes('isPristineDriveSnapshot'), 'Pristine local Drive detection is missing');
  assert.ok(source.includes("source === 'remote'"), 'Remote sync source is not recognized');
  assert.ok(source.includes("source === 'remote-merged'"), 'Merged remote sync source is not recognized');
  assert.ok(source.includes('restoredFromRemote'), 'Continuity result does not expose remote restore state');
  assert.ok(source.includes('directoryCount'), 'Restored folder count is missing');
  assert.ok(source.includes('fileCount'), 'Restored file count is missing');
});

test('device continuity classifier executes the new-device and encryption rules', () => {
  const { classifyDeviceContinuity } = load(path.join(root, 'app/src/deviceContinuity.ts'));
  const pristine = { revision: 0, dirty: false, directories: [], files: [], pending: [] };
  const restored = { revision: 42, dirty: false, directories: [{ id: 'docs' }], files: [{ id: 'file' }], pending: [] };
  const remote = { revision: 42, dirty: false, source: 'remote' };
  const current = { revision: 42, dirty: false, source: 'current' };

  const newDevice = classifyDeviceContinuity(pristine, remote, restored, { configured: true, unlocked: false });
  assert.equal(newDevice.restoredFromRemote, true, 'Pristine remote state was not classified as a restore');
  assert.equal(newDevice.directoryCount, 1);
  assert.equal(newDevice.fileCount, 1);
  assert.equal(newDevice.needsDriveUnlock, true, 'Encrypted restored Drive did not request the existing passphrase');

  const existingDevice = classifyDeviceContinuity(restored, remote, restored, { configured: true, unlocked: true });
  assert.equal(existingDevice.restoredFromRemote, false, 'Existing local state was falsely announced as a new-device restore');

  const localOnly = classifyDeviceContinuity(pristine, current, restored, { configured: false, unlocked: false });
  assert.equal(localOnly.restoredFromRemote, false, 'Current/local metadata was falsely classified as a remote restore');
});

test('device continuity metadata sync runs on mobile without mounting Drive', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.match(source, /cmd_drive_sync_metadata[\s\S]*15_000/, 'Authenticated clients do not retain periodic metadata sync');
  assert.match(source, /if \(!authenticated \|\| isMobile\) return;[\s\S]*cmd_drive_status[\s\S]*cmd_drive_mount/, 'Independent mount recovery is not gated to authenticated desktop clients');
  const metadataStart = source.indexOf('  useEffect(() => {\n    if (!authenticated) {');
  assert.ok(metadataStart >= 0, 'Metadata continuity must have an authentication-only entry guard');
  assert.ok(!source.slice(metadataStart).includes('authenticated || isMobile'), 'Mobile clients are excluded from metadata continuity');
});

test('App delegates Drive startup to device continuity instead of duplicating sync logic', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/App.tsx'), 'utf8');
  assert.ok(source.includes('useDeviceContinuity'), 'App does not use the continuity hook');
  assert.ok(source.includes('DriveContinuityUnlock'), 'App does not render the continuity unlock experience');
  assert.ok(!source.includes('const syncDrive = async () =>'), 'Old inline Drive sync orchestration still duplicates the continuity hook');
  assert.ok(source.includes('authenticated: authStatus === "authenticated"'), 'Continuity is not bound to authenticated state');
});

test('continuity unlock uses only the existing Drive passphrase path', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/components/shared/DriveContinuityUnlock.tsx'), 'utf8');
  assert.ok(source.includes("validateDriveCryptoStatus(response)")
    && source.includes("invoke<unknown>('cmd_drive_crypto_unlock'"),
    'Unlock dialog must verify native crypto status after existing unlock command');
  assert.ok(!source.includes('cmd_drive_crypto_setup'), 'New-device restore must never create a replacement encryption key');
  assert.ok(source.includes('Later'), 'Unlock dialog cannot be safely dismissed');
  assert.ok(source.includes('type="password"'), 'Drive passphrase field is not protected as a password input');
  assert.ok(source.includes("setPassphrase('')"), 'Passphrase is not cleared after use/dismissal');
});

test('continuity failures cannot masquerade as a pristine or unencrypted new device', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.ok(source.includes('revision: -1, dirty: true'), 'Failed local snapshot still looks pristine and can create a false restore classification');
  assert.ok(source.includes('securityError'), 'Drive crypto-status failure is silently treated as an unencrypted Drive');
  assert.ok(source.includes('Could not read Drive encryption status'), 'Secure-storage failure does not remain visible in continuity state');
});

test('App surfaces secure-storage continuity failures instead of silently continuing', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/App.tsx'), 'utf8');
  assert.ok(source.includes('continuity.securityError'), 'App never surfaces a continuity secure-storage failure');
  assert.ok(source.includes('secure Drive key storage could not be checked'), 'Secure-storage warning does not explain the degraded state');
});

test('periodic continuity sync never overlaps a slow Telegram reconciliation', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.ok(source.includes('let periodicSyncInFlight = false'), 'Continuity polling has no effect-local in-flight guard');
  assert.ok(source.includes('if (periodicSyncInFlight) return'), 'A slow sync can overlap the next interval tick');
  assert.ok(source.includes('periodicSyncInFlight = true'), 'Periodic sync never acquires its in-flight guard');
  assert.ok(source.includes('periodicSyncInFlight = false'), 'Periodic sync can leave its in-flight guard stuck');
});

test('continuity cancellation cannot leave a late signed-out desktop mount active', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.match(source, /if \(disposed \|\| inFlight\) return;/, 'Cancelled independent recovery can still start a desktop mount');
  assert.match(source, /validateNativeDriveMountStatus\(await invoke<unknown>\('cmd_drive_status'\)\);\s*if \(disposed\) return;/,
    'Late provider status can still start a cancelled mount');
  assert.match(source, /if \(!status.supported\) \{[\s\S]*?return;[\s\S]*?if \(status.mounted \|\| status.native_recovery \|\| mountHandedToNative\) \{[\s\S]*?return;/,
    'Recovery must not duplicate a mount already supported/owned natively');
  assert.match(source, /await invoke\('cmd_drive_mount'\);\s*mountHandedToNative = true;\s*if \(disposed && shouldDetachNativeDriveRef\.current\) \{\s*await invoke\('cmd_drive_unmount'\)/,
    'A mount that completes after resolved sign-out is not immediately undone');
});

test('native Drive lifetime is not owned by WebView effect cleanup', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.ok(source.includes('authResolved && !authenticated && !isMobile'), 'Resolved signed-out state does not explicitly detach desktop Drive');
  const cleanupStart = source.lastIndexOf('return () => {');
  const cleanupEnd = source.indexOf('};\n  }, [authenticated, authResolved, isMobile]);', cleanupStart);
  assert.ok(cleanupStart >= 0 && cleanupEnd > cleanupStart, 'Continuity effect cleanup could not be inspected');
  const cleanup = source.slice(cleanupStart, cleanupEnd);
  assert.ok(!cleanup.includes('cmd_drive_unmount'), 'WebView/effect teardown still owns native Drive unmounting');
  const appSource = fs.readFileSync(path.join(root, 'app/src/App.tsx'), 'utf8');
  assert.match(appSource, /authResolved:\s*authStatus === "authenticated" \|\| authStatus === "unauthenticated"/,
    'Continuity must treat saved-session recovery as unresolved, never as a signed-out Drive detachment');
});

test('cancelled continuity cannot mutate the next session crypto decision', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  const safeCryptoChecks = source.match(/crypto = validateDriveCryptoStatus\(await invoke<unknown>\('cmd_drive_crypto_status'\)\);\s*if \(disposed\) return;/g) || [];
  assert.ok(safeCryptoChecks.length >= 2, 'An old async crypto check can still mutate startup state after its effect was cancelled');
});

test('cancelled initial snapshot cannot overwrite a newer continuity baseline', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.match(source, /before = validateDriveSnapshot\(await invoke<unknown>\('cmd_drive_get_snapshot'\)\);\s*if \(disposed\) return;\s*initialSnapshotRef\.current = before;/,
    'A stale startup snapshot can overwrite the next session baseline');
});

test('successful continuity reconciliation advances the restore baseline', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  const updates = source.match(/initialSnapshotRef\.current = after;/g) || [];
  assert.ok(updates.length >= 2, 'Initial and periodic successful reconciliation must both advance the baseline');
});

test('continuity retries the startup unlock decision after secure storage recovers', () => {
  const source = fs.readFileSync(path.join(root, 'app/src/hooks/useDeviceContinuity.ts'), 'utf8');
  assert.ok(source.includes('startupCryptoCheckPendingRef'), 'A failed startup crypto check is forgotten');
  assert.ok(source.includes('allowStartupUnlock'), 'Periodic sync cannot distinguish startup recovery from a later manual Drive lock');
  assert.ok(source.includes('allowStartupUnlock && assessment.needsDriveUnlock'), 'Recovered secure storage never re-opens the existing-passphrase unlock path');
});

Promise.all(pending).then(() => { process.exitCode = failures ? 1 : 0; });
