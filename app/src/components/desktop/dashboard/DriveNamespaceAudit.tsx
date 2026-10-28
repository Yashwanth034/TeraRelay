import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {ShieldAlert,RefreshCw} from 'lucide-react';
import {readNamespaceAudit,summarizeNamespaceAudit,
 type NamespaceAuditReport} from '../../../driveNamespaceAudit';

const control='rounded-md border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
export function DriveNamespaceAudit({active}:{active:boolean}){
  const [open,setOpen]=useState(false);
  const [report,setReport]=useState<NamespaceAuditReport|null>(null);
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const [filter,setFilter]=useState<'all'|'critical'|'warning'>('all');
  const run=useRef(false),epoch=useRef(0);
  useEffect(()=>{
    if(active)return;
    epoch.current++;setOpen(false);setReport(null);setError('');setBusy(false);
  },[active]);
  const counts=useMemo(()=>report?summarizeNamespaceAudit(report):null,[report]);
  const inspect=async()=>{
    if(!active||run.current)return;
    run.current=true;const current=++epoch.current;
    setBusy(true);setError('');setReport(null);
    try{
      const r=readNamespaceAudit(await invoke<unknown>('cmd_drive_namespace_audit'));
      if(epoch.current===current)setReport(r);
    }catch(cause){
      if(epoch.current===current)setError('Drive namespace scan unavailable: '+String(cause));
    }finally{
      run.current=false;if(epoch.current===current)setBusy(false);
    }
  };
  const listed=(report?.issues??[]).filter(issue=>filter==='all'||issue.severity===filter);
  return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive namespace integrity audit">
    <div className="flex flex-wrap items-center justify-between gap-2">
      <div className="flex min-w-0 items-start gap-2">
        <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Namespace Integrity Audit</h3>
          <p className="text-xs text-telegram-subtext">
            Find invisible files, broken folders and conflicting names before backup or bulk operations.
          </p>
        </div>
      </div>
      <button type="button" className={control} aria-expanded={open}
        onClick={()=>setOpen(x=>!x)}>{open?'Hide audit':'Open audit'}</button>
    </div>
    {open&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        Device-local read-only metadata scan. Checks completed and unavailable file records,
        directory ancestry, pending edits, links and portable name collisions.
        It does not download file bytes, connect to Telegram, repair or remove anything.
        A clean report is not a guarantee of remote content integrity; use the content ledger to verify actual file bytes.
      </p>
      <button type="button" className={control} onClick={()=>void inspect()} disabled={busy||!active}>
        <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
        {busy?'Inspecting Drive metadata…':'Run namespace audit'}
      </button>
      {report&&counts&&<div className="space-y-2 text-xs" role="status" aria-live="polite">
        <p className="text-telegram-text">
          {report.raw_files} raw active file rows · {report.visible_files} visible file rows ·
          {' '}{report.folders} folders · {report.pending} unfinished edits · {report.links} links
        </p>
        <p className="text-telegram-text">
          {counts.critical} critical issue(s) · {counts.warnings} warning(s)
          {counts.limited?' · Scan/report limit reached; not complete.':''}
        </p>
        {counts.readyToTrust&&<p>No namespace inconsistencies detected in this bounded metadata scan.</p>}
        <label className="block space-y-1 text-telegram-subtext">
          Show findings
          <select className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text"
            value={filter} onChange={e=>setFilter(e.target.value as typeof filter)}>
            <option value="all">All findings</option>
            <option value="critical">Critical only</option>
            <option value="warning">Warnings only</option>
          </select>
        </label>
        <div className="max-h-60 overflow-auto space-y-2" aria-label="Read-only Drive namespace findings">
          {listed.map((issue,index)=><div key={index}
            className="rounded-md border border-telegram-border p-2 text-telegram-text">
            <p className="font-semibold">{issue.severity==='critical'?'Critical':'Warning'} · {issue.code.replace(/_/g,' ')}</p>
            <p className="break-all">{issue.kind}: {issue.name}</p>
            <p className="text-telegram-subtext">{issue.explanation}</p>
          </div>)}
          {listed.length===0&&!counts.readyToTrust&&<p className="text-telegram-subtext">
            No findings in this filter.</p>}
        </div>
        {report.truncated&&<p className="text-telegram-subtext">
          At least one inventory or issue limit was reached. This report is incomplete; no repair was attempted.
        </p>}
      </div>}
      {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
    </div>}
  </section>;
}
