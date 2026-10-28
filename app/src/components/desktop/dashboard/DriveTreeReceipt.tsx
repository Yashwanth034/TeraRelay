import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {open,save} from '@tauri-apps/plugin-dialog';
import {FileCheck2,Download,Upload,RefreshCw} from 'lucide-react';
import {readTreeTrashRuns,type TreeRuns} from '../../../driveFolderTreeTrash';
import {readTreeReceipt,type TreeReceiptResult} from '../../../driveTreeReceipt';
const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
export function DriveTreeReceipt({active}:{active:boolean}) {
 const [expanded,setExpanded]=useState(false);
 const [runs,setRuns]=useState<TreeRuns|null>(null),[root,setRoot]=useState('');
 const [busy,setBusy]=useState<'load'|'export'|'import'|null>(null);
 const [notice,setNotice]=useState(''),[error,setError]=useState('');
 const [result,setResult]=useState<TreeReceiptResult|null>(null);
 const epoch=useRef(0),inflight=useRef(false);
 useEffect(()=>{
  if(active)return;
  epoch.current++;setExpanded(false);setRuns(null);setRoot('');
  setResult(null);setNotice('');setError('');setBusy(null);
 },[active]);
 const load=async()=>{
  if(!active||inflight.current)return;
  const turn=++epoch.current;inflight.current=true;
  setBusy('load');setError('');setNotice('');setResult(null);
  try{
   const next=readTreeTrashRuns(await invoke<unknown>('cmd_drive_folder_tree_trash_runs'));
   if(turn===epoch.current){setRuns(next);setRoot('');}
  }catch(e){if(turn===epoch.current)setError('Cannot read recovery journals: '+String(e));}
  finally{inflight.current=false;if(turn===epoch.current)setBusy(null);}
 };
 const exportReceipt=async()=>{
  if(!active||inflight.current||!runs?.runs.some(x=>x.root_id===root))return;
  const turn=++epoch.current;inflight.current=true;
  setBusy('export');setError('');setNotice('');setResult(null);
  try{
   const path=await save({title:'Save private folder recovery receipt',
    defaultPath:'TeraRelay-folder-recovery.json',
    filters:[{name:'Private recovery receipt',extensions:['json']}]});
   if(!path||turn!==epoch.current)return;
   const data=readTreeReceipt(await invoke<unknown>(
    'cmd_drive_tree_receipt_export',{rootId:root,destinationPath:path}),path,false);
   if(data.root_id!==root)throw Error('Receipt root identity changed.');
   if(turn===epoch.current){
    setResult(data);setNotice('Recovery receipt exported. Keep it private and safe; it contains identities and paths but no file bytes.');
   }
  }catch(e){if(turn===epoch.current)setError(
   'Export result uncertain: '+String(e)+'. Inspect the destination before retrying.');}
  finally{inflight.current=false;if(turn===epoch.current)setBusy(null);}
 };
 const importReceipt=async()=>{
  if(!active||inflight.current)return;
  const turn=++epoch.current;inflight.current=true;
  setBusy('import');setError('');setNotice('');setResult(null);
  try{
   const path=await open({title:'Choose verified folder recovery receipt',
    multiple:false,directory:false,
    filters:[{name:'Private recovery receipt',extensions:['json']}]});
   if(typeof path!=='string'||turn!==epoch.current)return;
   const data=readTreeReceipt(await invoke<unknown>(
    'cmd_drive_tree_receipt_import',{sourcePath:path}),path,true);
   if(turn===epoch.current){
    setResult(data);setRuns(null);setRoot('');
    setNotice('Complete recovery journal verified and imported locally. No files were restored. Open Whole Folder Recovery, refresh, and preview to restore.');
   }
  }catch(e){if(turn===epoch.current)setError(
   'Import result uncertain: '+String(e)+'. Refresh journals before trying again; no automatic retries.');}
  finally{inflight.current=false;if(turn===epoch.current)setBusy(null);}
 };
 return <section className="tr-setting-row space-y-3 p-3" aria-label="Portable folder recovery receipt">
  <div className="flex items-start justify-between gap-2">
   <div className="flex items-start gap-2">
    <FileCheck2 aria-hidden="true" className="h-4 w-4 mt-0.5 text-telegram-subtext"/>
    <div><h3 className="text-sm font-medium text-telegram-text">Portable Folder Recovery Receipt</h3>
     <p className="text-xs text-telegram-subtext">Keep a private, verifiable copy of your whole-folder Trash recovery plan.</p></div>
   </div>
   <button type="button" className={ctl} aria-expanded={expanded} onClick={()=>setExpanded(v=>!v)}>
    {expanded?'Close':'Open'}
   </button>
  </div>
  {expanded&&<div className="space-y-3">
   <p className="text-xs text-telegram-subtext">
    Export a previously trashed complete folder journal for safekeeping.
    Import on a device using the same synced Drive namespace only if every original
    trashed item, content backing and path still matches. Nothing restores automatically.
    SHA-256 detects accidental changes; it is not a digital signature. This file contains
    sensitive filenames and identifiers, <strong>not file contents</strong>.
   </p>
   <button className={ctl} type="button" disabled={!active||busy!==null} onClick={()=>void load()}>
    <RefreshCw className="h-3.5 w-3.5 inline mr-1" aria-hidden="true"/>Refresh recoverable folders
   </button>
   {runs&&<label className="block space-y-1 text-xs text-telegram-subtext">
    Whole folder journal to export
    <select className={field} value={root} disabled={busy!==null}
     onChange={e=>{setRoot(e.target.value);setResult(null);setNotice('');setError('');}}>
     <option value="">Select trashed folder</option>
     {runs.runs.map(item=><option value={item.root_id} key={item.root_id}>
      {item.name} ({item.folder_count} folders, {item.file_count} files)
     </option>)}
    </select>
   </label>}
   <div className="flex flex-wrap gap-2">
    <button className={ctl} type="button" disabled={!active||busy!==null||!root||!runs}
     onClick={()=>void exportReceipt()}>
     <Download className="h-3.5 w-3.5 inline mr-1" aria-hidden="true"/>Export private JSON
    </button>
    <button className={ctl} type="button" disabled={!active||busy!==null}
     onClick={()=>void importReceipt()}>
     <Upload className="h-3.5 w-3.5 inline mr-1" aria-hidden="true"/>Verify &amp; import JSON
    </button>
   </div>
   {result&&<div className="text-xs text-telegram-text space-y-1" role="status">
    <p>{result.imported?'Imported':'Exported'} {result.folders} folders and {result.files} files.</p>
    <p className="break-all text-telegram-subtext">SHA-256: {result.checksum}</p>
   </div>}
   {busy&&<p role="status" className="text-xs text-telegram-subtext">
    {busy==='load'?'Loading local journals…':busy==='export'?'Verifying and exporting private receipt…':
     'Checking all original trash members before importing…'}
   </p>}
   {notice&&<p role="status" className="text-xs text-telegram-text">{notice}</p>}
   {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
  </div>}
 </section>;
}
