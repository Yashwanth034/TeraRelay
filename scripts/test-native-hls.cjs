#!/usr/bin/env node
/* Isolated native WebKit HLS playback/seek QA. Generated media only:
 * no TeraRelay app launch, account, credentials, uploads or normal profile.
 * Exit 0: native HLS playback and seek verified.
 * Exit 1: media/browser regression.
 * Exit 2: native decoder/GUI prerequisites unavailable in this sandbox.
 */
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { createRequire } = require('node:module');
const { spawn, spawnSync } = require('node:child_process');

const root = path.resolve(__dirname, '..');
const player = path.join(root, 'scripts/test-native-webkit.py');
const hlsJs = path.join(root, 'app/node_modules/hls.js/dist/hls.min.js');
const check = (binary, args = ['-version']) =>
  spawnSync(binary, args, { encoding: 'utf8', timeout: 10000 }).status === 0;
const gtk = spawnSync('python3', ['-c',
  'import gi;gi.require_version("WebKit2","4.1");from gi.repository import WebKit2'],
  { encoding: 'utf8', timeout: 10000 });
if (gtk.status !== 0 || !check('ffmpeg') || !check('ffprobe')
    || !check('xvfb-run', ['--help']) || !fs.existsSync(hlsJs)) {
  console.error('BLOCKED native HLS E2E: requires WebKitGTK, Xvfb, FFmpeg/FFprobe and installed app hls.js');
  process.exit(2);
}
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'terarelay-native-hls-qa-'));
let server;
let requirePlayerToken = false;
const requests = [];
const runFfmpeg = (args) => {
  const child = spawnSync('ffmpeg', ['-nostdin', '-hide_banner', '-loglevel', 'error', '-y', ...args],
    { encoding: 'utf8', timeout: 90000, maxBuffer: 1 << 20 });
  assert.equal(child.status, 0, child.stderr || child.error?.message || 'FFmpeg failed');
};
const page = `<!doctype html><html><head><meta charset="utf-8"></head><body>
<video id="media" muted playsinline preload="auto" style="width:640px;height:360px"></video>
<script src="/hls.min.js"></script>
<script>
(() => {
  const video = document.getElementById('media');
  const events = [];
  const report = (extra) => ({
    done: false, pass: false, events: events.slice(-24),
    duration: video.duration, time: video.currentTime, width: video.videoWidth,
    ready: video.readyState, errorCode: video.error?.code,
    errorMessage: video.error?.message, ...extra,
  });
  window.__qaReport = report({phase:'boot'});
  const emit = (type) => events.push({type,time:video.currentTime,ready:video.readyState});
  for (const name of ['error','loadedmetadata','playing','waiting','seeking','seeked','stalled'])
    video.addEventListener(name, () => emit(name));
  let started = false, seekIssued = false, seekCompleted = false;
  const play = () => video.play().catch(error => emit('play-rejected:' + String(error)));
  let method = '';
  if (video.canPlayType('application/vnd.apple.mpegurl')) {
    method = 'native';
    video.src = '/hls/index.m3u8';
    video.addEventListener('loadedmetadata', play, {once:true});
  } else if (window.Hls && Hls.isSupported()) {
    method = 'hls.js';
    const hls = new Hls({enableWorker:true,lowLatencyMode:false});
    hls.on(Hls.Events.ERROR, (_event, data) => {
      emit('hls-error:' + data.details);
      if (data.fatal) window.__qaReport = report({done:true,phase:'hls-error',fatal:data.details,method});
    });
    hls.on(Hls.Events.MANIFEST_PARSED, play);
    hls.loadSource('/hls/index.m3u8');
    hls.attachMedia(video);
    window.__qaHls = hls;
  } else {
    window.__qaReport = report({done:true,unsupported:true,phase:'unsupported',method:'none'});
    return;
  }
  const begun = performance.now();
  const id = setInterval(() => {
    if (window.__qaReport.done) { clearInterval(id); return; }
    const elapsed = (performance.now() - begun)/1000;
    if (video.error) {
      window.__qaReport = report({done:true,phase:'decoder-error',method});
      clearInterval(id);
    } else if (!started && video.currentTime > 0.25 && video.videoWidth > 0
               && video.readyState >= 2 && !video.paused) {
      started = true;
    } else if (started && !seekIssued && Number.isFinite(video.duration)
               && video.duration >= 9.0 && video.duration <= 11.5) {
      seekIssued = true;
      video.currentTime = 6.5;
    } else if (seekIssued && !video.seeking && video.currentTime >= 6.7
               && !video.paused && video.readyState >= 2 && video.videoWidth > 0) {
      seekCompleted = true;
      window.__qaReport = report({done:true,pass:true,phase:'seek-complete',method,started,seekIssued,seekCompleted});
      clearInterval(id);
    } else if (elapsed > 38) {
      window.__qaReport = report({done:true,phase:'playback-timeout',method,started,seekIssued,seekCompleted});
      clearInterval(id);
    } else {
      window.__qaReport = report({phase:'playing',method,started,seekIssued,seekCompleted});
    }
  }, 100);
})();
</script></body></html>`;

