import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {ShieldCheck,RefreshCw,FolderSync} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
 readBatchRepairPreview,readBatchRepairApplied,batchRepairRequest,
 type BatchRepairPreview,
} from '../../../driveBackupBatchRepair';

const btn='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const mb=(n:number)=>n<1024?n+' B':n<1048576?(n/1024).toFixed(1)+' KiB':(n/1048576).toFixed(1)+' MiB';
export function DriveBackupBatchRepair({active}:{active:boolean}){
 const [expanded,setExpanded]=useState(false);
 const [plan,setPlan]=useState<BatchRepairPreview|null>(null);
 const [busy,setBusy]=useState<'preview'|'apply'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const generation=useRef(0),inflight=useRef(false);
 useEffect(()=>{
  if(active)return;
  generation.current++;setExpanded(false);setPlan(null);
  setBusy(null);setError('');setNotice('');
 },[active]);
 const preview=async()=>{
  if(!active||inflight.current)return;
  inflight.current=true;const epoch=++generation.current;
  setBusy('preview');setError('');setNotice('');setPlan(null);
  try {
   const data=readBatchRepairPreview(
    await invoke<unknown>('cmd_drive_backup_batch_repair_preview'));
   if(epoch===generation.current)setPlan(data);
  }catch(e){if(epoch===generation.current)
   setError('Unable to validate all saved backups: '+String(e));}
  finally{inflight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 const repair=async()=>{
  if(!active||inflight.current||!plan?.ready)return;
  const confirmed=plan;
  let request;
  try{request=batchRepairRequest(confirmed);}
  catch(e){setError(String(e));return;}
  inflight.current=true;const epoch=++generation.current;
  setBusy('apply');setPlan(null);setError('');setNotice('');
  try{
   let result;
   try{
    result=readBatchRepairApplied(await invoke<unknown>(
     'cmd_drive_backup_batch_repair_apply',{request}),request,confirmed);
   }catch(e){
    if(epoch===generation.current)setError(
     'Outcome uncertain: '+String(e)+
     '. Inspect the watched backups and refresh before retrying; never auto-repeat.');
    return;
   }
   if(epoch!==generation.current)return;
   setNotice('Updated '+result.pairs_updated+' backup pairs: '+
    result.folders_added+' folders and '+result.files_added+
    ' completed file references. Verifying cloud metadata sync separately…');
   let sync='Cloud metadata synchronization unconfirmed.';
   try{
    const state=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
    if(!state.dirty&&state.source!=='qa-local')
     sync='Drive metadata synchronization reported clean.';
   }catch{/* Local atomic transaction must not be retried on a sync error. */}
   if(epoch===generation.current)setNotice('Backup repair committed: '+
    result.pairs_updated+' watched pairs updated, '+result.folders_added+
    ' folders and '+result.files_added+' completed file references added. '+
    sync+' Refresh and preview again to verify.');
  }finally{inflight.current=false;if(epoch===generation.current)setBusy(null);}
 };
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Atomic Watchlist backup repair">
  <div className="flex flex-wrap items-start justify-between gap-3">
   <div className="flex items-start gap-2 min-w-0">
    <FolderSync aria-hidden="true" className="h-4 w-4 mt-0.5 shrink-0 text-telegram-subtext"/>
    <div>
     <h3 className="text-sm font-medium text-telegram-text">Repair All Saved Drive Backups</h3>
     <p className="text-xs text-telegram-subtext">
      Fill missing folders and files across your watched backup pairs in one atomic operation.
     </p>
    </div>
   </div>
   <button type="button" className={btn} aria-expanded={expanded}
    disabled={busy==='apply'} onClick={()=>setExpanded(v=>!v)}>
    {expanded?'Close':'Open'}
   </button>
  </div>
  {expanded&&<div className="space-y-3">
   <p className="text-xs text-telegram-subtext">
    Uses the saved pairs from Drive Backup Watchlist. Every tree is checked before any
    update. A missing or unsafe file, pending upload, content mismatch, overlapping
    folders, or path collision blocks the entire batch. No backup file is replaced or
    deleted, and existing completed file content is referenced without re-uploading.
    Maximum per batch: 24 pairs, 512 new folders and 2,000 new files.
   </p>
   <div className="flex flex-wrap gap-2">
    <button type="button" className={btn} disabled={!active||busy!==null}
     onClick={()=>void preview()}>
     <RefreshCw aria-hidden="true" className="inline mr-1 h-3.5 w-3.5"/>
     Preview all saved backup repairs
    </button>
    <button type="button" className={btn} disabled={!active||busy!==null||!plan?.ready}
     onClick={()=>void repair()}>
     <ShieldCheck aria-hidden="true" className="inline mr-1 h-3.5 w-3.5"/>
     Apply all safe repairs atomically
    </button>
   </div>
   {plan&&<div className="space-y-3 text-xs text-telegram-text" role="status">
    <p className="font-medium">{plan.ready?'Ready to repair':'Batch repair blocked'}:
     {' '}{plan.pairs.length} watched pairs, {plan.total_folders} folders and
     {' '}{plan.total_files} files to add ({mb(plan.added_logical_bytes)} logical).
     {' '}{plan.total_matches} existing files match.
    </p>
    {plan.problems.map((p,i)=><p className="text-red-400 break-words" key={i}>{p}</p>)}
    <div className="max-h-56 space-y-2 overflow-auto" aria-label="Backup repairs by folder pair">
     {plan.pairs.map(pair=><div key={pair.source_id+':'+pair.backup_id}
      className="space-y-1 rounded-md border border-telegram-border p-2">
      <p className="font-medium break-all">{pair.source_name} → {pair.backup_name}</p>
      <p>{pair.add_folders} folders, {pair.add_files} files needed;
       {' '}{pair.matching_files} already matched</p>
      {pair.problems.map((s,i)=><p className="break-all text-red-400" key={i}>{s}</p>)}
      {pair.examples.slice(0,5).map((s,i)=><p className="break-all text-telegram-subtext" key={i}>{s}</p>)}
      {pair.examples.length>5&&<p className="text-telegram-subtext">Showing first 5 changes for this pair.</p>}
     </div>)}
    </div>
    {plan.ready&&<p className="text-telegram-subtext">
     Confirmation applies exactly this preview or rejects a stale plan.
     An interrupted result is never automatically retried.
    </p>}
   </div>}
   {busy&&<p className="text-xs text-telegram-subtext" role="status">
    {busy==='preview'?'Scanning all saved folder trees for safe missing content…':
     'Applying every confirmed backup repair in one transaction…'}
   </p>}
   {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
   {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
  </div>}
 </section>;
}
