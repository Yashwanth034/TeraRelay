'use strict';
const assert=require('node:assert/strict');
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const root=path.resolve(__dirname,'..');
const ts=require(require.resolve('typescript',{paths:[path.join(root,'app')]}));
const src=fs.readFileSync(path.join(root,'app/src/driveSmartViews.ts'),'utf8');
const exported={};
vm.runInNewContext(ts.transpileModule(src,{
  compilerOptions:{module:ts.ModuleKind.CommonJS,target:ts.ScriptTarget.ES2020},
}).outputText,{exports:exported});
const id='smart-'+'a'.repeat(32);
const rule={...exported.EMPTY_SMART_RULE,name_contains:'report',
  extension:'pdf',min_size:10,max_size:1000,category:'document'};
assert.equal(exported.parseSmartRule(rule).name_contains,'report');
const saved={id,name:'Latest reports',revision:1,rule};
assert.equal(exported.parseSmartViews([saved])[0].id,id);
assert.throws(()=>exported.parseSmartViews([saved,saved]),/Invalid/);
assert.throws(()=>exported.parseSmartViews([{...saved,revision:0}]),/Invalid/);
assert.throws(()=>exported.parseSmartRule({...rule,extension:'../pdf'}),/Invalid/);
assert.throws(()=>exported.parseSmartRule({...rule,min_size:10000}),/Invalid/);
assert.throws(()=>exported.parseSmartRule({...exported.EMPTY_SMART_RULE}),/no search criteria/);
const preview={view_id:id,revision:1,snapshot_revision:9,total_matches:1,truncated:false,
  local_only:true,matches:[{entry_id:'file-1',name:'report.pdf',
    path:'/Work/report.pdf',size:100,updated_at:1000,category:'document'}]};
assert.equal(exported.parseSmartPreview(preview).matches[0].path,'/Work/report.pdf');
assert.throws(()=>exported.parseSmartPreview({...preview,local_only:false}),/Invalid/);
assert.throws(()=>exported.parseSmartPreview({...preview,total_matches:2,truncated:false}),/Invalid/);
assert.throws(()=>exported.parseSmartPreview({...preview,matches:[preview.matches[0],preview.matches[0]]}),/Invalid/);
const ui=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/DriveSmartViews.tsx'),'utf8');
const native=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/drive_smart_views.rs'),'utf8');
for(const action of ['cmd_drive_smart_views_save','cmd_drive_smart_views_delete',
  'cmd_drive_smart_views_preview','cmd_drive_smart_views_list']){
  assert.ok(ui.includes(action),'UI missing '+action);
  assert.ok(native.includes(action),'native command missing '+action);
}
for(const marker of ['SAVEPOINT smart_view_save','ROLLBACK TO smart_view_save',
  'MAX_VIEWS','MAX_FILES','MAX_RESULTS','drive_snapshot(&conn)','#[serde(deny_unknown_fields)]']){
  assert.ok(native.includes(marker),'missing bounded safe native contract '+marker);
}
assert.ok(ui.includes('never move, delete or upload Drive files'));
assert.ok(native.includes('updated_within_days'), 'rolling date rules must be evaluated at preview time');
const lib=fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'),'utf8');
const mods=fs.readFileSync(path.join(root,'app/src-tauri/src/commands/mod.rs'),'utf8');
const settings=fs.readFileSync(path.join(root,'app/src/components/desktop/dashboard/SettingsModal.tsx'),'utf8');
assert.equal((mods.match(/pub mod drive_smart_views;/g)||[]).length,1,
  'Smart Views native module must be declared exactly once');
for(const cmd of ['list','save','delete','preview']){
  assert.equal((lib.match(new RegExp('commands::cmd_drive_smart_views_'+cmd+',','g'))||[]).length,1,
    'duplicate or unregistered native Smart Views handler '+cmd);
}
assert.equal((settings.match(/<DriveSmartViews active=/g)||[]).length,1,
  'Settings should mount one Smart Views panel');
assert.equal((settings.match(/import \{ DriveSmartViews \}/g)||[]).length,1,
  'Settings should import Smart Views only once');
console.log('PASS Drive Smart Views: strict native IPC shapes, bounded live rules and unique Settings registration');
