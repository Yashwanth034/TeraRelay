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
  return vm.runInNewContext(js, {
    setTimeout, clearTimeout,
    buildAuthenticatedHlsUrl: loader()(helpersPath).buildAuthenticatedHlsUrl,
    buildAuthenticatedSubtitleUrl: loader()(helpersPath).buildAuthenticatedSubtitleUrl,
    ...context,
  });
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
test('native HTTP MP4 does not force WebKitGTK disk downloadbuffer via eager preload', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const native = source.split('/* Fallback: native playback with a source-duration timeline')[1]
    ?.split('onLoadedMetadata=')[0] || '';
  assert.match(native, /preload="metadata"/,
    'Original HTTP MP4 player must avoid WebKitGTK on-disk auto-preload buffering');
});

test('native remux E2E distinguishes failed-original fallback from audio selection', () => {
  const qa = fs.readFileSync(path.join(root, 'scripts/test-native-media.cjs'), 'utf8');
  assert.match(qa, /preparesBeforeAudioSwitch/,
    'The fixture must count remuxes before the user changes tracks');
  assert.match(qa, /preparesAfterAudioSwitch !== preparesBeforeAudioSwitch \+ 1/,
    'An audio switch must add exactly one new conversion job');
  assert.match(qa, /originalDecoderFailedBeforeAudioSwitch/,
    'Extra preparation is only acceptable when original MKV decoding failed');
  assert.match(qa, /cmd_cancel_fmp4_stream/,
    'Replacing a failed decoder remux must cancel stale preparation');
});

test('native decoder reload keeps blob and data URLs valid while remounting the video', () => {
  const { nativeProgressiveRevisionUrl } = loader()(helpersPath);
  assert.equal(typeof nativeProgressiveRevisionUrl, 'function',
    'A shared native URL helper must handle browser-owned in-memory sources');
  const blob = 'blob:http://127.0.0.1:4411/fixture-identifier';
  const data = 'data:video/mp4;base64,AAAA?not-an-http-query';
  assert.equal(nativeProgressiveRevisionUrl(blob, 1), blob);
  assert.equal(nativeProgressiveRevisionUrl(data, 2), data);
  assert.equal(nativeProgressiveRevisionUrl('http://127.0.0.1:4411/single.mp4?token=qa', 1),
    'http://127.0.0.1:4411/single.mp4?token=qa&_native=1');
  assert.equal(nativeProgressiveRevisionUrl('http://127.0.0.1:4411/single.mp4', 2),
    'http://127.0.0.1:4411/single.mp4?_native=2');
  assert.equal(nativeProgressiveRevisionUrl('http://127.0.0.1:4411/single.mp4?token=qa#seek', 3),
    'http://127.0.0.1:4411/single.mp4?token=qa&_native=3#seek',
    'The decoder revision must be sent as an HTTP query, not appended inside a fragment');
  assert.equal(nativeProgressiveRevisionUrl('http://127.0.0.1:4411/single.mp4#seek', 4),
    'http://127.0.0.1:4411/single.mp4?_native=4#seek');
  assert.equal(nativeProgressiveRevisionUrl('http://127.0.0.1:4411/single.mp4?token=qa', -1),
    'http://127.0.0.1:4411/single.mp4?token=qa');
  const player = fs.readFileSync(playerPath, 'utf8');
  assert.match(player, /nativeProgressiveRevisionUrl\(fallbackUrl, nativeDecoderRevision\)/,
    'The real React player must use the shared decoder retry URL guard');
  assert.match(player, /key=\{`\$\{nativeDecoderRevision\}:\$\{nativeMediaUrl\}`\}/,
    'When the blob URL is stable, React must still remount the decoder on retry');
});

test('memory MP4 cleanup aborts, revokes and clears only the owned private media', () => {
  const invoked = [];
  const state = {
    memoryGenerationRef: { current: 7 },
    memoryFetchRef: { current: { abort() { invoked.push('abort'); } } },
    memoryMediaUrlRef: { current: 'blob:private-qa' },
    URL: { revokeObjectURL(url) { invoked.push('revoke:' + url); } },
    setMemoryMedia(value) { invoked.push('media:' + String(value)); },
    setMemoryLoading(value) { invoked.push('loading:' + String(value)); },
  };
  const clear = callback('clearMemoryMedia', state);
  assert.equal(typeof clear, 'function');
  clear();
  assert.equal(state.memoryGenerationRef.current, 8);
  assert.equal(state.memoryFetchRef.current, null);
  assert.equal(state.memoryMediaUrlRef.current, null);
  assert.deepEqual(invoked, ['abort','revoke:blob:private-qa','media:null','loading:false']);
});

test('in-memory MP4 recovery cancels an obsolete file before allocating a Blob URL', async () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  assert.match(source, /memoryMedia\?\.source === streamUrl/,
    'Old memory-backed media must not render even once for a newly selected file');
  assert.match(source, /onClick=\{memoryLoading \? clearMemoryMedia : loadMemoryPlayback\}/,
    'The opt-in button must cancel its own active download rather than spin indefinitely');
  assert.match(source, /'Try in-memory MP4 playback'/,
    'There must be a deliberate in-app recovery action, not automatic extra downloading');
  assert.match(source, /'Cancel in-memory MP4 playback download'/,
    'The pending download must expose an accessible cancel action');
  let resolveFetch;
  let created = 0;
  let published = 0;
  const context = {
    file: { name: 'clip.mp4', size: 5, is_split: false },
    streamUrl: 'http://127.0.0.1:4411/stream/qa/7?token=fixture',
    memoryFetchRef: { current: null }, memoryGenerationRef: { current: 0 },
    memoryMediaUrlRef: { current: null },
    eligibleNativeMemoryMp4: () => true,
    fetchBoundedNativeMp4: () => new Promise(resolve => { resolveFetch = resolve; }),
    AbortController,
    URL: { createObjectURL() { created++; return 'blob:local'; }, revokeObjectURL() {} },
    setMemoryLoading() {}, setMemoryMedia() { published++; },
  };
  const fn = callback('loadMemoryPlayback', context);
  assert.ok(fn, 'The opt-in recovery action was not wired to the player');
  const pending = fn();
  assert.ok(context.memoryFetchRef.current, 'No abortable local HTTP read was started');
  context.memoryFetchRef.current.abort();
  context.memoryGenerationRef.current++;
  resolveFetch(new Blob([new Uint8Array([1,2,3,4,5])], { type: 'video/mp4' }));
  await pending;
  assert.equal(created, 0, 'A stale request allocated a private media URL');
  assert.equal(published, 0, 'A stale request replaced the selected media');
});

test('explicit bounded local MP4 memory fetch preserves headers and exact body', async () => {
  const { eligibleNativeMemoryMp4, fetchBoundedNativeMp4 } = loader()(helpersPath);
  const input = 'http://127.0.0.1:4411/stream/qa/7?token=fixture';
  assert.equal(eligibleNativeMemoryMp4('fixture.mp4', 5, false, input), true);
  assert.equal(eligibleNativeMemoryMp4('fixture.mp4', 9 * 1024 * 1024, false, input), false);
  assert.equal(eligibleNativeMemoryMp4('fixture.mp4', 5, true, input), false);
  assert.equal(eligibleNativeMemoryMp4('fixture.mkv', 5, false, input), false);
  for (const invalid of ['https://example.com/stream/qa/7?token=fixture',
    'file:///stream/qa/7', 'blob:http://127.0.0.1/test',
    'http://127.0.0.1:4411/elsewhere?token=fixture',
    'http://127.0.0.1:4411/stream/qa/7']) {
    assert.equal(eligibleNativeMemoryMp4('fixture.mp4', 5, false, invalid), false);
  }
  const fetcher = async (url, options) => {
    assert.equal(url, input);
    assert.equal(options.cache, 'no-store');
    assert.equal(options.redirect, 'error',
      'Authenticated loopback media must never follow redirects to another origin');
    assert.equal(options.signal.aborted, false);
    return new Response(new Uint8Array([1,2,3,4,5]), {
      status:200,headers:{'content-type':'video/mp4','content-length':'5'}
    });
  };
  const ctrl = new AbortController();
  const blob = await fetchBoundedNativeMp4(input, 5, ctrl.signal, fetcher);
  assert.deepEqual([...new Uint8Array(await blob.arrayBuffer())], [1,2,3,4,5]);
  assert.equal(blob.type, 'video/mp4');
  await assert.rejects(fetchBoundedNativeMp4(input, 5, ctrl.signal,
    async () => new Response(new Uint8Array([1,2]), {
      status:206,headers:{'content-type':'video/mp4','content-length':'2'}
    })), /complete HTTP 200/);
  await assert.rejects(fetchBoundedNativeMp4(input, 5, ctrl.signal,
    async () => new Response(new Uint8Array([1,2]), {
      status:200,headers:{'content-type':'video/mp4','content-length':'2'}
    })), /complete|size/);
  const excess = new Uint8Array(8 * 1024 * 1024 + 1);
  await assert.rejects(fetchBoundedNativeMp4(input, 5, ctrl.signal,
    async () => new Response(excess, {status:200,headers:{'content-type':'video/mp4'}})),
    /limit|size/);
  await assert.rejects(fetchBoundedNativeMp4(input, 5, ctrl.signal,
    async () => new Response(new Uint8Array([1,2,3,4,5]),
      {status:200,headers:{'content-type':'text/html'}})), /MP4/);
});

