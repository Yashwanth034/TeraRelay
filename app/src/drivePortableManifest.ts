import {readCapsuleComparison,type CapsuleComparison} from './driveTimeCapsule';

export interface PortableExportReceipt {
  destination_path:string;
  captured_at:number;
  item_count:number;
  revision:number;
  dirty:boolean;
  checksum:string;
}
export interface PortableComparison {
  captured_at:number;
  checksum:string;
  metadata_only:true;
  comparison:CapsuleComparison;
}
const object=(raw:unknown):Record<string,unknown>|null=>
  raw!==null&&typeof raw==='object'&&!Array.isArray(raw)
    ?raw as Record<string,unknown>:null;
const number=(value:unknown,max=Number.MAX_SAFE_INTEGER):value is number=>
  typeof value==='number'&&Number.isSafeInteger(value)&&value>=0&&value<=max;
const hash=(value:unknown):value is string=>
  typeof value==='string'&&/^[a-f0-9]{64}$/.test(value);
const safePath=(value:unknown):value is string=>
  typeof value==='string'&&value.length<=4096
    &&(value.startsWith('/')||/^[A-Za-z]:[\\/]/.test(value))&&value.endsWith('.json')
    &&!value.split(/[\\/]/).some(part=>part==='..'||part==='.')
    &&!/[\u0000-\u001f\u007f]/.test(value);
export function readPortableExport(raw:unknown,requested:string):PortableExportReceipt {
  const v=object(raw);
  if(!v||!safePath(v.destination_path)||v.destination_path!==requested
    ||!number(v.captured_at)||v.captured_at===0
    ||!number(v.item_count,5000)||!number(v.revision)||typeof v.dirty!=='boolean'
    ||!hash(v.checksum)) throw Error('Native manifest export confirmation is missing or invalid.');
  return v as unknown as PortableExportReceipt;
}
export function readPortableComparison(raw:unknown):PortableComparison {
  const v=object(raw);
  if(!v||!number(v.captured_at)||v.captured_at===0
    ||!hash(v.checksum)||v.metadata_only!==true)
    throw Error('Invalid portable manifest verification result.');
  const comparison=readCapsuleComparison(v.comparison,1,2);
  if(comparison.before.captured_at!==v.captured_at
     ||comparison.before.fingerprint!==v.checksum)
    throw Error('Portable manifest comparison does not match its signed inventory.');
  return {captured_at:v.captured_at,checksum:v.checksum,metadata_only:true,comparison};
}
export const portableChangeLabels:Record<string,string>={
 created:'New identity',removed:'No longer present in this inventory',
 moved:'Moved',renamed:'Renamed',ancestor_path_changed:'Ancestor path changed',
 availability_changed:'Backing availability changed',
 backing_state_changed:'Backing generation or state changed',
 content_metadata_changed:'Content metadata changed',
};
