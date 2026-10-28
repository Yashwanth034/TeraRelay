'use strict';
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..'),ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const code=ts.transpileModule(fs.readFileSync(path.join(root,'app/src/driveFolderParity.ts'),'utf8'),
 {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
const testModule={exports:{}};
const parseFolderInventory=(raw)=>raw;
vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{console,TextEncoder})
 (name=>{
  if(name==='./driveFolderOperations')return {parseFolderInventory};
  throw Error('Unexpected dependency: '+name);
 },testModule,testModule.exports);
const m=testModule.exports;
const inventory={skipped:0,folders:[
 {id:'left',path:'/Original',name:'Original'},
 {id:'right',path:'/Backup',name:'Backup'},
 {id:'child',path:'/Original/Docs',name:'Docs'},
]};
assert.equal(m.parityFolders(inventory).length,3);
assert.throws(()=>m.parityFolders({...inventory,skipped:1}),/incomplete/i);
assert.throws(()=>m.parityFolders({...inventory,folders:[{id:'',path:'/Invalid',name:'bad'}]}),/invalid/i);
const clean={revision:12,dirty:false,left_id:'left',right_id:'right',
 left_folders:1,right_folders:1,left_files:1,right_files:1,
 matching_file_metadata:1,unverified_file_content:0,only_left:0,only_right:0,
 different:0,truncated_differences:false,differences:[],read_only:true};
assert.equal(m.parityVerdict(m.readFolderParity(clean,'left','right')),'metadata_equal');
const unknown={...clean,matching_file_metadata:0,unverified_file_content:1,
 differences:[{kind:'content_unverified',relative_path:'Docs/file.dat',left:'file',right:'file'}]};
assert.equal(m.parityVerdict(m.readFolderParity(unknown,'left','right')),'unverified');
const changed={...clean,matching_file_metadata:0,only_left:1,
 differences:[{kind:'only_left',relative_path:'Docs/file.dat',left:'file',right:null}]};
assert.equal(m.parityVerdict(m.readFolderParity(changed,'left','right')),'differences');
assert.throws(()=>m.readFolderParity(clean,'left','left'),/invalid/i);
assert.throws(()=>m.readFolderParity({...clean,left_id:'other'},'left','right'),/invalid/i);
assert.throws(()=>m.readFolderParity({...changed,only_left:2},'left','right'),/invalid/i);
assert.throws(()=>m.readFolderParity({...changed,differences:[{...changed.differences[0],
 relative_path:'../escape'}]},'left','right'),/invalid/i);
assert.throws(()=>m.readFolderParity({...clean,truncated_differences:true},'left','right'),/invalid/i);
const rust=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_folder_parity.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveFolderParity.tsx'),'utf8');
const mod=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
for(const phrase of [
 'WITH RECURSIVE scoped','LEFT JOIN logical_files','LEFT JOIN drive_objects',
 'MAX_TREE: usize = 512','MAX_FILES: usize = 2000',
 'MAX_DIFFS: usize = 200','unavailable backing',
 'pending copy-on-write','colliding names','v2_generation_supersedes_legacy_same_identity',
 'fn cmd_drive_folder_parity',
])assert.ok(rust.includes(phrase),'Missing native parity safety: '+phrase);
assert.ok(ui.includes("cmd_drive_folder_parity"));
assert.ok(ui.includes("readFolderParity"));
assert.ok(ui.includes("file bytes"));
assert.ok(mod.includes("pub mod drive_folder_parity;"));
assert.ok(mod.includes("pub use drive_folder_parity::*;"));
assert.equal(lib.split("commands::cmd_drive_folder_parity,").length-1,1);
assert.equal(settings.split("<DriveFolderParity active={isOpen && activeTab === 'general'} />").length-1,1);
console.log('PASS Drive Folder Parity: strict typed native results, full tree comparison, safety guard and Settings IPC registration.');
