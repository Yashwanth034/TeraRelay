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
  console.log('SKIP native decoder QA: requires WebKitGTK, Xvfb and FFmpeg');
  process.exit(0);
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
const source = variant === 'remux' ? '/source.mkv' : '/' + variant + '.mp4';
const file = { id: 7, name: variant === 'remux' ? 'fixture.mkv' : 'fixture.mp4', size: 1000000, type: 'video', is_split: variant === 'multipart' };
createRoot(document.getElementById('root')).render(
  <AdaptiveMediaPlayer file={file} activeFolderId={1} onClose={() => {}} streamUrl={location.origin + source + '?token=qa'} preferRemux={variant === 'remux'} />
);
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
    const select = await wait(() => document.querySelector('select'), 'audio selector');
    select.value = '2';
    select.dispatchEvent(new Event('change', { bubbles: true }));
    video = await wait(() => [...document.querySelectorAll('video')].find(v => v.src.includes('/fmp4/') && v.videoWidth > 0 && v.currentTime > 0.3 && !v.paused), 'growing remux playback');
    if (ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length !== 1) throw Error('Audio switch did not use one remux');
    await wait(() => ipc.filter(x=>x === 'cmd_get_fmp4_status').length >= 2, 'final remux completion after early playback');
    video = await wait(() => [...document.querySelectorAll('video')].find(v => v.src.includes('/fmp4/') && v.src.includes('_r=2') && v.videoWidth > 0 && !v.paused), 'final ranged remux replaces live source');
    const before = video.currentTime;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    await wait(() => video.currentTime >= before + 9.5 && video.currentTime < 14 && !video.paused && !video.seeking, 'seek after live remux finishes');
  } else {
    if (ipc.includes('cmd_prepare_fmp4_stream') && !events.some(e => e.type === 'error')) throw Error('Playable MP4 triggered unnecessary remux');
    const before = video.currentTime;
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
    await wait(() => video.currentTime >= before + 9.5 && !video.seeking, 'seek by ten seconds');
    await wait(() => !video.paused && video.readyState >= 2, 'playback after seeking');
  }
  window.__qaReport = { done: true, pass: true, variant, width: video.videoWidth, time: video.currentTime, native: !!video.getAttribute('src')?.startsWith('http'), remuxCalls: ipc.filter(x => x === 'cmd_prepare_fmp4_stream').length };
 } catch(error) {
  window.__qaReport = { done: true, pass: false, variant, error: String(error), text: document.body.textContent.slice(0, 500), ipc, events, logs: logs.slice(-25), videos: [...document.querySelectorAll('video')].map(v=>({ src:v.src, time:v.currentTime, paused:v.paused, ready:v.readyState, error:v.error?.message })) };
 }
})();
`;
async function runBrowser(url) {
  const process = spawn('xvfb-run', ['-a', 'python3', path.join(root, 'scripts/test-native-webkit.py'), url], {
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
  const single = path.join(tmp, 'single.mp4'), source = path.join(tmp, 'source.mkv'), remux = path.join(tmp, 'remux.mp4');
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y',
    '-f','lavfi','-i','testsrc2=size=640x360:rate=24',
    '-f','lavfi','-i','sine=frequency=440:sample_rate=48000',
    '-f','lavfi','-i','sine=frequency=880:sample_rate=48000',
    '-t','16','-map','0:v:0','-map','1:a:0','-map','2:a:0',
    '-c:v','libx264','-preset','ultrafast','-pix_fmt','yuv420p','-g','48',
    '-c:a','aac', source]);
  execFileSync('ffmpeg', ['-hide_banner','-loglevel','error','-y','-i',source,'-map','0:v:0','-map','0:a:0','-c','copy',single]);
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
          if (name === 'cmd_get_transcode_capabilities') return { available: true, variants: [], mode: 'original' };
          if (name === 'cmd_probe_media_tracks') return { audio_tracks: [{ index: 1, language: 'eng', codec: 'aac' }, { index: 2, language: 'tel', codec: 'aac' }], subtitle_tracks: [] };
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
  const assets = { '/single.mp4': mp4, '/multipart.mp4': Buffer.concat(split), '/source.mkv': fs.readFileSync(source), '/fmp4/qa/output.mp4': fs.readFileSync(remux), '/player.js': fs.readFileSync(path.join(tmp,'player.js')) };
  const css = fs.readdirSync(path.join(root,'app/dist/assets')).filter(n=>n.endsWith('.css')).map(n=>fs.readFileSync(path.join(root,'app/dist/assets',n),'utf8')).join('\n');
  requests = [];
  let liveFinished = false;
  server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname === '/qa') {
      res.setHeader('Content-Type','text/html');
      res.end('<!doctype html><html><head><style>'+css+'</style></head><body><div id="root"></div><script>window.__qaIpc=[]</script><script src="/player.js"></script></body></html>');
      return;
    }
    if (url.pathname === '/qa-status') {
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({ status: 'ready', error: null, complete: liveFinished }));
      return;
    }
    if (url.pathname === '/fmp4/live/output.mp4' && !liveFinished) {
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
    const bytes = assets[url.pathname];
    if (!bytes) { res.writeHead(404); res.end(); return; }
    const mime = url.pathname.endsWith('.js') ? 'text/javascript' : url.pathname.endsWith('.mkv') ? 'video/x-matroska' : 'video/mp4';
    requests.push({ path: url.pathname, method: req.method, range: req.headers.range || '' });
    const range = req.headers.range?.match(/^bytes=(\d+)-(\d*)$/);
    const start = range ? Number(range[1]) : 0;
    const end = range && range[2] ? Math.min(Number(range[2]), bytes.length-1) : bytes.length-1;
    if (start > end) { res.writeHead(416, { 'Content-Range': 'bytes */'+bytes.length }); res.end(); return; }
    res.writeHead(range ? 206 : 200, { 'Content-Type':mime,'Accept-Ranges':'bytes','Content-Length':end-start+1,...(range ? { 'Content-Range': 'bytes '+start+'-'+end+'/'+bytes.length } : {}) });
    res.end(bytes.subarray(start,end+1));
  });
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  const check = await fetch('http://127.0.0.1:'+server.address().port+'/single.mp4', { headers: { Range: 'bytes=10-31' } });
  assert.equal(check.status, 206);
  assert.deepEqual(Buffer.from(await check.arrayBuffer()), mp4.subarray(10,32));
  const layout = path.join(tmp, 'folder-ui.html');
  execFileSync(process.execPath, [path.join(root, 'scripts/test-focused-ui.cjs'), '--fixture'], { cwd: root, env: { ...process.env, TERA_QA_UI_OUTPUT: layout } });
  await runBrowser(layout);
  const base = 'http://127.0.0.1:'+server.address().port+'/qa?case=';
  for (const variant of ['single','multipart','remux']) await runBrowser(base+variant);
  console.log('PASS real WebKit player: single MP4, multipart logical MP4, ten-second seeking, audio-switch growing fMP4');
  console.log('Range requests: '+JSON.stringify(requests.filter(x=>x.range).slice(-12)));
 } catch(error) {
  if (error.environmentUnavailable) {
    console.log('SKIP WebKit playback/seek QA: ' + error.message + '; decoder, range and UI checks still run');
  } else {
    console.error(error); console.error('Fixture requests: '+JSON.stringify(requests)); process.exitCode = 1;
  }
 } finally {
  if (server) await new Promise(resolve=>server.close(resolve));
  fs.rmSync(tmp,{ recursive:true,force:true });
 }
})();
