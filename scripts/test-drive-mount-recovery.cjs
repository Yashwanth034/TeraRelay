const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm'),assert=require('node:assert/strict');
const {createRequire}=require('node:module'),appRequire=createRequire(path.resolve(__dirname,'../app/package.json')),ts=appRequire('typescript');
const source=fs.readFileSync(path.resolve(__dirname,'../app/src/hooks/useDeviceContinuity.ts'),'utf8');
const ast=ts.createSourceFile('Continuity.ts',source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TS);
let expression, continuityExpression;
function visit(node){
 if(ts.isCallExpression(node)&&node.expression.getText(ast)==='useEffect'&&node.arguments[0]?.getText(ast).includes("'cmd_drive_status'"))expression=node.arguments[0].getText(ast);
 if(ts.isCallExpression(node)&&node.expression.getText(ast)==='useEffect'&&node.arguments[0]?.getText(ast).includes('const runInitialContinuity'))continuityExpression=node.arguments[0].getText(ast);
 ts.forEachChild(node,visit);
}visit(ast);
// Run the ACTUAL production decoder, not a stale simplified mock.
const statusModule={exports:{}};
const statusSource=fs.readFileSync(path.resolve(__dirname,'../app/src/driveMountStatus.ts'),'utf8');
vm.runInNewContext(ts.transpileModule(statusSource,{compilerOptions:{
 target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}}).outputText,
 {exports:statusModule.exports,module:statusModule});
