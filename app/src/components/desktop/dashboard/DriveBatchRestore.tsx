import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {Undo2,RefreshCw,ShieldCheck} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
  buildBatchRestoreRequest,readBatchRestoreApplied,readBatchRestoreInventory,
  readBatchRestorePreview,type BatchRestoreInventory,type BatchRestorePreview,
} from '../../../driveBatchRestore';
const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const size=(bytes:number)=>bytes>1024*1024?(bytes/1048576).toFixed(1)+' MiB':(bytes/1024).toFixed(1)+' KiB';

export function DriveBatchRestore({active}:{active:boolean}) {
  const [expanded,setExpanded]=useState(false);
  const [inventory,setInventory]=useState<BatchRestoreInventory|null>(null);
  const [selected,setSelected]=useState<string[]>([]);
  const [filter,setFilter]=useState('');
  const [preview,setPreview]=useState<{key:string;data:BatchRestorePreview}|null>(null);
  const [busy,setBusy]=useState<'load'|'check'|'apply'|null>(null);
  const [error,setError]=useState(''),[notice,setNotice]=useState('');
  const inFlight=useRef(false),epoch=useRef(0);
  useEffect(()=>{
    if(active)return;
    setExpanded(false);setPreview(null);
    if(busy!=='apply'){
      epoch.current++;
      setInventory(null);setSelected([]);setBusy(null);
    }
  },[active,busy]);
  const planned=useMemo(()=>{
    if(!inventory||selected.length===0)return {request:null,error:''};
    try{return {request:buildBatchRestoreRequest(inventory,selected),error:''};}
    catch(cause){return {request:null,error:String(cause)};}
  },[inventory,selected]);
  const request=planned.request;
  const key=request?JSON.stringify(request):'';
  const canApply=!!request&&preview?.key===key&&preview.data.ready;
  const load=async()=>{
    if(!active||inFlight.current)return;
    inFlight.current=true;const turn=++epoch.current;
    setBusy('load');setPreview(null);setError('');setNotice('');
    try{
      const [metadata,trash]=await Promise.all([
        invoke<unknown>('cmd_drive_get_snapshot'),invoke<unknown>('cmd_drive_list_trash')]);
      const data=readBatchRestoreInventory(metadata,trash);
      if(epoch.current===turn){setInventory(data);setSelected([]);}
    }catch(cause){
      if(epoch.current===turn)setError('Cannot safely load Recently Deleted: '+String(cause));
    }finally{
      inFlight.current=false;
      if(epoch.current===turn)setBusy(null);
    }
  };
  const check=async()=>{
    if(!active||inFlight.current)return;
    if(!request){setError(planned.error||'Choose restorable items in Recently Deleted.');return;}
    inFlight.current=true;const turn=++epoch.current;
    setBusy('check');setPreview(null);setError('');setNotice('');
    try{
      const result=readBatchRestorePreview(
        await invoke<unknown>('cmd_drive_batch_restore_preview',{request}),request);
      if(epoch.current===turn)setPreview({key,data:result});
    }catch(cause){
      if(epoch.current===turn)setError('Exact batch restore preview failed: '+String(cause));
    }finally{
      inFlight.current=false;
      if(epoch.current===turn)setBusy(null);
    }
  };
  const apply=async()=>{
    if(!active||inFlight.current||!request||!canApply)return;
    inFlight.current=true;const turn=++epoch.current;
    setBusy('apply');setPreview(null);setError('');setNotice('');
    try{
      let restored=0;
      try{
        const result=readBatchRestoreApplied(
          await invoke<unknown>('cmd_drive_batch_restore_apply',{request}),request);
        restored=result.restored;
      }catch(cause){
        // A lost response can follow a committed transaction: never replay it.
        if(epoch.current===turn)setError('Batch restore result is uncertain: '+String(cause)
          +'. Refresh Recently Deleted before trying another recovery. No automatic retries.');
        return;
      }
      if(epoch.current!==turn)return;
      setInventory(null);setSelected([]);
      setNotice(restored+' items restored to their exact original locations locally. Checking metadata sync…');
      let sync='Cloud metadata publication not independently verified; retry synchronization when online.';
      try{
        const result=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if(!result.dirty&&result.source!=='qa-local')
          sync='Drive metadata sync returned a clean result.';
      }catch{/* Never report a committed local recovery as rolled back. */}
      if(epoch.current===turn)setNotice(restored+' items restored locally. '+sync);
    }finally{
      inFlight.current=false;
      if(epoch.current===turn)setBusy(null);
    }
  };
  const matching=(inventory?.entries??[]).filter(x=>x.restorable
    &&(!filter||x.name.toLocaleLowerCase().includes(filter.trim().toLocaleLowerCase())));
  const shown=matching.slice(0,100);
  return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive atomic batch restore">
    <div className="flex items-center justify-between gap-2">
      <div className="flex min-w-0 items-start gap-2">
        <Undo2 className="h-4 w-4 mt-0.5 shrink-0 text-telegram-subtext" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Atomic Batch Restore</h3>
          <p className="text-xs text-telegram-subtext">
            Recover up to 100 recently deleted items together, with exact original locations and no silent renames.
          </p>
        </div>
      </div>
      <button type="button" className={ctl} aria-expanded={expanded}
        disabled={busy==='apply'} onClick={()=>setExpanded(v=>!v)}>
        {expanded?'Hide recovery':'Open batch recovery'}
      </button>
    </div>
    {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        Preview checks every original parent, name, content availability and current Drive revision.
        A missing parent or naming conflict blocks the entire batch. For safety, Linux batch restore
        requires the Drive mount to be unmounted; use normal single-item restore while mounted.
        Nothing is permanently deleted and no remote file content is rewritten.
      </p>
      <button type="button" className={ctl} disabled={!!busy||!active} onClick={()=>void load()}>
        <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Refresh Recently Deleted
      </button>
      {inventory&&<>
        <p className="text-xs text-telegram-subtext">
          {inventory.entries.length} validated trash item(s) · {inventory.skipped} malformed/ambiguous excluded · {selected.length} selected
        </p>
        <label className="block space-y-1 text-xs text-telegram-subtext">
          Find recently deleted items
          <input type="search" className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text"
            value={filter} disabled={!!busy} onChange={e=>setFilter(e.target.value)} placeholder="Search filenames or folders"/>
        </label>
        <div className="max-h-48 overflow-auto rounded-md border border-telegram-border p-2 space-y-1"
          role="group" aria-label="Choose recently deleted Drive entries">
          {shown.map(entry=>{
            const id=entry.kind+':'+entry.id;
            return <label key={id} className="flex items-start gap-2 rounded p-1 text-xs text-telegram-text">
              <input type="checkbox" className="mt-0.5" checked={selected.includes(id)}
                disabled={!!busy||(!selected.includes(id)&&selected.length>=100)}
                onChange={e=>{
                  setSelected(current=>e.target.checked
                    ?current.includes(id)?current:[...current,id]
                    :current.filter(x=>x!==id));
                  setPreview(null);
                }}/>
              <span className="min-w-0 flex-1 break-all">{entry.name} <span className="text-telegram-subtext">
                ({entry.kind}{entry.size!==null?', '+size(entry.size):''})
              </span></span>
            </label>;
          })}
          {shown.length===0&&<p className="text-xs text-telegram-subtext">No restorable items match this filter.</p>}
          {matching.length>100&&<p className="text-xs text-telegram-subtext">Showing 100 matches; refine your search.</p>}
        </div>
        {planned.error&&<p className="text-xs text-red-400" role="alert">{planned.error}</p>}
        <div className="flex flex-wrap gap-2">
          <button type="button" className={ctl} disabled={!!busy||!request}
            onClick={()=>void check()}>Preview exact restore of {request?.items.length??0} items</button>
          <button type="button" className={ctl} disabled={!!busy||!canApply}
            onClick={()=>void apply()}>
            <ShieldCheck className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
            Restore verified batch
          </button>
        </div>
        {preview&&preview.key===key&&<div className="space-y-1 rounded-md border border-telegram-border p-2 text-xs text-telegram-text"
          role="status">
          <p>{preview.data.ready
            ?preview.data.count+' items validated for exact-location restore.'
            :preview.data.problems.length+' conflict(s) prevent batch recovery.'}</p>
          {preview.data.problems.map((problem,i)=><p key={i} className="text-red-400">{problem}</p>)}
          {preview.data.ready&&<>
            <p>Restorable file data represented: {size(preview.data.restorable_bytes)}.
              No file bytes are transferred during restore.</p>
            <ol className="max-h-48 space-y-1 overflow-auto" aria-label="Original locations for all selected restore items">
              {preview.data.planned.map((item,index)=><li key={index} className="break-all">
                {item.kind} · {item.path}{item.size!==null?' · '+size(item.size):''}
              </li>)}
            </ol>
          </>}
        </div>}
      </>}
      {busy&&<p className="text-xs text-telegram-subtext" role="status">
        {busy==='apply'?'Committing one atomic batch recovery…':busy==='check'
          ?'Checking original names and locations…':'Reading Drive Recently Deleted…'}</p>}
      {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
      {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
    </div>}
  </section>;
}
