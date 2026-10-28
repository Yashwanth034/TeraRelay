import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {History,RefreshCw,GitCompareArrows} from 'lucide-react';
import {
  readCapsuleInfo,readCapsuleHistory,readCapsuleComparison,snapshotCaption,
  type CapsuleInfo,type CapsuleComparison,type CapsuleChangeKind,
} from '../../../driveTimeCapsule';

const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const fields='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
const kindLabels:Record<CapsuleChangeKind,string>={
  created:'Created',removed:'No longer present',moved:'Moved',renamed:'Renamed',
  ancestor_path_changed:'Parent folder path changed',
  availability_changed:'Backing availability changed',
  backing_state_changed:'Backing or storage state changed',
  content_metadata_changed:'Content metadata changed',
};
export function DriveTimeCapsule({active}:{active:boolean}){
  const [expanded,setExpanded]=useState(false);
  const [history,setHistory]=useState<CapsuleInfo[]|null>(null);
  const [earlier,setEarlier]=useState<number|null>(null);
  const [later,setLater]=useState<number|null>(null);
  const [comparison,setComparison]=useState<CapsuleComparison|null>(null);
  const [filter,setFilter]=useState<'all'|CapsuleChangeKind>('all');
  const [busy,setBusy]=useState<'list'|'capture'|'compare'|null>(null);
  const [error,setError]=useState(''),[note,setNote]=useState('');
  const locked=useRef(false),epoch=useRef(0);
  useEffect(()=>{
    if(active)return;
    epoch.current++;setExpanded(false);setHistory(null);setComparison(null);setBusy(null);
  },[active]);
  const load=async()=>{
    if(!active||locked.current)return;
    locked.current=true;const turn=++epoch.current;
    setBusy('list');setError('');setNote('');setComparison(null);
    try{
      const rows=readCapsuleHistory(await invoke<unknown>('cmd_drive_time_capsule_list'));
      if(epoch.current!==turn)return;
      setHistory(rows);setLater(rows[0]?.seq??null);setEarlier(rows[1]?.seq??null);
    }catch(cause){if(epoch.current===turn)setError('Could not read local Drive time capsules: '+String(cause));}
    finally{locked.current=false;if(epoch.current===turn)setBusy(null);}
  };
  const capture=async()=>{
    if(!active||locked.current)return;
    locked.current=true;const turn=++epoch.current;
    setBusy('capture');setError('');setNote('');setComparison(null);
    try{
      let recorded:CapsuleInfo;
      try{
        recorded=readCapsuleInfo(await invoke<unknown>('cmd_drive_time_capsule_capture'));
      }catch(cause){
        if(epoch.current===turn)setError('Snapshot result uncertain: '+String(cause)
          +'. Refresh history to check for a saved capture before attempting again.');
        return;
      }
      const rows=readCapsuleHistory(await invoke<unknown>('cmd_drive_time_capsule_list'));
      if(epoch.current!==turn)return;
      setHistory(rows);setLater(recorded.seq);
      setEarlier(rows.find(r=>r.seq<recorded.seq)?.seq??null);
      setNote('Captured '+recorded.item_count+' metadata entries locally at Drive revision '
        +recorded.revision+'. This does not back up file bytes.');
    }catch(cause){
      if(epoch.current===turn)setError('Capture saved, but refreshing history failed: '+String(cause)
        +'. Refresh history before another capture.');
    }finally{locked.current=false;if(epoch.current===turn)setBusy(null);}
  };
  const compare=async()=>{
    if(!active||locked.current||earlier===null||later===null||earlier>=later)return;
    locked.current=true;const turn=++epoch.current;
    setBusy('compare');setError('');setComparison(null);setNote('');
    try{
      const result=readCapsuleComparison(await invoke<unknown>(
        'cmd_drive_time_capsule_compare',{beforeSeq:earlier,afterSeq:later}),earlier,later);
      if(epoch.current===turn)setComparison(result);
    }catch(cause){
      if(epoch.current===turn)setError('Drive history comparison failed: '+String(cause));
    }finally{locked.current=false;if(epoch.current===turn)setBusy(null);}
  };
  const changes=useMemo(()=>comparison?.changes.filter(x=>filter==='all'||x.change===filter)??[],
    [comparison,filter]);
  return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive Time Capsule historical snapshots">
    <div className="flex flex-wrap items-center justify-between gap-2">
      <div className="flex min-w-0 items-start gap-2">
        <History className="h-4 w-4 mt-0.5 shrink-0 text-telegram-subtext" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Time Capsule</h3>
          <p className="text-xs text-telegram-subtext">See what changed between two saved Drive inventories.</p>
        </div>
      </div>
      <button type="button" className={ctl} aria-expanded={expanded}
        onClick={()=>setExpanded(s=>!s)}>{expanded?'Hide history':'Open history'}</button>
    </div>
    {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        Captures full local Drive <strong>metadata</strong> only, including file identities,
        availability, folder paths, symlinks and pending edits. No cloud or file-content writes.
        Up to 12 snapshots are stored in the app database; oldest snapshots are automatically
        replaced when the limit is reached. These are not file backups, version restoration,
        or proof that remote contents are available. Unsynced local revisions are clearly labeled.
      </p>
      <div className="flex flex-wrap gap-2">
        <button type="button" className={ctl} onClick={()=>void load()} disabled={busy!==null||!active}>
          <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Refresh history
        </button>
        <button type="button" className={ctl} onClick={()=>void capture()} disabled={busy!==null||!active}>
          <History className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Capture Drive inventory now
        </button>
      </div>
      {history&&<div className="space-y-2 text-xs text-telegram-text">
        <p>{history.length} of 12 retained local captures</p>
        {history.length===0&&<p className="text-telegram-subtext">Capture an inventory to start local change history.</p>}
        {history.length>0&&<div className="space-y-2">
          <label className="block space-y-1 text-telegram-subtext">Earlier baseline
            <select className={fields} value={earlier??''} disabled={busy!==null}
              onChange={e=>{setEarlier(e.target.value?Number(e.target.value):null);setComparison(null);}}>
              <option value="">Select earlier snapshot</option>
              {history.map(i=><option key={i.seq} value={i.seq}>{snapshotCaption(i)}</option>)}
            </select>
          </label>
          <label className="block space-y-1 text-telegram-subtext">Later snapshot
            <select className={fields} value={later??''} disabled={busy!==null}
              onChange={e=>{setLater(e.target.value?Number(e.target.value):null);setComparison(null);}}>
              <option value="">Select later snapshot</option>
              {history.map(i=><option key={i.seq} value={i.seq}>{snapshotCaption(i)}</option>)}
            </select>
          </label>
          <button type="button" className={ctl} disabled={busy!==null||earlier===null||later===null||earlier>=later}
            onClick={()=>void compare()}>
            <GitCompareArrows className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
            Compare Drive changes
          </button>
        </div>}
      </div>}
      {comparison&&<div className="space-y-2 text-xs text-telegram-text" role="status" aria-live="polite">
        <p>
          {comparison.created} created · {comparison.removed} no longer present ·
          {' '}{comparison.moved} moved · {comparison.renamed} renamed ·
          {' '}{comparison.ancestor_path_changed} changed by ancestor rename/move ·
          {' '}{comparison.availability_changed} availability changed ·
          {' '}{comparison.backing_state_changed} storage/state changed ·
          {' '}{comparison.content_metadata_changed} content metadata changed
        </p>
        {(comparison.before.dirty||comparison.after.dirty)&&<p className="text-telegram-subtext">
          At least one snapshot includes unsynced local Drive metadata; comparison does not
          establish that those changes reached the cloud.
        </p>}
        <label className="block space-y-1 text-telegram-subtext">Filter changes
          <select className={fields} value={filter}
            onChange={e=>setFilter(e.target.value as typeof filter)}>
            <option value="all">All changes</option>
            {(Object.keys(kindLabels) as CapsuleChangeKind[]).map(k=>
              <option key={k} value={k}>{kindLabels[k]}</option>)}
          </select>
        </label>
        {comparison.changes.length===0&&<p>No metadata changes detected between these captures.</p>}
        <div className="max-h-64 space-y-2 overflow-auto" aria-label="Historical Drive changes">
          {changes.slice(0,200).map((change,i)=><div key={i}
            className="space-y-1 rounded-md border border-telegram-border p-2">
            <p className="font-medium">{kindLabels[change.change]} · {change.kind}</p>
            {change.old_path&&<p className="break-all text-telegram-subtext">Earlier: {change.old_path}</p>}
            {change.new_path&&<p className="break-all">Later: {change.new_path}</p>}
            {change.previous_backing!==change.current_backing&&<p className="text-telegram-subtext">
              Backing: {change.previous_backing??'none'} → {change.current_backing??'none'}</p>}
          </div>)}
          {changes.length>200&&<p className="text-telegram-subtext">
            Showing first 200 of {changes.length} filtered changes. Refine the category to review others.
          </p>}
        </div>
      </div>}
      {busy&&<p role="status" className="text-xs text-telegram-subtext">
        {busy==='capture'?'Recording private local metadata…':
          busy==='compare'?'Comparing stored snapshots…':'Reading snapshot history…'}
      </p>}
      {note&&<p role="status" className="text-xs text-telegram-text">{note}</p>}
      {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
    </div>}
  </section>;
}
