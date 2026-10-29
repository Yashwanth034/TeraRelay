#!/usr/bin/env node
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs'), path = require('node:path'), vm = require('node:vm');
const ts = require('../app/node_modules/typescript');
const source = fs.readFileSync(path.resolve(__dirname,'../app/src/App.tsx'),'utf8');
const ast = ts.createSourceFile('App.tsx',source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
const app = ast.statements.find(n=>ts.isFunctionDeclaration(n)&&n.name?.text==='AppContent');
assert.ok(app);
const compiled = ts.transpileModule(app.getText(ast).replaceAll('import.meta.env.DEV','false').replaceAll('import.meta.env.VITE_TERA_QA_FEATURE_A','undefined')+'; exports.render=AppContent;',{
 compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS,jsx:ts.JsxEmit.ReactJSX}
}).outputText;
function harness(auth,detail='',extra={}){
 let cursor=0,effects=[];
 const states=[auth,detail,0,false,false], module={exports:{}};
 const element=(type,props)=>({type,props:props||{}});
 const context={
  module,exports:module.exports,require:()=>({jsx:element,jsxs:element}),
  useState:initial=>{const i=cursor++;if(!(i in states))states[i]=initial;return[states[i],next=>{states[i]=typeof next==='function'?next(states[i]):next;}];},
  useRef:value=>({current:value}),useEffect(fn){effects.push(fn);},
  useTheme:()=>({theme:'dark'}),usePlatform:()=>({isMobile:false}),
  useSettings:()=>({settings:{performanceMode:false,language:'en'},isLoaded:true,updateSetting(){}}),
  useTranslation:()=>({i18n:{}}),useDeviceContinuity:()=>({state:{phase:'idle'},markDriveUnlocked(){}}),
 };
 for(const name of ['MotionConfig','Toaster','AppUpdatePrompt','Suspense','ErrorBoundary','MobileDashboard',
 'DesktopDashboard','DriveContinuityUnlock','AuthWizard','TeraRelayBrand'])context[name]=name;
 Object.assign(context,extra);
 vm.runInNewContext(compiled,context);
 return{render(){cursor=0;effects=[];return module.exports.render();},states,get effects(){return effects;}};
}
function nodes(tree){
 if(Array.isArray(tree))return tree.flatMap(nodes);
 if(!tree||typeof tree!=='object')return[];
 return[tree,...nodes(tree.props.children)];
}
function text(tree){
 if(Array.isArray(tree))return tree.map(text).join('');
 return tree&&typeof tree==='object'?text(tree.props.children):String(tree??'');
}
test('normal session restoration shows the opening screen without retry or phone sign-in',()=>{
 const tree=harness('restoring').render();
 assert.ok(text(tree).includes('Opening your private vault'));
 assert.ok(!text(tree).includes('Retry now'));
 assert.ok(!text(tree).includes('Use phone sign-in instead'));
});
test('actual session restoration failure retains retry and optional phone sign-in',()=>{
 const tree=harness('restoring','Telegram is not responding yet.').render();
 assert.ok(text(tree).includes('Telegram is not responding yet.'));
 assert.ok(text(tree).includes('Retry now'));
 assert.ok(text(tree).includes('Use phone sign-in instead'));
});
test('missing API ID remains recoverable without discarding a saved session',()=>{
 const tree=harness('restoring','Your saved Telegram session exists, but its API ID is unavailable.').render();
 assert.ok(text(tree).includes('API ID is unavailable'));
 assert.ok(text(tree).includes('Retry now'));
 assert.ok(!nodes(tree).some(n=>n.type==='AuthWizard'));
});
test('retry returns to the neutral opening state while the next attempt runs',()=>{
 const h=harness('restoring','Offline'),tree=h.render();
 const retry=nodes(tree).find(n=>n.type==='button'&&text(n)==='Retry now');
 retry.props.onClick();
 assert.equal(h.states[1],'');
 assert.equal(h.states[2],1);
 assert.ok(!text(h.render()).includes('Retry now'));
});
for(const completesEarly of [false,true])test(completesEarly
 ? 'successful startup cancels the delayed connection notice'
 : 'a pending native restore exposes recovery after the grace period without cancelling it',async()=>{
 let finishRestore,restoreCalls=0,nextTimer=0;
 const pending=new Promise(resolve=>{finishRestore=resolve;}),timers=new Map();
 const h=harness('loading','',{
  invoke:async name=>{
   if(name==='cmd_saved_telegram_session_exists')return true;
   if(name==='cmd_migrate_legacy_api_credentials')return;
   if(name==='cmd_restore_saved_telegram_session'){restoreCalls++;return pending;}
   throw new Error('Unexpected native command '+name);
  },
  load:async()=>({get:async()=>12345}),globalThis:{},console:{warn(){}},
  window:{setTimeout:(fn,ms)=>{const id=++nextTimer;timers.set(id,{fn,ms});return id;},clearTimeout:id=>timers.delete(id)},
 });
 h.render();
 const stop=h.effects.find(fn=>fn.toString().includes('cmd_saved_telegram_session_exists'))();
 for(let i=0;i<12;i++)await Promise.resolve();
 assert.equal(restoreCalls,1);
 h.render();
 const watchdog=h.effects.find(fn=>fn.toString().includes('Connecting is taking longer'));
 assert.ok(watchdog,'pending startup needs a delayed recovery notice');
 const cleanup=watchdog();
 assert.equal(timers.size,1);
 assert.equal([...timers.values()][0].ms,8000);
 assert.ok(!text(h.render()).includes('Retry now'));
 if(completesEarly){
  finishRestore(true);
  await new Promise(resolve=>setImmediate(resolve));
  cleanup();
  assert.equal(timers.size,0);
  assert.equal(h.states[1],'');
  assert.ok(nodes(h.render()).some(n=>n.type==='DesktopDashboard'));
 }else{
  [...timers.values()][0].fn();
  assert.ok(text(h.render()).includes('Retry now'));
  assert.ok(text(h.render()).includes('taking longer'));
  assert.equal(restoreCalls,1);
  finishRestore(true);
  await new Promise(resolve=>setImmediate(resolve));
  assert.ok(nodes(h.render()).some(n=>n.type==='DesktopDashboard'));
  cleanup();
 }
 stop();
});
test('normal loading, signed-out and authenticated screens still route correctly',()=>{
 assert.ok(text(harness('loading').render()).includes('Opening your private vault'));
 assert.ok(nodes(harness('unauthenticated').render()).some(n=>n.type==='AuthWizard'));
 assert.ok(nodes(harness('authenticated').render()).some(n=>n.type==='DesktopDashboard'));
});