test('HLS playlist URL construction encodes local auth and rejects cross-origin URLs', () => {
  const { buildAuthenticatedHlsUrl } = loader()(helpersPath);
  assert.equal(typeof buildAuthenticatedHlsUrl, 'function',
    'HLS URL builder missing shared safe implementation');
  const token = 'fixture a&b+#?=/';
  const result = buildAuthenticatedHlsUrl(
    'http://127.0.0.1:4411',
    '/hls/5_7/360p/index.m3u8?v=1#ignored',
    token,
  );
  const parsed = new URL(result);
  assert.equal(parsed.origin, 'http://127.0.0.1:4411');
  assert.equal(parsed.pathname, '/hls/5_7/360p/index.m3u8');
  assert.equal(parsed.searchParams.get('v'), '1');
  assert.equal(parsed.searchParams.get('token'), token,
    'HLS query formatting corrupted a local bearer token');
  assert.equal(parsed.hash, '', 'HLS media fragment should never be sent');
  assert.equal(buildAuthenticatedHlsUrl('http://127.0.0.1:4411',
    'https://example.com/hls/index.m3u8',token), null,
    'HLS bearer token could be sent to an unexpected external origin');
  assert.equal(buildAuthenticatedHlsUrl('http://127.0.0.1:4411',
    '//example.com/hls/index.m3u8',token), null);
  assert.equal(buildAuthenticatedHlsUrl('http://127.0.0.1:4411',
    '/different/path',token),null,'Non-HLS playlist was accepted');
  assert.equal(buildAuthenticatedHlsUrl('http://127.0.0.1:4411',
    '/hls/index.m3u8',''),null,'Missing local auth token was accepted');
});

test('hls.js auth hook never exposes bearer tokens to another host', () => {
  const { authorizeHlsChildRequestUrl } = loader()(helpersPath);
  assert.equal(typeof authorizeHlsChildRequestUrl, 'function');
  const manifest = 'http://127.0.0.1:4411/hls/qa/360p/index.m3u8?token=fixture';
  const token = 'sensitive a&b+#?=/';
  const src = authorizeHlsChildRequestUrl(manifest,
    'http://127.0.0.1:4411/hls/qa/360p/segment_001.ts?v=1',token);
  assert.ok(src);
  const valid = new URL(src);
  assert.equal(valid.pathname,'/hls/qa/360p/segment_001.ts');
  assert.equal(valid.searchParams.get('token'),token);
  assert.equal(valid.searchParams.get('v'),'1');
  const override = authorizeHlsChildRequestUrl(manifest,
    './segment_002.ts?token=stale',token);
  assert.equal(new URL(override).searchParams.get('token'),token,
    'Old or malicious token was kept on an otherwise valid child request');
  const allowedManifest = authorizeHlsChildRequestUrl(manifest,
    '/hls/qa/master.m3u8',token);
  assert.equal(new URL(allowedManifest).searchParams.get('token'),token);
  for(const candidate of [
    'https://external.example/hls/qa/360p/segment_001.ts',
    '//external.example/hls/qa/360p/segment_001.ts',
    'http://127.0.0.1.evil:4411/hls/qa/360p/segment_001.ts',
    '/hls/qa/360p/../secret',
    '/hls/qa/360p/key.bin',
    'data:text/plain,/hls/qa/360p/segment_001.ts',
  ]) assert.equal(authorizeHlsChildRequestUrl(manifest,candidate,token),null,
    'Local HLS bearer token can escape via '+candidate);
  assert.equal(authorizeHlsChildRequestUrl(manifest,
    '/hls/qa/360p/segment_001.ts',''),null);
  const player = fs.readFileSync(playerPath,'utf8');
  assert.match(player,/authorizeHlsChildRequestUrl\(hlsPlaylistUrl, url, authValue\)/,
    'The actual hls.js xhrSetup must use same-origin HLS child URL validation');
});

test('hls.js XHR hook prevents external fragment fetches, not only token leakage', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let expression;
  function visit(node) {
    if(ts.isPropertyAssignment(node) && node.name.getText(ast)==='xhrSetup')
      expression=node.initializer.getText(ast);
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(expression, 'HLS XHR authorization callback is missing');
  const compiled = ts.transpileModule('const fn = '+expression+'; fn;', {
    compilerOptions:{target:ts.ScriptTarget.ES2022}
  }).outputText;
  const playlist='http://127.0.0.1:4411/hls/qa/360p/index.m3u8';
  const token='isolated&secret';
  const hook = vm.runInNewContext(compiled,{
    hlsPlaylistUrl:playlist, authValue:token, URL,
    authorizeHlsChildRequestUrl:loader()(helpersPath).authorizeHlsChildRequestUrl,
  });
  const requests=[];
  const xhr={open:(method,url,asyncFlag)=>requests.push({method,url,asyncFlag})};
  hook(xhr,'https://untrusted.example/hls/qa/360p/segment_000.ts');
  assert.equal(requests.length,1,'hls.js would fetch an external HLS child unchanged');
  const blocked=new URL(requests[0].url);
  assert.equal(blocked.origin,'http://127.0.0.1:4411');
  assert.equal(blocked.pathname,'/hls/_rejected');
  assert.equal(blocked.searchParams.has('token'),false);
  requests.length=0;
  hook(xhr,'http://127.0.0.1:4411/hls/qa/360p/segment_001.ts');
  assert.equal(new URL(requests[0].url).searchParams.get('token'),token);
});

test('subtitle URLs stay on the loopback origin and safely encode the session token', () => {
  const { buildAuthenticatedSubtitleUrl } = loader()(helpersPath);
  assert.equal(typeof buildAuthenticatedSubtitleUrl, 'function',
    'The subtitle URL builder must validate the local media origin');
  const token = 'private a&b+#?=/';
  const url = buildAuthenticatedSubtitleUrl('http://127.0.0.1:4411',
    '/subtitle/5_9/2.vtt?lang=en#local', token);
  assert.ok(url, 'Valid local subtitle URL was rejected');
  const parsed = new URL(url);
  assert.equal(parsed.origin, 'http://127.0.0.1:4411');
  assert.equal(parsed.pathname, '/subtitle/5_9/2.vtt');
  assert.equal(parsed.searchParams.get('lang'), 'en');
  assert.equal(parsed.searchParams.get('token'), token);
  assert.equal(parsed.hash, '');
  for (const invalid of [
    'https://example.com/subtitle/5_9/2.vtt',
    '//example.com/subtitle/5_9/2.vtt',
    '/subtitle/../secret/2.vtt',
    '/not-subtitle/5_9/2.vtt',
    '/subtitle/5_9/-1.vtt'
  ]) {
    assert.equal(buildAuthenticatedSubtitleUrl('http://127.0.0.1:4411', invalid, token), null,
      'Untrusted subtitle URL was authorized: ' + invalid);
  }
  assert.equal(buildAuthenticatedSubtitleUrl('http://127.0.0.1:4411',
    '/subtitle/5_9/2.vtt', ''), null);
});

