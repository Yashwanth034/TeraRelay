import {parseFolderInventory, type FolderInventory} from './driveFolderOperations';

export interface BlueprintInventory extends FolderInventory {}
export interface BlueprintRequest {
  revision:number;
  destination_parent_id:string|null;
  paths:string[];
}
export interface BlueprintPreview {
  revision:number;ready:boolean;count:number;paths:string[];
  truncated:boolean;problems:string[];
}
export interface BlueprintApplied {created:number;revision:number;dirty:boolean;}
const asObject=(value:unknown):Record<string,unknown>|null=>
  value!==null&&typeof value==='object'&&!Array.isArray(value)
    ?value as Record<string,unknown>:null;
const bytes=(text:string)=>new TextEncoder().encode(text).length;
function validName(name:string):boolean {
  return name.length>0&&bytes(name)<=255&&name.trim()===name
    &&name!=='.'&&name!=='..'&&!name.endsWith('.')
    &&!/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(name)
    &&!/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(name);
}
export const blueprintInventory=(raw:unknown):BlueprintInventory=>parseFolderInventory(raw);
export function buildBlueprint(
  inventory:BlueprintInventory,destinationParent:string|null,input:string,
):BlueprintRequest {
  if(typeof input!=='string'||bytes(input)>64*1024)throw Error('Blueprint exceeds 64 KiB.');
  if(destinationParent!==null&&!inventory.folders.some(folder=>folder.id===destinationParent))
    throw Error('Destination is unavailable. Refresh Drive folders.');
  const paths=input.replace(/^\uFEFF/,'').split('\n')
    .map(line=>line.endsWith('\r')?line.slice(0,-1):line)
    .filter(line=>line.length>0);
  if(!paths.length||paths.length>512)throw Error('Provide 1–512 folder paths.');
  const seen=new Set<string>();
  const exact=new Set<string>();
  for(const [index,path] of paths.entries()){
    if(path.startsWith('/')||path.endsWith('/')||bytes(path)>4096||path.includes('\\'))
      throw Error('Line '+(index+1)+': enter a relative Drive path, not an absolute path.');
    const parts=path.split('/');
    if(parts.length>64||parts.some(part=>!validName(part)))
      throw Error('Line '+(index+1)+': invalid portable directory name or depth.');
    const portable=parts.map(p=>p.normalize('NFC').toLocaleLowerCase()).join('/');
    if(seen.has(portable))throw Error('Line '+(index+1)+': duplicate folder path.');
    seen.add(portable);
    exact.add(path);
  }
  for(const path of paths){
    const slash=path.lastIndexOf('/');
    if(slash>=0&&!exact.has(path.slice(0,slash)))
      throw Error(path+': list parent folder '+path.slice(0,slash)+' on its own line.');
  }
  return {revision:inventory.revision,destination_parent_id:destinationParent,paths};
}
export function readBlueprintPreview(raw:unknown,request:BlueprintRequest):BlueprintPreview {
  const o=asObject(raw);
  if(!o||!Number.isSafeInteger(o.revision)||o.revision!==request.revision
    ||typeof o.ready!=='boolean'||!Number.isSafeInteger(o.count)
    ||o.count!==request.paths.length||typeof o.truncated!=='boolean'
    ||!Array.isArray(o.paths)||o.paths.length>80||o.paths.some(x=>typeof x!=='string')
    ||!Array.isArray(o.problems)||o.problems.length>100
    ||o.problems.some(x=>typeof x!=='string')){
    throw Error('Invalid or stale native folder blueprint preview.');
  }
  if(o.ready===true&&(o.problems as unknown[]).length!==0)
    throw Error('Native blueprint reports success and errors simultaneously.');
  return o as unknown as BlueprintPreview;
}
export function readBlueprintApplied(raw:unknown,request:BlueprintRequest):BlueprintApplied {
  const o=asObject(raw);
  if(!o||!Number.isSafeInteger(o.created)||o.created!==request.paths.length
    ||!Number.isSafeInteger(o.revision)||(o.revision as number)<=request.revision
    ||o.dirty!==true){
    throw Error('Blueprint apply outcome uncertain. Refresh Drive before retrying.');
  }
  return o as unknown as BlueprintApplied;
}