const validateNativeDriveMountStatus=statusModule.exports.validateNativeDriveMountStatus;
assert.equal(typeof validateNativeDriveMountStatus,'function');
let failures=0;
async function test(name,run){try{assert.ok(expression,'Native Drive recovery must run independently of metadata sync');await run();console.log('PASS '+name)}catch(e){failures++;console.error('FAIL '+name+': '+e.message)}}
async function flush(){for(let i=0;i<12;i++)await Promise.resolve()}
function deferred(){let resolve,reject;const promise=new Promise((a,b)=>{resolve=a;reject=b});return{promise,resolve,reject}}
function harness(options={}){
 const calls=[],timers=new Map(),listeners=new Map(),detach={current:false};
 let mounted=options.mounted??false,hookState={nativeMountError:null};
 const context={
 setState:update=>{hookState=typeof update==='function'?update(hookState):update},
 validateNativeDriveMountStatus,
 authenticated:options.authenticated??true,isMobile:options.isMobile??false,
 shouldDetachNativeDriveRef:detach,
 console:{warn:()=>{}},
 window:{
  setInterval:(fn,ms)=>{assert.ok(ms>=1000);timers.set(1,fn);return 1},
  clearInterval:id=>timers.delete(id),
  addEventListener:(name,fn)=>listeners.set(name,fn),
  removeEventListener:(name,fn)=>{if(listeners.get(name)===fn)listeners.delete(name)}
 },
 invoke:async command=>{
  calls.push(command);
  if(command==='cmd_drive_status'){
   const raw=await (options.status?options.status():{supported:options.supported??true,mounted});
   if(options.rawStatus) return raw;
   return {native_recovery:false,mount_point:raw.mounted?'/fixture/drive':null,...raw};
  }
  if(command==='cmd_drive_mount'){if(options.mount)return options.mount();mounted=true;return{mounted:true}}
  if(command==='cmd_drive_unmount'){mounted=false;return{mounted:false}}
  throw new Error('Unexpected cloud operation '+command);
 }
 };
 const js=ts.transpileModule('const effect='+expression+';effect;',{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}}).outputText;
 const cleanup=vm.runInNewContext(js,context)();
 return{calls,cleanup,timers,listeners,detach,state:()=>hookState,
  setMounted:v=>{mounted=v},tick:()=>{for(const fn of timers.values())fn()}};
}
(async()=>{
 await test('a missing mount is restored without waiting for cloud metadata',async()=>{const h=harness();await flush();assert.deepEqual(h.calls,['cmd_drive_status','cmd_drive_mount']);h.cleanup()});
 await test('native handoff owns subsequent mount loss without JS overriding an explicit unmount',async()=>{
  const h=harness({mounted:true});await flush();h.setMounted(false);h.tick();await flush();
  assert.equal(h.calls.filter(c=>c==='cmd_drive_mount').length,0);h.cleanup()});
 await test('healthy providers are not mounted twice',async()=>{const h=harness({mounted:true});await flush();h.tick();await flush();assert.ok(!h.calls.includes('cmd_drive_mount'));h.cleanup()});
 await test('signed-out and mobile sessions never start recovery',async()=>{for(const options of [{authenticated:false},{isMobile:true}]){const h=harness(options);await flush();assert.equal(h.calls.length,0);assert.equal(h.timers.size,0)}});
 await test('unsupported platforms never attempt a mount',async()=>{const h=harness({supported:false});await flush();assert.ok(!h.calls.includes('cmd_drive_mount'));h.cleanup()});
 await test('focus and timer checks cannot overlap an in-flight recovery',async()=>{const pending=deferred(),h=harness({status:()=>pending.promise});h.tick();h.listeners.get('focus')?.();assert.equal(h.calls.length,1);pending.resolve({supported:true,mounted:false});await flush();assert.equal(h.calls.filter(c=>c==='cmd_drive_mount').length,1);h.cleanup()});
 await test('cleanup cancels future checks and ignores a late status result',async()=>{const pending=deferred(),h=harness({status:()=>pending.promise});h.cleanup();pending.resolve({supported:true,mounted:false});await flush();assert.ok(!h.calls.includes('cmd_drive_mount'));assert.equal(h.timers.size,0);assert.equal(h.listeners.size,0)});
 await test('a late mount after sign-out is detached',async()=>{const pending=deferred(),h=harness({mount:()=>pending.promise});await flush();h.detach.current=true;h.cleanup();pending.resolve({mounted:true});await flush();assert.equal(h.calls.filter(c=>c==='cmd_drive_unmount').length,1)});
 await test('a WebView reload does not detach its completed native mount',async()=>{const pending=deferred(),h=harness({mount:()=>pending.promise});await flush();h.cleanup();pending.resolve({mounted:true});await flush();assert.ok(!h.calls.includes('cmd_drive_unmount'))});
 await test('a failed mount can retry on the next check',async()=>{let n=0;const h=harness({mount:()=>{if(++n===1)throw new Error('Temporary failure');return{mounted:true}}});await flush();h.tick();await flush();assert.equal(n,2);h.cleanup()});
 await test('native-owned recovery does not override an explicit unmount on timer or focus',async()=>{let mounted=true;const h=harness({status:()=>({supported:true,mounted,native_recovery:true})});await flush();mounted=false;h.tick();await flush();h.listeners.get('focus')?.();await flush();assert.ok(!h.calls.includes('cmd_drive_mount'));h.cleanup()});
 await test('native recovery ownership prevents frontend from remounting an explicitly unmounted Drive',async()=>{
  const h=harness({status:()=>({supported:true,mounted:false,native_recovery:true})});
  await flush();assert.equal(h.calls.filter(c=>c==='cmd_drive_mount').length,0);h.cleanup()});
 await test('malformed native mount IPC fails closed and next valid probe safely recovers',async()=>{
  let response={supported:true,mounted:true}; // omits required safety fields
  const h=harness({rawStatus:true,status:()=>response});
  await flush();
  assert.ok(h.state().nativeMountError,'malformed provider status must surface an error');
  assert.equal(h.calls.filter(c=>c==='cmd_drive_mount').length,0);
  response={supported:true,mounted:false,native_recovery:false,mount_point:null};
  h.tick();await flush();
  assert.equal(h.calls.filter(c=>c==='cmd_drive_mount').length,1);
  assert.equal(h.state().nativeMountError,null);
  h.cleanup();
 });
 await test('slow initial metadata sync cannot remount after an explicit unmount',async()=>{
  const sync=deferred(),calls=['cmd_drive_mount'],snapshot={directories:[],files:[],pending:[],revision:0,dirty:false};
  const context={authenticated:true,authResolved:true,isMobile:false,setState:()=>{},
   initialSnapshotRef:{current:null},startupCryptoCheckPendingRef:{current:true},shouldDetachNativeDriveRef:{current:false},
   classifyDeviceContinuity:()=>({directoryCount:0,fileCount:0,restoredFromRemote:false,needsDriveUnlock:false,syncSource:null}),
   validateDriveSnapshot:v=>v,validateDriveSyncResult:v=>v,validateDriveCryptoStatus:v=>v,
   console:{warn:()=>{}},window:{setInterval:()=>1,clearInterval:()=>{}},
   invoke:async command=>{calls.push(command);
    if(command==='cmd_drive_sync_metadata')return sync.promise;
    if(command==='cmd_drive_get_snapshot')return snapshot;
    if(command==='cmd_drive_crypto_status')return{configured:false,unlocked:false};
    return {};
   }};
  assert.ok(continuityExpression);
  const js=ts.transpileModule('const effect='+continuityExpression+';effect;',{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}}).outputText;
  const cleanup=vm.runInNewContext(js,context)();await flush();
  assert.ok(calls.includes('cmd_drive_sync_metadata'));
  calls.push('cmd_drive_unmount');sync.resolve({source:'fixture'});await flush();
  assert.equal(calls.filter(c=>c==='cmd_drive_mount').length,1,'Completed metadata sync must preserve the explicit unmount');
  cleanup();
 });
 process.exitCode=failures?1:0;
})();
