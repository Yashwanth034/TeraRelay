import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {History,RefreshCw,ShieldCheck,Trash2} from 'lucide-react';
import {auditChanges,readAuditHistory,type AuditHistory} from '../../../driveBackupTimeline';
import {readWatchList} from '../../../driveBackupWatchlist';
const button='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
export function DriveBackupTimeline({active}:{active:boolean}){
 const [open,setOpen]=useState(false),[history,setHistory]=useState<AuditHistory|null>(null);
 const [error,setError]=useState(''),[notice,setNotice]=useState('');
 const [busy,setBusy]=useState(false),[confirmClear,setConfirmClear]=useState(false);
 const [selected,setSelected]=useState<number|null>(null);
 const [labels,setLabels]=useState<Record<string,string>>({});
 const inFlight=useRef(false),epoch=useRef(0);
 useEffect(()=>{if(active)return;epoch.current++;setOpen(false);setHistory(null);
  setBusy(false);setConfirmClear(false);setError('');setNotice('');setSelected(null);setLabels({});
 },[active]);
 const run=async(kind:'list'|'record'|'clear')=>{
  if(!active||inFlight.current)return;
  inFlight.current=true;const turn=++epoch.current;
  setBusy(true);setError('');setNotice('');setConfirmClear(false);
  try{
   const command=kind==='list'?'cmd_drive_backup_timeline_list':
    kind==='record'?'cmd_drive_backup_timeline_record':'cmd_drive_backup_timeline_clear';
   const next=readAuditHistory(await invoke<unknown>(command));
   if(epoch.current!==turn)return;
   setHistory(next);setSelected(next.runs[0]?.id??null);
   try{
    const saved=readWatchList(await invoke<unknown>('cmd_drive_backup_watchlist_list'));
    if(epoch.current===turn){
     const names:Record<string,string>={};
     for(const pair of saved.pairs){names[pair.source_id]=pair.source_name;names[pair.backup_id]=pair.backup_name;}
     setLabels(names);
    }
   }catch{/* Historical backup IDs remain visible when current labels are unavailable. */}
   if(kind==='record')setNotice('Recorded one local read-only backup audit. No Drive file was changed.');
   if(kind==='clear')setNotice('Local backup audit history cleared. Saved backup pairs and Drive files remain.');
  }catch(e){if(epoch.current===turn)setError(
   (kind==='record'?'Audit outcome uncertain; refresh history before retrying: ':
    kind==='clear'?'Clear outcome uncertain; refresh history: ':'Could not load audit timeline: ')
   +String(e));}
  finally{inFlight.current=false;if(epoch.current===turn)setBusy(false);}
 };
 const index=history?.runs.findIndex(r=>r.id===selected)??-1;
 const current=index<0?null:history!.runs[index];
 const prior=index<0?null:history!.runs[index+1]??null;
 const changes=current?auditChanges(current,prior):[];
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Backup assurance history">
  <div className="flex items-start justify-between gap-2">
   <div className="flex min-w-0 items-start gap-2">
    <History className="mt-0.5 h-4 w-4 shrink-0 text-telegram-subtext"/>
    <div><h3 className="text-sm font-medium text-telegram-text">Backup Assurance Timeline</h3>
     <p className="text-xs text-telegram-subtext">
      Track whether your backup checks improve or deteriorate over time.</p></div>
   </div>
   <button type="button" className={button} aria-expanded={open}
    onClick={()=>setOpen(v=>!v)}>{open?'Hide history':'Open history'}</button>
  </div>
  {open&&<div className="space-y-3 border-t border-telegram-border pt-3">
   <p className="text-xs text-telegram-subtext">
    Runs an on-demand comparison of your saved Backup Watchlist pairs and records
    the last 40 checks in this device's private database. It reports missing files,
    changed metadata, unknown backing and unsynced state. This is not automatic monitoring,
    cloud verification, or a byte-level integrity test. No files are modified.
   </p>
   <div className="flex flex-wrap gap-2">
    <button type="button" className={button} disabled={busy} onClick={()=>void run('list')}>
     <RefreshCw className="mr-1 inline h-3.5 w-3.5"/>Refresh history</button>
    <button type="button" className={button} disabled={busy} onClick={()=>void run('record')}>
     <ShieldCheck className="mr-1 inline h-3.5 w-3.5"/>Check and record backups</button>
    {history&&history.runs.length>0&&<button type="button" className={button}
     disabled={busy} onClick={()=>setConfirmClear(true)}>
     <Trash2 className="mr-1 inline h-3.5 w-3.5"/>Clear local history</button>}
   </div>
   {confirmClear&&<div className="flex flex-wrap items-center gap-2 text-xs">
    <span className="text-telegram-text">Delete all saved audit history on this device?</span>
    <button type="button" className={button} disabled={busy} onClick={()=>void run('clear')}>
     Confirm clear</button>
    <button type="button" className={button} onClick={()=>setConfirmClear(false)}>Cancel</button>
   </div>}
   {history&&<div className="space-y-2 text-xs text-telegram-text">
    <p>{history.runs.length} of {history.max_runs} recent audit records retained.</p>
    {history.runs.length===0&&<p className="text-telegram-subtext">
     Save source/backup pairs in Backup Watchlist, then run the first audit here.</p>}
    <div className="max-h-44 space-y-1 overflow-auto" aria-label="Saved backup audit runs">
     {history.runs.map(r=><button type="button" key={r.id}
      className={button+' block w-full text-left'}
      aria-pressed={selected===r.id} onClick={()=>setSelected(r.id)}>
      {new Date(r.scan.checked_at).toLocaleString()} · {r.scan.healthy} metadata matches,
      {' '}{r.scan.drift} drift, {r.scan.unverified} unverified,
      {' '}{r.scan.blocked} blocked
     </button>)}
    </div>
    {current&&<div className="space-y-2 rounded-md border border-telegram-border p-2">
     <p className="font-medium">Recorded Drive revision {current.scan.revision}
      {current.scan.dirty?' (unsynced local metadata)':''}</p>
     {changes.map((c,i)=>{
      const item=current.scan.checks[i];
      return <div className="space-y-1 border-t border-telegram-border pt-2"
       key={item.source_id+':'+item.backup_id}>
       <p className="break-all">{labels[item.source_id]??item.source_id} → {labels[item.backup_id]??item.backup_id}</p>
       <p>{c.after==='healthy'?'Metadata matches':c.after==='drift'?'Drift detected':
        c.after==='unverified'?'Not fully verified':'Comparison blocked'}
        {' · '}{c.direction==='new'?'First recorded check':
         c.direction==='improved'?'Improved since prior check':
         c.direction==='worse'?'Worsened since prior check':
         c.direction==='unchanged'?'Same status as prior check':'Different issue since prior check'}</p>
       <p className="text-telegram-subtext">{item.explanation}</p>
       <p className="text-telegram-subtext">{item.only_source} source-only,
        {' '}{item.only_backup} backup-only, {item.different} metadata differences,
        {' '}{item.unverified} unverified files</p>
       {item.samples.map((sample,j)=><p key={j} className="break-all text-telegram-subtext">
        {sample}</p>)}
      </div>;
     })}
    </div>}
   </div>}
   {busy&&<p role="status" className="text-xs text-telegram-subtext">Checking local backups…</p>}
   {notice&&<p role="status" className="text-xs text-telegram-text">{notice}</p>}
   {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
  </div>}
 </section>;
}
