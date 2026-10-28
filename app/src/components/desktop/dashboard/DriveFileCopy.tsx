import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Copy,RefreshCw,Loader2} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
 instantCopyInventory,buildInstantCopyRequest,readInstantCopyPreview,readInstantCopyApplied,
 type InstantCopyInventory,type InstantCopyPreview,
} from '../../../driveFileCopy';

const ctl='rounded-lg border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';

export function DriveFileCopy({active}:{active:boolean}){
 const [expanded,setExpanded]=useState(false);
 const [inventory,setInventory]=useState<InstantCopyInventory|null>(null);
 const [selected,setSelected]=useState<string[]>([]);
 const [destination,setDestination]=useState<string|null>(null);
 const [prefix,setPrefix]=useState('Copy of '),[suffix,setSuffix]=useState('');
 const [filter,setFilter]=useState('');
 const [checked,setChecked]=useState<{key:string;preview:InstantCopyPreview}|null>(null);
 const [busy,setBusy]=useState<'load'|'check'|'apply'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const gate=useRef(false),generation=useRef(0);
 useEffect(()=>{
   if(active)return;
   setExpanded(false);setChecked(null);
   if(busy!=='apply'){generation.current++;setInventory(null);setSelected([]);setBusy(null);}
 },[active,busy]);
 const selectedPlan=useMemo(()=>{
   if(!inventory||!selected.length)return {request:null,error:''};
   try{return {request:buildInstantCopyRequest(inventory,selected,destination,prefix,suffix),error:''};}
   catch(e){return {request:null,error:String(e)};}
 },[inventory,selected,destination,prefix,suffix]);
 const request=selectedPlan.request;
 const key=request?JSON.stringify(request):'';
 const canApply=!!request&&checked?.key===key&&checked.preview.ready;
 const change=()=>{setChecked(null);setNotice('');setError('');};
 const load=async()=>{
   if(gate.current||!active)return;gate.current=true;
   const epoch=++generation.current;setBusy('load');setError('');setChecked(null);setNotice('');
   try{
     const data=instantCopyInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
     if(generation.current===epoch){setInventory(data);setSelected([]);setDestination(null);}
   }catch(e){if(generation.current===epoch)setError('Cannot load completed Drive files: '+String(e));}
   finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
 };
 const preview=async()=>{
   if(gate.current||!active)return;
   if(!request){setError(selectedPlan.error||'Select completed Drive files and a valid destination.');return;}
   gate.current=true;const epoch=++generation.current;
   setBusy('check');setChecked(null);setError('');setNotice('');
   try{
     const value=readInstantCopyPreview(await invoke<unknown>(
       'cmd_drive_file_copy_preview',{request}),request);
     if(generation.current===epoch)setChecked({key,preview:value});
   }catch(e){if(generation.current===epoch)setError('Native copy preview failed: '+String(e));}
   finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
 };
 const apply=async()=>{
   if(gate.current||!active||!request||!canApply)return;
   gate.current=true;const epoch=++generation.current;
   setBusy('apply');setChecked(null);setError('');setNotice('');
   try{
     let count:number;
     try{
       const done=readInstantCopyApplied(await invoke<unknown>(
         'cmd_drive_file_copy_apply',{request}),request);
       count=done.copied;
     }catch(e){
       if(generation.current===epoch)setError('Copy outcome uncertain: '+String(e)
         +'. Refresh files to inspect before trying again. No automatic retries.');
       return;
     }
     if(generation.current!==epoch)return;
     setSelected([]);setInventory(null);
     setNotice(count+' independent Drive copies created locally. Checking cloud metadata sync…');
     let sync='Cloud synchronization not independently confirmed; check again when online.';
     try{
       const result=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
       if(!result.dirty&&result.source!=='qa-local')sync='Drive metadata sync returned a clean result.';
     }catch{/* Committed local copies are not undone by a metadata sync outage. */}
     if(generation.current===epoch)setNotice(count+' independent Drive copies created locally. '+sync);
   }finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
 };
 const found=(inventory?.files??[]).filter(f=>
   !filter||f.path.toLocaleLowerCase().includes(filter.trim().toLocaleLowerCase()));
 const visible=found.slice(0,80);
 const toggle=(id:string,on:boolean)=>{
   setSelected(previous=>on
     ?previous.includes(id)?previous:previous.length>=100?previous:[...previous,id]
     :previous.filter(value=>value!==id));
   change();
 };
 return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive instant file copies">
   <div className="flex flex-wrap items-center justify-between gap-2">
     <div className="flex items-start gap-2">
       <Copy className="w-4 h-4 mt-0.5 text-telegram-subtext shrink-0" aria-hidden="true"/>
       <div><h3 className="text-sm font-medium text-telegram-text">Drive Instant File Copies</h3>
         <p className="text-xs text-telegram-subtext">
           Duplicate completed Drive files into another folder without downloading or re-uploading the contents.
         </p></div>
     </div>
     <button className={ctl} type="button" aria-expanded={expanded}
       onClick={()=>setExpanded(v=>!v)}>{expanded?'Close copier':'Open copier'}</button>
   </div>
   {expanded&&<div className="space-y-3">
     <button className={ctl} type="button" onClick={()=>void load()} disabled={!!busy||!active}>
       <RefreshCw className="w-3.5 h-3.5 inline mr-1" aria-hidden="true"/>Refresh completed files
     </button>
     {inventory&&<>
       <p className="text-xs text-telegram-subtext">
         {inventory.files.length} completed file(s) · {selected.length} selected (max 100).
         These are independent file entries referencing existing content, not new remote uploads.
         Later edits may require copy-on-write and a new upload.
       </p>
       <label className="block text-xs text-telegram-subtext space-y-1">
         Find completed file paths
         <input className={field+' w-full'} value={filter} disabled={!!busy}
           onChange={e=>setFilter(e.target.value)} placeholder="Filter by file or folder"/>
       </label>
       <div role="group" aria-label="Select completed files to copy"
         className="max-h-44 overflow-auto space-y-1 rounded-md border border-telegram-border p-2">
         {visible.map(file=><label key={file.id} className="flex items-center gap-2 text-xs text-telegram-text">
           <input type="checkbox" checked={selected.includes(file.id)}
             disabled={!!busy||(!selected.includes(file.id)&&selected.length>=100)}
             onChange={e=>toggle(file.id,e.target.checked)}/>
           <span title={file.path} className="min-w-0 truncate">{file.path}</span>
         </label>)}
         {!found.length&&<p className="text-xs text-telegram-subtext">No matching files.</p>}
         {found.length>visible.length&&<p className="text-xs text-telegram-subtext">
           Showing first 80 matches; narrow your search.
         </p>}
       </div>
       <label className="block text-xs text-telegram-subtext space-y-1">Copy all selected files into
         <select className={field+' w-full'} value={destination??'__root__'}
           disabled={!!busy} onChange={e=>{setDestination(e.target.value==='__root__'?null:e.target.value);change();}}>
           <option value="__root__">Drive root</option>
           {inventory.folders.map(folder=>
             <option key={folder.id} value={folder.id}>{folder.path}</option>)}
         </select>
       </label>
       <div className="grid grid-cols-2 gap-2">
         <label className="text-xs text-telegram-subtext space-y-1">Filename prefix
           <input className={field+' w-full'} disabled={!!busy} value={prefix}
             onChange={e=>{setPrefix(e.target.value);change();}}/>
         </label>
         <label className="text-xs text-telegram-subtext space-y-1">Filename suffix
           <input className={field+' w-full'} disabled={!!busy} value={suffix}
             onChange={e=>{setSuffix(e.target.value);change();}}/>
         </label>
       </div>
       {selectedPlan.error&&<p role="alert" className="text-xs text-telegram-text">{selectedPlan.error}</p>}
       <div className="flex flex-wrap gap-2">
         <button className={ctl} type="button" disabled={!!busy||!request}
           onClick={()=>void preview()}>Preview {request?.files.length??0} instant copies</button>
         <button className={ctl} type="button" disabled={!!busy||!canApply}
           onClick={()=>void apply()}>Create verified copies</button>
       </div>
       {checked&&<div className="space-y-1 rounded-md border border-telegram-border p-2 text-xs text-telegram-text" role="status">
         <p>{checked.preview.ready?checked.preview.count+' independent copies are ready; no file contents will transfer.'
           :'Cannot copy: '+checked.preview.problems.length+' native validation problem(s).'}</p>
         {checked.preview.problems.map((p,i)=><p key={i}>{p}</p>)}
         {checked.preview.ready&&request&&<div className="max-h-36 overflow-auto space-y-1">
           {request.files.map(f=><p key={f.entry_id} className="break-all">
             {inventory.files.find(x=>x.id===f.entry_id)?.path} → {destination
               ?inventory.folders.find(x=>x.id===destination)?.path+'/'+f.new_name
               :'/'+f.new_name}
           </p>)}
         </div>}
       </div>}
     </>}
     {busy&&<p className="text-xs text-telegram-subtext" role="status">
       <Loader2 className="w-3.5 h-3.5 inline mr-1 animate-spin" aria-hidden="true"/>
       {busy==='apply'?'Creating independent copy entries…':busy==='check'
         ?'Checking all source and destination entries…':'Reading Drive metadata…'}</p>}
     {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
     {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
   </div>}
 </section>;
}
