/* Real player/decoder QA; no account, Telegram session, or user files. */
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');
const { createRequire } = require('node:module');
const { spawn, spawnSync, execFileSync } = require('node:child_process');
const assert = require('node:assert/strict');
const root = path.resolve(__dirname, '..');
const appRequire = createRequire(path.join(root, 'app/package.json'));
const deps = spawnSync('python3', ['-c', 'import gi; gi.require_version("WebKit2", "4.1"); from gi.repository import WebKit2'], { encoding: 'utf8' });
if (deps.status !== 0 || spawnSync('sh', ['-c', 'command -v xvfb-run && command -v ffmpeg']).status !== 0) {
  console.error('BLOCKED native media E2E: requires WebKitGTK, Xvfb and FFmpeg');
  process.exit(2);
}
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'terarelay-native-qa-'));
let server;
let requests = [];
const entry = `
import React from 'react';
import { createRoot } from 'react-dom/client';
import { AdaptiveMediaPlayer } from './src/components/desktop/dashboard/AdaptiveMediaPlayer';
const variant = new URLSearchParams(location.search).get('case') || 'single';
const ipc = window.__qaIpc;
const events = [];
const logs = [];
for (const name of ['log','error','warn']) {
  const original = console[name];
  console[name] = (...args) => { logs.push(name + ': ' + args.map(String).join(' ')); original(...args); };
}
new MutationObserver(() => {
  for (const video of document.querySelectorAll('video')) {
    if (video.dataset.qaObserved) continue;
    video.dataset.qaObserved = '1';
    for (const type of ['error','playing','pause','waiting','loadedmetadata','stalled']) {
      video.addEventListener(type, () => events.push({ type, src: video.src, time: video.currentTime, paused: video.paused, error: video.error?.message, code: video.error?.code }));
    }
  }
}).observe(document.getElementById('root'), { childList: true, subtree: true });
const source = variant === 'remux' ? '/source.mkv'
  : variant === 'finished' ? '/fmp4/qa/output.mp4'
  : variant === 'memory' ? '/stream/qa/7'
  : '/' + (variant === 'blob' ? 'single' : variant) + '.mp4';
const file = { id: 7, name: variant === 'remux' ? 'fixture.mkv' : 'fixture.mp4', size: variant === 'memory' ? window.__qaMediaSize : 1000000, type: 'video', is_split: variant === 'multipart' };
// A separate blob fixture exercises the actual native WebKit video decoder
// without its HTTP GstDownloadBuffer. This is NOT proof that HTTP range/seek
// playback works; that remains a distinct required native QA case.
async function mountPlayer() {
  let streamUrl = location.origin + source + '?token=qa';
  if (variant === 'blob') {
    const response = await fetch(streamUrl);
    if (!response.ok) throw Error('Could not fetch isolated blob media fixture');
    streamUrl = URL.createObjectURL(await response.blob());
  }
  createRoot(document.getElementById('root')).render(
    <AdaptiveMediaPlayer file={file} activeFolderId={1} onClose={() => {}} streamUrl={streamUrl} preferRemux={variant === 'remux'} />
  );
}
mountPlayer().catch(error => {
  window.__qaReport = { done: true, pass: false, variant, error: 'Fixture mount failed: ' + String(error) };
});
const wait = async (predicate, label) => {
  window.__qaReport = { done: false, pass: false, phase: label };
  const end = performance.now() + 35000;
  while (performance.now() < end) {
    window.__qaReport.videos = [...document.querySelectorAll('video')].map(v=>({ src:v.src, time:v.currentTime, duration:v.duration, paused:v.paused, ready:v.readyState, seeking:v.seeking, seekable:v.seekable.length, error:v.error?.code }));
    const result = predicate();
    if (result) return result;
    await new Promise(r => setTimeout(r, 100));
  }
  throw Error('Timed out: ' + label);
};
(async () => {
 try {
  if (variant === 'memory') {
    // Exercise the actual React error action, not a standalone blob decoder.
    const httpVideo = await wait(() => [...document.querySelectorAll('video')].find(v =>
      v.src.includes('/stream/qa/7')), 'original HTTP MP4 element');
    httpVideo.dispatchEvent(new Event('error'));
    const action = await wait(() => document.querySelector('button[aria-label="Try in-memory MP4 playback"]'),
      'small-file memory recovery option');
    action.click();
    await wait(() => [...document.querySelectorAll('video')].find(v => v.src.startsWith('blob:')),
      'in-memory native source handoff');
  }
  let requestedPlay = '';
  let video = await wait(() => {
    const candidate = [...document.querySelectorAll('video')].find(v => v.videoWidth > 0 && v.readyState >= 2);
    if (candidate?.paused && requestedPlay !== candidate.src) {
      requestedPlay = candidate.src;
      candidate.play().catch(error => events.push({ type: 'play-rejected', error: String(error) }));
    }
    return candidate && candidate.currentTime > 0.3 && !candidate.paused ? candidate : null;
  }, 'initial playback');
  if (document.body.textContent.includes('Playback Error')) throw Error('Inactive parser error covers playback');
  if (variant === 'remux') {
    // Native WebKit may fail the original MKV decoder before the user changes
    // audio tracks. That legitimate automatic fallback can prepare one remux
    // already; measure the explicit switch relative to the previous count.
    const preparesBeforeAudioSwitch = ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length;
    const originalDecoderFailedBeforeAudioSwitch = events.some(e =>
      e.type === 'error' && e.src.includes('/source.mkv'));
    const select = await wait(() => document.querySelector('select'), 'audio selector');
    select.value = '2';
    select.dispatchEvent(new Event('change', { bubbles: true }));
    await wait(() => ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length > preparesBeforeAudioSwitch,
      'audio switch starts its remux');
    video = await wait(() => [...document.querySelectorAll('video')].find(v => v.src.includes('/fmp4/') && v.videoWidth > 0 && v.currentTime > 0.3 && !v.paused), 'growing remux playback');
    const preparesAfterAudioSwitch = ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length;
    if (preparesAfterAudioSwitch !== preparesBeforeAudioSwitch + 1)
      throw Error('Audio switch started multiple remux preparations');
    if (preparesBeforeAudioSwitch > 0 &&
        (!originalDecoderFailedBeforeAudioSwitch || !ipc.includes('cmd_cancel_fmp4_stream')))
      throw Error('Unexplained extra remux before audio switch or uncancelled stale conversion');
    await wait(() => ipc.filter(x=>x === 'cmd_get_fmp4_status').length >= 2, 'final remux completion after early playback');
    video = await wait(() => [...document.querySelectorAll('video')].find(v => v.src.includes('/fmp4/') && v.src.includes('__refresh_') && !v.src.includes('__refresh_1/') && v.videoWidth > 0 && !v.paused), 'final ranged remux replaces live source');
    const before = video.currentTime;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    // A stalled *completed* decoder is allowed one bounded replacement.
    // Observe the CURRENT video node, not the old detached _refresh_2
    // element, or a correctly recovered native player is falsely reported
    // as a timeout even after playing at the requested position.
    video = await wait(() => [...document.querySelectorAll('video')].find(v =>
      v.src.includes('/fmp4/') && v.src.includes('__refresh_')
      && !v.src.includes('__refresh_1/')
      && v.videoWidth > 0 && v.currentTime >= before + 9.5
      && v.currentTime < 14 && !v.paused && !v.seeking
    ), 'seek after live remux finishes');
    if (ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length !== preparesAfterAudioSwitch)
      throw Error('Completed remux seek unexpectedly triggered another conversion');
  } else {
    if (ipc.includes('cmd_prepare_fmp4_stream') && !events.some(e => e.type === 'error')) throw Error('Playable MP4 triggered unnecessary remux');
    const before = video.currentTime;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    // Decoder-only recovery changes the underlying <video> element. Observe
    // the CURRENT player, not a detached element that can no longer seek.
    video = await wait(() => [...document.querySelectorAll('video')].find(v =>
      v.videoWidth > 0 && v.currentTime >= before + 9.5 && !v.seeking
    ), 'seek by ten seconds');
    await wait(() => [...document.querySelectorAll('video')].some(v =>
      v.videoWidth > 0 && v.currentTime >= before + 9.5
      && !v.paused && v.readyState >= 2
    ), 'playback after seeking');
  }
  window.__qaReport = { done: true, pass: true, variant, width: video.videoWidth, time: video.currentTime, native: !!video.getAttribute('src')?.startsWith('http'), remuxCalls: ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length };
 } catch(error) {
  window.__qaReport = { done: true, pass: false, variant, error: String(error), text: document.body.textContent.slice(0, 500), ipc, events, logs: logs.slice(-25), videos: [...document.querySelectorAll('video')].map(v=>({ src:v.src, time:v.currentTime, paused:v.paused, ready:v.readyState, duration:v.duration, networkState:v.networkState, seekable:[...Array(v.seekable.length)].map((_,i)=>[v.seekable.start(i),v.seekable.end(i)]), buffered:[...Array(v.buffered.length)].map((_,i)=>[v.buffered.start(i),v.buffered.end(i)]), error:v.error?.message })) };
 }
})();
`;
async function runBrowser(url) {
  // Native AI Workspace QA already owns an isolated X server on :91. Test
  // directly there when explicitly requested instead of nesting xvfb-run,
  // which can select a different synthetic GPU/media-driver configuration.
  // Refuse direct mode on the user's normal desktop.
  const direct = global.process.argv.includes('--direct-display');
  if (direct && global.process.env.DISPLAY !== ':91') {
    throw new Error('Direct-display native QA requires isolated DISPLAY=:91');
  }
  const command = direct ? 'python3' : 'xvfb-run';
  const args = direct
    ? [path.join(root, 'scripts/test-native-webkit.py'), url]
    : ['-a', 'python3', path.join(root, 'scripts/test-native-webkit.py'), url];
  const process = spawn(command, args, {
    env: { ...global.process.env, TMPDIR: tmp, TMP: tmp, TEMP: tmp, GST_AUDIO_SINK: 'fakesink', ALSOFT_DRIVERS: 'null', GST_DEBUG: '2', GST_DEBUG_NO_COLOR: '1' }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '', stderr = '';
  process.stdout.on('data', data => { stdout += data; });
  process.stderr.on('data', data => { stderr += data; });
  const status = await new Promise((resolve, reject) => {
    process.once('error', reject); process.once('close', resolve);
  });
  console.log(stdout.trim());
  if (status !== 0 && (stdout + stderr).includes('Could not create temp file')) {
    const error = new Error('WebKit media buffer directory is unavailable in this isolated runtime');
    error.environmentUnavailable = true;
    throw error;
  }
  assert.equal(status, 0, stderr.slice(-2500));
}
(async () => {
 try {
  const single = path.join(tmp, 'single.mp4'), faststart = path.join(tmp, 'faststart.mp4'), source = path.join(tmp, 'source.mkv'), remux = path.join(tmp, 'remux.mp4');
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y',
    '-f','lavfi','-i','testsrc2=size=640x360:rate=24',
    '-f','lavfi','-i','sine=frequency=440:sample_rate=48000',
    '-f','lavfi','-i','sine=frequency=880:sample_rate=48000',
    '-t','16','-map','0:v:0','-map','1:a:0','-map','2:a:0',
    '-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-g','48',
    '-c:a','aac', source]);
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y','-i',source,'-map','0:v:0','-map','0:a:0','-c','copy',single]);
  // Identical packets, but moov metadata moved to the start. Isolate
  // WebKitGTK's short-duration failure on moov-at-end HTTP responses.
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y','-i',single,'-c','copy','-movflags','+faststart',faststart]);
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y','-i',source,'-map','0:v:0','-map','0:2','-c','copy','-movflags','frag_keyframe+empty_moov+default_base_moof','-frag_duration','2000000','-f','mp4',remux]);
  await appRequire('esbuild').build({
    stdin: { contents: entry, resolveDir: path.join(root, 'app'), sourcefile: 'native-qa.tsx', loader: 'tsx' },
    bundle: true, outfile: path.join(tmp, 'player.js'), nodePaths: [path.join(root, 'app/node_modules')],
    define: { 'process.env.NODE_ENV': '"production"' },
    plugins: [{ name: 'qa-ipc', setup(build) {
      build.onResolve({ filter: /^@tauri-apps\// }, args => ({ path: args.path, namespace: 'qa-ipc' }));
      build.onLoad({ filter: /.*/, namespace: 'qa-ipc' }, () => ({ contents: `
        export async function invoke(name) {
          window.__qaIpc.push(name);
          if (name === 'cmd_get_transcode_capabilities') return { available: new URLSearchParams(location.search).get('case') !== 'memory', variants: [], mode: 'original' };
          if (name === 'cmd_probe_media_tracks') return { audio_tracks: [{ index: 1, language: 'eng', codec: 'aac' }, { index: 2, language: 'tel', codec: 'aac' }], subtitle_tracks: [], duration_secs: 16 };
          if (name === 'cmd_prepare_fmp4_stream') {
            if (new URLSearchParams(location.search).get('case') === 'remux') return { url: '/fmp4/live/output.mp4', output_file_key: 'live', status: 'processing' };
            return { url: '/fmp4/qa/output.mp4', output_file_key: 'qa', status: 'ready' };
          }
          if (name === 'cmd_get_fmp4_status') return fetch('/qa-status').then(r=>r.json());
          return null;
        }
        export function getCurrentWindow() { return { listen: async () => () => {}, onResized: async () => () => {}, isFullscreen: async () => false }; }
        export function type() { return 'linux'; }
      `, loader: 'js' }));
    } }],
  });
  if (spawnSync('sh', ['-c', 'command -v gst-launch-1.0']).status === 0) {
    for (const media of [single, source, remux]) {
      execFileSync('gst-launch-1.0', ['-q', 'playbin', 'uri=' + require('node:url').pathToFileURL(media).href,
        'flags=3', 'audio-sink=fakesink sync=false', 'video-sink=fakesink sync=false'], { timeout: 30000 });
    }
    console.log('PASS GStreamer decodes generated MP4, two-audio MKV, and selected-audio fragmented MP4');
  }
  const mp4 = fs.readFileSync(single);
  const split = [mp4.subarray(0, 620013), mp4.subarray(620013, 1212037), mp4.subarray(1212037)];
  assert.deepEqual(Buffer.concat(split), mp4);
  const assets = { '/single.mp4': mp4, '/faststart.mp4': fs.readFileSync(faststart), '/multipart.mp4': Buffer.concat(split), '/source.mkv': fs.readFileSync(source), '/fmp4/qa/output.mp4': fs.readFileSync(remux), '/player.js': fs.readFileSync(path.join(tmp,'player.js')) };
  const css = fs.readdirSync(path.join(root,'app/dist/assets')).filter(n=>n.endsWith('.css')).map(n=>fs.readFileSync(path.join(root,'app/dist/assets',n),'utf8')).join('\n');
  // Both no-store spellings are valid HTTP. Keep the native experiment
  // selectable without allowing untrusted arbitrary response headers.
  const qaCachePolicy = process.env.TERA_QA_NO_STORE === '1'
    ? (process.env.TERA_QA_PLAIN_NO_STORE === '1' ? 'no-store' : 'private, no-store')
    : null;
  requests = [];
  let liveFinished = false;
  server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname === '/qa') {
      res.setHeader('Content-Type','text/html');
      res.end('<!doctype html><html><head><style>'+css+'</style></head><body><div id="root"></div><script>window.__qaIpc=[];window.__qaMediaSize='+mp4.length+'</script><script src="/player.js"></script></body></html>');
      return;
    }
    if (url.pathname === '/qa-status') {
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ status: 'ready', error: null, complete: liveFinished }));
      return;
    }
    // The production service maps path-versioned media aliases to the same
    // owned fMP4 file. Keep the native QA origin faithful to that behavior,
    // and record the actual request path to verify WebKit refetched it.
    const remuxPath = url.pathname
      .replace(/^\/fmp4\/live__refresh_[0-9]{1,10}\/output\.mp4$/,
        '/fmp4/live/output.mp4')
      .replace(/^\/fmp4\/qa__refresh_[0-9]{1,10}\/output\.mp4$/,
        '/fmp4/qa/output.mp4');
    if (remuxPath === '/fmp4/live/output.mp4' && !liveFinished) {
      res.writeHead(200, { 'Content-Type': 'video/mp4', 'Accept-Ranges': 'none', 'Cache-Control': 'no-store' });
      const bytes = fs.readFileSync(remux);
      let offset = 0;
      const timer = setInterval(() => {
        const end = Math.min(bytes.length, offset + 65536);
        res.write(bytes.subarray(offset,end)); offset = end;
        if (offset === bytes.length) { clearInterval(timer); liveFinished = true; assets['/fmp4/live/output.mp4'] = bytes; res.end(); }
      }, 25);
      res.once('close', () => clearInterval(timer));
      return;
    }
    const bytes = assets[remuxPath === '/stream/qa/7' ? '/single.mp4' : remuxPath];
    if (!bytes) { res.writeHead(404); res.end(); return; }
    const mime = url.pathname.endsWith('.js') ? 'text/javascript' : url.pathname.endsWith('.mkv') ? 'video/x-matroska' : 'video/mp4';
    const record = {
      path: url.pathname,
      method: req.method,
      range: req.headers.range || '',
      status: 0,
      expectedBytes: 0,
      responseFinished: false,
      disconnectedBeforeFinish: false,
    };
    requests.push(record);
    res.once('finish', () => { record.responseFinished = true; });
    res.once('close', () => {
      if (!record.responseFinished) record.disconnectedBeforeFinish = true;
    });
    const range = req.headers.range?.match(/^bytes=(\d+)-(\d*)$/);
    const start = range ? Number(range[1]) : 0;
    const end = range && range[2] ? Math.min(Number(range[2]), bytes.length-1) : bytes.length-1;
    if (start > end) { res.writeHead(416, { 'Content-Range': 'bytes */'+bytes.length }); res.end(); return; }
    record.status = range ? 206 : 200;
    record.expectedBytes = end - start + 1;
    res.writeHead(record.status, { 'Content-Type':mime,'Accept-Ranges':'bytes','Content-Length':record.expectedBytes,...(qaCachePolicy && mime === 'video/mp4' ? { 'Cache-Control':qaCachePolicy } : {}),...(range ? { 'Content-Range': 'bytes '+start+'-'+end+'/'+bytes.length } : {}) });
    res.end(bytes.subarray(start,end+1));
  });
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  const check = await fetch('http://127.0.0.1:'+server.address().port+'/single.mp4', { headers: { Range: 'bytes=10-31' } });
  assert.equal(check.status, 206);
  assert.deepEqual(Buffer.from(await check.arrayBuffer()), mp4.subarray(10,32));
  assert.equal(check.headers.get('cache-control'), qaCachePolicy,
    'A ranged MP4 must follow the production MP4-specific browser cache policy');
  const fullHeaders = await fetch('http://127.0.0.1:'+server.address().port+'/single.mp4', { method: 'HEAD' });
  assert.equal(fullHeaders.status, 200);
  assert.equal(Number(fullHeaders.headers.get('content-length')), mp4.length);
  assert.equal(fullHeaders.headers.get('cache-control'), qaCachePolicy,
    'A full MP4 must use the same cache policy as 206 ranges');
  const mkvCheck = await fetch('http://127.0.0.1:'+server.address().port+'/source.mkv', { headers: { Range: 'bytes=0-3' } });
  assert.equal(mkvCheck.status, 206);
  assert.deepEqual(Buffer.from(await mkvCheck.arrayBuffer()), fs.readFileSync(source).subarray(0, 4));
  assert.equal(mkvCheck.headers.get('cache-control'), null,
    'MP4-only QA no-store must not disable unrelated MKV caching');
  const layout = path.join(tmp, 'folder-ui.html');
  execFileSync(process.execPath, [path.join(root, 'scripts/test-focused-ui.cjs'), '--fixture'], { cwd: root, env: { ...process.env, TERA_QA_UI_OUTPUT: layout } });
  await runBrowser(layout);
  const base = 'http://127.0.0.1:'+server.address().port+'/qa?case=';
  if (process.argv.includes('--blob-only')) {
    // Independent native codec/seek diagnostic, never substitute for HTTP QA.
    await runBrowser(base + 'blob');
    console.log('PASS native WebKit blob-media player: MP4 decoder and seek only');
  } else if (process.argv.includes('--remux-only')) {
    // Targeted replay of the real remux handoff without re-running unrelated
    // MP4 cases that already passed in the same native environment.
    await runBrowser(base + 'remux');
    assert.ok(requests.some(x => x.path === '/fmp4/live__refresh_2/output.mp4'),
      'Native media engine reused a growing-media URL instead of refetching the completed file');
    console.log('PASS native WebKit HTTP player: remux and seek');
  } else if (process.argv.includes('--faststart-only')) {
    // Controlled native test for otherwise identical MP4 packets with moov
    // moved to the beginning by FFmpeg. Do not change production media.
    await runBrowser(base + 'faststart');
    assert.ok(requests.some(x => x.path === '/faststart.mp4' && x.responseFinished),
      'Faststart HTTP MP4 fixture did not complete a media response');
    console.log('PASS native WebKit HTTP faststart MP4 and seek');
  } else if (process.argv.includes('--memory-only')) {
    // Test the real React opt-in action against a generated authenticated
    // loopback MP4 stream, not a pre-created Blob passed as a player prop.
    await runBrowser(base + 'memory');
    assert.ok(requests.some(x => x.path === '/stream/qa/7' && x.status === 200
      && x.responseFinished && x.expectedBytes === mp4.length),
      'Bounded in-memory recovery did not receive the complete local MP4');
    console.log('PASS native React opt-in bounded in-memory MP4 playback and seeking');
  } else if (process.argv.includes('--single-only')) {
    // Isolate native HTTP MP4 parsing; report whether the fixture sent its
    // entire response if the native decoder still reports a tiny duration.
    await runBrowser(base + 'single');
    assert.ok(requests.some(x => x.path === '/single.mp4' && x.responseFinished && x.expectedBytes === mp4.length),
      'The QA origin failed to finish a complete native MP4 HTTP response');
    console.log('PASS native WebKit HTTP single MP4 and seek with full server response');
  } else if (process.argv.includes('--finished-only')) {
    // Diagnose the exact finalized fMP4 bytes in WebKit without an in-place
    // source switch. A failure here is a decoder/HTTP format problem rather
    // than the growing-to-completed player handoff.
    await runBrowser(base + 'finished');
    console.log('PASS native WebKit HTTP finished fragmented MP4 and seek');
  } else if (process.argv.includes('--multipart-only')) {
    // Isolate intermittent WebKit range-media initialization for the same MP4
    // bytes that pass in the single-document case.
    await runBrowser(base + 'multipart');
    console.log('PASS native WebKit HTTP player: multipart MP4 and seek');
  } else {
    for (const variant of ['single','multipart','remux']) await runBrowser(base+variant);
    assert.ok(requests.some(x => x.path === '/fmp4/live__refresh_2/output.mp4'),
      'Completed fMP4 never issued an independent native media fetch');
    console.log('PASS native WebKit HTTP player: single MP4, multipart MP4, seeking and remux');
  }
  console.log('Range requests: '+JSON.stringify(requests.filter(x=>x.range).slice(-12)));
 } catch(error) {
  if (error.environmentUnavailable) {
    console.error('BLOCKED WebKit playback/seek E2E: ' + error.message +
      '; generated decoder/range/UI checks passed where reported, but media E2E did not run');
    process.exitCode = 2;
  } else {
    console.error(error); console.error('Fixture requests: '+JSON.stringify(requests)); process.exitCode = 1;
  }
 } finally {
  if (server) await new Promise(resolve=>server.close(resolve));
  fs.rmSync(tmp,{ recursive:true,force:true });
 }
})();
