import {parseFolderInventory,type FolderInventory} from './driveFolderOperations';
export {parseFolderInventory};
export type {FolderInventory};
export interface BackupWatchPair {
 source_id:string;backup_id:string;source_name:string;backup_name:string;
 source_parent_id:string|null;backup_parent_id:string|null;created_at:number;
}
export interface BackupWatchList {revision:number;pairs:BackupWatchPair[];}
export type WatchStatus='healthy'|'drift'|'unverified'|'blocked';
export interface BackupWatchCheck {
 source_id:string;backup_id:string;status:WatchStatus;explanation:string;
 source_files:number;backup_files:number;only_source:number;only_backup:number;
 different:number;unverified:number;samples:string[];
}
export interface BackupWatchScan {
 revision:number;dirty:boolean;checked_at:number;
 healthy:number;drift:number;unverified:number;blocked:number;
 checks:BackupWatchCheck[];metadata_only:true;
}
const rec=(x:unknown):Record<string,unknown>|null=>
 x!==null&&typeof x==='object'&&!Array.isArray(x)?x as Record<string,unknown>:null;
const nat=(n:unknown):n is number=>
 typeof n==='number'&&Number.isSafeInteger(n)&&n>=0;
const safe=(x:unknown):x is string=>
 typeof x==='string'&&x.length>0&&x.length<=256&&x.trim()===x
 &&x!=='.'&&x!=='..'&&!/[/\\\u0000-\u001f\u007f]/.test(x);
const validParent=(x:unknown):x is string|null=>x===null||safe(x);
const word=(x:unknown,length=255):x is string=>
 typeof x==='string'&&x.length>0&&x.length<=length&&!/[\u0000-\u001f\u007f]/.test(x);
export function readWatchList(raw:unknown):BackupWatchList {
 const x=rec(raw);
 if(!x||!nat(x.revision)||!Array.isArray(x.pairs)||x.pairs.length>24)
  throw Error('Invalid backup Watchlist response.');
 const keys=new Set<string>();
 const pairs=x.pairs.map((item:unknown)=>{
  const p=rec(item);
  if(!p||!safe(p.source_id)||!safe(p.backup_id)||p.source_id===p.backup_id
   ||!word(p.source_name)||!word(p.backup_name)
   ||!validParent(p.source_parent_id)||!validParent(p.backup_parent_id)
   ||!nat(p.created_at)||p.created_at===0)
   throw Error('Invalid saved backup pair.');
  const key=p.source_id+'\u0000'+p.backup_id;
  if(keys.has(key))throw Error('Duplicated backup pair returned by native storage.');
  keys.add(key);
  return p as unknown as BackupWatchPair;
 });
 return {revision:x.revision as number,pairs};
}
export function readWatchScan(raw:unknown,list:BackupWatchList):BackupWatchScan {
 const x=rec(raw);
 if(!x||!nat(x.revision)||typeof x.dirty!=='boolean'||!nat(x.checked_at)
  ||x.checked_at===0||x.metadata_only!==true
  ||!nat(x.healthy)||!nat(x.drift)||!nat(x.unverified)||!nat(x.blocked)
  ||!Array.isArray(x.checks)||x.checks.length!==list.pairs.length)
  throw Error('Invalid backup Watchlist scan response.');
 const counts:Record<WatchStatus,number>={healthy:0,drift:0,unverified:0,blocked:0};
 const keys=new Set<string>();
 const expected=new Set(list.pairs.map(p=>p.source_id+'\u0000'+p.backup_id));
 const checks=x.checks.map((item:unknown)=>{
  const c=rec(item);
  if(!c||!safe(c.source_id)||!safe(c.backup_id)||!word(c.explanation,1500)
   ||!['healthy','drift','unverified','blocked'].includes(String(c.status))
   ||!nat(c.source_files)||!nat(c.backup_files)||!nat(c.only_source)
   ||!nat(c.only_backup)||!nat(c.different)||!nat(c.unverified)
   ||!Array.isArray(c.samples)||c.samples.length>6
   ||c.samples.some(v=>typeof v!=='string'||v.length>4096))
   throw Error('Invalid backup Watchlist check.');
  const key=c.source_id+'\u0000'+c.backup_id;
  if(!expected.has(key)||keys.has(key))throw Error('Stale or duplicate backup pair in scan.');
  keys.add(key);
  counts[c.status as WatchStatus]++;
  if(c.status==='healthy'&&(c.only_source!==0||c.only_backup!==0||c.different!==0
   ||c.unverified!==0||x.dirty!==false))
   throw Error('Native scan inaccurately claims a healthy backup.');
  if(c.status==='drift'&&c.only_source+c.only_backup+c.different===0)
   throw Error('Native scan drift result is inconsistent.');
  return c as unknown as BackupWatchCheck;
 });
 if(counts.healthy!==x.healthy||counts.drift!==x.drift
  ||counts.unverified!==x.unverified||counts.blocked!==x.blocked)
  throw Error('Native backup Watchlist status totals disagree with rows.');
 return {...x,checks} as BackupWatchScan;
}
export function validateWatchPair(
 inventory:FolderInventory, source:string, backup:string, existing:BackupWatchList
):{revision:number;sourceId:string;backupId:string}{
 const a=inventory.folders.find(p=>p.id===source),b=inventory.folders.find(p=>p.id===backup);
 if(!a||!b||source===backup)throw Error('Choose two distinct visible Drive folders.');
 if(a.path.startsWith(b.path+'/')||b.path.startsWith(a.path+'/'))
  throw Error('Original and backup folders must not be nested.');
 if(existing.pairs.some(p=>(p.source_id===source&&p.backup_id===backup)
   ||(p.source_id===backup&&p.backup_id===source)))
  throw Error('These folders are already monitored.');
 if(existing.pairs.length>=24)throw Error('Watchlist is full (24 pairs).');
 return {revision:inventory.revision,sourceId:source,backupId:backup};
}
