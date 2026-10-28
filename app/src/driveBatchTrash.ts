import {validateDriveSnapshot} from './deviceContinuity';
import {instantCopyInventory} from './driveFileCopy';

export interface BatchTrashSource {
  entryId:string;
  name:string;
  path:string;
  parentId:string|null;
  storageVersion:1|2;
  logicalBytes:number|null;
}
export interface BatchTrashInventory {
  revision:number;files:BatchTrashSource[];skipped:number;
}
export interface BatchTrashRow {
  entry_id:string;
  expected_name:string;
  expected_parent_id:string|null;
  expected_storage_version:1|2;
}
export interface BatchTrashRequest {revision:number;files:BatchTrashRow[]}
export interface BatchTrashPreview {
  revision:number;ready:boolean;count:number;logical_bytes:number;problems:string[];
}
export interface BatchTrashApplied {trashed:number;revision:number;dirty:boolean}
const object=(x:unknown):Record<string,unknown>|null=>
  x!==null&&typeof x==='object'&&!Array.isArray(x)?x as Record<string,unknown>:null;
export function readBatchTrashInventory(raw:unknown):BatchTrashInventory {
  const snapshot=validateDriveSnapshot(raw);
  const index=instantCopyInventory(raw);
  const versions=new Map<string,1|2>();
  for(const item of snapshot.files){
    const value=object(item);
    const id=value?.entry_id;
    if(typeof id!=='string')continue;
    const version=value?.storage_version===2?2:1;
    if(!versions.has(id)||version===2)versions.set(id,version);
  }
  const files:BatchTrashSource[]=index.files.flatMap(file=>{
    const version=versions.get(file.id);
    if(!version)return [];
    return [{
      entryId:file.id,name:file.name,path:file.path,parentId:file.parentId,
      storageVersion:version,logicalBytes:file.bytes,
    }];
  });
  return {revision:index.revision,files,skipped:index.skipped};
}
export function buildBatchTrashRequest(
  inventory:BatchTrashInventory, selectedIds:readonly string[],
):BatchTrashRequest {
  if(!selectedIds.length||selectedIds.length>100||
      new Set(selectedIds).size!==selectedIds.length)
    throw Error('Select 1–100 distinct completed Drive files.');
  const lookup=new Map(inventory.files.map(file=>[file.entryId,file]));
  return {revision:inventory.revision,files:selectedIds.map(entryId=>{
    const file=lookup.get(entryId);
    if(!file)throw Error('Drive file disappeared from current inventory; refresh.');
    return {
      entry_id:file.entryId,expected_name:file.name,
      expected_parent_id:file.parentId,expected_storage_version:file.storageVersion,
    };
  })};
}
function readObject(x:unknown):Record<string,unknown>{
  const data=object(x);
  if(!data)throw Error('Invalid Drive batch Trash response.');
  return data;
}
export function readBatchTrashPreview(
  raw:unknown, request:BatchTrashRequest,
):BatchTrashPreview {
  const data=readObject(raw);
  if(data.revision!==request.revision||typeof data.ready!=='boolean'||
      data.count!==request.files.length||!Number.isSafeInteger(data.logical_bytes)||
      (data.logical_bytes as number)<0||
      !Array.isArray(data.problems)||data.problems.length>100||
      data.problems.some(x=>typeof x!=='string'||x.length>2048)||
      (data.ready&&(data.problems.length!==0))||
      (!data.ready&&data.logical_bytes!==0))
    throw Error('Invalid or stale native Drive batch Trash preview.');
  return data as unknown as BatchTrashPreview;
}
export function readBatchTrashApplied(
  raw:unknown, request:BatchTrashRequest,
):BatchTrashApplied {
  const data=readObject(raw);
  if(data.trashed!==request.files.length||
      !Number.isSafeInteger(data.revision)||
      (data.revision as number)<=request.revision||data.dirty!==true)
    throw Error('Drive batch Trash outcome is uncertain; inspect Recently Deleted before retrying.');
  return data as unknown as BatchTrashApplied;
}