test('fMP4 remux URL securely binds the session token to a same-origin output', () => {
  const { buildAuthenticatedRemuxUrl } = loader()(helpersPath);
  assert.equal(typeof buildAuthenticatedRemuxUrl, 'function');
  const token = 'private a&b+#?=/';
  const result = buildAuthenticatedRemuxUrl('http://127.0.0.1:4411',
    '/fmp4/qa_123/output.mp4?v=1#ignored',token);
  assert.ok(result);
  const url = new URL(result);
  assert.equal(url.origin,'http://127.0.0.1:4411');
  assert.equal(url.pathname,'/fmp4/qa_123/output.mp4');
  assert.equal(url.searchParams.get('v'),'1');
  assert.equal(url.searchParams.get('token'),token);
  assert.equal(url.hash,'');
  for(const candidate of [
    'https://external.example/fmp4/qa/output.mp4',
    '//external.example/fmp4/qa/output.mp4',
    '/fmp4/../private/output.mp4',
    '/hls/qa/480p/index.m3u8',
    '/fmp4/qa/other.mp4',
  ]) assert.equal(buildAuthenticatedRemuxUrl('http://127.0.0.1:4411',
    candidate,token),null, 'Unexpected remux URL was authorized');
  assert.equal(buildAuthenticatedRemuxUrl('http://127.0.0.1:4411',
    '/fmp4/qa/output.mp4',''),null);
  const player = fs.readFileSync(playerPath,'utf8');
  assert.match(player,/buildAuthenticatedRemuxUrl\(\s*streamBaseRef\.current, result\.url, streamAuthRef\.current/,
    'fMP4 preparation must use the validated token-bearing URL');
});

test('stale subtitle requests never override the later track or re-enable cleared subtitles', async () => {
  let finishOld, finishNew;
  const old = new Promise(resolve => { finishOld = resolve; });
  const newer = new Promise(resolve => { finishNew = resolve; });
  const selections=[], urls=[], loading=[], toasts=[];
  const ctx = {
    subtitleSelectionGenerationRef:{current:0},
    file:{id:9},activeFolderId:5,
    streamBaseRef:{current:'http://127.0.0.1:4411'},
    streamAuthRef:{current:'secret&private'},
    setSelectedSubtitleStream:v=>selections.push(v),
    setSubtitleUrl:v=>urls.push(v),
    setSubtitleLoading:v=>loading.push(v),
    invoke:async(_command,args)=>args.streamIndex===1?old:newer,
    toast:{error:message=>toasts.push(message)},
  };
  const change = callback('handleSubtitleTrackChange',ctx);
  const oldAction=change(1);
  const newAction=change(2);
  finishNew('/subtitle/5_9/2.vtt');
  await newAction;
  finishOld('/subtitle/5_9/1.vtt');
  await oldAction;
  assert.equal(new URL(urls.at(-1)).pathname,'/subtitle/5_9/2.vtt');
  assert.equal(urls.filter(v=>v && v.includes('/1.vtt')).length,0,
    'Late old subtitle response replaced the newer track');
  assert.equal(loading.at(-1),false);
  await change(null);
  assert.equal(urls.at(-1),null,'Turning subtitles off failed');
  assert.deepEqual(toasts,[]);
});

test('switching source never carries an earlier local bearer token into new HLS requests', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let body;
  function visit(node) {
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect') {
      const code = node.arguments[0].getText(ast);
      if (code.includes('streamAuthRef.current') && code.includes('STREAM_BASE_KEY')) body = code;
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(body, 'No local stream auth/URL reset effect was found');
  const js = ts.transpileModule('const run = '+body+'; run;', {
    compilerOptions: {target:ts.ScriptTarget.ES2022}
  }).outputText;
  const stub = () => {};
  const ctx = {
    streamUrl:'http://127.0.0.1:3333/stream/1?token=old_private_qa',
    streamAuthRef:{current:''}, streamBaseRef:{current:''}, STREAM_BASE_KEY:'/stream/',
    URL, clearMemoryMedia:stub, setFmp4Remuxing:stub, setFmp4RemuxError:stub, setFmp4StreamUrl:stub,
    setNativeAutoPlay:stub, setNativeDecoderRevision:stub, setMovieTime:stub,
    setNativePaused:stub, fmp4RemuxingRef:{current:false}, remuxGenerationRef:{current:0},
    hlsGenerationRef:{current:0}, hlsDesiredJobRef:{current:null},
    pollTimerRef:{current:null}, currentJobIdRef:{current:null},
    remuxWindowRef:{current:{}}, pendingSeekRef:{current:null},
    savedTimeRef:{current:0}, savedPlaybackRef:{current:true},
    nativeUserPauseRef:{current:false}, nativeOriginalRetryRef:{current:0},
    nativeCompletedRetryRef:{current:0}, invoke:async () => null
  };
  const run = vm.runInNewContext(js,ctx);
  run();
  assert.equal(ctx.streamAuthRef.current,'old_private_qa');
  assert.equal(ctx.streamBaseRef.current,'http://127.0.0.1:3333');
  ctx.streamUrl='http://127.0.0.1:4444/stream/2';
  run();
  assert.equal(ctx.streamAuthRef.current,'',
    'A tokenless new source inherited the previous file authentication token');
  assert.equal(ctx.streamBaseRef.current,'http://127.0.0.1:4444');
  ctx.streamUrl='not-a-url';
  run();
  assert.equal(ctx.streamAuthRef.current,'',
    'An invalid new source kept its prior bearer token');
  assert.equal(ctx.streamBaseRef.current,'',
    'An invalid new source kept its prior stream origin');
});

test('opt-in compatibility quality never upscales and fails closed without FFmpeg variants', () => {
  const { chooseCompatibilityQuality } = loader()(helpersPath);
  const variants = ['360p', '480p', '720p', '1080p'].map(label => ({
    label, height: Number.parseInt(label, 10), available: true,
  }));
  assert.equal(chooseCompatibilityQuality(variants, null), '480p');
  assert.equal(chooseCompatibilityQuality(variants, 240), null);
  assert.equal(chooseCompatibilityQuality(variants, 360), '360p');
  assert.equal(chooseCompatibilityQuality(variants, 480), '480p');
  assert.equal(chooseCompatibilityQuality(variants, 1080), '480p');
  assert.equal(chooseCompatibilityQuality(variants.map(v => ({ ...v, available: false })), null), null);
  assert.equal(chooseCompatibilityQuality([variants[2]], 720), '720p');
  assert.equal(chooseCompatibilityQuality([variants[2]], 480), null);
});

test('native playback failure offers explicit compatibility action instead of automatic FFmpeg remux', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let handler = null;
  function visit(node) {
    if (ts.isJsxOpeningElement(node) && node.tagName.getText(ast) === 'button') {
      const label = node.attributes.properties.find(p => ts.isJsxAttribute(p) &&
        p.name.getText(ast) === 'aria-label');
      if (label?.initializer?.text === 'Try compatibility playback') {
        const attr = node.attributes.properties.find(p => ts.isJsxAttribute(p) &&
          p.name.getText(ast) === 'onClick');
        handler = attr?.initializer?.expression?.getText(ast);
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(handler, 'Native player error overlay has no accessible compatibility action');
  assert.match(source, /!isHlsMode && useFallback && !fmp4RemuxError && compatibilityQuality &&/,
    'Compatibility option appears without verifying native-failure context and FFmpeg support');
  const requested = [];
  const callback = vm.runInNewContext(ts.transpileModule('(' + handler + ')', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText, { startTranscode: quality => requested.push(quality), compatibilityQuality: '360p' });
  assert.deepEqual(requested, [], 'Compatibility conversion started without explicit user action');
  callback();
  assert.deepEqual(requested, ['360p'], 'Click did not start the selected supported alternate output');
});

test('native and hls.js compatibility initialization preserve seek and explicit paused intent', () => {
  const { restoreCompatibilityPlayback } = loader()(helpersPath);
  let started = 0, stopped = 0;
  const video = {
    currentTime: 0,
    play() { started++; return Promise.resolve(); },
    pause() { stopped++; },
  };
  restoreCompatibilityPlayback(video, 53.75, false);
  assert.equal(video.currentTime, 53.75);
  assert.equal(started, 0);
  assert.equal(stopped, 1, 'HLS initialization ignored the user-pause intent');
  restoreCompatibilityPlayback(video, 72.5, true);
  assert.equal(video.currentTime, 72.5);
  assert.equal(started, 1);
  restoreCompatibilityPlayback(video, Number.NaN, false);
  assert.equal(video.currentTime, 72.5, 'Invalid saved position corrupted HLS seeking');
  assert.equal(stopped, 2);
  const source = fs.readFileSync(playerPath, 'utf8');
  assert.equal((source.match(/restoreCompatibilityPlayback\(video, savedTime, savedPlaybackRef\.current\)/g) ?? []).length, 2,
    'Both native HLS and hls.js must respect the shared play/pause restoration');
});

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
      // The explicit memory-retry controls add two earlier useState slots.
      if (index === 6) return ['http://localhost/fmp4/window/output.mp4', () => {}];
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
    nativeUserPauseRef: { current: false },
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
test('completed remux resumes after natural EOF but preserves intentional pause', () => {
  const { shouldResumeAfterRemuxFinish } = loader()(helpersPath);
  assert.equal(shouldResumeAfterRemuxFinish(null, { paused: true, ended: true }), true,
    'The partial stream reached EOF; its implicit pause must not suppress playback of the completed source');
  assert.equal(shouldResumeAfterRemuxFinish(null, { paused: true, ended: false }), false,
    'A user-paused partial stream must stay paused');
  assert.equal(shouldResumeAfterRemuxFinish({ play: false }, { paused: true, ended: true }), false,
    'An explicit pending pause must override natural EOF');
  assert.equal(shouldResumeAfterRemuxFinish({ play: true }, { paused: true, ended: false }), true,
    'An explicit pending play must survive buffer stalls');
  assert.equal(shouldResumeAfterRemuxFinish(null, null), true,
    'An absent partial video should not silently cancel the requested playback');
  assert.equal(shouldResumeAfterRemuxFinish(null, { paused: true, ended: false }, true), true,
    'A growing stream may pause without ended=true and must preserve a prior explicit play');
  assert.equal(shouldResumeAfterRemuxFinish(null, { paused: true, ended: false }, false), false,
    'An explicitly user-paused stream should stay paused');
});
test('native remux URL revisions change the path rather than only the query', () => {
  const { nativeRemuxRevisionUrl } = loader()(helpersPath);
  const url = 'http://127.0.0.1:1420/fmp4/folder_12/output.mp4?token=fixture_auth';
  assert.equal(nativeRemuxRevisionUrl(url, 1),
    'http://127.0.0.1:1420/fmp4/folder_12__refresh_1/output.mp4?token=fixture_auth');
  assert.equal(nativeRemuxRevisionUrl(url, 2),
    'http://127.0.0.1:1420/fmp4/folder_12__refresh_2/output.mp4?token=fixture_auth');
  assert.equal(nativeRemuxRevisionUrl(url, 0), url, 'Invalid revision must not rewrite a URL');
  assert.equal(nativeRemuxRevisionUrl('http://127.0.0.1:1420/stream/12?token=qa', 2),
    'http://127.0.0.1:1420/stream/12?token=qa', 'Only finished remux streams have this route');
});

test('manual native playback retry is opt-in, source-only and preserves a paused absolute seek', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  assert.match(source, /aria-label="Retry video playback"/,
    'A failed native decoder offers no accessible recovery control');
  let detached = false, unloaded = false, revisions = 0, shownErrors = [];
  const context = {
    fallbackVideoRef: { current: {
      currentTime: 9.25, pause() {},
      removeAttribute(name) { detached = name === 'src'; },
      load() { unloaded = true; },
    } },
    pendingSeekRef: { current: null },
    nativeUserPauseRef: { current: true },
    remuxWindowRef: { current: { start: 5400 } },
    nativeOriginalRetryRef: { current: 1 },
    nativeCompletedRetryRef: { current: 1 },
    setNativeAutoPlay(value) { assert.equal(value, false); },
    setNativeError(value) { shownErrors.push(value); },
    setNativePhase(value) { assert.equal(value, 'loading'); },
    setNativeDecoderRevision(next) { revisions = next(revisions); },
    setRestartNonce() { throw Error('Manual native decoder retry restarted the MSE pipeline'); },
    requestRemuxAt() { throw Error('Manual native decoder retry triggered an FFmpeg remux'); },
  };
  const fn = callback('retryNativePlayback', context);
  assert.ok(fn);
  fn();
  assert.equal(context.pendingSeekRef.current.time, 5409.25);
  assert.equal(context.pendingSeekRef.current.play, false);
  assert.equal(context.nativeOriginalRetryRef.current, 0);
  assert.equal(context.nativeCompletedRetryRef.current, 0);
  assert.equal(revisions, 1);
  assert.equal(detached, true);
  assert.equal(unloaded, true);
  assert.deepEqual(shownErrors, [null]);
});

test('ordinary native MP4 recovers one truncated-duration decoder and preserves pause intent', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let body;
  function visit(node) {
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect') {
      const code = node.arguments[0].getText(ast);
      if (code.includes('nativeOriginalRetryRef.current') && code.includes('2500')) body = code;
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(body, 'Ordinary MP4 fallback has no bounded native duration-recovery guard');
  const js = ts.transpileModule('const check = ' + body + '; check;', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const check = vm.runInNewContext(js, {});
  let retries = 0, errors = [], timer, unloaded = 0;
  const retry = { current: 0 };
  const makeVideo = () => ({
    currentTime: 0.021, duration: 0.021,
    pause() {}, removeAttribute(name) { if (name === 'src') unloaded++; }, load() {},
  });
  const videoRef = { current: makeVideo() };
  const ctx = {
    fmp4StreamUrl: null, useFallback: true, playbackMode: 'original',
    nativePhase: 'playing', fmp4RemuxingRef: { current: false },
    remuxGenerationRef: { current: 3 }, remuxWindowRef: { current: { key: null } },
    fallbackVideoRef: videoRef, sourceDurationRef: { current: 16 },
    nativeOriginalRetryRef: retry, pendingSeekRef: { current: { time: 9, play: false } },
    nativeUserPauseRef: { current: true },
    setNativeAutoPlay(play) { assert.equal(play, false); },
    setNativeError(s) { errors.push(s); }, setNativePhase() {},
    setNativeDecoderRevision(update) { retries = update(retries); },
    setRestartNonce() { throw Error('A native decoder-only retry restarted the streaming pipeline'); },
    setTimeout(fn, ms) { assert.equal(ms, 2500); timer = fn; return 1; },
    clearTimeout() {},
  };
  const evaluate = vm.runInNewContext(js, ctx);
  evaluate(); timer();
  assert.equal(retry.current, 1);
  assert.equal(retries, 1);
  assert.equal(unloaded, 1);
  assert.equal(ctx.pendingSeekRef.current.time, 9);
  videoRef.current = makeVideo();
  evaluate(); timer();
  assert.equal(retries, 1, 'Ordinary native player retried an unbounded number of times');
  assert.equal(errors.length, 1, 'Repeated decoder truncation did not surface a user-visible error');
  assert.match(source,
    /shouldRemuxAfterNativeDecoderFailure\(preferRemux, nativeOriginalRetryRef\.current\)/,
    'Native fallback must distinguish ordinary MP4 from explicitly requested remux');
  const choose = loader()(helpersPath).shouldRemuxAfterNativeDecoderFailure;
  assert.equal(choose(false, 1), false,
    'Ordinary native-only decoder retry incorrectly triggers FFmpeg remux');
  assert.equal(choose(false, 0), true,
    'Initial supported native error may still require FFmpeg');
  assert.equal(choose(true, 1), true,
    'Explicit remux request must survive failed native decoder recovery');
});

test('explicit remux preference invokes FFmpeg after exhausted native decoder retry', () => {
  const source = fs.readFileSync(playerPath,'utf8');
  const ast = ts.createSourceFile('Player.tsx',source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
  let body;
  const visit = node => {
    if(ts.isCallExpression(node) && node.expression.getText(ast)==='useEffect'){
      const text=node.arguments[0]?.getText(ast)||'';
      if(text.includes("nativePhase === 'error'") && text.includes('preferRemux')
        && text.includes('handleProgressiveDetected')) body=text;
    }
    ts.forEachChild(node,visit);
  };
  visit(ast);
  assert.ok(body,'The native error to remux recovery effect is missing');
  const run=ts.transpileModule('const run = '+body+';run();',{
    compilerOptions:{target:ts.ScriptTarget.ES2022}
  }).outputText;
  const choose=loader()(helpersPath).shouldRemuxAfterNativeDecoderFailure;
  function invoke(preferRemux,fileName,retries,memory=false,memoryPending=false){
    let calls=0;
    vm.runInNewContext(run,{
      nativePhase:'error',transcodeCapabilities:{available:true},
      preferRemux,file:{name:fileName},memoryMediaUrl:memory?'blob:private-qa':null,
      memoryLoading:memoryPending,fmp4StreamUrl:null,
      fmp4RemuxError:null,fmp4RemuxingRef:{current:false},
      nativeOriginalRetryRef:{current:retries},
      shouldRemuxAfterNativeDecoderFailure:choose,
      handleProgressiveDetected:()=>{calls++;},
    });
    return calls;
  }
  assert.equal(invoke(true,'fixture.mkv',1),1,
    'Explicit remux was discarded after the native decoder retry failed');
  assert.equal(invoke(false,'fixture.mp4',1),0,
    'An ordinary failed MP4 silently spent FFmpeg conversion resources');
  assert.equal(invoke(false,'fixture.mp4',0),1,
    'Initial native MP4 decoder errors lost their existing fallback');
  assert.equal(invoke(false,'fixture.mp4',0,true),0,
    'An opted-in local MP4 memory recovery was unexpectedly converted with FFmpeg');
  assert.equal(invoke(false,'fixture.mp4',0,false,true),0,
    'An active bounded memory download must never race with an automatic FFmpeg remux');
});

test('completed native decoder retries a short-duration source at most once without another remux', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let body;
  function visit(node) {
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'useEffect'
      && node.arguments[0].getText(ast).includes('nativeCompletedRetryRef.current')
      && node.arguments[0].getText(ast).includes('2500')) {
      body = node.arguments[0].getText(ast);
    }
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(body, 'There is no bounded recovery for the short-duration native fMP4 parser');
  const js = ts.transpileModule('const start = ' + body + '; start;', {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  let retryCalls = 0, errors = [], timer = null, detached = 0, started = 0;
  const retry = { current: 0 };
  const makeVideo = () => ({
    currentTime: 0.2, duration: 0.2, pause() { started++; },
    removeAttribute(name) { if (name === 'src') detached++; }, load() {},
  });
  const videoRef = { current: makeVideo() };
  const ctx = {
    fmp4StreamUrl: 'http://localhost/fmp4/qa/output.mp4',
    useFallback: true, playbackMode: 'original',
    remuxWindowRef: { current: { start: 0, complete: true } },
    sourceDurationRef: { current: 16 },
    remuxGenerationRef: { current: 2 }, fallbackVideoRef: videoRef,
    nativeCompletedRetryRef: retry, pendingSeekRef: { current: { time: 10, play: false } },
    nativeUserPauseRef: { current: true },
    setNativeAutoPlay(play) { assert.equal(play, false, 'Recovery restarted an explicitly paused video'); },
    setNativeError(message) { errors.push(message); }, setNativePhase() {},
    setNativeDecoderRevision(next) { retryCalls = next(retryCalls); },
    setRestartNonce() { throw Error('Finished decoder retry restarted the adaptive remux pipeline'); },
    setTimeout(fn, ms) { assert.equal(ms, 2500); timer = fn; return 1; },
    clearTimeout() {},
  };
  const check = vm.runInNewContext(js, ctx);
  check();
  assert.ok(timer);
  timer();
  assert.equal(retry.current, 1);
  assert.equal(retryCalls, 1);
  assert.equal(detached, 1);
  assert.equal(started, 1);
  assert.equal(ctx.pendingSeekRef.current.time, 10);
  videoRef.current = makeVideo();
  check();
  timer();
  assert.equal(retryCalls, 1, 'Native recovery retried indefinitely');
  assert.equal(errors.length, 1, 'A second decoder stall did not surface a visible error');
});

test('truncated original MP4 defers seek and decoder-only retry does not start remux', () => {
  const helpers = loader()(helpersPath);
  const video = {
    currentTime: 0.021, duration: 0.021, paused: false,
    seekable: ranges([0, 0.021]), buffered: ranges([0, 0.021]),
    play() { return Promise.resolve(); }, pause() {},
  };
  const pending = { current: null }, requests = [], positions = [];
  const ctx = {
    playbackMode: 'original', fmp4StreamUrl: null, useFallback: true,
    pendingSeekRef: pending, nativeOriginalRetryRef: { current: 0 },
    fmp4RemuxingRef: { current: false },
    remuxWindowRef: { current: { key: null, start: 0, complete: false } },
    sourceDurationRef: { current: 16 }, nativeUserPauseRef: { current: false },
    getActiveVideo: () => video, requestRemuxAt: t => requests.push(t),
    setMovieTime: t => positions.push(t), setNativeAutoPlay() {},
    setNativePaused() {}, savedTimeRef: { current: 0 }, ...helpers,
  };
  const seekTo = callback('seekTo', ctx);
  const restore = callback('restoreNativePosition', ctx);
  seekTo(10);
  assert.deepEqual(requests, [], 'The short original native timeline incorrectly triggered FFmpeg');
  assert.ok(pending.current, 'The truncated native decoder failed to save a pending seek');
  assert.equal(pending.current.time, 10);
  assert.equal(video.currentTime, 0.021, 'Native decoder seek clamped to its bogus EOF');
  // A second seek before the bounded decoder retry starts must still avoid
  // FFmpeg: the first seek is waiting on WebKit's truncated native parser.
  seekTo(10.5);
  assert.equal(pending.current.time, 10.5,
    'A second original MP4 seek before decoder recovery did not update the pending target');
  assert.deepEqual(requests, [], 'A second native original seek started an unnecessary FFmpeg remux');
  ctx.nativeOriginalRetryRef.current = 1;
  seekTo(11);
  assert.equal(pending.current.time, 11, 'Decoder retry lost a newer seek target');
  assert.deepEqual(requests, [], 'Seeking during native-only recovery triggered a new remux');
  restore(video);
  assert.equal(pending.current.time, 11, 'Restore applied position before native media became seekable');
  video.duration = 16;
  video.seekable = ranges([0, 16]);
  video.buffered = ranges([0, 16]);
  restore(video);
  assert.equal(video.currentTime, 11);
  assert.equal(pending.current, null);
});

test('completed ranged remux defers seeks until first metadata instead of re-encoding again', () => {
  const { shouldDeferCompletedRemuxSeek, planPlaybackSeek } = loader()(helpersPath);
  const video = { currentTime: 0.25, paused: false, duration: 0,
    seekable: ranges(), buffered: ranges() };
  assert.equal(shouldDeferCompletedRemuxSeek(video, true), true);
  assert.equal(shouldDeferCompletedRemuxSeek(video, false), false);
  video.seekable = ranges([0, 2]);
  assert.equal(shouldDeferCompletedRemuxSeek(video, true), false);
  video.seekable = ranges();
  const reremux = [], saved = [], pending = { current: null };
  const ctx = {
    playbackMode: 'original', pendingSeekRef: pending, useFallback: true,
    fmp4StreamUrl: 'http://localhost/fmp4/finished/output.mp4',
    remuxWindowRef: { current: { complete: true, start: 0, key: 'finished' } },
    sourceDurationRef: { current: 120 }, getActiveVideo: () => video,
    requestRemuxAt: time => reremux.push(time), setMovieTime: time => saved.push(time),
    nativeUserPauseRef: { current: false }, planPlaybackSeek, shouldDeferCompletedRemuxSeek,
  };
  callback('seekTo', ctx)(10.25);
  assert.equal(reremux.length, 0, 'Initial zero-duration native parser spawned a redundant new remux');
  assert.equal(pending.current.time, 10.25);
  assert.equal(pending.current.play, true);
  assert.deepEqual(saved, [10.25]);
  video.seekable = ranges([0, 0.2]); video.buffered = ranges([0, 0.2]);
  assert.equal(shouldDeferCompletedRemuxSeek(video, true, 10.25), true,
    'A tiny finished WebKit range cannot satisfy a later movie seek');
  callback('seekTo', ctx)(10.25);
  assert.deepEqual(reremux, [], 'A temporarily short seekable range must not launch another remux');
  assert.equal(video.currentTime, 0.25,
    'Seeking beyond a tiny native range clamps to EOF and stalls the player');
  assert.equal(pending.current.time, 10.25);
  video.seekable = ranges([0, 16]); video.buffered = ranges([0, 16]);
  assert.equal(shouldDeferCompletedRemuxSeek(video, true, 10.25), false);
  callback('seekTo', ctx)(10.25);
  assert.equal(video.currentTime, 10.25, 'The completed ranged file should seek once that time becomes available');
  pending.current = null;
  ctx.remuxWindowRef.current.start = 20;
  callback('seekTo', ctx)(10.25);
  assert.deepEqual(reremux, [10.25], 'Seeking before the remux window must start a new window');
});

test('completed remux handoff remounts native player and applies play intent before seekable ranges exist', () => {
  const source = fs.readFileSync(playerPath, 'utf8');
  const ast = ts.createSourceFile('Player.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let keyedNativeVideo = false, explicitResume = 0;
  function visit(node) {
    if (ts.isJsxOpeningElement(node) && node.tagName.getText(ast) === 'video') {
      const src = node.attributes.properties.find(p => ts.isJsxAttribute(p) && p.name.getText(ast) === 'src');
      const key = node.attributes.properties.find(p => ts.isJsxAttribute(p) && p.name.getText(ast) === 'key');
      if (src?.initializer?.expression?.getText(ast) === 'nativeMediaUrl') {
        keyedNativeVideo = key?.initializer?.expression?.getText(ast)
          === '`${nativeDecoderRevision}:${nativeMediaUrl}`';
      }
    }
    if (ts.isCallExpression(node) && node.expression.getText(ast) === 'setNativeAutoPlay'
        && node.arguments[0]?.getText(ast) === 'resume') explicitResume++;
    ts.forEachChild(node, visit);
  }
  visit(ast);
  assert.ok(keyedNativeVideo, 'The completed native stream reused a WebKit video element from its growing source');
  assert.ok(explicitResume >= 1, 'Completed remux did not allow requested play to prime the final HTTP range source');
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
test('explicit compatibility playback uses the existing HLS backend once and carries movie position', async () => {
  const called = [], states = [], urls = [];
  const context = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 10.5, paused: false }),
    useFallback: true, playbackMode: 'original',
    fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null },
    nativeUserPauseRef: { current: false },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: false },
    remuxGenerationRef: { current: 3 }, fmp4RemuxingRef: { current: false },
    hlsGenerationRef: { current: 0 }, hlsDesiredJobRef: { current: null },
    currentJobIdRef: { current: null }, pollTimerRef: { current: null },
    HLS_QUALITIES: ['360p', '480p', '720p', '1080p'],
    log() {}, activeFolderId: 5, file: { id: 9 },
    hlsQualityRef: { current: null }, hlsRef: { current: null },
    streamBaseRef: { current: 'http://localhost:1420' }, streamAuthRef: { current: 'qa' },
    abortMse() { called.push('abortMse'); },
    invoke: async (name, args) => {
      called.push({ name, args });
      if (name === 'cmd_prepare_transcoded_stream') {
        return { status: 'ready', playlist_url: '/hls/qa/index.m3u8' };
      }
      throw Error('Unexpected alternate route');
    },
    setHlsVariantStates(update) { assert.equal(typeof update, 'function'); },
    setHlsPlaylistUrl(value) { urls.push(value); },
    setPlaybackMode(value) { states.push(value); },
    ...Object.fromEntries([
      'Fmp4Remuxing', 'HlsQuality', 'HlsPhase', 'HlsProgress', 'HlsError',
    ].map(name => ['set' + name, () => {}])),
  });
  await callback('startTranscode', context)('480p');
  assert.equal(context.savedTimeRef.current, 10.5);
  assert.equal(context.savedPlaybackRef.current, true);
  assert.deepEqual(states, ['hls']);
  assert.equal(called.filter(item => item?.name === 'cmd_prepare_transcoded_stream').length, 1);
  const args = called.find(item => item?.name === 'cmd_prepare_transcoded_stream').args;
  assert.equal(args.messageId, 9);
  assert.equal(args.folderId, 5);
  assert.equal(args.quality, '480p');
  assert.equal(urls[0], null, 'Compatibility playback did not clear a previous HLS playlist');
  assert.equal(urls.at(-1), 'http://localhost:1420/hls/qa/index.m3u8?token=qa');
});

test('cancel HLS immediately invalidates old requests and cannot erase newer quality', async () => {
  let resolveCancel;
  const backend = new Promise(resolve => { resolveCancel = resolve; });
  const changes = [], cancelled = [];
  const oldHls = { destroy() { changes.push('destroy-old-hls'); } };
  const ctx = {
    hlsGenerationRef: { current: 17 },
    hlsDesiredJobRef: { current: '1_7_360p' },
    currentJobIdRef: { current: '1_7_360p' },
    pollTimerRef: { current: 55 },
    hlsRef: { current: oldHls },
    hlsQualityRef: { current: '360p' },
    clearInterval(id) { cancelled.push(id); },
    invoke(name, args) {
      assert.equal(name, 'cmd_cancel_transcode');
      assert.equal(args.jobId, '1_7_360p');
      return backend;
    },
    log() {},
    setPlaybackMode(v) { changes.push('mode:'+v); },
    setHlsPhase(v) { changes.push('phase:'+v); },
    setHlsQuality(v) { changes.push('quality:'+v); },
    setHlsPlaylistUrl(v) { changes.push('playlist:'+v); },
    setHlsError() {},setHlsVideoReady() {},
    setQuality(v) {changes.push('source-quality:'+v);},
    setRestartNonce(fn) {changes.push('restart:'+fn(3));},
  };
  const cancel = callback('cancelTranscode', ctx);
  assert.ok(cancel, 'HLS cancellation handler is missing');
  const operation = cancel();
  assert.equal(ctx.hlsGenerationRef.current, 18);
  assert.equal(ctx.currentJobIdRef.current, null,
    'Old job remained current while cancellation backend was pending');
  assert.equal(ctx.hlsDesiredJobRef.current, null);
  assert.deepEqual(cancelled,[55], 'Old poller kept running during backend cancellation');
  assert.ok(changes.includes('mode:original') && changes.includes('phase:idle'),
    'HLS UI stays active until the slow backend responds');
  ctx.hlsGenerationRef.current = 19;
  ctx.hlsDesiredJobRef.current = '1_7_480p';
  ctx.currentJobIdRef.current = '1_7_480p';
  changes.push('mode:hls');
  resolveCancel();
  await operation;
  assert.equal(ctx.currentJobIdRef.current, '1_7_480p',
    'Old cancellation overwrote the new quality job ID');
  assert.equal(changes.at(-1), 'mode:hls',
    'Late backend cancellation reset the new quality selection');
});

test('ready HLS preparation without a playlist fails instead of displaying an unusable player', async () => {
  const phases = [], errors = [], urls = [], variants = [];
  const context = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 1, paused: false }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null }, nativeUserPauseRef: { current: false },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    remuxGenerationRef: { current: 0 }, hlsGenerationRef: { current: 0 },
    hlsDesiredJobRef: { current: null }, fmp4RemuxingRef: { current: false },
    pollTimerRef: { current: null }, currentJobIdRef: { current: null },
    hlsQualityRef: { current: null }, hlsRef: { current: null },
    HLS_QUALITIES: ['480p'], activeFolderId: 5, file: { id: 9 },
    streamBaseRef: { current: 'http://localhost:1420' },
    streamAuthRef: { current: 'qa' },
    log() {}, abortMse() {},
    invoke: async () => ({ status: 'ready', job_id: '5_9_480p', playlist_url: null }),
    setHlsPhase(v) { phases.push(v); },
    setHlsError(v) { errors.push(v); },
    setHlsPlaylistUrl(v) { urls.push(v); },
    setHlsVariantStates(update) { variants.push(update({})); },
    ...Object.fromEntries(['Fmp4Remuxing','PlaybackMode','HlsQuality','HlsProgress']
      .map(k => ['set' + k, () => {}])),
  });
  await callback('startTranscode', context)('480p');
  assert.equal(phases.at(-1), 'failed', 'Ready-without-playlist incorrectly marked video as playable');
  assert.match(errors.at(-1), /playlist/i);
  assert.deepEqual(urls, [null]);
  assert.equal(variants.at(-1)['480p'], 'failed');
});

