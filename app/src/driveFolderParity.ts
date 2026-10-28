import {parseFolderInventory} from './driveFolderOperations';
/** Complete, read-only structural/content-metadata parity for two Drive trees. */
export interface ParityFolder { id:string;path:string;name:string; }
export interface ParityDifference {
 kind:'only_left'|'only_right'|'metadata_difference'|'content_unverified';
 relative_path:string;left:string|null;right:string|null;
}
export interface ParityReport {
 revision:number;dirty:boolean;left_id:string;right_id:string;
 left_folders:number;right_folders:number;left_files:number;right_files:number;
 matching_file_metadata:number;unverified_file_content:number;
 only_left:number;only_right:number;different:number;
 truncated_differences:boolean;differences:ParityDifference[];read_only:true;
}
const obj=(x:unknown):Record<string,unknown>|null=>
 x!==null&&typeof x==='object'&&!Array.isArray(x)?x as Record<string,unknown>:null;
const id=(v:unknown):v is string=>typeof v==='string'&&v.length>0&&v.length<=256
 &&v!=='.'&&v!=='..'&&!/[/\\\u0000-\u001f\u007f]/.test(v);
const natural=(v:unknown,max=100_000):v is number=>
 typeof v==='number'&&Number.isSafeInteger(v)&&v>=0&&v<=max;
export function parityFolders(snapshot:unknown):ParityFolder[] {
 const result=parseFolderInventory(snapshot);
 if(result.skipped)throw Error('Drive folder inventory is incomplete. Refresh metadata.');
 if(result.folders.length>50_000)throw Error('Drive folder inventory too large.');
 return result.folders.map(d=>{
  if(!id(d.id)||typeof d.path!=='string'||!d.path.startsWith('/')
    ||d.path.length>4096)throw Error('Invalid current Drive folder.');
  return {id:d.id,path:d.path,name:d.name};
 });
}
export function readFolderParity(raw:unknown,left:string,right:string):ParityReport {
 const r=obj(raw);if(!r||r.read_only!==true||!id(left)||!id(right)||left===right
   ||r.left_id!==left||r.right_id!==right||!natural(r.revision,Number.MAX_SAFE_INTEGER)
   ||typeof r.dirty!=='boolean'
   ||!['left_folders','right_folders','left_files','right_files','matching_file_metadata',
        'unverified_file_content','only_left','only_right','different'].every(x=>natural(r[x],5_000))
   ||r.truncated_differences!==true&&r.truncated_differences!==false
   ||!Array.isArray(r.differences)||r.differences.length>200
   ||r.differences.some(x=>{
       const d=obj(x);
       return !d||!['only_left','only_right','metadata_difference','content_unverified'].includes(String(d.kind))
         ||typeof d.relative_path!=='string'||d.relative_path.length===0||d.relative_path.length>4096
         ||d.relative_path.startsWith('/')||d.relative_path.split('/').some(part=>
             !part||part==='.'||part==='..'||/[<>:"\\|?*\u0000-\u001f\u007f]/.test(part))
         ||(d.left!==null&&!['file','folder','link'].includes(String(d.left)))
         ||(d.right!==null&&!['file','folder','link'].includes(String(d.right)));
   })
   ||!r.truncated_differences&&r.differences.length!==(
       (r.only_left as number)+(r.only_right as number)+(r.different as number)+(r.unverified_file_content as number))
   ||r.truncated_differences&&r.differences.length!==200
   )throw Error('Drive parity response invalid, stale or truncated unexpectedly.');
 return r as unknown as ParityReport;
}
export function parityVerdict(r:ParityReport):'differences'|'unverified'|'metadata_equal' {
 if(r.only_left||r.only_right||r.different||r.truncated_differences)return 'differences';
 if(r.unverified_file_content)return 'unverified';
 return 'metadata_equal';
}
