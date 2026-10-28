export interface BatchRepairPair {
 source_id:string;backup_id:string;source_name:string;backup_name:string;
 add_folders:number;add_files:number;matching_files:number;
 added_logical_bytes:number;examples:string[];problems:string[];
}
export interface BatchRepairPreview {
 revision:number;plan_token:string;ready:boolean;
 total_folders:number;total_files:number;total_matches:number;
 added_logical_bytes:number;pairs:BatchRepairPair[];problems:string[];
}
export interface BatchRepairRequest {revision:number;plan_token:string}
export interface BatchRepairApplied {
 revision:number;dirty:true;pairs_updated:number;
 folders_added:number;files_added:number;files_matched:number;
}
const obj=(v:unknown):Record<string,unknown>|null=>
 v!==null&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const nat=(v:unknown,max:number):v is number=>
 typeof v==='number'&&Number.isSafeInteger(v)&&v>=0&&v<=max;
const id=(v:unknown):v is string=>
 typeof v==='string'&&v.length>0&&v.length<=256
 &&v.trim()===v&&v!=='.'&&v!=='..'&&!/[/\\\u0000-\u001f\u007f]/.test(v);
const name=(v:unknown):v is string=>
 typeof v==='string'&&v.length>0&&v.length<=255
 &&v.trim()===v&&v!=='.'&&v!=='..'
 &&!/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(v);
const bounded=(v:unknown,maxItems:number,maxLength:number):v is string[]=>
 Array.isArray(v)&&v.length<=maxItems
 &&v.every(row=>typeof row==='string'&&row.length<=maxLength);
export function readBatchRepairPreview(raw:unknown):BatchRepairPreview {
 const x=obj(raw);
 if(!x||!nat(x.revision,Number.MAX_SAFE_INTEGER)||typeof x.ready!=='boolean'
  ||!nat(x.total_folders,512)||!nat(x.total_files,2000)
  ||!nat(x.total_matches,48000)
  ||!nat(x.added_logical_bytes,Number.MAX_SAFE_INTEGER)
  ||!Array.isArray(x.pairs)||x.pairs.length>24
  ||!bounded(x.problems,25,4000)
  ||typeof x.plan_token!=='string'||(!/^[0-9a-f]{64}$/.test(x.plan_token)&&x.ready))
  throw Error('Invalid native Backup Batch Repair preview.');
 const seen=new Set<string>();
 let folders=0,files=0,matches=0,bytes=0,unready=false;
 const pairs=x.pairs.map(row=>{
  const p=obj(row);
  if(!p||!id(p.source_id)||!id(p.backup_id)||p.source_id===p.backup_id
   ||!name(p.source_name)||!name(p.backup_name)
   ||!nat(p.add_folders,512)||!nat(p.add_files,2000)
   ||!nat(p.matching_files,2000)||!nat(p.added_logical_bytes,Number.MAX_SAFE_INTEGER)
   ||!bounded(p.examples,25,4096)||!bounded(p.problems,25,4000))
   throw Error('Invalid saved backup pair repair preview.');
  const key=p.source_id+'\u0000'+p.backup_id;
  if(seen.has(key))throw Error('Duplicate backup repair pair.');
  seen.add(key);
  folders+=p.add_folders as number;files+=p.add_files as number;
  matches+=p.matching_files as number;bytes+=p.added_logical_bytes as number;
  if((p.problems as string[]).length)unready=true;
  return p as unknown as BatchRepairPair;
 });
 if(folders!==x.total_folders||files!==x.total_files||matches!==x.total_matches
   ||bytes!==x.added_logical_bytes
   ||(x.ready&&(x.problems as string[]).length!==0)
   ||(x.ready&&(unready||(!folders&&!files)||pairs.length===0))
   ||(x.ready&&!/^[0-9a-f]{64}$/.test(x.plan_token)))
  throw Error('Inconsistent native backup repair summary.');
 return {...x,pairs} as BatchRepairPreview;
}
export function batchRepairRequest(p:BatchRepairPreview):BatchRepairRequest{
 if(!p.ready||!p.plan_token.match(/^[0-9a-f]{64}$/))
  throw Error('The watched backups need a fresh successful preview before repair.');
 return {revision:p.revision,plan_token:p.plan_token};
}
export function readBatchRepairApplied(
 raw:unknown,request:BatchRepairRequest,preview:BatchRepairPreview
):BatchRepairApplied {
 const x=obj(raw);
 if(!x||x.dirty!==true||!nat(x.revision,Number.MAX_SAFE_INTEGER)
  ||(x.revision as number)<=request.revision
  ||!nat(x.pairs_updated,24)||x.pairs_updated===0
  ||x.folders_added!==preview.total_folders
  ||x.files_added!==preview.total_files
  ||x.files_matched!==preview.total_matches)
  throw Error('Backup batch repair result uncertain; inspect Drive before retrying.');
 return x as unknown as BatchRepairApplied;
}
