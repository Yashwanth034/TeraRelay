import {useEffect,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {save} from '@tauri-apps/plugin-dialog';
import {type as osType} from '@tauri-apps/plugin-os';
import {Archive, RefreshCw, Ban, Download, FolderArchive} from 'lucide-react';
import {parseFolderInventory, type FolderItem} from '../../../driveFolderOperations';
import {selectFolderForVerifiedZip, type FolderZipSelection} from '../../../driveFolderZipSelection';
import {parseBulkExportInventory,parseBulkExportProgress,selectedTotalBytes,
  type BulkExportFile,type BulkExportProgress} from '../../../driveBulkExport';

const ctl='rounded-md border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
const fmt=(b:number)=>b<1024?b+' B':(b/1048576).toFixed(1)+' MiB';

/** A Linux mounted-Drive file-content export, never a cloud mutation. */
export function DriveBulkExport({active}:{active:boolean}){
  const [expanded,setExpanded]=useState(false);
  const [files,setFiles]=useState<BulkExportFile[]|null>(null);
  const [skipped,setSkipped]=useState(0);
  const [selected,setSelected]=useState<string[]>([]);
  const [search,setSearch]=useState('');
  const [folders,setFolders]=useState<FolderItem[]>([]);
  const [folderId,setFolderId]=useState('');
  const [folderBundle,setFolderBundle]=useState<FolderZipSelection|null>(null);
  const [loadedSnapshot,setLoadedSnapshot]=useState<unknown>(null);
  const [job,setJob]=useState<BulkExportProgress|null>(null);
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState('');
  const [notice,setNotice]=useState('');
  const generation=useRef(0);
  const operation=useRef(false);
  const jobId=useRef<string|null>(null);
  const live=job?.phase==='running';

  useEffect(()=>{
    if(active)return;
    generation.current++;
    setExpanded(false);setFiles(null);setSelected([]);setFolders([]);
    setFolderId('');setFolderBundle(null);setLoadedSnapshot(null);setError('');setNotice('');
    setBusy(false);setJob(null);
  },[active]);
  useEffect(()=>{
    if(!active || !expanded)return;
    let disposed=false;
    const update=async()=>{
      try{
        const raw=await invoke<unknown>('cmd_drive_bulk_export_status');
        if(disposed)return;
        if(raw===null){setJob(null);return;}
        const next=parseBulkExportProgress(raw);
        jobId.current=next.id;setJob(next);
      }catch(cause){if(!disposed)setError('Cannot inspect bulk export: '+String(cause));}
    };
    void update();
    const tick=window.setInterval(()=>void update(),1600);
    return()=>{disposed=true;window.clearInterval(tick);};
  },[active,expanded]);
  const load=async()=>{
    if(!active||operation.current)return;
    operation.current=true;const rev=++generation.current;setBusy(true);setError('');
    try{
      const raw=await invoke<unknown>('cmd_drive_get_snapshot');
      const data=parseBulkExportInventory(raw);
      const folderList=parseFolderInventory(raw);
      if(rev===generation.current){
        setFiles(data.files);setSkipped(data.skipped);setSelected([]);
        setLoadedSnapshot(raw);setFolders(folderList.folders);
        setFolderId('');setFolderBundle(null);
      }
    }catch(cause){if(rev===generation.current)setError('Could not read completed files: '+String(cause));}
    finally{operation.current=false;if(rev===generation.current)setBusy(false);}
  };
  const chooseFolder=()=>{
    if(!files||!folderId||!loadedSnapshot||operation.current||live)return;
    setError('');setNotice('');setFolderBundle(null);
    try{
      const bundle=selectFolderForVerifiedZip(loadedSnapshot,files,folderId);
      setSelected(bundle.entryIds);setFolderBundle(bundle);
      setNotice(bundle.entryIds.length+' completed file(s) selected recursively from '+bundle.folderPath+'. Preview the list before exporting.');
    }catch(cause){setSelected([]);setError('Cannot select folder ZIP: '+String(cause));}
  };
  const start=async()=>{
    if(operation.current||!active||!files||(!selected.length&&!folderBundle)||live)return;
    try{if(selected.length)selectedTotalBytes(files,selected);}catch(cause){setError(String(cause));return;}
    operation.current=true;const rev=++generation.current;setBusy(true);setError('');setNotice('');
    try{
      const path=await save({title:'Save verified Drive files as ZIP',defaultPath:'TeraRelay-Drive-files.zip',
        filters:[{name:'ZIP archive',extensions:['zip']}]});
      if(!path||rev!==generation.current)return;
      if(folderBundle){
        const latest=await invoke<unknown>('cmd_drive_get_snapshot');
        const latestFiles=parseBulkExportInventory(latest).files;
        const latestBundle=selectFolderForVerifiedZip(latest,latestFiles,folderBundle.folderId);
        if(latestBundle.revision!==folderBundle.revision
          ||latestBundle.totalBytes!==folderBundle.totalBytes
          ||latestBundle.entryIds.length!==selected.length
          ||latestBundle.entryIds.some((id,i)=>id!==selected[i])){
          throw Error('Folder changed since selection; refresh and select its files again.');
        }
      }
      const next=parseBulkExportProgress(await invoke<unknown>('cmd_drive_bulk_export_start',
        {entryIds:selected,destinationPath:path,
          folderId:folderBundle?.folderId??null,
          folderRevision:folderBundle?.revision??null}));
      if(rev===generation.current){setJob(next);jobId.current=next.id;setNotice('Native Drive export started. Keep TeraRelay open until complete.');}
    }catch(cause){if(rev===generation.current)setError('Export not confirmed: '+String(cause)+'. Refresh status before retrying.');}
    finally{operation.current=false;if(rev===generation.current)setBusy(false);}
  };
  const cancel=async()=>{
    if(!jobId.current||!live||operation.current)return;
    operation.current=true;setBusy(true);setError('');
    try{
      const cancelled=await invoke<unknown>('cmd_drive_bulk_export_cancel',{exportId:jobId.current});
      if(cancelled===true)setNotice('Cancellation requested; the native worker will remove its incomplete ZIP.');
      else setNotice('Export already completed or stopped. Refresh status.');
    }catch(cause){setError('Cancellation outcome uncertain: '+String(cause));}
    finally{operation.current=false;setBusy(false);}
  };
  let supported=false;try{supported=osType()==='linux';}catch{/* Linux-only native path. */}
  if(!supported)return null;
  const visible=(files??[]).filter(file=>file.path.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase())).slice(0,100);
  return <section className="tr-setting-row space-y-3 p-3" aria-label="Drive verified bulk ZIP export">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex items-center gap-2">
        <Archive className="h-4 w-4 text-telegram-subtext" aria-hidden="true"/>
        <div><p className="text-sm font-medium text-telegram-text">Drive verified bulk ZIP export</p>
          <p className="text-xs text-telegram-subtext">Export multiple completed Drive files into one local ZIP through the native mount.</p></div>
      </div>
      <button type="button" className={ctl} aria-expanded={expanded} onClick={()=>setExpanded(!expanded)}>
        {expanded?'Hide exporter':'Open exporter'}
      </button>
    </div>
    {expanded&&<div className="space-y-3 border-t border-telegram-border pt-3">
      <p className="text-xs text-telegram-subtext">
        ZIP files contain <strong>plaintext</strong>, not encrypted Drive blocks. Choose a private destination outside the Drive mount.
        Files require an active Linux mount, unlocked Drive keys where applicable, and network access if not already cached.
        Only complete files are included; pending replacements are rejected. The mounted Drive reader performs its normal block integrity checks. Every ZIP member is SHA-256 checked by reading the completed local archive back before publication.
        This tool does not upload, delete or change cloud files. It does not prove native behavior on Windows or macOS.
      </p>
      <button type="button" className={ctl} disabled={busy||live||!active} onClick={()=>void load()}>
        <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/> Refresh completed files
      </button>
      {files&&<>
        <div className="space-y-2 rounded-lg border border-telegram-border p-2">
          <label className="block text-xs text-telegram-subtext">Select completed files from a folder and all its descendants
            <select className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-telegram-text"
              value={folderId} disabled={busy||live} onChange={e=>{
                setFolderId(e.target.value);setFolderBundle(null);setSelected([]);
                setNotice('');setError('');
              }}>
              <option value="">Choose Drive folder</option>
              {folders.map(folder=><option key={folder.id} value={folder.id}>{folder.path}</option>)}
            </select>
          </label>
          <button className={ctl} type="button" disabled={busy||live||!folderId}
            onClick={chooseFolder}><FolderArchive className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/>
            Select all completed files recursively</button>
          {folderBundle&&<p className="text-xs text-telegram-text" role="status">
            Selected {folderBundle.entryIds.length} files across {folderBundle.foldersCovered} folder(s),
            {' '}{fmt(folderBundle.totalBytes)} of logical content.
            {folderBundle.preservedEmptyFolders>0
              ?' '+folderBundle.preservedEmptyFolders+' empty folder(s) will be preserved in ZIP.':''}
          </p>}
          <p className="text-xs text-telegram-subtext">This selects visible completed contents, not an unverified whole-Drive backup.
            Unfinished uploads and links block recursive selection. Empty directories are archived
            with their original folder paths, even if the entire tree is empty. The folder structure
            and file selection are rechecked before the native ZIP archive is published.</p>
        </div>
        <label className="block text-xs text-telegram-subtext">Filter Drive paths
          <input className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-telegram-text"
            value={search} onChange={e=>setSearch(e.target.value)} disabled={busy||live} placeholder="Search folders or filenames"/>
        </label>
        <p className="text-xs text-telegram-subtext">{files.length} completed file(s) · {selected.length} selected
          {skipped>0?' · '+skipped+' unresolvable records excluded':''}</p>
        <div className="max-h-48 space-y-1 overflow-auto border border-telegram-border rounded-md p-2" role="group" aria-label="Select completed files to export">
          {visible.map(file=><label key={file.id} className="flex items-center gap-2 text-xs text-telegram-text">
            <input type="checkbox" checked={selected.includes(file.id)} disabled={busy||live||(!selected.includes(file.id)&&selected.length>=100)}
              onChange={event=>{setFolderBundle(null);setSelected(old=>event.target.checked?[...old,file.id]:old.filter(id=>id!==file.id));}}/>
            <span className="min-w-0 truncate" title={file.path}>{file.path}</span>
            <span className="shrink-0 text-telegram-subtext">{fmt(file.size)}</span>
          </label>)}
          {visible.length===0&&<p className="text-xs text-telegram-subtext">No matching completed files.</p>}
          {files.length>visible.length&&<p className="text-xs text-telegram-subtext">Showing first 100 matching items. Narrow your filter to find more.</p>}
        </div>
        <button type="button" className={ctl} disabled={busy||live||(!selected.length&&!folderBundle)} onClick={()=>void start()}>
          <Download className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/> Export selected ZIP
        </button>
      </>}
      {job&&<div className="space-y-2 text-xs text-telegram-text" role="status" aria-live="polite">
        <p>{job.phase==='done'?'Export completed':job.phase==='failed'?'Export failed':job.phase==='cancelled'?'Export cancelled':'Native export running'}
          {' · '}{job.current}/{job.count} files, {fmt(job.bytes_written)} / {fmt(job.total_bytes)} transferred
        </p>
        <progress className="w-full" value={job.bytes_written} max={Math.max(1,job.total_bytes)}/>
        <p className="text-telegram-subtext">{job.files_verified} ZIP member(s) re-read and SHA-256 verified.</p>
        {job.phase==='running'&&<button type="button" className={ctl} disabled={busy} onClick={()=>void cancel()}>
          <Ban className="mr-1 inline h-3.5 w-3.5" aria-hidden="true"/> Cancel export
        </button>}
        {job.error&&<p role="alert" className="text-red-400">{job.error}</p>}
      </div>}
      {notice&&<p role="status" className="text-xs text-telegram-text">{notice}</p>}
      {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
    </div>}
  </section>;
}
