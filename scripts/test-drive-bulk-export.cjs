'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
  const absolute=path.resolve(root,'app/src',name+'.ts');
  if(cache.has(absolute))return cache.get(absolute);
  const module={exports:{}};cache.set(absolute,module.exports);
  const source=ts.transpileModule(fs.readFileSync(absolute,'utf8'),
    {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
  const localRequire=specifier=>{
    if(!specifier.startsWith('./'))throw Error('Unexpected dependency');
    return load(path.normalize(path.join(path.dirname(name),specifier)));
  };
  vm.runInNewContext('(function(require,module,exports){'+source+'\n})',
    {TextEncoder,console})(localRequire,module,module.exports);
  return module.exports;
}
const mod=load('driveBulkExport');
const raw={revision:7,dirty:false,
  directories:[{id:'parent',name:'Reports',parent_id:null,trashed_at:null}],
  files:[{entry_id:'entry-1',display_name:'Revenue.csv',directory_id:'parent',total_size:160,
    content_fingerprint:'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',updated_at:42},
    {entry_id:'entry-2',display_name:'Costs.csv',directory_id:'parent',total_size:32,updated_at:43}],
  pending:[],symlinks:[]};
const data=mod.parseBulkExportInventory(raw);
assert.equal(data.files.length,2);
assert.equal(data.files.find(file=>file.id==='entry-1').path,'/Reports/Revenue.csv');
assert.equal(mod.selectedTotalBytes(data.files,['entry-1','entry-2']),192);
assert.throws(()=>mod.selectedTotalBytes(data.files,['entry-1','entry-1']),/different/);
assert.throws(()=>mod.selectedTotalBytes(data.files,['unknown']),/no longer visible/);
assert.throws(()=>mod.selectedTotalBytes(data.files,[]),/Choose/);
const valid={id:'export-'+'a'.repeat(32),phase:'running',
 current:1,count:2,bytes_written:160,total_bytes:192,files_verified:0,error:null};
assert.equal(mod.parseBulkExportProgress(valid).phase,'running');
assert.equal(mod.parseBulkExportProgress({...valid,phase:'done',current:2,bytes_written:192,files_verified:2}).phase,'done');
assert.throws(()=>mod.parseBulkExportProgress({...valid,phase:'done'}),/Incomplete/);
assert.throws(()=>mod.parseBulkExportProgress({...valid,files_verified:2}),/Invalid/);
assert.throws(()=>mod.parseBulkExportProgress({...valid,bytes_written:200}),/Invalid/);
assert.throws(()=>mod.parseBulkExportProgress({...valid,id:'../abc'}),/Invalid/);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/drive_bulk_export.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBulkExport.tsx'),'utf8');
const registry=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const marker of ['fn open_beneath(', 'libc::O_NOFOLLOW','fn verify_written_archive(',
 'Sha256::new()','fs::hard_link(', 'fn check_snapshot(', 'fn destination_guard(',
 'cmd_drive_bulk_export_cancel','cmd_drive_bulk_export_status',
 'cmd_drive_bulk_export_start','MAX_TOTAL','Uncommitted(']){
 assert.ok(native.includes(marker),'Missing native bulk content safety: '+marker);
}
assert.ok(!native.includes('expected_sha'),'Drive content fingerprint is NOT whole-file SHA-256');
for(const marker of ['cmd_drive_bulk_export_cancel','cmd_drive_bulk_export_start',
 'cmd_drive_bulk_export_status','SHA-256 verified','Cancel export','plaintext']){
 assert.ok(ui.includes(marker),'Missing bulk export UI '+marker);
}
for(const name of ['cmd_drive_bulk_export_cancel','cmd_drive_bulk_export_status','cmd_drive_bulk_export_start'])
 assert.equal(registry.split('drive_bulk_export::'+name+',').length-1,1,name+' native command registered once');
assert.equal(settings.split('<DriveBulkExport active={isOpen && activeTab === \'general\'} />').length-1,1);
console.log('PASS Verified Bulk ZIP Export: strict TS selection/progress, native streamed ZIP/symlink/rollback/sha256 and UI integration contract');