test('ready HLS poll without playlist fails and allows retry', async () => {
  let tick;
  const phases = [], errors = [], variants = [];
  const ctx = {
    hlsGenerationRef: { current: 1 },
    pollTimerRef: { current: null },
    currentJobIdRef: { current: '5_9_480p' },
    setInterval(fn) { tick = fn; return 71; },
    clearInterval() {}, log() {},
    invoke: async () => ({ job_id: '5_9_480p', status: 'ready', progress: 1, playlist_url: null }),
    setHlsPhase(v) { phases.push(v); },
    setHlsError(v) { errors.push(v); },
    setHlsProgress() {},
    setHlsVariantStates(update) { variants.push(update({})); },
    setHlsPlaylistUrl() { throw Error('Cannot publish a null HLS playlist'); },
    streamBaseRef: { current: 'http://localhost' }, streamAuthRef: { current: 'qa' },
  };
  callback('pollTranscodeStatus', ctx)('5_9_480p', '480p');
  await tick();
  assert.equal(phases.at(-1), 'failed', 'Ready poll published an unusable HLS state');
  assert.match(errors.at(-1), /playlist/i);
  assert.equal(variants.at(-1)['480p'], 'failed');
  assert.equal(ctx.pollTimerRef.current, null, 'Missing playlist kept polling indefinitely');
});

