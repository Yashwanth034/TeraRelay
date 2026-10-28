'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const modules=new Map();
function load(name) {
 const full=path.resolve(root,'app/src',name+'.ts');
 if(modules.has(full))return modules.get(full).exports;
 const module={exports:{}};modules.set(full,module);
 const js=ts.transpileModule(fs.readFileSync(full,'utf8'),
  {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
 vm.runInNewContext('(function(require,module,exports){'+js+'\n})',{console,TextEncoder})
  (specifier=>{
   if(!specifier.startsWith('./'))throw Error('Unexpected import '+specifier);
   return load(path.normalize(path.join(path.dirname(name),specifier)));
  },module,module.exports);
 return module.exports;
}
const m=load('drivePortableManifest');
const hash='a'.repeat(64),other='b'.repeat(64);
const saved={destination_path:'/private/drive.json',captured_at:1700000000000,
 item_count:2,revision:7,dirty:false,checksum:hash};
assert.equal(m.readPortableExport(saved,'/private/drive.json').item_count,2);
assert.equal(m.readPortableExport({...saved,destination_path:'C:\\Drive\\inventory.json'},'C:\\Drive\\inventory.json').item_count,2);
assert.throws(()=>m.readPortableExport({...saved,destination_path:'/private/wrong.json'},'/private/drive.json'));
assert.throws(()=>m.readPortableExport({...saved,destination_path:'/private/../drive.json'},'/private/../drive.json'));
assert.throws(()=>m.readPortableExport({...saved,checksum:'x'},saved.destination_path));
assert.throws(()=>m.readPortableExport({...saved,dirty:'false'},saved.destination_path));
const info={seq:1,captured_at:saved.captured_at,revision:7,dirty:false,item_count:2,fingerprint:hash};
const later={...info,seq:2,revision:8,captured_at:saved.captured_at+1,item_count:3,fingerprint:other};
const comparison={before:info,after:later,
 created:1,removed:0,moved:0,renamed:0,ancestor_path_changed:0,
 availability_changed:1,backing_state_changed:0,content_metadata_changed:0,
 changes:[
  {change:'created',kind:'file',old_path:null,new_path:'/New.txt',
   previous_backing:null,current_backing:'v2'},
  {change:'availability_changed',kind:'file',old_path:'/Old.txt',new_path:'/Old.txt',
   previous_backing:'v2',current_backing:'v2_unavailable'},
 ]};
const raw={captured_at:saved.captured_at,checksum:hash,metadata_only:true,comparison};
assert.equal(m.readPortableComparison(raw).comparison.availability_changed,1);
assert.throws(()=>m.readPortableComparison({...raw,metadata_only:false}));
assert.throws(()=>m.readPortableComparison({...raw,checksum:other}));
assert.throws(()=>m.readPortableComparison({...raw,comparison:{...comparison,created:2}}));
assert.throws(()=>m.readPortableComparison({...raw,comparison:{...comparison,
 changes:[{...comparison.changes[0],new_path:'../../escape'},comparison.changes[1]]}}));
assert.ok(m.portableChangeLabels.availability_changed);
const native=fs.readFileSync(path.join(root,
 'app/src-tauri/src/commands/drive_portable_manifest.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,
 'app/src/components/desktop/dashboard/DrivePortableManifest.tsx'),'utf8');
for(const required of [
 'capture_snapshot(&conn)','create_new(true)','inventory_sha256',
 'fn validate_graph','fn read_checked','fn safe_parent',
 'MAX_BYTES','metadata_only: true','checked_path','sync_all()',
 'verified_roundtrip_and_checksum_tamper_rejected',
 'invalid_parent_cycle_path_and_duplicate_identity_fail_closed',
 'create_only_export_and_import_preserve_original_bytes',
])assert.ok(native.includes(required),'Missing native contract '+required);
for(const name of ['cmd_drive_portable_manifest_export','cmd_drive_portable_manifest_compare'])
 assert.ok(ui.includes("'"+name+"'"),'Missing frontend invocation '+name);
assert.ok(ui.includes('not a backup of file contents'));
console.log('PASS Portable Drive Manifest: strict parsed receipts, integrity result contracts, native safety checks and export/compare UI actions');
