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
    transcodeCapsRef: { current: { available: true } }, remuxGenerationRef: { current: 0 },
    selectedAudioStreamRef: { current: 2 }, streamBaseRef: { current: 'http://localhost' },
    streamAuthRef: { current: 'qa' }, abortMseRef: { current() {} }, logRef: { current() {} },
    savedTimeRef: { current: 0 }, remuxWindowRef: { current: { start: 0, complete: false, key: null } },
    pendingSeekRef: { current: null }, nativePlaybackStartedRef: { current: true }, setMovieTime() {},
    fallbackVideoRef: { current: { currentTime: 3.5, paused: !!options.paused, pause() {}, removeAttribute() {}, load() {} } },
    setFmp4Remuxing() {}, setFmp4RemuxError: value => { state.error = value; },
    setNativeAutoPlay: value => { state.autoPlay = value; },
    setFmp4StreamUrl: value => { state.url = value; }, setRestartNonce: () => { state.reloads++; },
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
        if (stateIndex++ === 2) return ['fixture conversion failed', () => {}];
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
  const html = '<!doctype html><html><head><style>' + css + '</style></head><body>' + body + '<script>window.addEventListener("load",()=>{const b=[...document.querySelectorAll("button")].find(e=>e.textContent.trim()==="Add folder");const r=b.getBoundingClientRect();const h=document.elementFromPoint(r.x+r.width/2,r.y+r.height/2);window.__qaReport={pass:b===h||b.contains(h),button:[r.x,r.y,r.width,r.height],hit:h?.outerHTML.slice(0,180)};});</script></body></html>';
  assert.ok(process.env.TERA_QA_UI_OUTPUT, '--fixture requires TERA_QA_UI_OUTPUT');
  fs.writeFileSync(process.env.TERA_QA_UI_OUTPUT, html);
}
Promise.all(pending).then(() => { process.exitCode = failures ? 1 : 0; });
