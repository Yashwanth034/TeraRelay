import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Trash2,RefreshCw,ShieldCheck} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
  readBatchTrashInventory,buildBatchTrashRequest,readBatchTrashPreview,readBatchTrashApplied,
  type BatchTrashInventory,type BatchTrashPreview,
} from '../../../driveBatchTrash';

const button='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
function bytes(value:number){return value>1048576?(value/1048576).toFixed(1)+' MiB':(value/1024).toFixed(1)+' KiB';}

export function DriveBatchTrash({active}:{active:boolean}) {
  const [expanded,setExpanded]=useState(false);
  const [inventory,setInventory]=useState<BatchTrashInventory|null>(null);
  const [selected,setSelected]=useState<string[]>([]);
  const [filter,setFilter]=useState('');
  const [preview,setPreview]=useState<{key:string;data:BatchTrashPreview}|null>(null);
  const [busy,setBusy]=useState<'load'|'preview'|'apply'|null>(null);
  const [error,setError]=useState(''),[notice,setNotice]=useState('');
  const inflight=useRef(false),epoch=useRef(0);
  useEffect(()=>{
    if(active)return;
    setExpanded(false);setPreview(null);
    if(busy!=='apply') {
      epoch.current++;setInventory(null);setSelected([]);setBusy(null);
    }
  },[active,busy]);
  const planned=useMemo(()=>{
    if(!inventory||!selected.length)return {request:null,error:''};
    try{return {request:buildBatchTrashRequest(inventory,selected),error:''};}
    catch(cause){return {request:null,error:String(cause)};}
  },[inventory,selected]);
  const request=planned.request;
  const key=request?JSON.stringify(request):'';
  const ready=!!request&&preview?.key===key&&preview.data.ready;
  const invalidate=()=>{setPreview(null);setError('');setNotice('');};
  const load=async()=>{
    if(!active||inflight.current)return;
    inflight.current=true;const turn=++epoch.current;
    setBusy('load');setError('');setNotice('');setPreview(null);
    try {
      const data=readBatchTrashInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
      if(epoch.current===turn){setInventory(data);setSelected([]);}
    }catch(cause){
      if(epoch.current===turn)setError('Cannot read current Drive files: '+String(cause));
    }finally{
      inflight.current=false;if(epoch.current===turn)setBusy(null);
    }
  };
  const check=async()=>{
    if(!active||inflight.current)return;
    if(!request){setError(planned.error||'Select 1–100 completed Drive files.');return;}
    inflight.current=true;const turn=++epoch.current;
    setBusy('preview');setError('');setNotice('');setPreview(null);
    try{
      const data=readBatchTrashPreview(await invoke<unknown>(
        'cmd_drive_batch_trash_preview',{request}),request);
      if(epoch.current===turn)setPreview({key,data});
    }catch(cause){
      if(epoch.current===turn)setError('Native batch Trash preview failed: '+String(cause));
    }finally{
      inflight.current=false;if(epoch.current===turn)setBusy(null);
    }
  };
  const apply=async()=>{
    if(!active||inflight.current||!ready||!request)return;
    inflight.current=true;const turn=++epoch.current;
    setBusy('apply');setError('');setNotice('');setPreview(null);
    try{
      let moved=0;
      try{
        const result=readBatchTrashApplied(await invoke<unknown>(
          'cmd_drive_batch_trash_apply',{request}),request);
        moved=result.trashed;
      }catch(cause){
        if(epoch.current===turn)setError('Batch Trash result uncertain: '+String(cause)+
          '. Refresh Drive and Recently Deleted before retrying. No automatic retries.');
        return;
      }
      if(epoch.current!==turn)return;
      setSelected([]);setInventory(null);
      setNotice(moved+' Drive files moved to Recently Deleted locally. Checking metadata sync…');
      let sync='Cloud metadata publication not independently verified.';
      try{
        const result=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if(!result.dirty&&result.source!=='qa-local')
          sync='Drive metadata sync returned a clean result.';
      }catch{/* A completed local Trash transaction remains committed. */}
      if(epoch.current===turn)
        setNotice(moved+' files moved to Recently Deleted. '+sync+
          ' Restore them with Atomic Batch Restore when needed.');
    }finally{
      inflight.current=false;if(epoch.current===turn)setBusy(null);
    }
  };
  const matching=(inventory?.files??[]).filter(f=>
    !filter||f.path.toLocaleLowerCase().includes(filter.trim().toLocaleLowerCase()));
  const shown=matching.slice(0,100);
  const toggle=(id:string,on:boolean)=>{
    setSelected(old=>on?old.includes(id)?old:old.length>=100?old:[...old,id]
      :old.filter(x=>x!==id));
    invalidate();
  };
  return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive Atomic Batch Trash">
    <div className="flex items-start justify-between gap-3">
      <div className="flex min-w-0 items-start gap-2">
        <Trash2 className="w-4 h-4 mt-0.5 text-telegram-subtext shrink-0" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Atomic Batch Trash</h3>
          <p className="text-xs text-telegram-subtext">
            Clean up up to 100 completed files in one all-or-nothing operation; items stay in Recently Deleted.
          </p>
        </div>
      </div>
      <button className={button} type="button" disabled={busy==='apply'}
        aria-expanded={expanded} onClick={()=>setExpanded(x=>!x)}>
        {expanded?'Close cleanup':'Open cleanup'}
      </button>
    </div>
    {expanded&&<div className="space-y-3">
      <button className={button} type="button" disabled={!!busy||!active} onClick={()=>void load()}>
        <RefreshCw className="w-3.5 h-3.5 inline mr-1" aria-hidden="true"/>Refresh completed files
      </button>
      {inventory&&<>
        <p className="text-xs text-telegram-subtext">
          {inventory.files.length} completed entries · {selected.length} selected.
          {inventory.skipped>0?' '+inventory.skipped+' unresolved metadata records excluded.':''}
          This does not permanently erase remote file content or guarantee reclaimed storage.
        </p>
        <label className="block text-xs text-telegram-subtext space-y-1">
          Find Drive files
          <input className={field} value={filter} disabled={!!busy}
            onChange={e=>setFilter(e.target.value)} placeholder="Filter complete file paths"/>
        </label>
        <div className="max-h-48 overflow-y-auto space-y-1 rounded-md border border-telegram-border p-2"
          role="group" aria-label="Select files for Atomic Batch Trash">
          {shown.map(file=><label key={file.entryId} className="flex items-center gap-2 text-xs text-telegram-text">
            <input type="checkbox" checked={selected.includes(file.entryId)}
              disabled={!!busy||(!selected.includes(file.entryId)&&selected.length>=100)}
              onChange={event=>toggle(file.entryId,event.target.checked)}/>
            <span className="min-w-0 truncate" title={file.path}>{file.path}</span>
            {file.logicalBytes!==null&&<span className="shrink-0 text-telegram-subtext">
              {bytes(file.logicalBytes)}</span>}
          </label>)}
          {!shown.length&&<p className="text-xs text-telegram-subtext">No matching files.</p>}
          {matching.length>shown.length&&<p className="text-xs text-telegram-subtext">
            Showing first 100 matches. Narrow your search for more.</p>}
        </div>
        {planned.error&&<p role="alert" className="text-xs text-red-400">{planned.error}</p>}
        <div className="flex flex-wrap gap-2">
          <button className={button} type="button" disabled={!!busy||!request}
            onClick={()=>void check()}>Preview {request?.files.length??0} files</button>
          <button className={button} type="button" disabled={!!busy||!ready}
            onClick={()=>void apply()}>
            <ShieldCheck className="w-3.5 h-3.5 inline mr-1" aria-hidden="true"/>
            Move verified batch to Trash
          </button>
        </div>
        {preview&&<div role="status" className="rounded-md border border-telegram-border p-2 space-y-1 text-xs text-telegram-text">
          <p>{preview.data.ready
            ?preview.data.count+' completed files ready. '+bytes(preview.data.logical_bytes)+
              ' logical size; remote storage is not necessarily reclaimed.'
            :'Batch blocked by '+preview.data.problems.length+' validation issue(s).'}</p>
          {preview.data.problems.map((reason,i)=><p key={i}>{reason}</p>)}
          {preview.data.ready&&<div className="max-h-36 overflow-y-auto">
            {selected.map(id=><p key={id} className="break-all">
              {inventory.files.find(file=>file.entryId===id)?.path}</p>)}
          </div>}
        </div>}
      </>}
      {busy&&<p role="status" className="text-xs text-telegram-subtext">
        {busy==='load'?'Reading Drive metadata…':busy==='preview'
          ?'Checking complete files and pending edits…':'Committing atomic Recently Deleted transaction…'}
      </p>}
      {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
      {notice&&<p role="status" className="text-xs text-telegram-text">{notice}</p>}
    </div>}
  </section>;
}
