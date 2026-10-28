import {parseFolderInventory,type FolderInventory} from './driveFolderOperations';

export type MirrorInventory=FolderInventory;
export const readMirrorInventory=parseFolderInventory;
export interface MirrorRequest {revision:number;source_id:string;target_id:string;}
export interface MirrorPreview {
 revision:number;ready:boolean;add_folders:number;add_files:number;
 matching_files:number;added_logical_bytes:number;examples:string[];problems:string[];
}
export interface MirrorApplied {
 revision:number;dirty:boolean;folders_added:number;files_added:number;files_matched:number;
}
const object=(value:unknown):Record<string,unknown>|null=>
  value!==null&&typeof value==='object'&&!Array.isArray(value)
    ?value as Record<string,unknown>:null;
export function buildMirrorRequest(
 inventory:MirrorInventory,source:string,target:string,
):MirrorRequest {
 if(!source||!target||source===target)
   throw Error('Choose two distinct existing Drive folders.');
 const sourceFolder=inventory.folders.find(f=>f.id===source);
 const targetFolder=inventory.folders.find(f=>f.id===target);
 if(!sourceFolder||!targetFolder)
   throw Error('Source or backup folder is no longer available; refresh Drive.');
 if(targetFolder.path.startsWith(sourceFolder.path+'/')
   ||sourceFolder.path.startsWith(targetFolder.path+'/'))
   throw Error('Source and backup folders must not be nested inside each other.');
 return {revision:inventory.revision,source_id:source,target_id:target};
}
export function readMirrorPreview(raw:unknown,request:MirrorRequest):MirrorPreview {
 const x=object(raw);
 const natural=(key:string,max:number):boolean=>!!x&&typeof x[key]==='number'
   &&Number.isSafeInteger(x[key])&&(x[key] as number)>=0&&(x[key] as number)<=max;
 if(!x||x.revision!==request.revision||typeof x.ready!=='boolean'
   ||!natural('add_folders',512)||!natural('add_files',2000)
   ||!natural('matching_files',2000)
   ||!natural('added_logical_bytes',Number.MAX_SAFE_INTEGER)
   ||!Array.isArray(x.examples)||x.examples.length>25
   ||x.examples.some(p=>typeof p!=='string'||p.length>4096)
   ||!Array.isArray(x.problems)||x.problems.length>100
   ||x.problems.some(p=>typeof p!=='string'||p.length>4000)
   ||(x.ready&&(x.problems as unknown[]).length>0)
   ||(!x.ready&&(x.add_folders!==0||x.add_files!==0
       ||x.matching_files!==0||x.added_logical_bytes!==0)))
   throw Error('Invalid or stale native incremental Drive mirror preview.');
 return x as unknown as MirrorPreview;
}
export function readMirrorApplied(raw:unknown,request:MirrorRequest,preview:MirrorPreview):MirrorApplied{
 const x=object(raw);
 if(!x||!Number.isSafeInteger(x.revision)||(x.revision as number)<=request.revision
   ||x.dirty!==true||x.folders_added!==preview.add_folders
   ||x.files_added!==preview.add_files||x.files_matched!==preview.matching_files)
   throw Error('Incremental Drive mirror outcome uncertain. Refresh before retrying.');
 return x as unknown as MirrorApplied;
}
