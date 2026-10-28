import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Folders,RefreshCw,ShieldCheck} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
 readMirrorInventory,buildMirrorRequest,readMirrorPreview,readMirrorApplied,
 type MirrorInventory,type MirrorPreview,
} from '../../../driveFolderMirror';

const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
const formatBytes=(n:number)=>n>1048576?(n/1048576).toFixed(1)+' MiB':(n/1024).toFixed(1)+' KiB';

export function DriveFolderMirror({active}:{active:boolean}) {
 const [expanded,setExpanded]=useState(false);
 const [inventory,setInventory]=useState<MirrorInventory|null>(null);
 const [source,setSource]=useState(''),[target,setTarget]=useState('');
 const [checked,setChecked]=useState<{key:string;data:MirrorPreview}|null>(null);
 const [busy,setBusy]=useState<'load'|'preview'|'apply'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const inflight=useRef(false),generation=useRef(0);
 useEffect(()=>{
  if(active)return;
  setExpanded(false);setChecked(null);
  if(busy!=='apply') {
   generation.current++;setInventory(null);setBusy(null);
  }
 },[active,busy]);
 const proposed=useMemo(()=>{
  if(!inventory||!source||!target)return {request:null,error:''};
  try{return {request:buildMirrorRequest(inventory,source,target),error:''};}
  catch(e){return {request:null,error:String(e)};}
 },[inventory,source,target]);
 const request=proposed.request,key=request?JSON.stringify(request):'';
 const ready=!!request&&checked?.key===key&&checked.data.ready
   &&checked.data.add_folders+checked.data.add_files>0;
 const reset=()=>{setChecked(null);setError('');setNotice('');};
 const refresh=async()=>{
  if(!active||inflight.current)return;
  inflight.current=true;const epoch=++generation.current;
  setBusy('load');setError('');setNotice('');setChecked(null);
  try{
   const fresh=readMirrorInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
   if(epoch===generation.current) {
    setInventory(fresh);setSource('');setTarget('');
   }
  }catch(e){if(epoch===generation.current)setError('Could not read Drive folders: '+String(e));}
  finally{inflight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 const preview=async()=>{
  if(!active||inflight.current)return;
  if(!request){setError(proposed.error||'Select an original folder and separate backup folder.');return;}
  inflight.current=true;const epoch=++generation.current;
  setBusy('preview');setError('');setNotice('');setChecked(null);
  try{
   const data=readMirrorPreview(await invoke<unknown>(
    'cmd_drive_folder_mirror_preview',{request}),request);
   if(epoch===generation.current)setChecked({key,data});
  }catch(e){if(epoch===generation.current)setError('Drive mirror preview failed: '+String(e));}
  finally{inflight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 const apply=async()=>{
  if(!active||inflight.current||!request||!ready||!checked)return;
  inflight.current=true;const epoch=++generation.current;
  const plan=checked.data;
  setBusy('apply');setError('');setNotice('');setChecked(null);
  try{
   let done;
   try{
    done=readMirrorApplied(await invoke<unknown>(
      'cmd_drive_folder_mirror_apply',{request}),request,plan);
   }catch(e){
    if(epoch===generation.current)setError(
     'Mirror outcome uncertain: '+String(e)+
     '. Refresh Drive and inspect the backup folder before trying again. No automatic retries.');
    return;
   }
   if(epoch!==generation.current)return;
   setInventory(null);setSource('');setTarget('');
   setNotice(done.folders_added+' folders and '+done.files_added+
    ' completed files added locally. Checking cloud metadata sync…');
   let sync='Cloud metadata synchronization has not been independently confirmed.';
   try{
    const response=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
    if(!response.dirty&&response.source!=='qa-local')
      sync='Drive metadata sync returned a clean result.';
   }catch{/* Sync failure does not undo a successful local transaction. */}
   if(epoch===generation.current)setNotice(
    'Mirror updated: '+done.folders_added+' new folders, '+done.files_added+
    ' new files and '+done.files_matched+' existing matching files left untouched. '+sync);
  }finally{inflight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Incremental Drive folder mirror">
  <div className="flex items-start justify-between gap-3">
   <div className="flex min-w-0 items-start gap-2">
    <Folders className="w-4 h-4 mt-0.5 text-telegram-subtext shrink-0" aria-hidden="true"/>
    <div>
     <h3 className="text-sm font-medium text-telegram-text">Drive Incremental Folder Mirror</h3>
     <p className="text-xs text-telegram-subtext">
      Keep an existing backup folder updated with only missing completed files and subfolders, without re-uploading.
     </p>
    </div>
   </div>
   <button className={ctl} type="button" disabled={busy==='apply'} aria-expanded={expanded}
    onClick={()=>setExpanded(p=>!p)}>{expanded?'Close mirror':'Open mirror'}</button>
  </div>
  {expanded&&<div className="space-y-3">
   <button className={ctl} type="button" disabled={!!busy||!active} onClick={()=>void refresh()}>
    <RefreshCw className="inline h-3.5 w-3.5 mr-1" aria-hidden="true"/>Refresh Drive folders
   </button>
   {inventory&&<>
    <label className="block text-xs text-telegram-subtext space-y-1">
     Original folder (read-only source)
     <select className={field} value={source} disabled={!!busy}
      onChange={e=>{setSource(e.target.value);reset();}}>
      <option value="">Choose source folder</option>
      {inventory.folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <label className="block text-xs text-telegram-subtext space-y-1">
     Existing backup folder (only missing entries added)
     <select className={field} value={target} disabled={!!busy}
      onChange={e=>{setTarget(e.target.value);reset();}}>
      <option value="">Choose separate backup folder</option>
      {inventory.folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <p className="text-xs text-telegram-subtext">
     No file is overwritten, renamed or removed. Matching completed content is skipped.
     Conflicting contents, unfinished uploads, symlinks, unsafe names and nested source/backup folders
     stop the entire mirror. New entries reference existing completed content without transferring bytes.
     Limit: 512 folders and 2,000 files per tree.
    </p>
    {proposed.error&&<p className="text-xs text-red-400" role="alert">{proposed.error}</p>}
    <div className="flex flex-wrap gap-2">
     <button type="button" className={ctl} disabled={!!busy||!request}
      onClick={()=>void preview()}>Preview incremental mirror</button>
     <button type="button" className={ctl} disabled={!!busy||!ready}
      onClick={()=>void apply()}>
      <ShieldCheck className="inline h-3.5 w-3.5 mr-1" aria-hidden="true"/>
      Add missing entries atomically
     </button>
    </div>
    {checked&&<div role="status" className="space-y-1 rounded-md border border-telegram-border p-2 text-xs text-telegram-text">
      <p>{!checked.data.ready
       ?'Mirror blocked: '+checked.data.problems.length+' conflict(s) or unsafe condition(s).'
       :checked.data.add_folders+checked.data.add_files===0
        ?'Backup is already up to date. No changes needed.'
        :'Will add '+checked.data.add_folders+' folders and '+checked.data.add_files+
           ' completed files ('+formatBytes(checked.data.added_logical_bytes)+
           '), leaving '+checked.data.matching_files+' matching files untouched.'}</p>
      {checked.data.problems.map((p,i)=><p key={i}>{p}</p>)}
      {checked.data.ready&&checked.data.examples.length>0&&<div className="max-h-36 overflow-y-auto space-y-1">
       {checked.data.examples.map((path,i)=><p key={i} className="break-all">{path}</p>)}
       {checked.data.add_folders+checked.data.add_files>checked.data.examples.length
        &&<p>Showing first 25 planned paths.</p>}
      </div>}
    </div>}
   </>}
   {busy&&<p className="text-xs text-telegram-subtext" role="status">
    {busy==='load'?'Reading Drive folder inventory…':
     busy==='preview'?'Checking both complete folder trees…':
      'Atomically creating missing Drive entries…'}
   </p>}
   {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
   {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
  </div>}
 </section>;
}
