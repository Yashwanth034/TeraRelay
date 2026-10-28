import { normalizeDriveFinderSnapshot } from './driveLocalFinder';
export interface BulkExportFile { id:string; name:string; path:string; size:number; }
export interface BulkExportInventory { files:BulkExportFile[]; skipped:number; }
export interface BulkExportProgress {
  id:string; phase:'running'|'done'|'cancelled'|'failed';
  current:number; count:number; bytes_written:number; total_bytes:number;
  files_verified:number; error:string|null;
}
export const MAX_BULK_EXPORT_BYTES = 8 * 1024 * 1024 * 1024;
export function parseBulkExportInventory(value:unknown): BulkExportInventory {
  const snapshot=normalizeDriveFinderSnapshot(value);
  const seen=new Set<string>();
  const files:BulkExportFile[]=[];
  let skipped=snapshot.skipped + snapshot.truncated;
  for(const entry of snapshot.entries) {
    if(entry.kind!=='file') continue;
    if(!Number.isSafeInteger(entry.size) || (entry.size ?? -1)<0
      || entry.size!>MAX_BULK_EXPORT_BYTES || seen.has(entry.id)) {skipped++;continue;}
    seen.add(entry.id);
    files.push({id:entry.id,name:entry.name,path:entry.path,size:entry.size!});
  }
  return {files,skipped};
}
export function parseBulkExportProgress(raw: unknown): BulkExportProgress {
  if(!raw || typeof raw!=='object' || Array.isArray(raw)) throw Error('Invalid Drive export status');
  const r=raw as Record<string,unknown>;
  const number=(key:string)=>typeof r[key]==='number'&&Number.isSafeInteger(r[key])&&(r[key] as number)>=0;
  if(typeof r.id!=='string' || !/^export-[a-f0-9]{32}$/.test(r.id)
    || !['running','done','cancelled','failed'].includes(String(r.phase))
    || !['current','count','bytes_written','total_bytes','files_verified'].every(number)
    || (r.count as number)>100 || (r.current as number)>(r.count as number)
    || (r.files_verified as number)>(r.current as number)
    || (r.total_bytes as number)>MAX_BULK_EXPORT_BYTES
    || (r.bytes_written as number)>(r.total_bytes as number)
    || (r.error!==null && typeof r.error!=='string'))
    throw Error('Invalid native Drive export status');
  if(r.phase==='done' && ((r.current as number)!==(r.count as number)
      || (r.bytes_written as number)!==(r.total_bytes as number)))
    throw Error('Incomplete Drive export cannot be marked done');
  return r as unknown as BulkExportProgress;
}
export function selectedTotalBytes(files:readonly BulkExportFile[],selected:readonly string[]):number{
  if(selected.length===0 || selected.length>100 || selected.length!==new Set(selected).size)
    throw Error('Choose 1–100 different completed Drive files.');
  const index=new Map(files.map(file=>[file.id,file]));
  let total=0;
  for(const id of selected){
    const row=index.get(id);
    if(!row)throw Error('A selected Drive file is no longer visible.');
    total+=row.size;
    if(!Number.isSafeInteger(total)||total>MAX_BULK_EXPORT_BYTES)
      throw Error('Maximum local ZIP export size is 8 GiB.');
  }
  return total;
}