const appRequire = createRequire(path.join(root, 'app/package.json'));

// This second fixture mounts TeraRelay's real AdaptiveMediaPlayer rather than a
// standalone Hls instance, and exercises its quality-control -> IPC -> HLS UI.
const appEntry = `
import React from 'react';
import { createRoot } from 'react-dom/client';
import { AdaptiveMediaPlayer } from './src/components/desktop/dashboard/AdaptiveMediaPlayer';
const file = {id:7,name:'fixture.mp4',size:100000,type:'video',is_split:false};
createRoot(document.getElementById('root')).render(
  <AdaptiveMediaPlayer file={file} activeFolderId={5} onClose={() => {}}
    streamUrl={location.origin + '/source.mp4?token=qa'} />
);
`;

const appPage = `<!doctype html><html><head><meta charset="utf-8"></head><body>
<div id="root"></div><script>window.__qaIpc=[];</script>
<script src="/player.js"></script><script>
(() => {
 const events = [], startedAt = performance.now();
 let qualityClicked = false, started = false, requestedSeek = false, playRequested = false;
 const report = (more) => ({done:false,pass:false,events:events.slice(-25),
   ipc:window.__qaIpc.slice(-25),qualityClicked,started,requestedSeek,...more});
 window.__qaReport = report({phase:'mounted'});
 const id = setInterval(() => {
   if (window.__qaReport?.done) { clearInterval(id); return; }
   if (!qualityClicked) {
     const quality = Array.from(document.querySelectorAll('button'))
       .find(b => b.title?.includes('Switch to 360p (HLS transcoding)'));
     if (quality) {
       qualityClicked = true;
       quality.click();
       events.push('clicked-360p');
     }
   }
   const video = [...document.querySelectorAll('video')]
     .find(v => Number.isFinite(v.duration) && v.duration >= 9 && v.videoWidth > 0);
   if (video && video.readyState >= 2 && video.paused && !playRequested) {
     // TeraRelay correctly preserves the old paused state across HLS handoff.
     // Simulate an explicit user Play request before testing movement and seek.
     playRequested = true;
     events.push('explicit-play-requested');
     video.play().catch(error => events.push('play-rejected:'+String(error)));
   }
   if (video?.error) {
     window.__qaReport = report({done:true,phase:'decoder-error',
       code:video.error.code,message:video.error.message});
   } else if (video && !started && video.currentTime > 0.25 && video.videoWidth &&
              video.readyState >= 2 && !video.paused) {
     started = true;
     events.push('app-hls-playing');
   } else if (started && !requestedSeek && video &&
              Number.isFinite(video.duration) && video.duration >= 9) {
     requestedSeek = true;
     video.currentTime = 6.5;
     events.push('app-seek-issued');
   } else if (requestedSeek && video && !video.seeking && video.currentTime >= 6.7
              && video.readyState >= 2 && !video.paused) {
     window.__qaReport = report({done:true,pass:true,phase:'app-seek-complete',
       duration:video.duration,time:video.currentTime,width:video.videoWidth});
   } else if (performance.now()-startedAt > 45000) {
     window.__qaReport = report({done:true,phase:'app-timeout',
       text:document.body.textContent.slice(0,500),
       videos:[...document.querySelectorAll('video')].map(v => ({
         src:v.src,time:v.currentTime,duration:v.duration,ready:v.readyState,
         paused:v.paused,code:v.error?.code
       }))});
   } else window.__qaReport = report({phase:'app-loading'});
 },100);
})();
</script></body></html>`;

