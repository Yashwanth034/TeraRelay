import {normalizeDriveFinderSnapshot} from './driveLocalFinder';
export interface IntegrityFile {id:string;path:string;size:number;}
export interface IntegrityInventory {files:IntegrityFile[];skipped:number;}
export interface IntegrityResult {path:string;outcome:'verified'|'hash_mismatch'|'read_error'|'missing'|'pending'|'unsafe_path'|'metadata_changed'|'unavailable';explanation:string;}
export interface IntegrityProgress {
  id:string;operation:'capture'|'verify';phase:'running'|'done'|'failed'|'cancelled';
  current:number;count:number;bytes_read:number;total_bytes:number;
  verified:number;issues:number;results:IntegrityResult[];error:string|null;
}
export const MAX_INTEGRITY_SIZE=8*1024*1024*1024;
export function integrityInventory(raw:unknown):IntegrityInventory {
  const snap=normalizeDriveFinderSnapshot(raw);const files:IntegrityFile[]=[];const ids=new Set<string>();
  let skipped=snap.skipped+snap.truncated;
  for(const item of snap.entries) {
    if(item.kind!=='file')continue;
    if(typeof item.size!=='number'||!Number.isSafeInteger(item.size)||item.size<0||item.size>MAX_INTEGRITY_SIZE||ids.has(item.id)){
      skipped++;continue;
    }
    ids.add(item.id);files.push({id:item.id,path:item.path,size:item.size});
  }
  return {files,skipped};
}
export function integritySelection(files:readonly IntegrityFile[],ids:readonly string[]):number {
  if(!ids.length||ids.length>100||new Set(ids).size!==ids.length)throw Error('Select 1–100 unique completed files.');
  const index=new Map(files.map(x=>[x.id,x]));let total=0;
  for(const id of ids){
    const f=index.get(id);if(!f)throw Error('Selected file is missing; refresh inventory.');
    total+=f.size;if(!Number.isSafeInteger(total)||total>MAX_INTEGRITY_SIZE)throw Error('Maximum scan size is 8 GiB.');
  }
  return total;
}
export function integrityBaselineInfo(raw:unknown):string[]|null {
  if(raw===null)return null;
  if(!Array.isArray(raw)||raw.length===0||raw.length>100||raw.some(v=>typeof v!=='string'||v.length===0||v.length>4096))
    throw Error('Invalid stored integrity baseline');
  return raw as string[];
}
export function integrityProgress(raw:unknown):IntegrityProgress {
  if(!raw||typeof raw!=='object'||Array.isArray(raw))throw Error('Invalid integrity progress');
  const o=raw as Record<string,unknown>;
  const natural=(key:string)=>typeof o[key]==='number'&&Number.isSafeInteger(o[key])&&(o[key] as number)>=0;
  if(typeof o.id!=='string'||!/^integrity-[a-f0-9]{32}$/.test(o.id)
    ||!['capture','verify'].includes(String(o.operation))
    ||!['running','done','failed','cancelled'].includes(String(o.phase))
    ||!['current','count','bytes_read','total_bytes','verified','issues'].every(natural)
    ||(o.count as number)>100||(o.current as number)>(o.count as number)
    ||(o.verified as number)+(o.issues as number)>(o.current as number)
    ||(o.total_bytes as number)>MAX_INTEGRITY_SIZE||(o.bytes_read as number)>(o.total_bytes as number)
    ||(o.error!==null&&typeof o.error!=='string')
    ||!Array.isArray(o.results)||(o.results as unknown[]).length>(o.current as number)
    ||(o.results as unknown[]).some(item=>{
      if(!item||typeof item!=='object'||Array.isArray(item))return true;
      const r=item as Record<string,unknown>;
      return typeof r.path!=='string'||r.path.length>4096||typeof r.explanation!=='string'||r.explanation.length>4096
        ||!['verified','hash_mismatch','read_error','missing','pending','unsafe_path','metadata_changed','unavailable'].includes(String(r.outcome));
    }))throw Error('Invalid native integrity result');
  if(o.phase==='done'&&(o.current!==o.count||o.operation==='capture'&&o.verified!==o.count))
    throw Error('Incomplete integrity operation cannot report success');
  return o as unknown as IntegrityProgress;
}
