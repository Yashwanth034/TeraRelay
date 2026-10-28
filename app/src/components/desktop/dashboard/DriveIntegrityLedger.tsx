import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {type as osType} from '@tauri-apps/plugin-os';
import {ShieldCheck, RefreshCw, Ban} from 'lucide-react';
import {integrityBaselineInfo,integrityInventory,integrityProgress,integritySelection,
  type IntegrityFile,type IntegrityProgress} from '../../../driveIntegrityLedger';
const ctl='rounded-md border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
const size=(b:number)=>b<1024?b+' B':(b/1048576).toFixed(1)+' MiB';

export function DriveIntegrityLedger({active}:{active:boolean}){
  const [expanded,setExpanded]=useState(false),[files,setFiles]=useState<IntegrityFile[]|null>(null);
  const [selected,setSelected]=useState<string[]>([]),[query,setQuery]=useState('');
  const [baseline,setBaseline]=useState<string[]|null>(null),[job,setJob]=useState<IntegrityProgress|null>(null);
  const [busy,setBusy]=useState(false),[error,setError]=useState(''),[message,setMessage]=useState('');
  const mounted=useRef(true),lock=useRef(false);
  const running=job?.phase==='running';
  useEffect(()=>{mounted.current=active&&expanded;return()=>{mounted.current=false;};},[active,expanded]);
  useEffect(()=>{if(!active){setExpanded(false);setFiles(null);setSelected([]);setJob(null);setBaseline(null);setError('');}},[active]);
  useEffect(()=>{
    if(!active||!expanded)return;
    let gone=false;
    const poll=async()=>{
      try{
        const raw=await invoke<unknown>('cmd_drive_integrity_ledger_status');
        if(!gone)setJob(raw===null?null:integrityProgress(raw));
      }catch(e){if(!gone)setError('Integrity status unavailable: '+String(e));}
    };
    void poll();const timer=window.setInterval(()=>void poll(),1800);
    return()=>{gone=true;window.clearInterval(timer);};
  },[active,expanded]);
  const refresh=async()=>{
    if(lock.current||running||!active)return;lock.current=true;setBusy(true);setError('');
    try{
      const [snapshot,stored]=await Promise.all([
        invoke<unknown>('cmd_drive_get_snapshot'),
        invoke<unknown>('cmd_drive_integrity_ledger_info')]);
      if(!mounted.current)return;
      setFiles(integrityInventory(snapshot).files);setBaseline(integrityBaselineInfo(stored));setSelected([]);
    }catch(e){if(mounted.current)setError('Integrity inventory unavailable: '+String(e));}
    finally{lock.current=false;if(mounted.current)setBusy(false);}
  };
  const start=async(operation:'capture'|'verify')=>{
    if(lock.current||running||!active)return;
    if(operation==='capture'){
      try{integritySelection(files??[],selected);}catch(e){setError(String(e));return;}
    }else if(!baseline){setError('Capture a local baseline before verifying.');return;}
    lock.current=true;setBusy(true);setError('');setMessage('');
    try{
      const next=integrityProgress(await invoke<unknown>('cmd_drive_integrity_ledger_start',
        {operation,entryIds:operation==='capture'?selected:[]}));
      if(mounted.current){setJob(next);setMessage(operation==='capture'
        ?'Hashing mounted bytes. Existing baseline remains intact until all files finish.'
        :'Comparing mounted bytes against the previously saved local baseline.');}
    }catch(e){if(mounted.current)setError('Scan outcome uncertain: '+String(e)+'; refresh status before retrying.');}
    finally{lock.current=false;if(mounted.current)setBusy(false);}
  };
  const cancel=async()=>{
    if(lock.current||!running||!job)return;lock.current=true;setBusy(true);setError('');
    try{
      const ok=await invoke<unknown>('cmd_drive_integrity_ledger_cancel',{id:job.id});
      if(mounted.current)setMessage(ok===true?'Cancellation requested. If baseline publication already finished, it may have been saved; refresh status.':'Worker already finished.');
    }catch(e){if(mounted.current)setError('Cannot confirm scan cancellation: '+String(e));}
    finally{lock.current=false;if(mounted.current)setBusy(false);}
  };
  let linux=false;try{linux=osType()==='linux';}catch{/*Linux-only native worker*/}if(!linux)return null;
  const visible=(files??[]).filter(f=>f.path.toLowerCase().includes(query.trim().toLowerCase())).slice(0,100);
  return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive content integrity ledger">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex items-center gap-2"><ShieldCheck className="h-4 w-4 text-telegram-subtext" aria-hidden="true"/>
        <div><p className="text-sm font-medium text-telegram-text">Drive content integrity ledger</p>
          <p className="text-xs text-telegram-subtext">Remember full-file SHA-256 locally, then verify the same mounted files later.</p></div>
      </div>
      <button type="button" className={ctl} aria-expanded={expanded} onClick={()=>setExpanded(!expanded)}>{expanded?'Hide ledger':'Open ledger'}</button>
    </div>
    {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        Linux only. Requires an active mounted and unlocked Drive, plus network access for uncached bytes.
        Baseline hashes are stored <strong>on this device only</strong> in the app database. Replacing your baseline replaces the previous set;
        it does not upload, restore, repair or delete any cloud files. A matching SHA-256 confirms consistency with the prior local scan,
        not the original author's intent or permanent cloud availability. Only completed files can be baselined.
      </p>
      <button type="button" className={ctl} disabled={busy||running} onClick={()=>void refresh()}>
        <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/> Refresh files and saved baseline
      </button>
      {baseline&&<p className="text-xs text-telegram-text">Saved local baseline: {baseline.length} file(s).{' '}
        <button type="button" className={ctl} disabled={busy||running} onClick={()=>void start('verify')}>Verify saved files now</button>
      </p>}
      {files&&<>
        <label className="block text-xs text-telegram-subtext">Filter completed Drive paths
          <input className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-telegram-text"
            value={query} disabled={busy||running} onChange={e=>setQuery(e.target.value)} placeholder="Search filename or folder"/>
        </label>
        <p className="text-xs text-telegram-subtext">{files.length} candidate(s) · {selected.length} selected · maximum 100 files / 8 GiB</p>
        <div className="max-h-44 space-y-1 overflow-auto border border-telegram-border rounded-md p-2" role="group" aria-label="Select integrity baseline files">
          {visible.map(f=><label key={f.id} className="flex items-center gap-2 text-xs text-telegram-text">
            <input type="checkbox" disabled={busy||running||(!selected.includes(f.id)&&selected.length===100)}
              checked={selected.includes(f.id)} onChange={e=>setSelected(v=>e.target.checked?[...v,f.id]:v.filter(x=>x!==f.id))}/>
            <span className="min-w-0 truncate" title={f.path}>{f.path}</span><span className="shrink-0 text-telegram-subtext">{size(f.size)}</span>
          </label>)}
          {visible.length===0&&<p className="text-xs text-telegram-subtext">No matching completed files.</p>}
          {files.length>visible.length&&<p className="text-xs text-telegram-subtext">Only first 100 matches shown; refine filter.</p>}
        </div>
        <button type="button" className={ctl} disabled={busy||running||!selected.length}
          onClick={()=>void start('capture')}>{baseline?'Replace saved baseline with selection':'Capture local SHA-256 baseline'}</button>
      </>}
      {job&&<div className="space-y-2 text-xs text-telegram-text" role="status" aria-live="polite">
        <p>{job.operation==='capture'?'Baseline capture':'Integrity verification'}: {job.phase} · {job.current}/{job.count} files
          {' · '}{size(job.bytes_read)} / {size(job.total_bytes)} read · {job.verified} verified · {job.issues} issue(s)</p>
        <progress className="w-full" max={Math.max(1,job.total_bytes)} value={job.bytes_read}/>
        {job.phase==='running'&&<button type="button" className={ctl} disabled={busy} onClick={()=>void cancel()}>
          <Ban className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/> Cancel integrity scan</button>}
        {job.phase==='done'&&job.operation==='capture'&&<p>New baseline saved locally. Refresh to view the current baseline.</p>}
        {job.results.length>0&&<div className="max-h-44 overflow-y-auto space-y-1" aria-label="Integrity findings">
          {job.results.map((r,index)=><p key={index} className="break-all">
            <strong>{r.outcome}</strong> · {r.path} · {r.explanation}</p>)}
        </div>}
        {job.error&&<p className="text-red-400" role="alert">{job.error}</p>}
      </div>}
      {message&&<p className="text-xs text-telegram-text" role="status">{message}</p>}
      {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
    </div>}
  </section>;
}