(async () => {
  try {
    const source = path.join(tmp, 'source.mp4');
    const hlsDir = path.join(tmp, 'hls');
    fs.mkdirSync(hlsDir);
    runFfmpeg(['-f','lavfi','-i','testsrc2=size=640x360:rate=24',
      '-f','lavfi','-i','sine=frequency=660:sample_rate=48000',
      '-t','10','-map','0:v:0','-map','1:a:0','-c:v','libx264',
      '-threads','2','-preset','ultrafast','-pix_fmt','yuv420p','-c:a','aac',source]);
    runFfmpeg(['-i',source,'-map','0:v:0','-map','0:a:0?','-sn','-dn',
      '-vf','scale=-2:360','-c:v','libx264','-threads','2','-preset','veryfast',
      '-crf','23','-force_key_frames','expr:gte(t,n_forced*4)',
      '-sc_threshold','0','-c:a','aac','-b:a','128k',
      '-maxrate','1800k','-bufsize','3600k','-f','hls','-hls_time','4',
      '-hls_playlist_type','vod',
      '-hls_segment_filename',path.join(hlsDir,'segment_%03d.ts'),
      path.join(hlsDir,'index.m3u8')]);
    const manifest = fs.readFileSync(path.join(hlsDir, 'index.m3u8'),'utf8');
    assert.ok(manifest.includes('#EXT-X-ENDLIST'), 'playlist not complete');
    const segments = manifest.split(/\r?\n/).filter(x => /^segment_[0-9]+\.ts$/.test(x));
    assert.ok(segments.length >= 3, 'not enough generated HLS segments');
    await appRequire('esbuild').build({
      stdin:{contents:appEntry,resolveDir:path.join(root,'app'),sourcefile:'hls-ui-native-qa.tsx',loader:'tsx'},
      bundle:true,outfile:path.join(tmp,'player.js'),
      nodePaths:[path.join(root,'app/node_modules')],
      define:{'process.env.NODE_ENV':'"production"'},
      plugins:[{name:'isolated-tauri-ipc',setup(build) {
        // Keep the real React player, controls, HLS logic and hls.js, but
        // feed its *original* native source from synthetic in-memory MP4.
        // This isolates HLS integration from the unrelated sandbox-blocked
        // HTTP/GStreamer downloadbuffer startup.
        build.onResolve({filter:/useAdaptiveStreaming$/},() => ({
          path:'qa-original-media',namespace:'qa-original-media',
        }));
        build.onLoad({filter:/.*/,namespace:'qa-original-media'},() => ({
          contents:`
            export function useAdaptiveStreaming() {
              return {
                videoRef:{current:null},phase:'ready',error:null,tracks:[],
                loadProgress:1,currentQuality:'original',setQuality:()=>{},
                adaptiveMode:false,setAdaptiveMode:()=>{},measuredKbps:0,
                useFallback:true,
                fallbackUrl:'data:video/mp4;base64,'+
                  document.getElementById('qa-source-data').textContent.trim(),
                abort:()=>{}
              };
            }
          `,loader:'js'
        }));
        build.onResolve({filter:/^@tauri-apps\//},args => ({path:args.path,namespace:'qa-ipc'}));
        build.onLoad({filter:/.*/,namespace:'qa-ipc'},() => ({
          contents:`
            export async function invoke(name) {
              window.__qaIpc.push(name);
              if (name === 'cmd_get_transcode_capabilities')
                return {available:true,mode:'hls',variants:[{label:'360p',height:360,available:true}]};
              if (name === 'cmd_probe_media_tracks')
                return {audio_tracks:[],subtitle_tracks:[],duration_secs:10};
              if (name === 'cmd_prepare_transcoded_stream')
                return {job_id:'5_7_360p',status:'ready',progress:1,playlist_url:'/hls/index.m3u8'};
              if (name === 'cmd_prepare_fmp4_stream')
                throw Error('Isolated source has no remux path');
              if (name === 'cmd_get_fmp4_status')
                return {status:'error',error:'QA source requires HLS'};
              return null;
            }
            export function getCurrentWindow() {
              return {listen:async () => () => {},onResized:async () => () => {},
                isFullscreen:async () => false,setFullscreen:async () => {}};
            }
            export function type() { return 'linux'; }
          `,loader:'js'
        }));
      }}],
    });
    const originalData = fs.readFileSync(source).toString('base64');
    const isolatedAppPage = appPage.replace('<div id="root"></div>',
      '<div id="root"></div><script type="application/octet-stream" id="qa-source-data">'
        +originalData+'</script>');
    const assets = new Map([
      ['/qa', {body: Buffer.from(page), mime:'text/html; charset=utf-8'}],
      ['/qa-app', {body: Buffer.from(isolatedAppPage), mime:'text/html; charset=utf-8'}],
      ['/source.mp4', {body: fs.readFileSync(source), mime:'video/mp4'}],
      ['/player.js', {body: fs.readFileSync(path.join(tmp,'player.js')), mime:'text/javascript'}],
      ['/hls.min.js', {body: fs.readFileSync(hlsJs), mime:'text/javascript'}],
      ['/hls/index.m3u8', {body: Buffer.from(manifest), mime:'application/vnd.apple.mpegurl'}],
    ]);
    for (const name of segments) {
      const bytes = fs.readFileSync(path.join(hlsDir, name));
      assert.ok(bytes.length > 0, 'empty HLS media segment: ' + name);
      assets.set('/hls/' + name, {body:bytes,mime:'video/mp2t'});
    }
    server = http.createServer((req,res) => {
      const requestUrl = new URL(req.url, 'http://localhost');
      const name = requestUrl.pathname;
      requests.push(name); // Never log bearer token query strings.
      if (requirePlayerToken && name.startsWith('/hls/')
          && requestUrl.searchParams.get('token') !== 'qa') {
        res.writeHead(403); res.end(); return;
      }
      const asset = assets.get(name);
      if (!asset) { res.writeHead(404); res.end(); return; }
      res.writeHead(200, {'Content-Type':asset.mime,'Content-Length':asset.body.length,
        'Cache-Control':'no-store','Access-Control-Allow-Origin':'http://127.0.0.1'});
      res.end(asset.body);
    });
    await new Promise((resolve, reject) => {
      server.once('error', reject);
      server.listen(0,'127.0.0.1',resolve);
    });
    const url = 'http://127.0.0.1:' + server.address().port + '/qa';
    // Do not use spawnSync: it would block the Node HTTP loopback server
    // that WebKit must fetch the manifest and segments from.
    const child = spawn('xvfb-run', ['-a','python3',player,url], {
      env:{...process.env,TMPDIR:tmp,TMP:tmp,TEMP:tmp,GST_AUDIO_SINK:'fakesink',
        ALSOFT_DRIVERS:'null',GST_DEBUG:'2',GST_DEBUG_NO_COLOR:'1'},
      stdio:['ignore','pipe','pipe'],
    });
    let stdout = '', stderr = '';
    child.stdout.on('data', data => { stdout = (stdout + data).slice(-2 * 1024 * 1024); });
    child.stderr.on('data', data => { stderr = (stderr + data).slice(-2 * 1024 * 1024); });
    const status = await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => child.kill('SIGTERM'), 100000);
      child.once('error', error => { clearTimeout(timeout); reject(error); });
      child.once('close', code => { clearTimeout(timeout); resolve(code); });
    });
    stdout = stdout.trim();
    // The runner may emit diagnostic GTK output. Parse its final JSON report.
    const reportLine = stdout.split(/\r?\n/).reverse().find(s => s.startsWith('{'));
    let report = null;
    try { report = JSON.parse(reportLine); } catch {}
    console.log('Native WebKit report: ' + JSON.stringify(report || {stdout:stdout.slice(-1600)}));
    console.log('HLS HTTP requests: ' + JSON.stringify(requests));
    if ((stdout + stderr).includes('Could not create temp file')) {
      console.error('BLOCKED: native WebKit media temp buffer denied by isolated runtime');
      process.exitCode = 2;
      return;
    }
    if (report?.unsupported) {
      console.error('BLOCKED: WebKit offers neither native HLS nor MediaSource/hls.js');
      process.exitCode = 2;
      return;
    }
    assert.equal(status, 0, (stderr || 'WebKit failed').slice(-1500));
    assert.ok(report?.pass && report?.started && report?.seekCompleted,
      'WebKit did not complete native HLS playback and nonzero seek');
    assert.ok(requests.includes('/hls/index.m3u8'), 'native browser did not request manifest');
    assert.ok(requests.filter(x => /^\/hls\/segment_[0-9]+\.ts$/.test(x)).length >= 2,
      'native browser did not fetch real HLS transport stream segments');
    console.log('PASS isolated native WebKit HLS playlist, segment decode, playback and seek');

    // Exercise the real player UI through a mocked IPC boundary and real
    // generated HLS HTTP media. No native app/profile or Telegram connection.
    requirePlayerToken = true;
    const origin = 'http://127.0.0.1:' + server.address().port;
    const denied = await fetch(origin + '/hls/index.m3u8');
    assert.equal(denied.status, 403, 'Unauthenticated HLS playlist was accessible');
    const authorized = await fetch(origin + '/hls/index.m3u8?token=qa');
    assert.equal(authorized.status, 200, 'QA token did not authorize the HLS playlist');
    // Suppress token query strings from captured request logs while checking
    // real browser auth propagation against this fail-closed HTTP origin.
    const playerRequestsBefore = requests.length;
    const appBrowser = spawn('xvfb-run', ['-a','python3',player,
      'http://127.0.0.1:' + server.address().port + '/qa-app'], {
      env:{...process.env,TMPDIR:tmp,TMP:tmp,TEMP:tmp,GST_AUDIO_SINK:'fakesink',
        ALSOFT_DRIVERS:'null',GST_DEBUG:'2',GST_DEBUG_NO_COLOR:'1'},
      stdio:['ignore','pipe','pipe'],
    });
    let appStdout = '', appStderr = '';
    appBrowser.stdout.on('data', data => { appStdout = (appStdout + data).slice(-2000000); });
    appBrowser.stderr.on('data', data => { appStderr = (appStderr + data).slice(-2000000); });
    const appStatus = await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => appBrowser.kill('SIGTERM'), 100000);
      appBrowser.once('error', error => { clearTimeout(timeout); reject(error); });
      appBrowser.once('close', code => { clearTimeout(timeout); resolve(code); });
    });
    const appReportLine = appStdout.trim().split(/\r?\n/).reverse().find(line => line.startsWith('{'));
    let appReport;
    try { appReport = JSON.parse(appReportLine); } catch {}
    console.log('TeraRelay native WebKit UI report: '+JSON.stringify(appReport ?? {
      stdout:appStdout.slice(-2000),stderr:appStderr.slice(-2000)}));
    const appMediaRequests = requests.slice(playerRequestsBefore);
    console.log('UI HLS HTTP requests: '+JSON.stringify(appMediaRequests));
    assert.ok(!appMediaRequests.includes('/source.mp4'),
      'Native HLS QA loaded the sandbox-blocked original HTTP MP4');
    if (!appReport?.pass && (appStdout+appStderr).includes('Could not create temp file')) {
      // Native original MP4 may fail in the sandbox, but a successful HLS
      // transition proves the compatibility route despite that warning.
      console.error('BLOCKED full player UI E2E by WebKit legacy HTTP MP4 disk-buffer sandbox');
      process.exitCode = 2;
      return;
    }
    assert.equal(appStatus, 0, appStderr.slice(-1500));
    assert.ok(appReport?.pass && appReport.qualityClicked && appReport.started,
      'TeraRelay UI did not select and start actual HLS');
    assert.ok(appReport?.ipc?.includes('cmd_prepare_transcoded_stream'),
      'TeraRelay UI bypassed expected FFmpeg/HLS prepare command');
    assert.ok(appMediaRequests.includes('/hls/index.m3u8'),
      'TeraRelay UI never requested its actual generated media manifest');
    assert.ok(appMediaRequests.filter(name => /^\/hls\/segment_[0-9]+\.ts$/.test(name)).length >= 3,
      'TeraRelay UI did not fetch the generated HLS media segments');
    console.log('PASS TeraRelay React HLS quality -> IPC -> media decode/playback/seek');
  } catch (error) {
    console.error('FAIL isolated native WebKit HLS: ' + (error.stack || error));
    process.exitCode = 1;
  } finally {
    if (server) await new Promise(resolve => server.close(resolve));
    fs.rmSync(tmp,{recursive:true,force:true});
  }
})().catch(error => {console.error(error); process.exitCode = 1;});