test('HLS prepare refuses cross-origin playlist before publishing the local bearer token', async () => {
  const events = [], urls = [];
  const ctx = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 1, paused: false }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null }, nativeUserPauseRef: { current: false },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    remuxGenerationRef: { current: 0 }, hlsGenerationRef: { current: 0 },
    hlsDesiredJobRef: { current: null }, fmp4RemuxingRef: { current: false },
    pollTimerRef: { current: null }, currentJobIdRef: { current: null },
    hlsQualityRef: { current: null }, hlsRef: { current: null },
    HLS_QUALITIES: ['480p'], activeFolderId: 5, file: { id: 9 },
    streamBaseRef: { current: 'http://127.0.0.1:4411' },
    streamAuthRef: { current: 'local&private' },
    log() {}, abortMse() {},
    invoke: async () => ({status:'ready',job_id:'5_9_480p',
      playlist_url:'https://external.example/hls/index.m3u8'}),
    setHlsPhase(v) {events.push('phase:'+v);},
    setHlsError(v) {events.push('error:'+v);},
    setHlsPlaylistUrl(v) {urls.push(v);},
    setHlsVariantStates() {},
    ...Object.fromEntries(['Fmp4Remuxing','PlaybackMode','HlsQuality','HlsProgress']
      .map(k => ['set' + k, () => {}])),
  });
  await callback('startTranscode',ctx)('480p');
  assert.ok(events.includes('phase:failed'));
  assert.ok(events.some(v=>v.includes('valid playlist')));
  assert.deepEqual(urls,[null],
    'Cross-origin HLS response was published with a local bearer token');
});

