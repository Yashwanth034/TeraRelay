import {validateDriveSnapshot} from './deviceContinuity';
import {normalizeDriveFinderSnapshot} from './driveLocalFinder';
import {parseFolderInventory,type FolderItem} from './driveFolderOperations';

export interface InstantCopyFile {
 id:string;name:string;path:string;parentId:string|null;bytes:number|null;
}
export interface InstantCopyInventory {
 revision:number;files:InstantCopyFile[];folders:FolderItem[];skipped:number;
}
export interface InstantCopyRow {
 entry_id:string;expected_name:string;expected_parent_id:string|null;new_name:string;
}
export interface InstantCopyRequest {
 revision:number;destination_parent_id:string|null;files:InstantCopyRow[];
}
export interface InstantCopyPreview {
 revision:number;ready:boolean;count:number;total_bytes:number;problems:string[];
}
export interface InstantCopyApplied {copied:number;revision:number;dirty:boolean;}
const obj=(value:unknown):Record<string,unknown>|null=>
 !!value&&typeof value==='object'&&!Array.isArray(value)
   ?value as Record<string,unknown>:null;
const validName=(text:string):boolean=>text.length>0
 &&new TextEncoder().encode(text).length<=255
 &&text.trim()===text&&text!=='.'&&text!=='..'&&!text.endsWith('.')
 &&!/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(text)
 &&!/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(text);
export function instantCopyInventory(raw:unknown):InstantCopyInventory {
 const snapshot=validateDriveSnapshot(raw),finder=normalizeDriveFinderSnapshot(raw);
 const dirs=parseFolderInventory(raw);
 const files:InstantCopyFile[]=[];
 let skipped=finder.skipped+finder.truncated+dirs.skipped;
 const parents=new Map(dirs.folders.map(d=>[d.id,d.path]));
 const authoritative=new Map<string,{row:Record<string,unknown>;version:number}>();
 for(const item of snapshot.files){
   const row=obj(item),id=row?.entry_id;
   if(typeof id!=='string'||!id){skipped++;continue;}
   const version=row?.storage_version===2?2:1;
   const previous=authoritative.get(id);
   if(previous){
     if(version<=previous.version){skipped++;continue;}
     skipped++;
   }
   authoritative.set(id,{row:row!,version});
 }
 for(const [id,{row}] of authoritative){
   const name=row.display_name,parent=row.directory_id;
   if(typeof name!=='string'||!validName(name)
      ||(parent!==null&&(typeof parent!=='string'||!parent||!parents.has(parent)))) {
      skipped++;continue;
   }
   const parentId:string|null=parent===null?null:parent as string;
   const parentPath=parentId===null?'':parents.get(parentId)!;
   const bytes=row.plaintext_size??row.total_size;
   files.push({id,name,path:parentPath+'/'+name,parentId,
     bytes:typeof bytes==='number'&&Number.isSafeInteger(bytes)&&bytes>=0?bytes:null});
 }
 files.sort((a,b)=>a.path.localeCompare(b.path));
 return {revision:snapshot.revision,files,folders:dirs.folders,skipped};
}
export function buildInstantCopyRequest(
 inventory:InstantCopyInventory,ids:readonly string[],destination:string|null,
 prefix:string,suffix:string,
):InstantCopyRequest {
 if(!ids.length||ids.length>100||new Set(ids).size!==ids.length)
   throw Error('Select 1–100 distinct completed Drive files.');
 if(destination!==null&&!inventory.folders.some(f=>f.id===destination))
   throw Error('Destination Drive folder is no longer available.');
 const files=new Map(inventory.files.map(f=>[f.id,f]));
 const targets=new Set<string>();
 const changes=ids.map(id=>{
   const f=files.get(id);
   if(!f)throw Error('A source file is unavailable. Refresh the inventory.');
   const name=prefix+f.name+suffix;
   if(!validName(name))throw Error('Invalid copy filename: '+name.slice(0,80));
   const portable=name.normalize('NFC').toLocaleLowerCase();
   if(targets.has(portable))throw Error('Multiple files would have the same copy filename.');
   targets.add(portable);
   return {entry_id:f.id,expected_name:f.name,
      expected_parent_id:f.parentId,new_name:name};
 });
 return {revision:inventory.revision,destination_parent_id:destination,files:changes};
}
export function readInstantCopyPreview(raw:unknown,request:InstantCopyRequest):InstantCopyPreview {
 const r=obj(raw);
 if(!r||!Number.isSafeInteger(r.revision)||r.revision!==request.revision
   ||!Number.isSafeInteger(r.count)||r.count!==request.files.length
   ||!Number.isSafeInteger(r.total_bytes)||(r.total_bytes as number)<0
   ||typeof r.ready!=='boolean'||!Array.isArray(r.problems)
   ||r.problems.length>100||r.problems.some(s=>typeof s!=='string')
   ||(r.ready&&(r.problems as unknown[]).length!==0))
   throw Error('Invalid or stale native file copy preview.');
 return r as unknown as InstantCopyPreview;
}
export function readInstantCopyApplied(raw:unknown,request:InstantCopyRequest):InstantCopyApplied {
 const r=obj(raw);
 if(!r||!Number.isSafeInteger(r.copied)||r.copied!==request.files.length
   ||!Number.isSafeInteger(r.revision)||(r.revision as number)<=request.revision
   ||r.dirty!==true)
   throw Error('File copy outcome is uncertain. Refresh the inventory before retrying.');
 return r as unknown as InstantCopyApplied;
}
