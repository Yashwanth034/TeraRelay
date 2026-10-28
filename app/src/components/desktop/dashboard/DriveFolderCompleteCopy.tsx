import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Folders,RefreshCw,Loader2} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
  readCompleteCopyInventory,buildCompleteCopyRequest,
  decodeCompleteCopyPreview,decodeCompleteCopyApplied,
  type CompleteCopyInventory,type CompleteCopyPreview,
} from '../../../driveFolderCompleteCopy';
const ctl='rounded-lg border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
export function DriveFolderCompleteCopy({active}:{active:boolean}){
 const [opened,setOpened]=useState(false);
 const [inventory,setInventory]=useState<CompleteCopyInventory|null>(null);
 const [source,setSource]=useState(''),[destination,setDestination]=useState<string|null>(null);
 const [name,setName]=useState(''),[filter,setFilter]=useState('');
 const [preview,setPreview]=useState<{key:string;data:CompleteCopyPreview}|null>(null);
 const [busy,setBusy]=useState<'load'|'preview'|'apply'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const inFlight=useRef(false),generation=useRef(0);
 useEffect(()=>{
   if(active)return;
   setOpened(false);setPreview(null);
   if(busy!=='apply'){generation.current++;setInventory(null);setSource('');setBusy(null);}
 },[active,busy]);
 const chosen=inventory?.folders.find(f=>f.id===source);
 const available=useMemo(()=>inventory?.folders.filter(f=>
   !filter||f.path.toLocaleLowerCase().includes(filter.trim().toLocaleLowerCase()))??[],
   [inventory,filter]);
 const planning=useMemo(()=>{
   if(!inventory||!source)return {request:null,error:''};
   try{return {request:buildCompleteCopyRequest(inventory,source,destination,name),error:''};}
   catch(e){return {request:null,error:String(e)};}
 },[inventory,source,destination,name]);
 const request=planning.request,key=request?JSON.stringify(request):'';
 const ready=!!request&&preview?.key===key&&preview.data.ready;
 const invalidate=()=>{setPreview(null);setNotice('');setError('');};
 const refresh=async()=>{
   if(!active||inFlight.current)return;
   inFlight.current=true;const epoch=++generation.current;
   setBusy('load');setError('');setPreview(null);setNotice('');
   try{
     const fresh=readCompleteCopyInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
     if(epoch===generation.current){setInventory(fresh);setSource('');setDestination(null);setName('');}
   }catch(e){if(epoch===generation.current)setError('Could not read Drive folders: '+String(e));}
   finally{inFlight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 const check=async()=>{
   if(!active||inFlight.current)return;
   if(!request){setError(planning.error||'Select a source folder and a safe name.');return;}
   inFlight.current=true;const epoch=++generation.current;
   setBusy('preview');setPreview(null);setError('');setNotice('');
   try{
     const result=decodeCompleteCopyPreview(await invoke<unknown>(
       'cmd_drive_complete_copy_preview',{request}),request);
     if(epoch===generation.current)setPreview({key,data:result});
   }catch(e){if(epoch===generation.current)setError('Native complete-copy preview failed: '+String(e));}
   finally{inFlight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 const apply=async()=>{
   if(!active||inFlight.current||!request||!ready||!preview)return;
   inFlight.current=true;const epoch=++generation.current,checked=preview.data;
   setBusy('apply');setPreview(null);setError('');setNotice('');
   try{
     let result;
     try{
       result=decodeCompleteCopyApplied(await invoke<unknown>(
         'cmd_drive_complete_copy_apply',{request}),request,checked);
     }catch(e){
       if(epoch===generation.current)setError(
         'Copy outcome uncertain: '+String(e)+
         '. Refresh Drive and inspect destination before retrying. No automatic retries.');
       return;
     }
     if(epoch!==generation.current)return;
     setInventory(null);setSource('');
     setNotice(result.folders+' folders and '+result.files+' files copied locally. Checking cloud metadata sync…');
     let syncMessage='Cloud metadata sync unverified; confirm again when online.';
     try{
       const sync=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
       if(!sync.dirty&&sync.source!=='qa-local')syncMessage='Drive metadata sync returned a clean result.';
     }catch{/* Local commit persists if remote sync fails. */}
     if(epoch===generation.current)setNotice(
       result.folders+' folders and '+result.files+' files copied without transferring file bytes. '+syncMessage);
   }finally{inFlight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive complete folder copy">
  <div className="flex flex-wrap items-start justify-between gap-3">
   <div className="flex items-start gap-2">
    <Folders className="w-4 h-4 mt-0.5 shrink-0 text-telegram-subtext" aria-hidden="true"/>
    <div><h3 className="text-sm font-medium text-telegram-text">Drive Complete Folder Copy</h3>
     <p className="text-xs text-telegram-subtext">
      Duplicate a folder, its nested hierarchy and completed files without downloading or re-uploading contents.
     </p></div>
   </div>
   <button className={ctl} type="button" aria-expanded={opened}
    onClick={()=>setOpened(x=>!x)}>{opened?'Close copier':'Open complete copier'}</button>
  </div>
  {opened&&<div className="space-y-3">
   <button className={ctl} type="button" disabled={!!busy||!active} onClick={()=>void refresh()}>
    <RefreshCw className="w-3.5 h-3.5 inline mr-1" aria-hidden="true"/>Refresh folder inventory
   </button>
   {inventory&&<>
    <label className="block text-xs text-telegram-subtext space-y-1">Search source folders
     <input className={field} value={filter} disabled={!!busy}
      onChange={e=>setFilter(e.target.value)} placeholder="Filter folder paths"/>
    </label>
    <label className="block text-xs text-telegram-subtext space-y-1">Original folder
     <select className={field} disabled={!!busy} value={source}
      onChange={e=>{const id=e.target.value;setSource(id);
        setName(id ? (inventory.folders.find(f=>f.id===id)?.name??'Folder')+' Copy' : '');invalidate();}}>
      <option value="">Choose source folder</option>
      {available.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <label className="block text-xs text-telegram-subtext space-y-1">Copy into
     <select className={field} value={destination??'__root__'} disabled={!!busy}
      onChange={e=>{setDestination(e.target.value==='__root__'?null:e.target.value);invalidate();}}>
      <option value="__root__">Drive root</option>
      {inventory.folders.map(f=><option value={f.id} key={f.id}>{f.path}</option>)}
     </select>
    </label>
    <label className="block text-xs text-telegram-subtext space-y-1">New top folder name
     <input className={field} value={name} disabled={!!busy}
      onChange={e=>{setName(e.target.value);invalidate();}}/>
    </label>
    <p className="text-xs text-telegram-subtext">
     Copies up to 512 folders and 2,000 completed files. Unfinished uploads, symbolic links, unsafe
     names and copying into the source subtree are blocked. New files have independent identities,
     but reference the same completed remote content until later modifications.
    </p>
    {planning.error&&<p className="text-xs text-telegram-text" role="alert">{planning.error}</p>}
    <div className="flex flex-wrap gap-2">
     <button className={ctl} type="button" disabled={!!busy||!request}
      onClick={()=>void check()}>Preview complete copy</button>
     <button className={ctl} type="button" disabled={!!busy||!ready}
      onClick={()=>void apply()}>Create verified full copy</button>
    </div>
    {preview&&<div role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text space-y-1">
     <p>{preview.data.ready
      ?preview.data.folders+' directories and '+preview.data.files+
       ' completed files ready (logical total '+preview.data.referenced_bytes+' bytes).'
      :'Cannot copy: '+preview.data.problems.length+' native validation problem(s).'}</p>
     {preview.data.problems.map((p,i)=><p key={i}>{p}</p>)}
     {preview.data.ready&&chosen&&<p>Source: {chosen.path} → New: {name}</p>}
    </div>}
   </>}
   {busy&&<p className="text-xs text-telegram-subtext" role="status">
    <Loader2 className="w-3.5 h-3.5 mr-1 inline animate-spin" aria-hidden="true"/>
    {busy==='apply'?'Atomically copying all folder and file references…':
     busy==='preview'?'Validating complete tree and contents…':'Reading Drive metadata…'}</p>}
   {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
   {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
  </div>}
 </section>;
}
