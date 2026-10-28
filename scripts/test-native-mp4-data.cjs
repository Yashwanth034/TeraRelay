#!/usr/bin/env node
// Native WebKit MP4 diagnostic isolated from HTTP downloadbuffer.
// Only FFmpeg-generated QA media and an ephemeral WebKitGTK profile.
// This does NOT use the installed application, user data, or Telegram.
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { spawnSync } = require('node:child_process');
const { createRequire } = require('node:module');
const appRequire = createRequire(path.resolve(__dirname,'../app/package.json'));
const runner = path.join(__dirname, 'test-native-webkit.py');
const check = (binary, args) =>
  spawnSync(binary, args, {encoding:'utf8',timeout:10000}).status === 0;
const webkit = spawnSync('python3', ['-c',
  'import gi;gi.require_version("WebKit2","4.1");from gi.repository import WebKit2'],
  {encoding:'utf8',timeout:10000});
if (webkit.status !== 0 || !check('ffmpeg',['-version']) ||
    !check('xvfb-run',['--help'])) {
  console.error('BLOCKED: WebKitGTK, Xvfb or FFmpeg not installed');
  process.exit(2);
}
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'terarelay-mp4-data-qa-'));
(async () => {
try {
  const media = path.join(temp,'synthetic.mp4');
  const ffmpeg = spawnSync('ffmpeg', [
    '-nostdin','-hide_banner','-loglevel','error','-y',
    '-f','lavfi','-i','testsrc2=size=320x180:rate=24',
    '-f','lavfi','-i','sine=frequency=523:sample_rate=48000',
    '-t','10','-map','0:v:0','-map','1:a:0',
    '-c:v','libx264','-threads','2','-preset','ultrafast',
    '-pix_fmt','yuv420p','-c:a','aac','-movflags','+faststart',media,
  ],{encoding:'utf8',timeout:80000,maxBuffer:1024*1024});
  if(ffmpeg.status!==0) throw Error(ffmpeg.stderr || 'FFmpeg failed');
  const data = fs.readFileSync(media).toString('base64');
  const script = [
    "(function(){",
    "const v=document.getElementById('media'), events=[];",
    "const start=performance.now(); let began=false,seek=false;",
    "window.__qaReport={done:false,phase:'boot'};",
    "for(const name of ['loadedmetadata','playing','error','seeking','seeked'])",
    "  v.addEventListener(name,()=>events.push({name,time:v.currentTime,duration:v.duration}));",
    "const poll=setInterval(()=>{",
    "  const base={done:false,phase:'waiting',duration:v.duration,time:v.currentTime,",
    "    paused:v.paused,ready:v.readyState,width:v.videoWidth,",
    "    errorCode:v.error?.code,errorMessage:v.error?.message,events:events.slice(-16)};",
    "  if(v.error) window.__qaReport={...base,done:true,phase:'unsupported-media'};",
    "  else if(!began && v.currentTime>0.25 && v.videoWidth>0 && !v.paused) began=true;",
    "  else if(began && !seek && Number.isFinite(v.duration) &&",
    "          v.duration>=9 && v.duration<=11.5) {seek=true;v.currentTime=6.5;}",
    "  else if(seek && !v.seeking && v.currentTime>=6.7 && v.readyState>=2 && !v.paused)",
    "    window.__qaReport={...base,done:true,pass:true,phase:'seek-complete'};",
    "  else if(performance.now()-start>22000)",
    "    window.__qaReport={...base,done:true,phase:'timeout',began,seek};",
    "  else window.__qaReport=base;",
    "  if(window.__qaReport.done)clearInterval(poll);",
    "},100);",
    "})();",
  ].join('\n');
  const html = '<!doctype html><meta charset="utf-8"><video id="media" muted autoplay playsinline preload="auto" src="data:video/mp4;base64,'
      +data+'"></video><script>'+script+'</script>';
  const page = path.join(temp,'diagnostic.html');
  fs.writeFileSync(page,html,{mode:0o600});
  const browser = spawnSync('xvfb-run',['-a','python3',runner,page],{
    encoding:'utf8',timeout:85000,maxBuffer:2*1024*1024,
    env:{...process.env,TMPDIR:temp,TMP:temp,TEMP:temp,
      GST_AUDIO_SINK:'fakesink',ALSOFT_DRIVERS:'null',GST_DEBUG:'2'},
  });
  const line = (browser.stdout||'').trim().split(/\r?\n/).reverse()
    .find(value=>value.startsWith('{'));
  let report=null;
  try {report=JSON.parse(line);} catch {}
  console.log('Isolated native data-MP4 report: '+JSON.stringify(report||{
    stdout:(browser.stdout||'').slice(-900),stderr:(browser.stderr||'').slice(-900)}));
  if(report?.pass && browser.status===0) {
    console.log('PASS data-MP4 native WebKit duration/decode/seek independent of HTTP');

    // Run the actual TeraRelay React player on the same in-memory media.
    // This separates any AdaptiveMediaPlayer decoder state regression from
    // WebKit's sandbox-blocked native HTTP downloadbuffer.
    await appRequire('esbuild').build({
      stdin: {contents: "\nimport React from 'react';\nimport {createRoot} from 'react-dom/client';\nimport {AdaptiveMediaPlayer} from './src/components/desktop/dashboard/AdaptiveMediaPlayer';\nconst raw = document.getElementById('qa-mp4-bytes').textContent.trim();\nconst mediaUrl = 'data:video/mp4;base64,' + raw;\nconst file = {id:7,name:'isolated.mp4',size:100000,type:'video',is_split:false};\ncreateRoot(document.getElementById('root')).render(\n  <AdaptiveMediaPlayer file={file} activeFolderId={1} onClose={() => {}}\n    streamUrl={mediaUrl}/>\n);\n",resolveDir:path.resolve(__dirname,'../app'),
        sourcefile:'native-data-mp4-qa.tsx',loader:'tsx'},
      bundle:true,outfile:path.join(temp,'app-player.js'),
      define:{'process.env.NODE_ENV':'"production"'},
      nodePaths:[path.resolve(__dirname,'../app/node_modules')],
      plugins:[{name:'synthetic-ipc',setup(build){
        build.onResolve({filter:/^@tauri-apps\//},args=>({path:args.path,namespace:'qa-ipc'}));
        build.onLoad({filter:/.*/,namespace:'qa-ipc'},()=>({
          contents:`
            export async function invoke(name) {
              window.__qaIpc.push(name);
              if(name==='cmd_get_transcode_capabilities')
                return {available:false,variants:[],mode:'original'};
              if(name==='cmd_probe_media_tracks')
                return {audio_tracks:[],subtitle_tracks:[],duration_secs:10};
              return null;
            }
            export function getCurrentWindow(){return {listen:async()=>()=>{},
              onResized:async()=>()=>{},isFullscreen:async()=>false};}
            export function type(){return 'linux';}
          `,loader:'js'
        }));
      }}],
    });
    const js=fs.readFileSync(path.join(temp,'app-player.js'),'utf8')
      .replace(/<\/script/gi,'<\\/script');
    const appHtml='<html><head><meta charset="utf-8"></head><body>'
      +'<div id="root"></div><script>window.__qaIpc=[];</script>'
      +'<script type="application/octet-stream" id="qa-mp4-bytes">'+data+'</script>'
      +'<script>'+js+'</script><script>'+"(() => {\nconst startedAt = performance.now(), events=[];\nlet begun=false, requestedPlay=false, didSeek=false, invalidDuration=false;\nwindow.__qaReport={done:false,phase:'app-mount'};\nconst poll=setInterval(() => {\n  if(window.__qaReport.done){clearInterval(poll);return;}\n  const v=[...document.querySelectorAll('video')].find(el=>el.videoWidth>0 && el.readyState>=2);\n  const report={done:false,phase:'app-loading',events:events.slice(-20),ipc:window.__qaIpc.slice(-15),\n    count:document.querySelectorAll('video').length,\n    width:v?.videoWidth,duration:v?.duration,time:v?.currentTime,\n    paused:v?.paused,ready:v?.readyState,errorCode:v?.error?.code};\n  if(v?.error) window.__qaReport={...report,done:true,phase:'app-decoder-error'};\n  else if(v && Number.isFinite(v.duration) && v.duration>0 && v.duration<3) {\n    invalidDuration=true;window.__qaReport={...report,done:true,phase:'invalid-native-duration'};\n  } else if(v && v.paused && !requestedPlay) {\n    requestedPlay=true;events.push('explicit-app-play');\n    v.play().catch(error=>events.push('play-rejected:'+String(error)));\n  } else if(v && !begun && !v.paused && v.currentTime>0.3) {\n    begun=true;events.push('app-playing');\n  } else if(v && begun && !didSeek && v.duration>=9 && v.duration<=11.5) {\n    didSeek=true;v.currentTime=6.5;events.push('app-seek');\n  } else if(v && didSeek && v.currentTime>=6.7 && !v.seeking && !v.paused\n            && v.videoWidth>0 && v.readyState>=2) {\n    window.__qaReport={...report,done:true,pass:true,phase:'app-native-mp4-seek',\n      begun,didSeek,invalidDuration};\n  } else if(performance.now()-startedAt>30000) {\n    window.__qaReport={...report,done:true,phase:'app-timeout',\n      begun,didSeek,invalidDuration,text:document.body.textContent.slice(0,150)};\n  } else window.__qaReport=report;\n},100);\n})();"+'</script>'
      +'</body></html>';
    const appPage=path.join(temp,'app-data-mp4.html');
    fs.writeFileSync(appPage,appHtml,{mode:0o600});
    const appBrowser=spawnSync('xvfb-run',['-a','python3',runner,appPage],{
      encoding:'utf8',timeout:85000,maxBuffer:2*1024*1024,
      env:{...process.env,TMPDIR:temp,TMP:temp,TEMP:temp,
        GST_AUDIO_SINK:'fakesink',ALSOFT_DRIVERS:'null',GST_DEBUG:'2'},
    });
    const appLine=(appBrowser.stdout||'').trim().split(/\r?\n/).reverse()
      .find(value=>value.startsWith('{'));
    let appReport=null;
    try{appReport=JSON.parse(appLine);}catch{}
    console.log('TeraRelay real React player native data-MP4 report: '
      +JSON.stringify(appReport||{stdout:(appBrowser.stdout||'').slice(-700),
        stderr:(appBrowser.stderr||'').slice(-700)}));
    if(appBrowser.status!==0 || !appReport?.pass || !appReport.didSeek)
      throw Error('Actual React player failed isolated native MP4 playback or seek');
    console.log('PASS real AdaptiveMediaPlayer native MP4 playback, duration and seek');

  } else if(!report || report.phase==='unsupported-media' || browser.status===null) {
    console.error('BLOCKED: native WebKit does not accept this data-MP4 fixture');
    process.exitCode=2;
  } else {
    console.error('FAIL native MP4 transport-independent playback or duration');
    process.exitCode=1;
  }
} catch(e) {
  console.error('FAIL isolated MP4 diagnostic: '+(e.stack||e));
  process.exitCode=1;
} finally {
  fs.rmSync(temp,{recursive:true,force:true});
}
})().catch(error => {console.error('FAIL native data MP4 QA: '+error);process.exitCode=1;});