test('HLS status poll refuses cross-origin playlist before publishing local bearer token', async () => {
  let tick;
  const phases=[], urls=[], errors=[];
  const ctx={
    hlsGenerationRef:{current:1},pollTimerRef:{current:null},
    currentJobIdRef:{current:'5_9_480p'},
    setInterval(fn){tick=fn;return 71;},
    clearInterval(){},log(){},
    invoke:async()=>({job_id:'5_9_480p',status:'ready',progress:1,
      playlist_url:'//external.example/hls/index.m3u8'}),
    setHlsPhase(v){phases.push(v);},setHlsError(v){errors.push(v);},
    setHlsProgress(){},setHlsVariantStates(){},
    setHlsPlaylistUrl(v){urls.push(v);},
    streamBaseRef:{current:'http://127.0.0.1:4411'},
    streamAuthRef:{current:'local&private'},
  };
  callback('pollTranscodeStatus',ctx)('5_9_480p','480p');
  await tick();
  assert.equal(phases.at(-1),'failed');
  assert.match(errors.at(-1),/playlist/i);
  assert.deepEqual(urls,[],
    'Cross-origin HLS polling result was published with a local bearer token');
  assert.equal(ctx.pollTimerRef.current,null);
});

test('cancelled HLS status marks its quality failed and stops polling', async () => {
  let tick, clearCount = 0;
  const phases = [], errors = [], variants = [];
  const ctx = {
    hlsGenerationRef: {current: 1}, pollTimerRef: {current: null},
    currentJobIdRef: {current: '5_9_480p'},
    setInterval(fn) { tick = fn; return 71; },
    clearInterval(id) { assert.equal(id,71); clearCount++; },
    invoke: async () => ({ status: 'cancelled', job_id: '5_9_480p' }),
    setHlsPhase(value) { phases.push(value); }, setHlsError(value) { errors.push(value); },
    setHlsVariantStates(update) { variants.push(update({ '480p': 'transcoding' })); },
    setHlsProgress() {}, log() {},
  };
  callback('pollTranscodeStatus',ctx)('5_9_480p','480p');
  await tick();
  assert.deepEqual(phases,['failed']);
  assert.match(errors.at(-1), /Retry/);
  assert.equal(variants.at(-1)?.['480p'],'failed',
    'Cancelled transcode left its previous quality showing transcoding');
  assert.equal(ctx.currentJobIdRef.current,null);
  assert.equal(ctx.pollTimerRef.current,null);
  assert.equal(clearCount,1);
});

