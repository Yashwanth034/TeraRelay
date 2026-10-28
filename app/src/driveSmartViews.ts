export type SmartCategory='image'|'video'|'audio'|'document'|'archive'|'other';
export interface SmartRule {
  name_contains:string|null;
  extension:string|null;
  category:SmartCategory|null;
  min_size:number|null;
  max_size:number|null;
  updated_after:number|null;
  updated_within_days:number|null;
  folder_id:string|null;
}
export interface SmartView {
  id:string;name:string;revision:number;rule:SmartRule;
}
export interface SmartMatch {
  entry_id:string;name:string;path:string;size:number;updated_at:number;category:SmartCategory;
}
export interface SmartPreview {
  view_id:string;revision:number;snapshot_revision:number;total_matches:number;
  truncated:boolean;matches:SmartMatch[];local_only:true;
}
const record=(v:unknown):Record<string,unknown>|null=>
  !!v&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const count=(v:unknown):v is number=>typeof v==='number'&&Number.isSafeInteger(v)&&v>=0;
const validId=(v:unknown):v is string=>typeof v==='string'&&v.length===38&&/^smart-[a-f0-9]{32}$/i.test(v);
const category=(v:unknown):v is SmartCategory=>typeof v==='string'&&
  ['image','video','audio','document','archive','other'].includes(v);
const optionalString=(v:unknown,max:number):boolean=>v===null||(typeof v==='string'&&v.length>0&&v.length<=max);
const optionalNumber=(v:unknown):boolean=>v===null&&v!==undefined||count(v);
export const EMPTY_SMART_RULE:SmartRule={
  name_contains:null,extension:null,category:null,min_size:null,max_size:null,
  updated_after:null,updated_within_days:null,folder_id:null,
};
export function parseSmartRule(value:unknown):SmartRule{
  const r=record(value);
  if(!r||!optionalString(r.name_contains,100)||!optionalString(r.extension,16)
    ||!(r.category===null||category(r.category))
    ||!optionalNumber(r.min_size)||!optionalNumber(r.max_size)
    ||!optionalNumber(r.updated_after)||!optionalNumber(r.updated_within_days)||!optionalString(r.folder_id,256)
    ||(r.extension!==null&&!/^[a-z0-9-]{1,16}$/.test(String(r.extension)))
    ||(r.min_size!==null&&r.max_size!==null&&Number(r.min_size)>Number(r.max_size))
    ||(r.updated_after!==null&&r.updated_after!==undefined&&Number(r.updated_after)<=0)
    ||(r.updated_within_days!==null&&r.updated_within_days!==undefined&&
       (Number(r.updated_within_days)<1||Number(r.updated_within_days)>3650))
    ||(r.updated_within_days!==null&&r.updated_after!==null)){
    throw Error('Invalid native Smart View rule.');
  }
  const fields=['name_contains','extension','category','min_size','max_size','updated_after','updated_within_days','folder_id'] as const;
  if(!fields.some(k=>r[k]!==null&&r[k]!==undefined)) throw Error('Smart View has no search criteria.');
  return {
    name_contains:r.name_contains as string|null,
    extension:r.extension as string|null,
    category:r.category as SmartCategory|null,
    min_size:r.min_size as number|null,max_size:r.max_size as number|null,
    updated_after:r.updated_after as number|null,
    updated_within_days:r.updated_within_days as number|null,
    folder_id:r.folder_id as string|null,
  };
}
export function parseSmartViews(value:unknown):SmartView[]{
  if(!Array.isArray(value)||value.length>50)throw Error('Invalid native Smart Views list.');
  const ids=new Set<string>(),names=new Set<string>();
  return value.map(item=>{
    const r=record(item);
    if(!r||!validId(r.id)||!count(r.revision)||r.revision<1
      ||typeof r.name!=='string'||r.name.length<1||r.name.length>100
      ||ids.has(r.id)||names.has(r.name.toLocaleLowerCase()))throw Error('Invalid saved Smart View.');
    ids.add(r.id);names.add(r.name.toLocaleLowerCase());
    return{id:r.id,name:r.name,revision:r.revision,rule:parseSmartRule(r.rule)};
  });
}
export function parseSmartPreview(value:unknown):SmartPreview{
  const r=record(value);
  if(!r||!validId(r.view_id)||!count(r.revision)||r.revision<1
    ||!count(r.snapshot_revision)||!count(r.total_matches)||r.total_matches>25000
    ||typeof r.truncated!=='boolean'||r.local_only!==true
    ||!Array.isArray(r.matches)||r.matches.length>200
    ||r.matches.length>r.total_matches||(r.total_matches>r.matches.length&&!r.truncated)){
    throw Error('Invalid live Smart View preview.');
  }
  const seen=new Set<string>();
  const matches:SmartMatch[]=r.matches.map(v=>{
    const f=record(v);
    if(!f||typeof f.entry_id!=='string'||f.entry_id.length<1||f.entry_id.length>256
      ||seen.has(f.entry_id)||typeof f.name!=='string'||f.name.length<1||f.name.length>255
      ||typeof f.path!=='string'||!f.path.startsWith('/')||f.path.length>26000
      ||!count(f.size)||typeof f.updated_at!=='number'||!Number.isSafeInteger(f.updated_at)
      ||!category(f.category))throw Error('Invalid Smart View file match.');
    seen.add(f.entry_id);
    return f as unknown as SmartMatch;
  });
  return{view_id:r.view_id,revision:r.revision,snapshot_revision:r.snapshot_revision,
    total_matches:r.total_matches,truncated:r.truncated,matches,local_only:true};
}
