/** Strict data contracts for local-only Drive Time Capsule history. */
export interface CapsuleInfo {
  seq:number;captured_at:number;revision:number;dirty:boolean;
  item_count:number;fingerprint:string;
}
export type CapsuleChangeKind =
  'created'|'removed'|'moved'|'renamed'|'ancestor_path_changed'|'availability_changed'|'backing_state_changed'|'content_metadata_changed';
export interface CapsuleChange {
  change:CapsuleChangeKind;kind:'file'|'folder'|'pending'|'link';
  old_path:string|null;new_path:string|null;
  previous_backing:string|null;current_backing:string|null;
}
export interface CapsuleComparison {
  before:CapsuleInfo;after:CapsuleInfo;created:number;removed:number;
  moved:number;renamed:number;ancestor_path_changed:number;content_metadata_changed:number;
  availability_changed:number;backing_state_changed:number;changes:CapsuleChange[];
}
const obj=(r:unknown):Record<string,unknown>|null=>
  r!==null&&typeof r==='object'&&!Array.isArray(r)?r as Record<string,unknown>:null;
const count=(r:unknown,max=5000):r is number=>Number.isSafeInteger(r)&&(r as number)>=0&&(r as number)<=max;
const validFingerprint=(x:unknown):x is string=>
  typeof x==='string'&&/^[a-f0-9]{64}$/.test(x);
export function readCapsuleInfo(raw:unknown):CapsuleInfo {
 const r=obj(raw);
 if(!r||!count(r.seq,Number.MAX_SAFE_INTEGER)||r.seq===0
    ||!count(r.captured_at,Number.MAX_SAFE_INTEGER)||!count(r.revision,Number.MAX_SAFE_INTEGER)
    ||!count(r.item_count)||typeof r.dirty!=='boolean'||!validFingerprint(r.fingerprint))
   throw Error('Invalid local Drive history record.');
 return r as unknown as CapsuleInfo;
}
export function readCapsuleHistory(raw:unknown):CapsuleInfo[]{
 if(!Array.isArray(raw)||raw.length>12)throw Error('Invalid local Drive history index.');
 const history=raw.map(readCapsuleInfo);
 if(new Set(history.map(i=>i.seq)).size!==history.length
   ||history.some((i,index)=>index>0&&history[index-1].seq<=i.seq))
   throw Error('Unsorted or duplicated local Drive history.');
 return history;
}
const validPath=(v:unknown):v is string=>
 typeof v==='string'&&v.length<=4096&&v.startsWith('/')
  &&!v.includes('//')&&!/[\u0000-\u001f\u007f]/.test(v);
export function readCapsuleComparison(raw:unknown,older:number,newer:number):CapsuleComparison {
 const r=obj(raw);if(!r)throw Error('Missing historical Drive comparison.');
 const before=readCapsuleInfo(r.before),after=readCapsuleInfo(r.after);
 const types=['created','removed','moved','renamed','ancestor_path_changed','availability_changed','backing_state_changed','content_metadata_changed'];
 const counts=['created','removed','moved','renamed','ancestor_path_changed','availability_changed','backing_state_changed','content_metadata_changed'];
 if(before.seq!==older||after.seq!==newer||older>=newer
    ||!counts.every(c=>count(r[c]))
    ||!Array.isArray(r.changes)||r.changes.length>10000
    ||r.changes.some(value=>{
      const item=obj(value);
      return !item||!types.includes(String(item.change))
        ||!['file','folder','pending','link'].includes(String(item.kind))
        ||(item.old_path!==null&&!validPath(item.old_path))
        ||(item.new_path!==null&&!validPath(item.new_path))
        ||(item.previous_backing!==null&&(typeof item.previous_backing!=='string'||item.previous_backing.length>64))
        ||(item.current_backing!==null&&(typeof item.current_backing!=='string'||item.current_backing.length>64));
    })
    ||counts.reduce((sum,c)=>sum+(r[c] as number),0)!==r.changes.length)
   throw Error('Drive history comparison is stale or corrupt.');
 return r as unknown as CapsuleComparison;
}
export const snapshotCaption=(i:CapsuleInfo):string=>
 new Date(i.captured_at).toLocaleString()+' · '+i.item_count+' items'+(i.dirty?' · unsynced locally':'');
