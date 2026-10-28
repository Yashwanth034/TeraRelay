import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {GitCompareArrows,RefreshCw,ShieldCheck} from 'lucide-react';
import {parityFolders,readFolderParity,parityVerdict,
 type ParityFolder,type ParityReport} from '../../../driveFolderParity';

const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/50 disabled:opacity-50';
const input='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
const descriptions={
 only_left:'Only in first folder',only_right:'Only in second folder',
 metadata_difference:'Different type or content metadata',
 content_unverified:'Matching metadata but content identity unverified',
} as const;
export function DriveFolderParity({active}:{active:boolean}){
 const [open,setOpen]=useState(false),[folders,setFolders]=useState<ParityFolder[]|null>(null);
 const [left,setLeft]=useState(''),[right,setRight]=useState('');
 const [report,setReport]=useState<ParityReport|null>(null);
 const [busy,setBusy]=useState(false),[error,setError]=useState('');
 const [filter,setFilter]=useState<'all'|keyof typeof descriptions>('all');
 const lock=useRef(false),epoch=useRef(0);
 useEffect(()=>{
  if(active)return;epoch.current++;setOpen(false);setReport(null);
  setFolders(null);setLeft('');setRight('');setBusy(false);setError('');
 },[active]);
 const refresh=async()=>{
  if(!active||lock.current)return;
  lock.current=true;const turn=++epoch.current;
  setBusy(true);setError('');setReport(null);
  try {
   const inventory=parityFolders(await invoke<unknown>('cmd_drive_get_snapshot'));
   if(epoch.current!==turn)return;
   setFolders(inventory);
   setLeft(inventory[0]?.id??'');setRight(inventory[1]?.id??'');
  }catch(cause){
   if(epoch.current===turn)setError('Folder inventory unavailable: '+String(cause));
  }finally{lock.current=false;if(epoch.current===turn)setBusy(false);}
 };
 const compare=async()=>{
  if(!active||lock.current||!left||!right||left===right)return;
  lock.current=true;const turn=++epoch.current;
  setBusy(true);setError('');setReport(null);
  try{
   const result=readFolderParity(await invoke<unknown>('cmd_drive_folder_parity',
     {leftId:left,rightId:right}),left,right);
   if(epoch.current===turn)setReport(result);
  }catch(cause){
   if(epoch.current===turn)setError('Cannot verify folder parity: '+String(cause)
     +'. Unfinished edits or missing backing content must be resolved before trusting a comparison.');
  }finally{lock.current=false;if(epoch.current===turn)setBusy(false);}
 };
 const verdict=report?parityVerdict(report):null;
 const shown=report?.differences.filter(d=>filter==='all'||d.kind===filter)??[];
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive folder parity check">
  <div className="flex flex-wrap items-center justify-between gap-2">
   <div className="flex min-w-0 items-start gap-2">
    <GitCompareArrows className="h-4 w-4 mt-0.5 shrink-0 text-telegram-subtext" aria-hidden="true"/>
    <div>
     <h3 className="text-sm font-medium text-telegram-text">Drive Folder Parity Check</h3>
     <p className="text-xs text-telegram-subtext">Compare two entire project or backup folder trees, not just filenames.</p>
    </div>
   </div>
   <button type="button" className={ctl} aria-expanded={open}
    onClick={()=>setOpen(o=>!o)}>{open?'Hide comparison':'Compare folders'}</button>
  </div>
  {open&&<div className="space-y-3 border-t border-telegram-border pt-3">
   <p className="text-xs text-telegram-subtext">
    Read-only: compares nested directories, links, completed files, and available content metadata
    using the local Drive database. It rejects pending writes, unavailable backing content,
    and ambiguous names rather than giving false reassurance. It does not read file bytes,
    repair, synchronize or delete data. A metadata match does not prove byte-for-byte identity.
   </p>
   <button type="button" className={ctl} disabled={busy||!active} onClick={()=>void refresh()}>
    <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Refresh folders
   </button>
   {folders&&<div className="space-y-2">
    <label className="block space-y-1 text-xs text-telegram-subtext">First folder (source)
     <select className={input} disabled={busy} value={left}
      onChange={e=>{setLeft(e.target.value);setReport(null);}}>
      <option value="">Choose first folder</option>
      {folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <label className="block space-y-1 text-xs text-telegram-subtext">Second folder (backup/copy)
     <select className={input} disabled={busy} value={right}
      onChange={e=>{setRight(e.target.value);setReport(null);}}>
      <option value="">Choose second folder</option>
      {folders.map(f=><option key={f.id} value={f.id}>{f.path}</option>)}
     </select>
    </label>
    <button type="button" className={ctl} disabled={busy||!left||!right||left===right}
      onClick={()=>void compare()}>
     <ShieldCheck className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
     Compare complete folder trees
    </button>
    {left===right&&left&&<p className="text-xs text-telegram-subtext">Select two different folders.</p>}
   </div>}
   {report&&<div className="space-y-2 text-xs text-telegram-text" role="status" aria-live="polite">
    <p>{report.left_folders} vs {report.right_folders} nested directories,
     {' '}{report.left_files} vs {report.right_files} completed files.</p>
    <p>{report.matching_file_metadata} matching file-metadata pairs,
     {' '}{report.unverified_file_content} pairs without sufficient proof of content identity,
     {' '}{report.only_left} only in first, {report.only_right} only in second,
     {' '}{report.different} metadata mismatches.</p>
    {verdict==='metadata_equal'
      ?<p>Both trees match in recorded structure and content metadata. File bytes were not re-read.</p>
      :verdict==='unverified'
        ?<p>Structure matches, but some file contents cannot be confirmed from metadata alone.</p>
        :<p>Differences found. Review all findings before trusting the second folder as a copy.</p>}
    {report.dirty&&<p className="text-telegram-subtext">
      This Drive revision contains unsynced local metadata; the cloud may differ.</p>}
    {report.truncated_differences&&<p className="text-telegram-subtext">
      Showing only the first 200 findings. Full difference count is larger; not a complete per-path listing.</p>}
    <label className="block space-y-1 text-telegram-subtext">Findings
     <select className={input} value={filter}
      onChange={e=>setFilter(e.target.value as typeof filter)}>
      <option value="all">All findings</option>
      {(Object.keys(descriptions) as Array<keyof typeof descriptions>).map(k=>
       <option key={k} value={k}>{descriptions[k]}</option>)}
     </select>
    </label>
    <div className="max-h-60 space-y-2 overflow-auto" aria-label="Drive folder comparison findings">
     {shown.map((finding,i)=><div key={i} className="rounded-md border border-telegram-border p-2">
      <p className="font-medium">{descriptions[finding.kind]}</p>
      <p className="break-all">{finding.relative_path}</p>
      <p className="text-telegram-subtext">
       First: {finding.left??'absent'} · Second: {finding.right??'absent'}</p>
     </div>)}
    </div>
   </div>}
   {busy&&<p role="status" className="text-xs text-telegram-subtext">Checking Drive folder metadata…</p>}
   {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
  </div>}
 </section>;
}
