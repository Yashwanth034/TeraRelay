import {parseFolderInventory,type FolderInventory} from './driveFolderOperations';

export type TreeTrashInventory=FolderInventory;
export const readTreeTrashInventory=parseFolderInventory;
export interface TreeRequest {revision:number;root_id:string;}
export interface TreePreview {
  revision:number; ready:boolean; folders:number; files:number;
  logical_bytes:number; examples:string[]; problems:string[];
}
export interface TreeApplied {revision:number;dirty:boolean;folders:number;files:number;}
export interface TreeRun {
  root_id:string;name:string;folder_count:number;file_count:number;created_at:number;
}
export interface TreeRuns {revision:number;runs:TreeRun[];}
const object=(v:unknown):Record<string,unknown>|null =>
  v!==null&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const integer=(x:unknown,min:number,max:number):x is number =>
  typeof x==='number'&&Number.isSafeInteger(x)&&x>=min&&x<=max;
const id=(v:unknown):v is string =>
  typeof v==='string'&&v.length>0&&v.length<=256&&v.trim()===v
  &&v!=='.'&&v!=='..'&&!/[/\\\u0000-\u001f\u007f]/.test(v);
const name=(v:unknown):v is string =>
  typeof v==='string'&&v.length>0&&v.length<=255&&v.trim()===v
  &&v!=='.'&&v!=='..'&&!/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(v);

export function trashTreeRequest(inv:TreeTrashInventory,root:string):TreeRequest {
  if(!id(root)||!inv.folders.some(f=>f.id===root))
    throw Error('Selected Drive folder is no longer active. Refresh folders.');
  return {revision:inv.revision,root_id:root};
}
export function readTreeTrashRuns(raw:unknown):TreeRuns {
  const x=object(raw);
  if(!x||!integer(x.revision,0,Number.MAX_SAFE_INTEGER)||!Array.isArray(x.runs)
     ||x.runs.length>100)throw Error('Invalid native folder recovery journal listing.');
  const seen=new Set<string>();
  const runs:TreeRun[]=x.runs.map(r=>{
    const o=object(r);
    if(!o||!id(o.root_id)||seen.has(o.root_id)||!name(o.name)
       ||!integer(o.folder_count,1,512)||!integer(o.file_count,0,2000)
       ||!integer(o.created_at,0,Number.MAX_SAFE_INTEGER))
       throw Error('Invalid native folder recovery journal entry.');
    seen.add(o.root_id);
    return o as unknown as TreeRun;
  });
  return {revision:x.revision as number,runs};
}
export function restoreTreeRequest(runs:TreeRuns,root:string):TreeRequest {
  if(!id(root)||!runs.runs.some(r=>r.root_id===root))
    throw Error('Recovery journal no longer exists. Refresh recovery list.');
  return {revision:runs.revision,root_id:root};
}
export function readTreePreview(raw:unknown,request:TreeRequest):TreePreview {
  const x=object(raw);
  if(!x||x.revision!==request.revision||typeof x.ready!=='boolean'
      ||!integer(x.folders,0,512)||!integer(x.files,0,2000)
      ||!integer(x.logical_bytes,0,Number.MAX_SAFE_INTEGER)
      ||!Array.isArray(x.examples)||x.examples.length>25
      ||x.examples.some(p=>typeof p!=='string'||p.length>4096)
      ||!Array.isArray(x.problems)||x.problems.length>25
      ||x.problems.some(p=>typeof p!=='string'||p.length>4000)
      ||(x.ready&&(x.problems.length||!x.folders))
      ||(!x.ready&&(x.folders||x.files||x.logical_bytes||x.examples.length)))
    throw Error('Invalid or stale recursive Drive folder operation preview.');
  return x as unknown as TreePreview;
}
export function readTreeApplied(raw:unknown,req:TreeRequest,preview:TreePreview):TreeApplied {
  const x=object(raw);
  if(!x||!integer(x.revision,req.revision+1,Number.MAX_SAFE_INTEGER)
    ||x.dirty!==true||x.folders!==preview.folders||x.files!==preview.files)
    throw Error('Folder operation outcome uncertain. Refresh before retrying.');
  return x as unknown as TreeApplied;
}
