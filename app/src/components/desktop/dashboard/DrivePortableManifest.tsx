import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {open,save} from '@tauri-apps/plugin-dialog';
import {FileJson,GitCompareArrows,Download} from 'lucide-react';
import {
  portableChangeLabels,readPortableExport,readPortableComparison,
  type PortableExportReceipt,type PortableComparison,
} from '../../../drivePortableManifest';

const ctl='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text';
export function DrivePortableManifest({active}:{active:boolean}) {
  const [expanded,setExpanded]=useState(false);
  const [exported,setExported]=useState<PortableExportReceipt|null>(null);
  const [result,setResult]=useState<PortableComparison|null>(null);
  const [category,setCategory]=useState('all');
  const [busy,setBusy]=useState<'export'|'compare'|null>(null);
  const [error,setError]=useState(''),[notice,setNotice]=useState('');
  const generation=useRef(0),running=useRef(false);
  useEffect(()=>{
    if(active)return;
    generation.current++;setExpanded(false);setResult(null);setExported(null);
    setBusy(null);setError('');setNotice('');
  },[active]);
  const exportManifest=async()=>{
    if(!active||running.current)return;
    running.current=true;const turn=++generation.current;
    setBusy('export');setError('');setNotice('');setExported(null);
    try{
      const path=await save({
        title:'Save portable Drive metadata manifest',
        defaultPath:'TeraRelay-Drive-inventory.json',
        filters:[{name:'Drive inventory JSON',extensions:['json']}],
      });
      if(!path||turn!==generation.current)return;
      const raw=await invoke<unknown>('cmd_drive_portable_manifest_export',
        {destinationPath:path});
      const receipt=readPortableExport(raw,path);
      if(turn===generation.current){
        setExported(receipt);
        setNotice('Metadata manifest exported. Keep the JSON file private; it contains filenames and paths. No file contents were backed up.');
      }
    }catch(cause){
      if(turn===generation.current)setError('Export outcome unconfirmed: '+String(cause)+
        '. Inspect the selected destination before trying again; existing files are never overwritten.');
    }finally{running.current=false;if(turn===generation.current)setBusy(null);}
  };
  const compareManifest=async()=>{
    if(!active||running.current)return;
    running.current=true;const turn=++generation.current;
    setBusy('compare');setError('');setNotice('');setResult(null);
    try{
      const path=await open({title:'Choose an earlier TeraRelay Drive metadata manifest',
        multiple:false,directory:false,
        filters:[{name:'Drive inventory JSON',extensions:['json']}]});
      if(typeof path!=='string'||turn!==generation.current)return;
      const raw=await invoke<unknown>('cmd_drive_portable_manifest_compare',{sourcePath:path});
      const comparison=readPortableComparison(raw);
      if(turn===generation.current){setResult(comparison);setCategory('all');}
    }catch(cause){
      if(turn===generation.current)setError('Manifest comparison failed: '+String(cause));
    }finally{running.current=false;if(turn===generation.current)setBusy(null);}
  };
  const changeList=useMemo(()=>result?.comparison.changes.filter(
    row=>category==='all'||category===row.change)??[],[result,category]);
  const comparison=result?.comparison;
  return <section className="tr-setting-row space-y-3 p-3"
    aria-label="Portable Drive recovery inventory">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 items-start gap-2">
        <FileJson className="mt-0.5 h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Portable Drive inventory</h3>
          <p className="text-xs text-telegram-subtext">Export a verified reference and compare it with Drive later.</p>
        </div>
      </div>
      <button type="button" className={ctl} aria-expanded={expanded}
        onClick={()=>setExpanded(value=>!value)}>
        {expanded?'Hide inventory':'Open inventory'}
      </button>
    </div>
    {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        Exports filenames, folder paths, entry identities, sizes and backing availability into
        a private JSON file, with a SHA-256 integrity checksum. This is metadata only:
        <strong> not a backup of file contents</strong>, and it cannot restore missing files.
        File identities are meaningful only within the same underlying Drive namespace;
        another Drive or independently re-created account may not share them. Pending edits
        or local unsynced state may not exist in the cloud. Never share exported manifests publicly.
      </p>
      <div className="flex flex-wrap gap-2">
        <button type="button" className={ctl} disabled={busy!==null||!active}
          onClick={()=>void exportManifest()}>
          <Download className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>Export JSON manifest
        </button>
        <button type="button" className={ctl} disabled={busy!==null||!active}
          onClick={()=>void compareManifest()}>
          <GitCompareArrows className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
          Compare saved manifest
        </button>
      </div>
      {exported&&<div className="space-y-1 text-xs text-telegram-text" role="status">
        <p>Exported {exported.item_count} entries at revision {exported.revision}
          {exported.dirty?' (unsynced locally)':''}.</p>
        <p className="break-all text-telegram-subtext">File: {exported.destination_path}</p>
        <p className="break-all text-telegram-subtext">Inventory SHA-256: {exported.checksum}</p>
      </div>}
      {comparison&&<div className="space-y-3 text-xs text-telegram-text" role="status">
        <p>Verified earlier manifest ({comparison.before.item_count} items) against
          current Drive inventory ({comparison.after.item_count} items).</p>
        <p>{comparison.created} new · {comparison.removed} no longer present ·
          {' '}{comparison.moved} moved · {comparison.renamed} renamed ·
          {' '}{comparison.ancestor_path_changed} ancestor paths changed ·
          {' '}{comparison.availability_changed} availability changes ·
          {' '}{comparison.backing_state_changed} backing changes ·
          {' '}{comparison.content_metadata_changed} metadata changes</p>
        {(comparison.before.dirty||comparison.after.dirty)&&<p className="text-telegram-subtext">
          One inventory includes unsynced local changes. A missing identity does not prove
          cloud deletion; inspect unavailable backing and device context before acting.
        </p>}
        <label className="block space-y-1 text-telegram-subtext">Filter inventory changes
          <select className={field} value={category}
            onChange={e=>setCategory(e.target.value)}>
            <option value="all">All categories</option>
            {Object.entries(portableChangeLabels).map(([key,value])=>
              <option key={key} value={key}>{value}</option>)}
          </select>
        </label>
        {comparison.changes.length===0&&<p>No identity or metadata changes detected.</p>}
        <div className="max-h-64 space-y-2 overflow-auto" aria-label="Portable inventory differences">
          {changeList.slice(0,200).map((entry,index)=>
            <div key={index} className="space-y-1 rounded-md border border-telegram-border p-2">
              <p className="font-medium">{portableChangeLabels[entry.change]} · {entry.kind}</p>
              {entry.old_path&&<p className="break-all text-telegram-subtext">Before: {entry.old_path}</p>}
              {entry.new_path&&<p className="break-all">Now: {entry.new_path}</p>}
              {entry.previous_backing!==entry.current_backing&&
                <p className="text-telegram-subtext">Backing: {entry.previous_backing??'none'} →
                  {' '}{entry.current_backing??'none'}</p>}
            </div>)}
          {changeList.length>200&&<p className="text-telegram-subtext">
            Showing first 200 of {changeList.length} filtered results. Use category filters to review more.
          </p>}
        </div>
      </div>}
      {busy&&<p className="text-xs text-telegram-subtext" role="status">
        {busy==='export'?'Verifying and exporting local metadata…':'Reading and comparing verified metadata…'}
      </p>}
      {notice&&<p className="text-xs text-telegram-text" role="status">{notice}</p>}
      {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
    </div>}
  </section>;
}
