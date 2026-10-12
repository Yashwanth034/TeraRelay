const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm'),assert=require('node:assert/strict');
const {createRequire}=require('node:module');
const appRequire=createRequire(path.resolve(__dirname,'../app/package.json')),ts=appRequire('typescript');
const source=fs.readFileSync(path.resolve(__dirname,'../app/src/components/desktop/dashboard/RemoteUploadModal.tsx'),'utf8');
const ast=ts.createSourceFile('Modal.tsx',source,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
let expression, importExpression, importDisabled;
function visit(node){
 if(ts.isVariableDeclaration(node)&&node.name.getText(ast)==='handleSkipBatchEntry')expression=node.initializer.getText(ast);
 if(ts.isVariableDeclaration(node)&&node.name.getText(ast)==='handleBatchImport')importExpression=node.initializer.getText(ast);
 if(ts.isJsxOpeningElement(node)&&node.tagName.getText(ast)==='button'){
  const attrs=node.attributes.properties;
  if(attrs.some(a=>ts.isJsxAttribute(a)&&a.name.getText(ast)==='onClick'&&a.initializer?.expression?.getText(ast)==='handleBatchImport')){
   importDisabled=attrs.find(a=>ts.isJsxAttribute(a)&&a.name.getText(ast)==='disabled')?.initializer?.expression?.getText(ast);
  }
 }
 ts.forEachChild(node,visit);
}visit(ast);
let failures=0;
async function test(name,run){try{assert.ok(expression,'Skip does not serialize its pending configuration update');await run();console.log('PASS '+name)}catch(e){failures++;console.error('FAIL '+name+': '+e.message)}}
function harness(){
 let resolve,reject,calls=0;
 const pending=new Promise((a,b)=>{resolve=a;reject=b});
 const state={batch:{id:'batch'},busy:false,error:null,errorMessage:e=>e.message,remoteImportBatch:{skipEntry:()=>{calls++;return pending}}};
 state.setBusy=value=>{state.busy=value};state.setError=value=>{state.error=value};
 const js=ts.transpileModule('const handler='+expression+';handler;', {compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}}).outputText;
 return{state,run:vm.runInNewContext(js,state),resolve,reject,get calls(){return calls}};
}
(async()=>{
await test('skip holds the busy gate until its durable update completes',async()=>{
 const h=harness(),result=h.run('entry');assert.equal(h.state.busy,true);
 h.resolve();await result;assert.equal(h.state.busy,false);assert.equal(h.calls,1);
});
await test('a repeated skip cannot race the same pending update',async()=>{
 const h=harness(),first=h.run('entry');await h.run('entry');assert.equal(h.calls,1);
 h.resolve();await first;
});
await test('a failed skip surfaces its error and releases the busy gate',async()=>{
 const h=harness(),result=h.run('entry');h.reject(new Error('Could not save skip'));
 await result;assert.equal(h.state.error,'Could not save skip');assert.equal(h.state.busy,false);
});
await test('Import stays disabled until Skip has finished saving',async()=>{
 assert.ok(importDisabled,'Batch Import must have a disabled gate');
 assert.equal(vm.runInNewContext(importDisabled,{busy:true,selectionReady:true}),true);
 assert.equal(vm.runInNewContext(importDisabled,{busy:false,selectionReady:true}),false);
 assert.equal(vm.runInNewContext(importDisabled,{busy:false,selectionReady:false}),true);
});
await test('the Import handler refuses to start a batch while Skip is pending',async()=>{
 const h=harness(),first=h.run('entry');let imports=0,closes=0;
 h.state.remoteImportBatch.startBatch=()=>{imports++;return Promise.resolve()};
 h.state.onClose=()=>{closes++};h.state.onBatchSettled=()=>{};h.state.toast={error:()=>{}};
 const js=ts.transpileModule('const batchHandler='+importExpression+';batchHandler;', {compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.CommonJS}}).outputText;
 const run=vm.runInNewContext(js,h.state);
 run();assert.equal(imports,0);assert.equal(closes,0);
 h.resolve();await first;run();assert.equal(imports,1);assert.equal(closes,1);
});
process.exitCode=failures?1:0;
})();
