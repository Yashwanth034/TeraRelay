const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { createRequire } = require('node:module');
const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const ts = appRequire('typescript');
const React = appRequire('react');
const { renderToStaticMarkup } = appRequire('react-dom/server');
const playerPath = path.join(root, 'app/src/components/desktop/dashboard/AdaptiveMediaPlayer.tsx');
const helpersPath = path.join(root, 'app/src/components/desktop/dashboard/playbackTimeline.tsx');
function loader(overrides = {}) {
  const cache = new Map();
  function load(file) {
    file = path.resolve(file);
    if (cache.has(file)) return cache.get(file).exports;
    const mod = { exports: {} }; cache.set(file, mod);
    const source = ts.transpileModule(fs.readFileSync(file, 'utf8'), {
      compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022 },
    }).outputText;
    function req(name) {
      if (name in overrides) return overrides[name];
      if (name.startsWith('@tauri-apps/')) return { invoke: async () => null, getCurrentWindow: () => ({}) };
      if (name.startsWith('.')) {
        const base = path.resolve(path.dirname(file), name);
        return load([base, base + '.ts', base + '.tsx', path.join(base, 'index.ts')].find(p => fs.existsSync(p)));
      }
      return appRequire(name);
    }
    new Function('require', 'module', 'exports', source)(req, mod, mod.exports);
    return mod.exports;
  }
  return load;
}
function callback(name, context) {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let body;
  function visit(node) {
    if (ts.isVariableDeclaration(node) && node.name.getText(ast) === name) body = node.initializer.arguments[0].getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  if (!body) return null;
  const js = ts.transpileModule('(() => { const fn = ' + body + '; return fn; })()', { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  return vm.runInNewContext(js, context);
}
let failures = 0;
const pendingTests = [];
function test(name, fn) {
  try {
    const result = fn();
    if (result?.then) pendingTests.push(result.then(() => console.log('PASS ' + name), error => {
      failures++; console.error('FAIL ' + name + ': ' + error.message);
    }));
    else console.log('PASS ' + name);
  } catch (error) { failures++; console.error('FAIL ' + name + ': ' + error.message); }
}
function wireMoviePosition(context) {
  context.getMoviePosition = callback('getMoviePosition', context) ?? (() => {
    const video = context.getActiveVideo();
    const remux = context.useFallback && context.playbackMode === 'original' && context.fmp4StreamUrl;
    return remux && context.pendingSeekRef?.current ? context.pendingSeekRef.current.time
      : (video?.currentTime ?? context.movieTime ?? 0) + (remux ? context.remuxWindowRef.current.start : 0);
  });
  return context;
}
function ranges(...pairs) { return { length: pairs.length, start: i => pairs[i][0], end: i => pairs[i][1] }; }
test('forward seek uses the movie duration beyond the first remux frontier', () => {
  const requested = [];
  const video = { currentTime: 585, duration: 586, paused: false, seekable: ranges([0,586]), buffered: ranges([0,586]) };
  const context = { getActiveVideo: () => video, useFallback: true, playbackMode: 'original',
    fmp4StreamUrl: 'http://localhost/fmp4/window/output.mp4',
    remuxWindowRef: { current: { start: 0, complete: false, key: 'window' } },
    sourceDurationRef: { current: 7200 }, pendingSeekRef: { current: null }, requestRemuxAt: time => requested.push(time),
  };
  wireMoviePosition(context);
  if (fs.existsSync(helpersPath)) Object.assign(context, loader()(helpersPath));
  context.seekTo = callback('seekTo', context);
  callback('seekBy', context)(10);
  assert.equal(requested[0], 595, 'Seeking was clamped to the 586-second growing file');
  assert.equal(video.currentTime, 585, 'A far seek should prepare its source window');
});
test('a remux timeline displays the complete source duration', () => {
  let state = 0;
  const fakeReact = { ...React,
    useState(initial) {
      const index = state++;
      if (index === 3) return ['http://localhost/fmp4/window/output.mp4', () => {}];
      if (initial && Array.isArray(initial.audio_tracks)) return [{ audio_tracks: [], subtitle_tracks: [], duration_secs: 7200, start_time_secs: 0 }, () => {}];
      return [typeof initial === 'function' ? initial() : initial, () => {}];
    }, useRef: value => ({ current: value }), useEffect() {}, useMemo: fn => fn(), useCallback: fn => fn,
  };
  const load = loader({ react: fakeReact, '../../../hooks/useAdaptiveStreaming': {
    useAdaptiveStreaming: url => ({ videoRef: { current: null }, phase: 'ready', error: null,
      tracks: [], loadProgress: 0, currentQuality: 'original', setQuality() {}, adaptiveMode: false,
      setAdaptiveMode() {}, measuredKbps: 0, useFallback: true, fallbackUrl: url, abort() {} }),
  }});
  const html = renderToStaticMarkup(React.createElement(load(playerPath).AdaptiveMediaPlayer, {
    file: { id: 7, name: 'movie.mkv', size: 2400000000, type: 'video', is_split: true },
    activeFolderId: 1, streamUrl: 'http://localhost/stream/1/7?token=qa', onClose() {},
  }));
  assert.ok(/aria-label="Movie timeline"[^>]*max="7200"|max="7200"[^>]*aria-label="Movie timeline"/.test(html), 'Full-duration timeline is absent');
  assert.ok(html.includes('2:00:00'), 'The displayed duration comes from the partial stream');
});
test('dragging the movie timeline prepares one window when released', () => {
  const requested = [];
  const fakeReact = { ...React, useState: value => [value, () => {}] };
  const timeline = loader({ react: fakeReact })(helpersPath).PlaybackTimeline({
    currentTime: 10, duration: 7200, paused: false, onTogglePlay() {}, onSeek: time => requested.push(time),
  });
  const input = timeline.props.children.find(child => child?.type === 'input');
  input.props.onChange({ target: { value: '5403.25' } });
  assert.equal(requested.length, 0, 'Dragging launched a remux for every input update');
  input.props.onPointerUp({ currentTarget: { value: '5403.25' } });
  assert.deepEqual(requested, [5403.25]);
});
test('pausing while a seek prepares prevents the new window from autoplaying', () => {
  let plays = 0, paused;
  const context = {
    getActiveVideo: () => ({ paused: true, play() { plays++; return Promise.resolve(); }, pause() {} }),
    pendingSeekRef: { current: { time: 5403.25, play: true } },
    setNativePaused: value => { paused = value; },
  };
  context.togglePlayback = callback('togglePlayback', context);
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let handler;
  function visit(node) {
    if (ts.isJsxAttribute(node) && node.name.getText(ast) === 'onTogglePlay') handler = node.initializer.expression.getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  const js = ts.transpileModule('(' + handler + ')', { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
  vm.runInNewContext(js, context)();
  assert.equal(context.pendingSeekRef.current.play, false, 'The pending window ignored the pause action');
  assert.equal(plays, 0, 'A loading window resumed its abandoned native source');
  assert.equal(paused, true);
});
test('subtitle times follow the movie window without losing overlapping cues', () => {
  const before = { startTime: 5390, endTime: 5399 };
  const overlapping = { startTime: 5399, endTime: 5401 };
  const next = { startTime: 5403, endTime: 5404 };
  const track = { cues: [before, overlapping, next], removeCue(cue) { this.cues.splice(this.cues.indexOf(cue), 1); } };
  loader()(helpersPath).shiftSubtitleCues(track, 5400);
  assert.deepEqual(track.cues, [{ startTime: 0, endTime: 1 }, { startTime: 3, endTime: 4 }]);
});
test('seek restoration waits past the first fragment and preserves pause', () => {
  const video = { currentTime: 0, duration: 2, seekable: ranges([0,2]), buffered: ranges([0,2]),
    paused: true, play() { throw Error('A paused seek auto-played'); }, pause() { this.paused = true; } };
  const context = { pendingSeekRef: { current: { time: 5403.25, play: false } },
    remuxWindowRef: { current: { start: 5400, key: 'window' } }, sourceDurationRef: { current: 7200 },
    savedTimeRef: { current: 5403.25 }, setNativeAutoPlay() {}, setNativePaused() {}, ...loader()(helpersPath) };
  const restore = callback('restoreNativePosition', context);
  restore(video);
  assert.equal(video.currentTime, 0, 'The target was clamped to the initial two-second fragment');
  assert.equal(context.pendingSeekRef.current.time, 5403.25);
  video.duration = 4; video.seekable = ranges([0,4]); video.buffered = ranges([0,4]);
  restore(video);
  assert.equal(video.currentTime, 3.25);
  assert.equal(context.pendingSeekRef.current, null);
  assert.equal(video.paused, true);
});
test('relative seeks accumulate against the pending movie position', () => {
  const requested = [];
  const video = { currentTime: 0, duration: NaN, seekable: ranges(), buffered: ranges() };
  const context = wireMoviePosition({ getActiveVideo: () => video, useFallback: true, playbackMode: 'original',
    fmp4StreamUrl: 'http://localhost/fmp4/old/output.mp4', movieTime: 5408,
    remuxWindowRef: { current: { start: 5400, key: 'old' } }, sourceDurationRef: { current: 7200 },
    pendingSeekRef: { current: { time: 5408, play: false } },
    requestRemuxAt(time) { requested.push(time); this.pendingSeekRef.current.time = time; },
    ...loader()(helpersPath),
  });
  context.requestRemuxAt = time => { requested.push(time); context.pendingSeekRef.current.time = time; };
  context.seekTo = callback('seekTo', context);
  const seekBy = callback('seekBy', context);
  seekBy(5); seekBy(5);
  assert.deepEqual(requested, [5413, 5418], 'A cleared native element lost the pending seek target');
});
test('selecting HLS quality saves the absolute position of a remux window', async () => {
  const context = wireMoviePosition({ getActiveVideo: () => ({ currentTime: 3.25, paused: true }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: 'http://localhost/fmp4/window/output.mp4',
    movieTime: 5403.25, remuxWindowRef: { current: { start: 5400, key: 'window' } },
    pendingSeekRef: { current: null }, savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    remuxGenerationRef: { current: 1 }, fmp4RemuxingRef: { current: false },
    HLS_QUALITIES: ['720p'], log() {}, activeFolderId: 1, file: { id: 7 },
    hlsQualityRef: { current: null }, hlsRef: { current: null }, abortMse() {},
    invoke: async name => name === 'cmd_cancel_fmp4_stream' ? true : { status: 'error' },
    ...Object.fromEntries(['PlaybackMode','HlsQuality','HlsPhase','HlsProgress','HlsError','HlsPlaylistUrl','HlsVariantStates','Fmp4Remuxing'].map(name => ['set' + name, () => {}])),
  });
  await callback('startTranscode', context)('720p');
  assert.equal(context.savedTimeRef.current, 5403.25, 'HLS restarted at the local remux time');
  assert.equal(context.savedPlaybackRef.current, false, 'A paused quality change lost playback intent');
});
test('audio selection in HLS does not reuse a stale remux offset', () => {
  const requested = [];
  const context = wireMoviePosition({ getActiveVideo: () => ({ currentTime: 5410, paused: false }),
    useFallback: true, playbackMode: 'hls', fmp4StreamUrl: 'http://localhost/fmp4/stale/output.mp4',
    movieTime: 5403.25, remuxWindowRef: { current: { start: 5400, key: 'stale' } },
    pendingSeekRef: { current: null }, savedTimeRef: { current: 0 }, selectedAudioStreamRef: { current: 1 },
    transcodeCapsRef: { current: { available: true } }, toast: { error() { throw Error('Unexpected audio error'); } },
    hlsRef: { current: null }, pollTimerRef: { current: null },
    setSelectedAudioStream() {}, setPlaybackMode() {}, setQuality() {}, requestRemuxAt: time => requested.push(time),
  });
  callback('handleAudioTrackChange', context)(2);
  assert.deepEqual(requested, [5410], 'An HLS timestamp received the stale native window offset');
});
test('returning from HLS prepares the original movie at its current absolute time', () => {
  const requested = [];
  const context = wireMoviePosition({ getActiveVideo: () => ({ currentTime: 5410, paused: true }),
    useFallback: true, playbackMode: 'hls', fmp4StreamUrl: 'http://localhost/fmp4/cancelled/output.mp4',
    movieTime: 5403.25, remuxWindowRef: { current: { start: 5400, key: 'cancelled' } },
    pendingSeekRef: { current: null }, savedTimeRef: { current: 0 },
    hlsRef: { current: null }, pollTimerRef: { current: null }, hlsQualityRef: { current: '720p' },
    currentJobIdRef: { current: 'hls-job' }, transcodeCapabilities: { available: true }, log() {},
    requestRemuxAt: time => requested.push(time), startTranscode() { throw Error('Unexpected HLS request'); },
    ...Object.fromEntries(['Quality','PlaybackMode','HlsPhase','HlsQuality','HlsPlaylistUrl','HlsError','HlsVideoReady','RestartNonce'].map(name => ['set' + name, () => {}])),
  });
  callback('handleQualityChange', context)('original');
  assert.deepEqual(requested, [5410], 'Original mode reused a cancelled window at its old position');
});
test('a pending original seek remains absolute before any remux URL is ready', () => {
  const requested = [];
  const context = wireMoviePosition({ getActiveVideo: () => null, useFallback: false, playbackMode: 'original',
    fmp4StreamUrl: null, movieTime: 5408, remuxWindowRef: { current: { start: 5400, key: 'pending' } },
    sourceDurationRef: { current: 7200 }, pendingSeekRef: { current: { time: 5408, play: false } },
    requestRemuxAt: time => requested.push(time), ...loader()(helpersPath),
  });
  context.seekTo = callback('seekTo', context);
  callback('seekBy', context)(5);
  assert.deepEqual(requested, [5413], 'Pending seek intent was lost while the native element was absent');
});
test('custom movie controls remain in the fullscreen subtree', async () => {
  const targets = [];
  const context = { getActiveVideo: () => ({ requestFullscreen: async () => targets.push('video') }),
    containerRef: { current: { requestFullscreen: async () => targets.push('container') } },
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: 'http://localhost/fmp4/window/output.mp4',
    sourceDuration: 7200, sourceDurationRef: { current: 7200 }, setIsFullscreen() {},
    getCurrentWindow: () => ({ setFullscreen: async () => targets.push('window') }),
  };
  await callback('enterFullscreen', context)();
  assert.deepEqual(targets, ['container'], 'Only the video entered fullscreen and hid the timeline');
});
Promise.all(pendingTests).then(() => { process.exitCode = failures ? 1 : 0; });
