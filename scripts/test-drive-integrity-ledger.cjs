'use strict';
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..'),ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]})),cache=new Map();
function load(name){
 const file=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(file))return cache.get(file);
 const module={exports:{}};cache.set(file,module.exports);
 const output=ts.transpileModule(fs.readFileSync(file,'utf8'),{compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
 vm.runInNewContext('(function(require,module,exports){'+output+'\n})',{console,TextEncoder})
  (n=>{if(!n.startsWith('./'))throw Error('Unexpected dependency '+n);return load(path.normalize(path.join(path.dirname(name),n)));},module,module.exports);
 return module.exports;
}
const m=load('driveIntegrityLedger');
const snapshot={revision:8,dirty:false,directories:[{id:'d',name:'Docs',parent_id:null,trashed_at:null}],
 files:[{entry_id:'a',display_name:'a.txt',directory_id:'d',total_size:16,updated_at:1},
 {entry_id:'b',display_name:'b.txt',directory_id:'d',total_size:17,updated_at:1}],pending:[],symlinks:[]};
const got=m.integrityInventory(snapshot);
assert.equal(got.files.length,2);assert.equal(got.files[0].path,'/Docs/a.txt');
assert.equal(m.integritySelection(got.files,['a','b']),33);
assert.throws(()=>m.integritySelection(got.files,[]),/Select/);
assert.throws(()=>m.integritySelection(got.files,['a','a']),/unique/);
assert.throws(()=>m.integritySelection(got.files,['unknown']),/missing/);
assert.equal(m.integrityBaselineInfo(['/Docs/a.txt'])[0],'/Docs/a.txt');
assert.throws(()=>m.integrityBaselineInfo([]),/Invalid/);
const base={id:'integrity-'+'a'.repeat(32),operation:'verify',phase:'running',
 current:1,count:2,bytes_read:16,total_bytes:33,verified:0,issues:1,
 results:[{path:'/Docs/a.txt',outcome:'hash_mismatch',explanation:'hash changed'}],error:null};
assert.equal(m.integrityProgress(base).issues,1);
assert.equal(m.integrityProgress({...base,phase:'done',current:2,bytes_read:33,verified:1}).phase,'done');
assert.throws(()=>m.integrityProgress({...base,phase:'done',current:0}),/Invalid|Incomplete/);
assert.throws(()=>m.integrityProgress({...base,issues:2}),/Invalid/);
assert.throws(()=>m.integrityProgress({...base,results:[{path:'/Docs/a.txt',outcome:'success',explanation:''}]}),/Invalid/);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/drive_integrity_ledger.rs'),'utf8');
const registry=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveIntegrityLedger.tsx'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const key of ['libc::O_NOFOLLOW','Sha256::new()','fn check_fresh(','fn has_pending(',
 'fn open_under(','fn valid_ledger(','fn save(','fn load(','spawn_blocking','cmd_drive_integrity_ledger_start',
 'cmd_drive_integrity_ledger_cancel','cmd_drive_integrity_ledger_info'])
 assert.ok(rust.includes(key),'Missing integrity code '+key);
for(const name of ['cmd_drive_integrity_ledger_start','cmd_drive_integrity_ledger_status',
 'cmd_drive_integrity_ledger_info','cmd_drive_integrity_ledger_cancel']){
 assert.equal(registry.split('drive_integrity_ledger::'+name+',').length-1,1,name+' registered once');
 assert.ok(ui.includes(name));
}
assert.equal(settings.split("<DriveIntegrityLedger active={isOpen && activeTab === 'general'} />").length-1,1);
console.log('PASS native Drive Content Integrity Ledger: persistent baseline / strict decode / file select / UI IPC contracts');
