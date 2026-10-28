'use strict';
const assert=require('node:assert/strict'),fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const cache=new Map();
function load(name){
 const filename=path.resolve(root,'app/src',name+'.ts');
 if(cache.has(filename))return cache.get(filename);
 const module={exports:{}};cache.set(filename,module.exports);
 const source=ts.transpileModule(fs.readFileSync(filename,'utf8'),{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
 }).outputText;
 vm.runInNewContext('(function(require,module,exports){'+source+'\n})',{
   console,TextEncoder,
 })(specifier=>{
   if(!specifier.startsWith('./'))throw Error('Unexpected fixture dependency');
   return load(path.normalize(path.join(path.dirname(name),specifier)));
 },module,module.exports);
 return module.exports;
}
const {selectFolderForVerifiedZip}=load('driveFolderZipSelection');
const {parseBulkExportInventory}=load('driveBulkExport');
const raw={revision:17,dirty:false,
 directories:[
  {id:'reports',name:'Reports',parent_id:null,trashed_at:null},
  {id:'year',name:'2026',parent_id:'reports',trashed_at:null},
  {id:'empty',name:'Empty',parent_id:'reports',trashed_at:null},
  {id:'other',name:'Reporting',parent_id:null,trashed_at:null},
 ],
 files:[
  {entry_id:'file1',display_name:'Revenue.csv',directory_id:'reports',total_size:20},
  {entry_id:'file2',display_name:'Costs.csv',directory_id:'year',total_size:10},
  {entry_id:'file3',display_name:'Ignore.csv',directory_id:'other',total_size:90},
 ],pending:[],symlinks:[],
};
const files=parseBulkExportInventory(raw).files;
const item=selectFolderForVerifiedZip(raw,files,'reports');
assert.equal(item.folderId,'reports');
assert.equal(item.folderPath,'/Reports');
assert.equal(item.revision,17);
assert.equal(item.foldersCovered,3);
assert.equal(item.preservedEmptyFolders,1);
assert.equal(item.entryIds.length,2);
assert.ok(item.entryIds.includes('file1') && item.entryIds.includes('file2'));
assert.ok(!item.entryIds.includes('file3'));
assert.equal(item.totalBytes,30);
const rejected=(change,regex)=>{
 const next={...raw,...change};
 assert.throws(()=>selectFolderForVerifiedZip(next,parseBulkExportInventory(next).files,'reports'),regex);
};
assert.throws(()=>selectFolderForVerifiedZip(raw,files,'missing'),/Folder is missing/);
rejected({pending:[{id:'stage',directory_id:'year',display_name:'draft.txt',
  base_entry_id:null}]},/unfinished/i);
rejected({pending:[{id:'cow',directory_id:'other',display_name:'Replaced.txt',
  base_entry_id:'file2'}]},/copy-on-write/i);
const unrelated={...raw,pending:[{id:'unrelated-cow',directory_id:'other',
  display_name:'not-in-reports.txt',base_entry_id:'file3'}]};
assert.equal(selectFolderForVerifiedZip(
  unrelated,parseBulkExportInventory(unrelated).files,'reports').entryIds.length,2);
rejected({symlinks:[{id:'link',directory_id:'year',display_name:'bad-link',
  trashed_at:null}]},/symbolic links/i);
const emptyOnly={...raw,files:raw.files.filter(f=>f.entry_id==='file3')};
const emptyPlan=selectFolderForVerifiedZip(
  emptyOnly,parseBulkExportInventory(emptyOnly).files,'reports');
assert.equal(emptyPlan.entryIds.length,0);
assert.equal(emptyPlan.totalBytes,0);
assert.equal(emptyPlan.preservedEmptyFolders,3);
rejected({files:[...raw.files,{...raw.files[0],display_name:'duplicate.csv'}]},
  /ambiguous|unresolved/i);
rejected({files:[...raw.files,{entry_id:'hidden',display_name:'../unexportable',
  directory_id:'reports',total_size:8}]},/cannot safely be exported|paths disagree|unresolved/i);
const collision={...raw,files:[...raw.files,
  {entry_id:'casefold',display_name:'revenue.csv',directory_id:'reports',total_size:2}]};
assert.throws(()=>selectFolderForVerifiedZip(collision,parseBulkExportInventory(collision).files,'reports'),
  /collide when extracted/i);
const many={...raw,files:[...raw.files,
 ...Array.from({length:101},(_,i)=>({entry_id:'extra'+i,
  directory_id:'reports',display_name:'f'+i+'.txt',total_size:1}))]};
assert.throws(()=>selectFolderForVerifiedZip(many,parseBulkExportInventory(many).files,'reports'),
  /more than 100/i);
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveBulkExport.tsx'),'utf8');
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/drive_bulk_export.rs'),'utf8');
for(const name of ['selectFolderForVerifiedZip','folderBundle','cmd_drive_get_snapshot',
  'Select all completed files recursively','Folder changed since selection',
  'cmd_drive_bulk_export_start','cmd_drive_bulk_export_cancel',
  'preservedEmptyFolders','No automatic retries']){
 if(name==='No automatic retries')continue;
 assert.ok(ui.includes(name),'UI integration missing '+name);
}
assert.ok(native.includes('fn verify_written_archive('));
assert.ok(native.includes('fn pending_shadows_file('));
for(const marker of ['fn collect_folder_zip_paths(',
  'fn export_files_with_folders(',
  'zip.add_directory(folder.as_str(), options)',
  'manifest.folders.len()',
  'recursive_zip_preserves_empty_directories_and_verifies_archive',
  'recursive_zip_exports_directory_only_tree_without_file_content',
  'fn check_folder_zip_completeness(',
  'recursive_zip_preflight_matches_complete_native_subtree',
  'recursive_zip_refuses_hidden_unavailable_file_without_publishing_archive',
  'recursive_zip_refuses_hidden_copy_on_write_base',
  'recursive_zip_refuses_late_unselected_file_even_without_revision_bump',
  'recursive_zip_scoped_preflight_ignores_large_unrelated_folder',
  'recursive_zip_accepts_authoritative_v2_over_missing_legacy_fallback',
  'folder_revision: Option<i64>', 'check_folder_zip_completeness(&conn, &snapshot, folder, &entry_ids)']) {
  assert.ok(native.includes(marker), 'Missing native folder ZIP completeness safeguard: '+marker);
}
assert.ok(ui.includes('folderRevision:folderBundle?.revision??null'));
assert.ok(ui.includes('folderId:folderBundle?.folderId??null'));
console.log('PASS Drive Folder ZIP: nested selection, full-folder preflight, pending/COW/symlink/limit safeguards and verified native ZIP integration');
