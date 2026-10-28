'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths:[path.join(root,'app')] }));
const out = {};
vm.runInNewContext(ts.transpileModule(
  fs.readFileSync(path.join(root,'app/src/driveFinderCsv.ts'),'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, { exports:out });
const r = (id,kind,pathValue,size=0) => Object.freeze({
  id,kind,name:pathValue.split('/').pop(),path:pathValue,size,
});
const entries = Object.freeze([
  r('secret-file-id','file','/Documents/Quarter "A".csv',1024),
  r('secret-pending-id','pending',"/Documents/unfinished\twrite\nnew.xlsx",0),
  r('secret-folder-id','folder','/Documents/Folder',Number.MAX_SAFE_INTEGER),
  r('formula','file','=1+1',null),
  r('formula2','file','@SUM(1,2)',null),
  r('negative','file','-SUM(1,2)',null),
  r('plus','file','+SUM(1,2)',null),
  r('strange','file','/Private\u2028path\u0085broken',-8),
]);
const csv = out.formatDriveFinderCsv(entries);
assert.equal(csv.exported,entries.length);
assert.equal(csv.omitted,0);
assert.equal(csv.csv.split('\r\n').length,entries.length+2,
  'each row stays on its own CSV line even with adversarial filenames');
assert.match(csv.csv,/Quarter ""A""\.csv/,'embedded quotes properly doubled');
assert.match(csv.csv,/"/);
assert.match(csv.csv,/"'=[^"]+"/);
assert.match(csv.csv,/"'@SUM/);
assert.match(csv.csv,/"'-SUM/);
assert.match(csv.csv,/"'\+SUM/);
assert.doesNotMatch(csv.csv,/\nnew\.xlsx|\twrite|\u2028|\u0085/,
  'control and line separators must never appear literally');
assert.match(csv.csv,/"Folder","Unknown"/,'folder physical size not implied');
assert.match(csv.csv,/"Unfinished write","0"/,'pending empty file size remains known zero');
assert.doesNotMatch(csv.csv,/secret-file-id|secret-pending-id|secret-folder-id/,
  'metadata internal identities never exported');
const exactLarge = out.formatDriveFinderCsv([
  r('very-large','file','/Big data.bin',Number.MAX_SAFE_INTEGER),
]);
assert.match(exactLarge.csv,/"'9007199254740991"/,
  'spreadsheets must not silently round a >15-digit byte size');
const many = Array.from({length:1103},(_,i)=>r('file-'+i,'file','/file-'+i+'.txt',i));
const bounded = out.formatDriveFinderCsv(many);
assert.equal(bounded.exported,1000);
assert.equal(bounded.omitted,103);
assert.match(bounded.csv,/file-999\.txt/);
assert.doesNotMatch(bounded.csv,/file-1000\.txt/);
assert.equal(out.formatDriveFinderCsv(entries,2).exported,2);
assert.equal(out.formatDriveFinderCsv(entries,0).exported,0);
assert.equal(out.formatDriveFinderCsv(entries,Infinity).exported,entries.length);
assert.equal(many[0].size,0,'source rows remain unchanged');
const source = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.match(source,/aria-label="Copy filtered local Drive inventory as CSV"/);
assert.match(source,/formatDriveFinderCsv\(driveFinderMatches\)/,
  'export consumes current filtered and sorted Finder results');
assert.match(source,/navigator\.clipboard\.writeText\(exported\.csv\)/);
assert.match(source,/driveFinderCsvCopyInFlightRef\.current/);
// Execute actual SettingsModal clipboard handler, not a duplicate toy helper.
const filename = path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx');
const ast = ts.createSourceFile(filename,source,ts.ScriptTarget.ES2020,true,ts.ScriptKind.TSX);
const component = ast.statements.find(node => ts.isFunctionDeclaration(node) && node.name?.text === 'SettingsModal');
const declaration = component.body.statements.filter(ts.isVariableStatement)
  .flatMap(node => Array.from(node.declarationList.declarations))
  .find(node => ts.isIdentifier(node.name) && node.name.text === 'copyLocalDriveInventoryCsv');
assert.ok(declaration?.initializer,'real Settings CSV export handler must be present');
const handler = ts.transpileModule(
  'globalThis.copyCsv = '+declaration.initializer.getText(ast)+';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
function createClipboardHarness(items) {
  const calls=[],alerts=[],busy=[];
  const refs={
    driveSettingsOpenRef: {current:true},
    driveFinderCsvCopyInFlightRef: {current:false},
    driveFinderCsvCopyGenerationRef: {current:1},
  };
  const pending=[];
  const ctx={
    ...refs, driveFinderOpen:true,driveFinderBusy:false,driveFinderError:null,
    driveFinderMatches:items,
    formatDriveFinderCsv:out.formatDriveFinderCsv,
    setDriveFinderCsvCopyBusy:value=>busy.push(value),
    navigator:{clipboard:{writeText:csv=>{
      calls.push(csv);
      let resolve,reject;
      const promise=new Promise((ok,fail)=>{resolve=ok;reject=fail;});
      pending.push({resolve,reject});
      return promise;
    }}},
    toast:{success:message=>alerts.push(['success',message]),
      error:message=>alerts.push(['error',message])},
  };
  vm.runInNewContext(handler,ctx);
  return {calls,alerts,busy,refs,pending,run:()=>ctx.copyCsv(),
    close(){refs.driveSettingsOpenRef.current=false;
      refs.driveFinderCsvCopyGenerationRef.current++;
      refs.driveFinderCsvCopyInFlightRef.current=false;},
  };
}
(async()=>{
  const h=createClipboardHarness(many);
  const first=h.run();
  await h.run();
  assert.equal(h.calls.length,1,'double-click must not overlap clipboard writes');
  assert.match(h.calls[0],/file-999\.txt/);
  assert.doesNotMatch(h.calls[0],/file-1000\.txt/);
  assert.deepEqual(h.busy,[true]);
  h.pending[0].resolve();
  await first;
  assert.deepEqual(h.busy,[true,false]);
  assert.equal(h.alerts[0][0],'success');
  assert.match(h.alerts[0][1],/103 additional matches omitted/);

  const denial=createClipboardHarness(entries);
  const denied=denial.run();
  denial.pending[0].reject(new Error('Permission denied'));
  await denied;
  assert.equal(denial.alerts[0][0],'error','clipboard permission errors shown');
  assert.equal(denial.refs.driveFinderCsvCopyInFlightRef.current,false);

  const empty=createClipboardHarness([]);
  await empty.run();
  assert.equal(empty.calls.length,0,'no accidental empty export');
  const closed=createClipboardHarness(entries);
  const late=closed.run();
  closed.close();
  closed.pending[0].resolve();
  await late;
  assert.equal(closed.alerts.length,0,'old dialog cannot toast after closing');
  assert.deepEqual(closed.busy,[true],'old dialog cannot update state on late completion');
  console.log('PASS Drive Finder CSV: real clipboard handler, single-flight, error and close-safe lifecycle');
})().catch(error=>{console.error(error);process.exitCode=1;});
console.log('PASS Drive Finder CSV export: filtered order, 1000 cap, sizes, quotes, formula defense and UI');
