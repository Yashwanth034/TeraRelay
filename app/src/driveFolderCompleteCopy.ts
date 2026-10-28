import {parseFolderInventory,type FolderInventory} from './driveFolderOperations';
import {validCloneName} from './driveFolderClone';
export type CompleteCopyInventory=FolderInventory;
export const readCompleteCopyInventory=parseFolderInventory;
export interface CompleteCopyRequest {
 revision:number;source_id:string;expected_source_name:string;
 expected_source_parent_id:string|null;destination_parent_id:string|null;
 new_root_name:string;
}
export interface CompleteCopyPreview {
 revision:number;ready:boolean;folders:number;files:number;referenced_bytes:number;
 problems:string[];
}
export interface CompleteCopyApplied {
 revision:number;dirty:boolean;root_id:string;folders:number;files:number;
}
const obj=(raw:unknown):Record<string,unknown>|null=>
 raw!==null&&typeof raw==='object'&&!Array.isArray(raw)?raw as Record<string,unknown>:null;
export function buildCompleteCopyRequest(
 inventory:CompleteCopyInventory,sourceId:string,destinationId:string|null,title:string
):CompleteCopyRequest {
 const src=inventory.folders.find(f=>f.id===sourceId);
 if(!src)throw Error('Source folder is unavailable; refresh.');
 if(destinationId!==null&&!inventory.folders.some(f=>f.id===destinationId))
   throw Error('Destination folder is unavailable; refresh.');
 if(!validCloneName(title))throw Error('Invalid portable name for the copied folder.');
 return {revision:inventory.revision,source_id:src.id,
   expected_source_name:src.name,expected_source_parent_id:src.parentId,
   destination_parent_id:destinationId,new_root_name:title};
}
export function decodeCompleteCopyPreview(raw:unknown,req:CompleteCopyRequest):CompleteCopyPreview {
 const o=obj(raw),natural=(k:string)=>o!==null&&typeof o[k]==='number'
   &&Number.isSafeInteger(o[k])&&(o[k] as number)>=0;
 if(!o||!natural('revision')||o.revision!==req.revision
   ||typeof o.ready!=='boolean'||!natural('folders')||!natural('files')
   ||(o.folders as number)>512||(o.files as number)>2000
   ||!natural('referenced_bytes')
   ||!Array.isArray(o.problems)||o.problems.length>100
   ||o.problems.some(p=>typeof p!=='string'||p.length>3000)
   ||(o.ready&&(o.folders===0||o.problems.length!==0))
   ||(!o.ready&&(o.folders!==0||o.files!==0||o.referenced_bytes!==0)))
   throw Error('Invalid or stale native complete-folder-copy preview.');
 return o as unknown as CompleteCopyPreview;
}
export function decodeCompleteCopyApplied(
 raw:unknown,req:CompleteCopyRequest,checked:CompleteCopyPreview
):CompleteCopyApplied {
 const o=obj(raw);
 if(!o||!Number.isSafeInteger(o.revision)||(o.revision as number)<=req.revision
   ||o.dirty!==true||o.folders!==checked.folders||o.files!==checked.files
   ||typeof o.root_id!=='string'||o.root_id===req.source_id
   ||!o.root_id||o.root_id.length>256)
   throw Error('Complete folder copy outcome cannot be confirmed. Refresh before retrying.');
 return o as unknown as CompleteCopyApplied;
}
