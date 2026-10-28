// Strict typed validation for portable whole-folder recovery receipt IPC.
export interface TreeReceiptResult {
 root_id:string;path:string;folders:number;files:number;
 revision:number;checksum:string;imported:boolean;
}
const rec=(v:unknown):Record<string,unknown>|null=>
 v!==null&&typeof v==='object'&&!Array.isArray(v)?v as Record<string,unknown>:null;
const integer=(v:unknown,max:number):v is number=>
 typeof v==='number'&&Number.isSafeInteger(v)&&v>=0&&v<=max;
export function readTreeReceipt(raw:unknown,path:string,imported:boolean):TreeReceiptResult {
 const v=rec(raw);
 if(!v||typeof v.root_id!=='string'||v.root_id.length===0||v.root_id.length>256
  ||v.root_id.trim()!==v.root_id||/[/\\\u0000-\u001f\u007f]/.test(v.root_id)
  ||v.path!==path||typeof v.path!=='string'||!v.path.endsWith('.json')
  ||v.imported!==imported||!integer(v.folders,512)||v.folders===0
  ||!integer(v.files,2000)||!integer(v.revision,Number.MAX_SAFE_INTEGER)
  ||typeof v.checksum!=='string'||!/^[0-9a-f]{64}$/.test(v.checksum))
  throw Error('Native portable folder recovery receipt result is invalid or uncertain.');
 return v as unknown as TreeReceiptResult;
}
