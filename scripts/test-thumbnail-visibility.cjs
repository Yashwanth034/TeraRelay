const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm'),assert=require('node:assert/strict');
const {createRequire}=require('node:module');
const appRequire=createRequire(path.resolve(__dirname,'../app/package.json'));
const ts=appRequire('typescript');
const code=ts.transpileModule(fs.readFileSync(path.resolve(__dirname,'../app/src/components/shared/PremiumFileThumbnail.tsx'),'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2022,jsx:ts.JsxEmit.ReactJSX}}).outputText;
function harness(){
 let cursor=0, cells=[], effects=[], queries=[], calls=[], observer, cleanup;
 class Observer {
  constructor(callback){this.callback=callback;observer=this;}
  observe(element){this.element=element;}
  disconnect(){this.disconnected=true;}
  trigger(visible){this.callback([{isIntersecting:visible}]);}
 }
 const React={
  useMemo:fn=>fn(),
  useRef:()=>{const i=cursor++;return cells[i]??=( {current:{kind:'thumbnail-button'}});},
  useState:initial=>{const i=cursor++;if(!(i in cells))cells[i]=typeof initial==='function'?initial():initial;return[cells[i],value=>{cells[i]=typeof value==='function'?value(cells[i]):value}];},
  useEffect:fn=>effects.push(fn),
 };
 const module={exports:{}};
 const req=name=>{
  if(name==='react')return React;
  if(name==='@tauri-apps/api/core')return{invoke:(...args)=>{calls.push(args);return Promise.resolve(null)}};
  if(name==='@tanstack/react-query')return{useQuery:options=>{queries.push(options);return{data:null,isFetching:false}}};
  if(name==='lucide-react'||name==='./FileTypeIcon')return new Proxy({},{get:()=>()=>null});
  return appRequire(name);
 };
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{IntersectionObserver:Observer,console})(req,module,module.exports);
 const render=(name='movie.mp4',folderId=7)=>{cursor=0;return module.exports.PremiumFileThumbnail({file:{id:42,name,type:'video'},folderId});};
 return{render,flush:()=>{for(const fn of effects.splice(0))cleanup=fn();},get observer(){return observer},get queries(){return queries},get calls(){return calls},cleanup:()=>cleanup?.()};
}
let failures=0;
function test(name,fn){try{fn();console.log('PASS '+name)}catch(e){failures++;console.error('FAIL '+name+': '+e.message)}}
test('offscreen media does not start backend thumbnail work on mount',()=>{
 const h=harness();h.render();h.flush();assert.equal(h.calls.length,0,'mount fetched an offscreen thumbnail');
 assert.equal(h.queries.at(-1)?.enabled,false,'offscreen query must remain disabled');
});
test('viewport entry enables the thumbnail query and leaving disables new work',()=>{
 const h=harness();h.render();h.flush();assert.ok(h.observer,'media must observe viewport visibility');
 h.observer.trigger(true);h.render();assert.equal(h.queries.at(-1).enabled,true);
 h.observer.trigger(false);h.render();assert.equal(h.queries.at(-1).enabled,false);
});
test('thumbnail query identity separates folders and persists across rerenders',()=>{
 const h=harness();h.render();h.flush();assert.ok(h.observer,'viewport observation missing');
 h.observer.trigger(true);h.render();const first=h.queries.at(-1);
 h.render();assert.deepEqual(h.queries.at(-1).queryKey,first.queryKey);
 h.render('movie.mp4',8);assert.notDeepEqual(h.queries.at(-1).queryKey,first.queryKey);
 assert.ok(first.staleTime>=300000,'returning to a channel should reuse recent results');
 assert.equal(first.refetchOnWindowFocus,false,'window focus must not fan out thumbnail requests');
});
test('unmount disconnects visibility observation',()=>{
 const h=harness();h.render();h.flush();assert.ok(h.observer,'viewport observation missing');
 h.cleanup();assert.equal(h.observer.disconnected,true);
});
test('ordinary files do not fetch or observe thumbnail work',()=>{
 const h=harness();h.render('notes.txt');h.flush();assert.equal(h.calls.length,0);
 assert.ok(!h.observer,'non-media file should not register observer');
});
process.exitCode=failures?1:0;
