'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const full=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(full))return cache.get(full).exports;
 const module={exports:{}};cache.set(full,module);
 const code=ts.transpileModule(fs.readFileSync(full,'utf8'),
  {compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020}}).outputText;
 vm.runInNewContext('(function(require,module,exports){'+code+'\n})',{console,TextEncoder})
  (specifier=>{
    if(!specifier.startsWith('./'))throw Error('Unexpected test import '+specifier);
    return load(path.normalize(path.join(path.dirname(name),specifier)));
   },module,module.exports);
 return module.exports;
}
const m=load('driveBackupWatchlist');
const one={source_id:'src',backup_id:'bak',source_name:'Original',backup_name:'Backup',
 source_parent_id:null,backup_parent_id:null,created_at:1700000000000};
const rawList={revision:7,pairs:[one]};
assert.equal(m.readWatchList(rawList).pairs.length,1);
assert.throws(()=>m.readWatchList({revision:7,pairs:[one,one]}),/Duplicated/);
assert.throws(()=>m.readWatchList({...rawList,pairs:[{...one,source_id:'../x'}]}),/Invalid/);
assert.throws(()=>m.readWatchList({revision:7,pairs:Array(25).fill(one)}),/Invalid/);
const inventory={revision:7,skipped:0,folders:[
 {id:'src',name:'Original',path:'/Original',parentId:null},
 {id:'bak',name:'Backup',path:'/Backup',parentId:null},
 {id:'child',name:'Child',path:'/Original/Child',parentId:'src'},
]};
assert.throws(()=>m.validateWatchPair(inventory,'src','child',{revision:7,pairs:[]}),/nested/);
assert.throws(()=>m.validateWatchPair(inventory,'src','bak',rawList),/already/);
assert.equal(m.validateWatchPair(inventory,'src','bak',{revision:7,pairs:[]}).revision,7);
const check={source_id:'src',backup_id:'bak',status:'healthy',explanation:'Metadata matches',
 source_files:1,backup_files:1,only_source:0,only_backup:0,different:0,unverified:0,samples:[]};
const scan={revision:7,dirty:false,checked_at:1700000000100,
 healthy:1,drift:0,unverified:0,blocked:0,metadata_only:true,checks:[check]};
assert.equal(m.readWatchScan(scan,rawList).checks.length,1);
assert.throws(()=>m.readWatchScan({...scan,healthy:2},rawList),/totals/);
assert.throws(()=>m.readWatchScan({...scan,dirty:true},rawList),/inaccurately/);
assert.throws(()=>m.readWatchScan({...scan,checks:[{...check,different:1}]},rawList),/inaccurately/);
assert.throws(()=>m.readWatchScan({...scan,checks:[{...check,status:'drift'}],
 healthy:0,drift:1},rawList),/inconsistent/);
assert.throws(()=>m.readWatchScan({...scan,checks:[{...check,backup_id:'unknown'}]},rawList),/Stale/);
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_backup_watchlist.rs'),'utf8');
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBackupWatchlist.tsx'),'utf8');
for(const name of ['cmd_drive_backup_watchlist_list','cmd_drive_backup_watchlist_add',
 'cmd_drive_backup_watchlist_remove','cmd_drive_backup_watchlist_scan']){
 assert.ok(native.includes('fn '+name),'Native '+name);
 assert.ok(ui.includes("'"+name+"'"),'UI invoke '+name);
}
for(const s of ['MAX_PAIRS: usize = 24','fn inspect_pair','compare(s, conn,',
 'fn scan','fn add','fn remove','missing_backup_and_metadata_mismatch_are_drift_not_healthy',
 'moved_root_or_missing_backing_blocks_false_assurance'])
 assert.ok(native.includes(s),'Missing backup Watchlist contract '+s);
assert.ok(ui.includes('not proof of identical file bytes'));
console.log('PASS Drive Backup Watchlist: strict native response decoders, pair identity, drift and uncertain-status guards, save/remove/scan UI contracts');
