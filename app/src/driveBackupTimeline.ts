export type BackupVerdict='healthy'|'drift'|'unverified'|'blocked';
export interface AuditRow {source_id:string;backup_id:string;status:BackupVerdict;
 explanation:string;source_files:number;backup_files:number;
 only_source:number;only_backup:number;different:number;
 unverified:number;samples:string[];}
export interface AuditScan {revision:number;dirty:boolean;checked_at:number;
 healthy:number;drift:number;unverified:number;blocked:number;checks:AuditRow[];}
export interface AuditRun {id:number;scan:AuditScan;}
export interface AuditHistory {runs:AuditRun[];max_runs:40;}
const obj=(v:unknown):Record<string,unknown>|null=>
 v!==null&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const safe=(v:unknown,max=5000):v is number=>
 typeof v==='number'&&Number.isSafeInteger(v)&&v>=0&&v<=max;
const validId=(v:unknown):v is string=>
 typeof v==='string'&&v.length>0&&v.length<=256&&v.trim()===v
 &&v!=='.'&&v!=='..'&&!/[/\\\u0000-\u001f\u007f]/.test(v);
export function readAuditHistory(raw:unknown):AuditHistory {
 const outer=obj(raw);
 if(!outer||outer.max_runs!==40||!Array.isArray(outer.runs)||outer.runs.length>40)
  throw Error('Invalid native backup audit timeline.');
 const ids=new Set<number>();
 const runs=outer.runs.map((item:unknown)=>{
  const r=obj(item),s=obj(r?.scan);
  if(!r||!s||!safe(r.id,Number.MAX_SAFE_INTEGER)||r.id===0||ids.has(r.id)
   ||!safe(s.revision,Number.MAX_SAFE_INTEGER)||!safe(s.checked_at,Number.MAX_SAFE_INTEGER)
   ||!s.checked_at||typeof s.dirty!=='boolean'||!Array.isArray(s.checks)
   ||s.checks.length===0||s.checks.length>24
   ||!safe(s.healthy,24)||!safe(s.drift,24)
   ||!safe(s.unverified,24)||!safe(s.blocked,24))
   throw Error('Invalid saved backup audit record.');
  ids.add(r.id);
  const seen=new Set<string>();
  const counts:{[K in BackupVerdict]:number}={healthy:0,drift:0,unverified:0,blocked:0};
  const checks=s.checks.map((value:unknown)=>{
   const c=obj(value);
   if(!c||!validId(c.source_id)||!validId(c.backup_id)||c.source_id===c.backup_id
    ||!['healthy','drift','unverified','blocked'].includes(String(c.status))
    ||typeof c.explanation!=='string'||!c.explanation||c.explanation.length>1500
    ||!safe(c.source_files,2000)||!safe(c.backup_files,2000)
    ||!safe(c.only_source)||!safe(c.only_backup)||!safe(c.different)||!safe(c.unverified)
    ||!Array.isArray(c.samples)||c.samples.length>6
    ||c.samples.some(v=>typeof v!=='string'||v.length>4096))
    throw Error('Invalid historical backup comparison.');
   const key=c.source_id+'\0'+c.backup_id;
   if(seen.has(key))throw Error('Duplicate saved backup identity.');
   seen.add(key);
   counts[c.status as BackupVerdict]++;
   if(c.status==='healthy'&&(s.dirty===true||c.only_source||c.only_backup
      ||c.different||c.unverified))throw Error('Audit inaccurately reports a healthy backup.');
   if(c.status==='drift'&&!(c.only_source+c.only_backup+c.different))
    throw Error('Audit drift details missing.');
   return c as unknown as AuditRow;
  });
  if(counts.healthy!==s.healthy||counts.drift!==s.drift
   ||counts.unverified!==s.unverified||counts.blocked!==s.blocked)
   throw Error('Backup timeline summary conflicts with its comparisons.');
  return {id:r.id,scan:{...s,checks}} as AuditRun;
 });
 if(runs.some((r,i)=>i>0&&r.id>=runs[i-1].id))
  throw Error('Backup timeline runs are out of order.');
 return {runs,max_runs:40};
}
export interface AuditChange {pair:string;before:BackupVerdict|null;
 after:BackupVerdict;direction:'new'|'unchanged'|'improved'|'worse'|'changed';}
export function auditChanges(current:AuditRun,prior:AuditRun|null):AuditChange[]{
 const old=new Map((prior?.scan.checks??[]).map(x=>[x.source_id+'\0'+x.backup_id,x.status]));
 return current.scan.checks.map(c=>{
  const pair=c.source_id+'\0'+c.backup_id;
  const before=old.get(pair)??null;
  let direction:AuditChange['direction']='changed';
  if(before===null)direction='new';
  else if(before===c.status)direction='unchanged';
  else if(c.status==='healthy')direction='improved';
  else if(before==='healthy'||c.status==='drift')direction='worse';
  return {pair,before,after:c.status,direction};
 });
}