test('HLS polling fails visibly after persistent backend status errors', async () => {
  let tick, clearCount = 0, calls = 0;
  const phases=[], errors=[], variants=[];
  const ctx = {
    hlsGenerationRef:{current:1}, pollTimerRef:{current:null},
    currentJobIdRef:{current:'5_9_480p'},
    setInterval(fn){tick=fn;return 18;},clearInterval(){clearCount++;},
    invoke:async()=>{calls++;throw Error('temporary backend IPC unavailable');},
    setHlsPhase(value){phases.push(value);},
    setHlsError(value){errors.push(value);},
    setHlsVariantStates(update){variants.push(update({'480p':'preparing'}));},
    setHlsProgress(){},log(){},
  };
  callback('pollTranscodeStatus',ctx)('5_9_480p','480p');
  for(let attempt=1;attempt<5;attempt++){
    await tick();
    assert.equal(ctx.pollTimerRef.current,18,
      'Transient status failure ended retries prematurely');
    assert.equal(phases.length,0);
  }
  await tick();
  assert.equal(calls,5);
  assert.equal(phases.at(-1),'failed','Permanent IPC failure never surfaced to user');
  assert.match(errors.at(-1),/status|connection|Retry/i);
  assert.equal(variants.at(-1)?.['480p'],'failed');
  assert.equal(ctx.pollTimerRef.current,null);
  assert.equal(ctx.currentJobIdRef.current,null);
  assert.equal(clearCount,1);
});

test('a successful HLS status resets the transient IPC failure budget', async () => {
  let tick, requestNumber = 0;
  const phases = [], failures = [];
  const ctx = {
    hlsGenerationRef:{current:3}, pollTimerRef:{current:null},
    currentJobIdRef:{current:'1_7_360p'},
    setInterval(fn){tick=fn;return 72;},
    clearInterval(){throw Error('A recovered poll should not be cancelled');},
    invoke:async()=>{
      requestNumber += 1;
      if(requestNumber === 5)
        return {job_id:'1_7_360p',status:'caching',progress:0.5};
      throw Error('Transient status request failure');
    },
    setHlsPhase(value){phases.push(value);},setHlsProgress(){},
    setHlsError(value){failures.push(value);},
    setHlsVariantStates(){throw Error('Transient errors must not fail variant');},
    log(){},
  };
  callback('pollTranscodeStatus',ctx)('1_7_360p','360p');
  for(let i=0;i<6;i++) await tick();
  assert.equal(requestNumber,6);
  assert.deepEqual(phases,['caching']);
  assert.deepEqual(failures,[]);
  assert.equal(ctx.pollTimerRef.current,72);
  assert.equal(ctx.currentJobIdRef.current,'1_7_360p');
});

test('hanging HLS status request times out and never starts overlapping polls', async () => {
  let tick, expire, invoked=0, clearCount=0, timeoutClears=0;
  const phases=[], errors=[], variants=[];
  const ctx = {
    hlsGenerationRef:{current:2}, pollTimerRef:{current:null},
    currentJobIdRef:{current:'5_9_480p'},
    setInterval(fn){tick=fn;return 60;},clearInterval(){clearCount++;},
    setTimeout(fn,ms){assert.equal(ms,8000);expire=fn;return 99;},
    clearTimeout(id){assert.equal(id,99);timeoutClears++;},
    invoke(){invoked++;return new Promise(()=>{});},
    log(){},setHlsPhase(v){phases.push(v);},setHlsError(v){errors.push(v);},
    setHlsVariantStates(update){variants.push(update({'480p':'preparing'}));},
    setHlsProgress(){},
  };
  callback('pollTranscodeStatus',ctx)('5_9_480p','480p');
  let pending=tick();
  assert.equal(typeof expire,'function',
    'Hanging native status IPC has no bounded request timeout');
  await tick(); // A second interval firing must not overlap the hung IPC.
  assert.equal(invoked,1,'Concurrent HLS poll requests accumulated');
  for(let attempt=1;attempt<=5;attempt++){
    expire();
    await pending;
    if(attempt<5){
      pending=tick();
      assert.equal(invoked,attempt+1);
    }
  }
  assert.equal(phases.at(-1),'failed');
  assert.match(errors.at(-1),/Retry/);
  assert.equal(variants.at(-1)?.['480p'],'failed');
  assert.equal(ctx.pollTimerRef.current,null);
  assert.equal(clearCount,1);
  assert.equal(timeoutClears,5);
});

