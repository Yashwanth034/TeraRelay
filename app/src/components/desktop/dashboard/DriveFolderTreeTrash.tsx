import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {FolderX,RefreshCw,RotateCcw} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
  readTreeTrashInventory,readTreeTrashRuns,trashTreeRequest,restoreTreeRequest,
  readTreePreview,readTreeApplied,type TreeTrashInventory,type TreeRuns,
  type TreePreview,
} from '../../../driveFolderTreeTrash';

const button='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
const size=(n:number)=>n<1024?n+' B':n<1048576?(n/1024).toFixed(1)+' KiB':(n/1048576).toFixed(1)+' MiB';
type Mode='trash'|'restore';

export function DriveFolderTreeTrash({active}:{active:boolean}) {
 const [expanded,setExpanded]=useState(false),[mode,setMode]=useState<Mode>('trash');
 const [inventory,setInventory]=useState<TreeTrashInventory|null>(null);
 const [runs,setRuns]=useState<TreeRuns|null>(null);
 const [folder,setFolder]=useState('');
 const [checked,setChecked]=useState<{key:string;data:TreePreview}|null>(null);
 const [busy,setBusy]=useState<'load'|'preview'|'apply'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const inflight=useRef(false),epoch=useRef(0);
 useEffect(()=>{
  if(active)return;
  setExpanded(false);setChecked(null);
  if(busy!=='apply'){epoch.current++;setInventory(null);setRuns(null);setBusy(null);}
 },[active,busy]);
 const plan=useMemo(()=>{
  if(!folder)return {request:null,error:''};
  try {
   const request=mode==='trash'
     ?inventory?trashTreeRequest(inventory,folder):null
     :runs?restoreTreeRequest(runs,folder):null;
   return {request,error:''};
  }catch(e){return {request:null,error:String(e)};}
 },[mode,folder,inventory,runs]);
 const request=plan.request;
 const key=request?mode+JSON.stringify(request):'';
 const ready=!!request&&checked?.key===key&&checked.data.ready;
 const reset=()=>{setChecked(null);setError('');setNotice('');};
 const changeMode=(value:Mode)=>{setMode(value);setFolder('');reset();};
 const load=async()=>{
  if(!active||inflight.current)return;
  inflight.current=true;const turn=++epoch.current;
  setBusy('load');reset();
  try {
   const [snapshot,history]=await Promise.all([
    invoke<unknown>('cmd_drive_get_snapshot'),
    invoke<unknown>('cmd_drive_folder_tree_trash_runs'),
   ]);
   const nextInventory=readTreeTrashInventory(snapshot);
   const nextRuns=readTreeTrashRuns(history);
   if(epoch.current===turn){
    setInventory(nextInventory);setRuns(nextRuns);setFolder('');
   }
  }catch(e){if(epoch.current===turn)setError('Cannot inspect Drive folder trees: '+String(e));}
  finally{inflight.current=false;if(epoch.current===turn)setBusy(null);}
 };
 const preview=async()=>{
  if(!active||inflight.current)return;
  if(!request){setError(plan.error||'Select a folder and refresh Drive first.');return;}
  inflight.current=true;const turn=++epoch.current;
  setBusy('preview');setError('');setNotice('');setChecked(null);
  try{
   const command=mode==='trash'?'cmd_drive_folder_tree_trash_preview':
      'cmd_drive_folder_tree_restore_preview';
   const data=readTreePreview(await invoke<unknown>(command,{request}),request);
   if(epoch.current===turn)setChecked({key,data});
  }catch(e){if(epoch.current===turn)setError('Native folder preview failed: '+String(e));}
  finally{inflight.current=false;if(epoch.current===turn)setBusy(null);}
 };
 const apply=async()=>{
  if(!active||inflight.current||!ready||!request||!checked)return;
  inflight.current=true;const turn=++epoch.current;
  const proposal=checked.data,operation=mode;
  setBusy('apply');setChecked(null);setError('');setNotice('');
  try{
   let result;
   try{
    result=readTreeApplied(await invoke<unknown>(
      operation==='trash'?'cmd_drive_folder_tree_trash_apply':
        'cmd_drive_folder_tree_restore_apply',
      {request}),request,proposal);
   }catch(e){
    if(epoch.current===turn)setError(
      'Outcome uncertain: '+String(e)+
      '. Inspect Drive and Recently Deleted before retrying; never auto-retry.');
    return;
   }
   if(epoch.current!==turn)return;
   setFolder('');setInventory(null);setRuns(null);
   setNotice((operation==='trash'?'Trashed':'Restored')+' locally: '+
     result.folders+' folders and '+result.files+
     ' completed files. Checking cloud metadata synchronization…');
   let sync='Cloud metadata sync not independently confirmed.';
   try{
    const status=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
    if(!status.dirty&&status.source!=='qa-local')
      sync='Drive metadata sync returned a clean result.';
   }catch{/* Local atomic result is not undone by a separate sync failure. */}
   if(epoch.current===turn)setNotice(
    (operation==='trash'?'Moved to Recently Deleted':'Restored original folder tree')+
    ': '+result.folders+' folders and '+result.files+' files. '+sync+
    ' Refresh to inspect the latest Drive state.');
  }finally{inflight.current=false;if(epoch.current===turn)setBusy(null);}
 };
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Recursive Drive folder Trash and recovery">
  <div className="flex items-start justify-between gap-3">
   <div className="flex min-w-0 items-start gap-2">
    <FolderX aria-hidden="true" className="mt-0.5 h-4 w-4 shrink-0 text-telegram-subtext"/>
    <div>
     <h3 className="text-sm font-medium text-telegram-text">Drive Folder Trash &amp; Complete Recovery</h3>
     <p className="text-xs text-telegram-subtext">
      Safely remove or recover a whole folder tree, including nested files, in one transaction.
     </p>
    </div>
   </div>
   <button type="button" className={button} disabled={busy==='apply'}
    aria-expanded={expanded} onClick={()=>setExpanded(v=>!v)}>
    {expanded?'Close':'Open'}
   </button>
  </div>
  {expanded&&<div className="space-y-3">
   <div className="flex flex-wrap gap-2">
    <button type="button" className={button} disabled={!!busy} aria-pressed={mode==='trash'}
     onClick={()=>changeMode('trash')}>Trash entire folder</button>
    <button type="button" className={button} disabled={!!busy} aria-pressed={mode==='restore'}
     onClick={()=>changeMode('restore')}>
     <RotateCcw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Restore whole folder
    </button>
    <button type="button" className={button} disabled={!!busy||!active} onClick={()=>void load()}>
     <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Refresh Drive
    </button>
   </div>
   {mode==='trash'&&inventory&&<>
    <label className="block space-y-1 text-xs text-telegram-subtext">
     Folder tree to move to Recently Deleted
     <select className={field} disabled={!!busy} value={folder}
      onChange={e=>{setFolder(e.target.value);reset();}}>
      <option value="">Choose folder</option>
      {inventory.folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <p className="text-xs text-telegram-subtext">
     Nested completed files and folders are included. Pending uploads, hidden edits,
     symbolic links, unavailable backing and unsafe collisions block the entire operation.
     Remote Telegram content is not deleted. Limit: 512 folders and 2,000 files.
    </p>
   </>}
   {mode==='restore'&&runs&&<>
    <label className="block space-y-1 text-xs text-telegram-subtext">
     Recoverable complete folder trees
     <select className={field} disabled={!!busy} value={folder}
      onChange={e=>{setFolder(e.target.value);reset();}}>
      <option value="">Choose previously trashed tree</option>
      {runs.runs.map(r=><option key={r.root_id} value={r.root_id}>
       {r.name} — {r.folder_count} folders, {r.file_count} files
      </option>)}
     </select>
    </label>
    {runs.runs.length===0&&<p className="text-xs text-telegram-subtext">
     No complete-tree recovery journal found. Single-item Recently Deleted remains available.
    </p>}
    <p className="text-xs text-telegram-subtext">
     Restore is all-or-nothing at the exact original locations and names.
     No conflict is silently renamed or overwritten. Only items from the original
     completed folder Trash operation are recovered. The recovery journal is device-local;
     it is not a cross-device backup of folder contents.
    </p>
   </>}
   {plan.error&&<p className="text-xs text-red-400" role="alert">{plan.error}</p>}
   <div className="flex flex-wrap gap-2">
    <button type="button" className={button} disabled={!!busy||!request}
     onClick={()=>void preview()}>Preview entire {mode==='trash'?'Trash':'recovery'}</button>
    <button type="button" className={button} disabled={!!busy||!ready}
     onClick={()=>void apply()}>
     {mode==='trash'?'Move full tree to Recently Deleted':'Restore full tree atomically'}
    </button>
   </div>
   {checked&&<div className="space-y-1 rounded-md border border-telegram-border p-2 text-xs text-telegram-text" role="status">
    <p>{checked.data.ready
      ?(mode==='trash'?'Ready to Trash ':'Ready to restore ')+checked.data.folders+
       ' folders and '+checked.data.files+' files ('+size(checked.data.logical_bytes)+').'
      :'Blocked: '+checked.data.problems.length+' unsafe condition(s).'}</p>
    {checked.data.problems.map((p,i)=><p key={i} className="break-words">{p}</p>)}
    {checked.data.ready&&<div className="max-h-32 overflow-y-auto space-y-1">
     {checked.data.examples.map((p,i)=><p key={i} className="break-all">{p}</p>)}
     {checked.data.folders+checked.data.files>checked.data.examples.length
       &&<p>Showing up to 25 affected names.</p>}
    </div>}
   </div>}
   {busy&&<p className="text-xs text-telegram-subtext" role="status">
    {busy==='load'?'Reading folders and recovery journals…':
      busy==='preview'?'Validating complete nested folder tree…':
       'Committing whole-tree changes atomically…'}
   </p>}
   {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
   {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
  </div>}
 </section>;
}
