import {validateDriveSnapshot} from './deviceContinuity';
import {normalizeDriveTrashEntries, type SafeDriveTrashEntry} from './driveTrashBrowse';

export interface BatchRestoreInventory {
  revision:number;
  entries:SafeDriveTrashEntry[];
  skipped:number;
}
export interface BatchRestoreRow {
  kind:'file'|'folder'|'symlink';
  id:string;
  expected_name:string;
  expected_parent_id:string|null;
  expected_deleted_at:number;
}
export interface BatchRestoreRequest {
  revision:number;
  items:BatchRestoreRow[];
}
export interface BatchRestorePreview {
  revision:number;ready:boolean;count:number;restorable_bytes:number;
  planned:Array<{kind:'file'|'folder'|'symlink';path:string;size:number|null}>;
  problems:string[];
}
export interface BatchRestoreApplied {restored:number;revision:number;dirty:boolean;}
const record=(raw:unknown):Record<string,unknown>|null =>
  raw !== null && typeof raw === 'object' && !Array.isArray(raw) ? raw as Record<string,unknown> : null;
const validId=(v:string):boolean =>
  v.length>0 && v.length<=256 && v.trim()===v && v!=='.' && v!=='..'
  && !/[/\\\u0000-\u001f\u007f]/.test(v);

export function readBatchRestoreInventory(snapshot:unknown,trash:unknown):BatchRestoreInventory {
  const snap=validateDriveSnapshot(snapshot);
  const rows=normalizeDriveTrashEntries(trash);
  return {revision:snap.revision,entries:rows.entries,skipped:rows.skipped};
}
export function buildBatchRestoreRequest(
  inventory:BatchRestoreInventory,selected:readonly string[],
):BatchRestoreRequest {
  if(selected.length<1||selected.length>100||new Set(selected).size!==selected.length)
    throw Error('Select 1–100 distinct Recently Deleted Drive items.');
  const index=new Map(inventory.entries.map(t=>[t.kind+':'+t.id,t]));
  const items:BatchRestoreRow[]=selected.map(key=>{
    const row=index.get(key);
    if(!row || !validId(row.id) || !row.restorable || !row.name || row.name.length>255
      || row.name.trim()!==row.name || row.name==='.' || row.name==='..'
      || row.name.endsWith('.') || /[<>:"/\\|?*\u0000-\u001f\u007f]/.test(row.name)
      || (row.original_directory_id!==null && !validId(row.original_directory_id))) {
      throw Error('Selected item is unavailable or unsafe; refresh Recently Deleted.');
    }
    return {kind:row.kind,id:row.id,expected_name:row.name,
      expected_parent_id:row.original_directory_id,expected_deleted_at:row.deleted_at};
  });
  return {revision:inventory.revision,items};
}
export function readBatchRestorePreview(raw:unknown,request:BatchRestoreRequest):BatchRestorePreview {
  const r=record(raw);
  if(!r || !Number.isSafeInteger(r.revision) || r.revision!==request.revision
    || !Number.isSafeInteger(r.count) || r.count!==request.items.length
    || typeof r.ready!=='boolean'
    || !Number.isSafeInteger(r.restorable_bytes) || (r.restorable_bytes as number)<0
    || !Array.isArray(r.problems) || r.problems.length>100
    || r.problems.some(p=>typeof p!=='string' || p.length>4096)
    || !Array.isArray(r.planned) || r.planned.length>100
    || r.planned.some(item=>{
      const row=record(item);
      return !row || !['file','folder','symlink'].includes(String(row.kind))
        || typeof row.path!=='string' || !row.path.startsWith('/') || row.path.length>4096
        || (row.size!==null && (!Number.isSafeInteger(row.size)||(row.size as number)<0));
    })
    || (r.ready && ((r.problems as unknown[]).length!==0
      || (r.planned as unknown[]).length!==request.items.length))
    || (!r.ready && (r.planned as unknown[]).length!==0))
    throw Error('Invalid or stale native batch restore preview.');
  return r as unknown as BatchRestorePreview;
}
export function readBatchRestoreApplied(raw:unknown,request:BatchRestoreRequest):BatchRestoreApplied {
  const r=record(raw);
  if(!r || !Number.isSafeInteger(r.restored) || r.restored!==request.items.length
    || !Number.isSafeInteger(r.revision) || (r.revision as number)<=request.revision
    || r.dirty!==true)
    throw Error('Batch restore outcome is uncertain; refresh before retrying.');
  return r as unknown as BatchRestoreApplied;
}