test('obsolete HLS preparation never publishes its playlist and only cancels its owned job', async () => {
  let resolvePrepare;
  const cancellations = [], phases = [], playlistUrls = [];
  const context = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 13, paused: false }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null }, nativeUserPauseRef: { current: false },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    remuxGenerationRef: { current: 1 }, hlsGenerationRef: { current: 2 }, hlsDesiredJobRef: { current: null },
    fmp4RemuxingRef: { current: false }, pollTimerRef: { current: null },
    currentJobIdRef: { current: null }, hlsQualityRef: { current: null },
    hlsRef: { current: null }, HLS_QUALITIES: ['480p'],
    file: { id: 7 }, activeFolderId: 5, log() {}, abortMse() {},
    invoke(name, args) {
      if (name === 'cmd_prepare_transcoded_stream') {
        return new Promise(resolve => { resolvePrepare = resolve; });
      }
      if (name === 'cmd_cancel_transcode') {
        cancellations.push(args.jobId);
        return Promise.resolve(true);
      }
      throw Error('Unexpected native/FFmpeg operation ' + name);
    },
    setHlsPhase(v) { phases.push(v); }, setHlsPlaylistUrl(v) { playlistUrls.push(v); },
    ...Object.fromEntries(['Fmp4Remuxing','PlaybackMode','HlsQuality',
      'HlsProgress','HlsError'].map(k => ['set' + k, () => {}])),
  });
  const promise = callback('startTranscode', context)('480p');
  assert.equal(context.hlsGenerationRef.current, 3);
  context.hlsGenerationRef.current += 1; // File change or user cancellation
  resolvePrepare({ status: 'pending', job_id: 'owned-obsolete', playlist_url: null });
  await promise;
  assert.deepEqual(cancellations, ['owned-obsolete']);
  assert.deepEqual(phases, ['preparing'], 'Obsolete conversion changed phase after cancellation');
  assert.deepEqual(playlistUrls, [null], 'Old HLS playlist resurfaced after cancellation');
});

test('obsolete HLS request does not cancel a newer request for the SAME deterministic job ID', async () => {
  const pending = [], cancelled = [], urls = [], phases = [];
  const context = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 14, paused: false }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null }, nativeUserPauseRef: { current: false },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    remuxGenerationRef: { current: 1 }, hlsGenerationRef: { current: 0 },
    hlsDesiredJobRef: { current: null }, fmp4RemuxingRef: { current: false },
    pollTimerRef: { current: null }, currentJobIdRef: { current: null },
    hlsQualityRef: { current: null }, hlsRef: { current: null },
    HLS_QUALITIES: ['480p'], activeFolderId: 5, file: { id: 7 },
    streamBaseRef: { current: 'http://localhost' }, streamAuthRef: { current: 'qa' },
    log() {}, abortMse() {},
    invoke(name, args) {
      if (name === 'cmd_prepare_transcoded_stream') {
        return new Promise(resolve => pending.push(resolve));
      }
      if (name === 'cmd_cancel_transcode') {
        cancelled.push(args.jobId);
        return Promise.resolve();
      }
      throw Error('Unexpected command');
    },
    setHlsPhase(v) { phases.push(v); },
    setHlsPlaylistUrl(v) { urls.push(v); },
    setHlsVariantStates() {},
    ...Object.fromEntries(['Fmp4Remuxing','PlaybackMode','HlsQuality','HlsProgress','HlsError']
      .map(k => ['set' + k, () => {}])),
  });
  const start = callback('startTranscode', context);
  const first = start('480p');
  const second = start('480p');
  const job = '5_7_480p';
  assert.equal(context.hlsDesiredJobRef.current, job);
  pending[0]({ status: 'pending', job_id: job, playlist_url: null });
  await first;
  assert.deepEqual(cancelled, [], 'A stale request cancelled its replacement\'s shared job');
  pending[1]({ status: 'ready', job_id: job, playlist_url: '/hls/5_7/480p/index.m3u8' });
  await second;
  assert.ok(urls.at(-1)?.startsWith('http://localhost/hls/5_7/480p/index.m3u8?'),
    'Newer preparation failed to publish its own playlist');
  assert.deepEqual(cancelled, [], 'A completed shared job was cancelled after being reused');
  assert.equal(phases.at(-1), 'ready');
});

test('stale HLS ready reply does not invalidate already cached shared output', async () => {
  let resolvePrepare;
  const cancelled = [], urls = [];
  const context = wireMoviePosition({
    getActiveVideo: () => ({ currentTime: 0, paused: true }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: null,
    remuxWindowRef: { current: { start: 0, key: null } },
    pendingSeekRef: { current: null }, nativeUserPauseRef: { current: true },
    savedTimeRef: { current: 0 }, savedPlaybackRef: { current: false },
    remuxGenerationRef: { current: 0 }, hlsGenerationRef: { current: 0 },
    hlsDesiredJobRef: { current: null }, fmp4RemuxingRef: { current: false },
    pollTimerRef: { current: null }, currentJobIdRef: { current: null },
    hlsQualityRef: { current: null }, hlsRef: { current: null },
    HLS_QUALITIES: ['480p'], activeFolderId: 5, file: { id: 7 },
    log() {}, abortMse() {},
    invoke(name, args) {
      if (name === 'cmd_prepare_transcoded_stream')
        return new Promise(resolve => { resolvePrepare = resolve; });
      if (name === 'cmd_cancel_transcode') {
        cancelled.push(args.jobId);
        return Promise.resolve();
      }
      throw Error('Unexpected command');
    },
    setHlsPlaylistUrl(value) { urls.push(value); },
    ...Object.fromEntries(['Fmp4Remuxing','PlaybackMode','HlsQuality','HlsPhase',
      'HlsProgress','HlsError','HlsVariantStates'].map(k => ['set' + k, () => {}])),
  });
  const prepare = callback('startTranscode', context)('480p');
  context.hlsGenerationRef.current += 1;
  context.hlsDesiredJobRef.current = null;
  resolvePrepare({ status: 'ready', job_id: '5_7_480p', playlist_url: '/hls/old/index.m3u8' });
  await prepare;
  assert.deepEqual(cancelled, [], 'Obsolete ready response invalidated finished cached HLS');
  assert.deepEqual(urls, [null], 'Obsolete ready response displayed a stale playlist');
});

test('stale in-flight HLS status replies do not republish a cancelled playlist', async () => {
  let onTick, resolveStatus;
  const phases = [], urls = [];
  const context = {
    hlsGenerationRef: { current: 8 },
    pollTimerRef: { current: null },
    currentJobIdRef: { current: 'owned-old-job' },
    setInterval(fn, ms) { assert.equal(ms, 1000); onTick = fn; return 23; },
    clearInterval() {}, log() {},
    invoke(name, args) {
      assert.equal(name, 'cmd_get_transcode_status');
      assert.equal(args.jobId, 'owned-old-job');
      return new Promise(resolve => { resolveStatus = resolve; });
    },
    setHlsPhase(v) { phases.push(v); }, setHlsPlaylistUrl(v) { urls.push(v); },
    setHlsProgress() {}, setHlsError() {}, setHlsVariantStates() {},
  };
  callback('pollTranscodeStatus', context)('owned-old-job', '480p');
  assert.equal(context.pollTimerRef.current, 23);
  const tick = onTick();
  context.hlsGenerationRef.current += 1; // Cancellation while IPC is in flight
  resolveStatus({ job_id: 'owned-old-job', status: 'ready', progress: 1,
    error: null, playlist_url: '/hls/obsolete/master.m3u8' });
  await tick;
  assert.deepEqual(phases, [], 'Obsolete status reply changed active player state');
  assert.deepEqual(urls, [], 'Old HLS status reply republished its playlist');
});

test('selecting HLS quality saves the absolute position of a remux window', async () => {
  const context = wireMoviePosition({ getActiveVideo: () => ({ currentTime: 3.25, paused: true }),
    useFallback: true, playbackMode: 'original', fmp4StreamUrl: 'http://localhost/fmp4/window/output.mp4',
    movieTime: 5403.25, remuxWindowRef: { current: { start: 5400, key: 'window' } },
    pendingSeekRef: { current: null }, savedTimeRef: { current: 0 }, savedPlaybackRef: { current: true },
    nativeUserPauseRef: { current: true },
    remuxGenerationRef: { current: 1 }, fmp4RemuxingRef: { current: false },
    hlsGenerationRef: { current: 0 }, hlsDesiredJobRef: { current: null },
    currentJobIdRef: { current: null }, pollTimerRef: { current: null },
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
    hlsGenerationRef: { current: 0 }, hlsDesiredJobRef: { current: null },
    currentJobIdRef: { current: null },
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
    currentJobIdRef: { current: 'hls-job' }, hlsGenerationRef: { current: 0 },
    hlsDesiredJobRef: { current: 'hls-job' },
    invoke: async name => { assert.equal(name, 'cmd_cancel_transcode'); },
    transcodeCapabilities: { available: true }, log() {},
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
    nativeOriginalRetryRef: { current: 0 },
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
