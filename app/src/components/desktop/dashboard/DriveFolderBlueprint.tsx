import {useEffect,useMemo,useRef,useState} from 'react';
import {invoke} from '@tauri-apps/api/core';
import {FolderPlus,RefreshCw,Loader2} from 'lucide-react';
import {validateDriveSyncResult} from '../../../deviceContinuity';
import {
  blueprintInventory,buildBlueprint,readBlueprintPreview,readBlueprintApplied,
  type BlueprintInventory,type BlueprintPreview,
} from '../../../driveFolderBlueprint';

const button='rounded-md border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field='rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';

export function DriveFolderBlueprint({active}:{active:boolean}){
  const [expanded,setExpanded]=useState(false);
  const [inventory,setInventory]=useState<BlueprintInventory|null>(null);
  const [parent,setParent]=useState<string|null>(null);
  const [input,setInput]=useState('Project\nProject/Documents\nProject/Documents/Reports\nProject/Source\nProject/Tests');
  const [preview,setPreview]=useState<{key:string;data:BlueprintPreview}|null>(null);
  const [busy,setBusy]=useState<'load'|'preview'|'apply'|null>(null);
  const [error,setError]=useState(''),[message,setMessage]=useState('');
  const gate=useRef(false),generation=useRef(0);
  useEffect(()=>{
    if(active)return;
    setExpanded(false);setPreview(null);
    if(busy!=='apply'){generation.current++;setInventory(null);setBusy(null);}
  },[active,busy]);
  const planned=useMemo(()=>{
    if(!inventory)return {request:null,error:''};
    try{return {request:buildBlueprint(inventory,parent,input),error:''};}
    catch(cause){return {request:null,error:String(cause)};}
  },[inventory,parent,input]);
  const request=planned.request;
  const key=request?JSON.stringify(request):'';
  const ready=!!request&&preview?.key===key&&preview.data.ready;
  const edit=()=>{setPreview(null);setError('');setMessage('');};
  const load=async()=>{
    if(gate.current||!active)return;
    gate.current=true;const epoch=++generation.current;
    setBusy('load');setPreview(null);setError('');setMessage('');
    try{
      const data=blueprintInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
      if(generation.current===epoch){setInventory(data);setParent(null);}
    }catch(cause){
      if(generation.current===epoch)setError('Could not read local Drive folders: '+String(cause));
    }finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
  };
  const check=async()=>{
    if(gate.current||!active)return;
    if(!request){setError(planned.error||'Refresh Drive and enter a valid blueprint.');return;}
    gate.current=true;const epoch=++generation.current;
    setBusy('preview');setPreview(null);setError('');setMessage('');
    try{
      const data=readBlueprintPreview(
        await invoke<unknown>('cmd_drive_folder_blueprint_preview',{request}),request);
      if(generation.current===epoch)setPreview({key,data});
    }catch(cause){
      if(generation.current===epoch)setError('Folder blueprint preview failed: '+String(cause));
    }finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
  };
  const apply=async()=>{
    if(gate.current||!active||!request||!ready)return;
    gate.current=true;const epoch=++generation.current;
    setBusy('apply');setPreview(null);setError('');setMessage('');
    try{
      let created:number;
      try{
        const result=readBlueprintApplied(
          await invoke<unknown>('cmd_drive_folder_blueprint_apply',{request}),request);
        created=result.created;
      }catch(cause){
        if(generation.current===epoch)setError(
          'Folder creation outcome is uncertain: '+String(cause)+
          '. Refresh the Drive inventory before retrying. No automatic retries.');
        return;
      }
      if(generation.current!==epoch)return;
      setInventory(null);
      setMessage(created+' new folders committed locally. Checking cloud metadata sync…');
      let sync='Cloud metadata sync not independently confirmed; check again when online.';
      try{
        const result=validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if(!result.dirty&&result.source!=='qa-local')sync='Drive metadata sync returned a clean result.';
      }catch{/* Local transaction remains committed if cloud synchronization fails. */}
      if(generation.current===epoch)setMessage(created+' new folders committed locally. '+sync);
    }finally{gate.current=false;if(generation.current===epoch)setBusy(null);}
  };
  return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive folder blueprint importer">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex items-start gap-2 min-w-0">
        <FolderPlus className="w-4 h-4 mt-0.5 text-telegram-subtext shrink-0" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Folder Blueprint</h3>
          <p className="text-xs text-telegram-subtext">
            Create an entire new nested folder hierarchy from a pasted outline. No existing files are moved or copied.
          </p>
        </div>
      </div>
      <button className={button} type="button" aria-expanded={expanded}
        onClick={()=>setExpanded(v=>!v)}>{expanded?'Close importer':'Open importer'}</button>
    </div>
    {expanded&&<div className="space-y-3">
      <button className={button} type="button" disabled={!!busy||!active}
        onClick={()=>void load()}><RefreshCw className="mr-1 inline w-3.5 h-3.5" aria-hidden="true"/>Refresh Drive destinations</button>
      {inventory&&<>
        <label className="block text-xs text-telegram-subtext space-y-1">
          Create these folders under
          <select className={field+' w-full'} value={parent??'__root__'} disabled={!!busy}
            onChange={e=>{setParent(e.target.value==='__root__'?null:e.target.value);edit();}}>
            <option value="__root__">Drive root</option>
            {inventory.folders.map(folder=><option key={folder.id} value={folder.id}>{folder.path}</option>)}
          </select>
        </label>
        <label className="block text-xs text-telegram-subtext space-y-1">
          One relative folder path per line
          <textarea className={field+' w-full font-mono'} value={input} rows={8}
            spellCheck={false} disabled={!!busy} aria-label="Nested folder blueprint paths"
            onChange={e=>{setInput(e.target.value);edit();}}
            placeholder={'Project\nProject/Docs\nProject/Docs/Reports\nProject/Tests'}/>
        </label>
        <p className="text-xs text-telegram-subtext">
          List each parent explicitly before or after its children. A slash creates a level.
          Maximum 512 folders, 64 levels. No existing parent names can be reused inside the new hierarchy.
          Preview checks collisions against files, folders and unfinished writes before any change.
        </p>
        {planned.error&&<p className="text-xs text-telegram-text" role="alert">{planned.error}</p>}
        <div className="flex flex-wrap gap-2">
          <button className={button} type="button" disabled={!!busy||!request}
            onClick={()=>void check()}>Preview {request?.paths.length??0} new folders</button>
          <button className={button} type="button" disabled={!!busy||!ready}
            onClick={()=>void apply()}>Create verified hierarchy</button>
        </div>
        {preview&&<div className="rounded-md border border-telegram-border p-2 space-y-1 text-xs text-telegram-text"
          role="status">
          <p>{preview.data.ready
            ?preview.data.count+' new folders validated against current Drive revision.'
            :'Blueprint blocked by '+preview.data.problems.length+' issue(s).'}</p>
          {preview.data.problems.map((problem,i)=><p key={i}>{problem}</p>)}
          {preview.data.ready&&<div className="max-h-40 overflow-y-auto">
            {preview.data.paths.map((path,i)=><p key={i} className="break-all">{path}</p>)}
            {preview.data.truncated&&<p>Showing first 80 folder paths only.</p>}
          </div>}
        </div>}
      </>}
      {busy&&<p className="text-xs text-telegram-subtext" role="status">
        <Loader2 className="mr-1 inline w-3.5 h-3.5 animate-spin" aria-hidden="true"/>
        {busy==='apply'?'Creating all folders atomically…':
         busy==='preview'?'Checking native folder blueprint…':'Reading Drive directories…'}</p>}
      {error&&<p className="text-xs text-red-400" role="alert">{error}</p>}
      {message&&<p className="text-xs text-telegram-text" role="status">{message}</p>}
    </div>}
  </section>;
}
