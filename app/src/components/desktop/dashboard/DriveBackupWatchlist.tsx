import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {ShieldCheck,RefreshCw,ListChecks,Trash2} from 'lucide-react';
import {
 parseFolderInventory,readWatchList,readWatchScan,validateWatchPair,
 type FolderInventory,type BackupWatchList,type BackupWatchScan,
} from '../../../driveBackupWatchlist';

const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/50 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
export function DriveBackupWatchlist({active}:{active:boolean}){
 const [expanded,setExpanded]=useState(false);
 const [list,setList]=useState<BackupWatchList|null>(null);
 const [folders,setFolders]=useState<FolderInventory|null>(null);
 const [source,setSource]=useState(''),[backup,setBackup]=useState('');
 const [scan,setScan]=useState<BackupWatchScan|null>(null);
 const [busy,setBusy]=useState<'refresh'|'add'|'remove'|'scan'|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const generation=useRef(0),inflight=useRef(false);
 useEffect(()=>{if(active)return;generation.current++;setExpanded(false);
  setList(null);setFolders(null);setScan(null);setBusy(null);
  setError('');setNotice('');setSource('');setBackup('');
 },[active]);
 const names=useMemo(()=>new Map((folders?.folders??[]).map(f=>[f.id,f.path])),[folders]);
 const changed=()=>{setScan(null);setNotice('');setError('');};
 const refresh=async()=>{
  if(!active||inflight.current)return;
  inflight.current=true;const turn=++generation.current;
  setBusy('refresh');changed();
  try{
   const latest=readWatchList(await invoke<unknown>('cmd_drive_backup_watchlist_list'));
   const inventory=parseFolderInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
   if(turn!==generation.current)return;
   setList(latest);setFolders(inventory);
   setSource('');setBackup('');
  }catch(cause){if(turn===generation.current)setError('Cannot refresh backup Watchlist: '+String(cause));}
  finally{inflight.current=false;if(turn===generation.current)setBusy(null);}
 };
 const add=async()=>{
  if(!active||inflight.current||!folders||!list)return;
  let args;
  try{args=validateWatchPair(folders,source,backup,list);}
  catch(cause){setError(String(cause));return;}
  inflight.current=true;const turn=++generation.current;setBusy('add');changed();
  try{
   const next=readWatchList(await invoke<unknown>('cmd_drive_backup_watchlist_add',args));
   if(turn===generation.current){setList(next);setNotice('Saved backup pair locally. Scan to check all pairs.');}
  }catch(cause){if(turn===generation.current)setError('Watchlist save outcome uncertain: '+String(cause)+'. Refresh before retrying.');}
  finally{inflight.current=false;if(turn===generation.current)setBusy(null);}
 };
 const remove=async(sourceId:string,backupId:string)=>{
  if(!active||inflight.current||!list)return;
  inflight.current=true;const turn=++generation.current;setBusy('remove');changed();
  try{
   const next=readWatchList(await invoke<unknown>('cmd_drive_backup_watchlist_remove',
    {sourceId,backupId}));
   if(turn===generation.current){setList(next);setNotice('Removed local Watchlist pair; no Drive file was changed.');}
  }catch(cause){if(turn===generation.current)setError('Cannot confirm Watchlist removal: '+String(cause)+'. Refresh before retrying.');}
  finally{inflight.current=false;if(turn===generation.current)setBusy(null);}
 };
 const checkAll=async()=>{
  if(!active||inflight.current||!list||list.pairs.length===0)return;
  inflight.current=true;const turn=++generation.current;setBusy('scan');changed();
  try{
   const report=readWatchScan(await invoke<unknown>('cmd_drive_backup_watchlist_scan'),list);
   if(turn===generation.current)setScan(report);
  }catch(cause){if(turn===generation.current)setError('Backup scan did not complete: '+String(cause));}
  finally{inflight.current=false;if(turn===generation.current)setBusy(null);}
 };
 const lookup=(id:string,stored:string)=>names.get(id)??stored;
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive Backup Watchlist">
  <div className="flex flex-wrap items-center justify-between gap-2">
   <div className="flex min-w-0 items-start gap-2">
    <ListChecks className="mt-0.5 h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true"/>
    <div><h3 className="text-sm font-medium text-telegram-text">Drive Backup Watchlist</h3>
     <p className="text-xs text-telegram-subtext">Check all your saved backup folder pairs in one scan.</p></div>
   </div>
   <button type="button" className={ctl} aria-expanded={expanded}
    onClick={()=>setExpanded(v=>!v)}>{expanded?'Hide Watchlist':'Open Watchlist'}</button>
  </div>
  {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
   <p className="text-xs text-telegram-subtext">
    Save up to 24 separate source/backup folder pairs in this device's local database.
    Every scan checks nested folders and completed-file metadata, including hidden backing risks,
    without uploading, downloading, deleting or changing Drive content.
    <strong> Metadata agreement is not proof of identical file bytes or cloud backup.</strong>
    A changed or unavailable folder, unsynced metadata, unknown content identity or unfinished edit
    is never called healthy. Scans run only when you click; there is no background monitoring.
   </p>
   <button type="button" className={ctl} disabled={busy!==null||!active}
    onClick={()=>void refresh()}><RefreshCw className="mr-1 inline h-3.5 w-3.5"/>
    Refresh folders and saved pairs</button>
   {list&&folders&&<div className="space-y-3">
    <p className="text-xs text-telegram-text">{list.pairs.length} of 24 saved backup pairs</p>
    <div className="grid gap-2 md:grid-cols-2">
     <label className="space-y-1 text-xs text-telegram-subtext">Original folder
      <select className={field} value={source} disabled={busy!==null}
       onChange={e=>{setSource(e.target.value);changed();}}>
       <option value="">Select original folder</option>
       {folders.folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
      </select></label>
     <label className="space-y-1 text-xs text-telegram-subtext">Separate backup folder
      <select className={field} value={backup} disabled={busy!==null}
       onChange={e=>{setBackup(e.target.value);changed();}}>
       <option value="">Select backup folder</option>
       {folders.folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
      </select></label>
    </div>
    <div className="flex flex-wrap gap-2">
     <button type="button" className={ctl} disabled={busy!==null||!source||!backup||list.pairs.length>=24}
      onClick={()=>void add()}>Save folder pair</button>
     <button type="button" className={ctl} disabled={busy!==null||list.pairs.length===0}
      onClick={()=>void checkAll()}><ShieldCheck className="mr-1 inline h-3.5 w-3.5"/>
      Check all saved backups</button>
    </div>
    {list.pairs.length===0&&<p className="text-xs text-telegram-subtext">
      Save a source and backup folder to start monitoring.</p>}
    <div className="max-h-64 space-y-2 overflow-auto" aria-label="Saved backup pairs">
     {list.pairs.map(pair=>{
      const report=scan?.checks.find(c=>c.source_id===pair.source_id&&c.backup_id===pair.backup_id);
      return <div key={pair.source_id+':'+pair.backup_id}
       className="space-y-2 rounded-md border border-telegram-border p-2 text-xs text-telegram-text">
       <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 space-y-1">
         <p className="break-all">Original: {lookup(pair.source_id,pair.source_name)}</p>
         <p className="break-all text-telegram-subtext">Backup: {lookup(pair.backup_id,pair.backup_name)}</p>
        </div>
        <button className={ctl} type="button" title="Remove this saved Watchlist pair only"
         aria-label={'Remove Watchlist pair '+pair.source_name+' to '+pair.backup_name}
         disabled={busy!==null} onClick={()=>void remove(pair.source_id,pair.backup_id)}>
         <Trash2 className="h-3.5 w-3.5"/></button>
       </div>
       {report&&<div className="space-y-1">
        <p className="font-medium">{report.status==='healthy'?'Metadata matches':
          report.status==='drift'?'Backup drift detected':
          report.status==='unverified'?'Needs content or sync verification':'Verification blocked'}</p>
        <p>{report.explanation}</p>
        {report.status!=='blocked'&&<p className="text-telegram-subtext">
         {report.source_files} source files / {report.backup_files} backup files ·
         {' '}{report.only_source} source only · {report.only_backup} backup only ·
         {' '}{report.different} different · {report.unverified} unverified
        </p>}
        {report.samples.map((s,i)=><p key={i} className="break-all text-telegram-subtext">{s}</p>)}
       </div>}
      </div>;
     })}
    </div>
   </div>}
   {scan&&<p role="status" className="text-xs text-telegram-text">
    Checked {scan.checks.length} pairs at Drive revision {scan.revision}:
    {' '}{scan.healthy} metadata matches, {scan.drift} drift,
    {' '}{scan.unverified} unverified, {scan.blocked} blocked.
    {scan.dirty?' Drive contains unsynced local metadata.':''}
   </p>}
   {busy&&<p role="status" className="text-xs text-telegram-subtext">
    {busy==='scan'?'Comparing all saved backup folders…':
     busy==='add'?'Saving pair locally…':
     busy==='remove'?'Removing saved pair…':'Reading local Drive folders and Watchlist…'}</p>}
   {notice&&<p role="status" className="text-xs text-telegram-text">{notice}</p>}
   {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
  </div>}
 </section>;
}
