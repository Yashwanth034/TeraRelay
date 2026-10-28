import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { FolderSearch, RefreshCw, Save, Trash2, Search } from 'lucide-react';
import {
  EMPTY_SMART_RULE, parseSmartRule, parseSmartPreview, parseSmartViews,
  type SmartView, type SmartPreview, type SmartRule,
} from '../../../driveSmartViews';

const button='rounded-lg border border-telegram-border px-2.5 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
const field='w-full rounded-lg border border-telegram-border bg-telegram-bg px-2.5 py-2 text-xs text-telegram-text';
const bytes=(v:number)=>v<1024?v+' B':(v/1024/1024).toFixed(1)+' MiB';
const displayError=(e:unknown)=>String(e).slice(0,500);

export function DriveSmartViews({active}:{active:boolean}){
  const [open,setOpen]=useState(false);
  const [views,setViews]=useState<SmartView[]>([]);
  const [selected,setSelected]=useState<string|null>(null);
  const [name,setName]=useState('');
  const [rule,setRule]=useState<SmartRule>({...EMPTY_SMART_RULE});
  const [preview,setPreview]=useState<SmartPreview|null>(null);
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState<string|null>(null);
  const [message,setMessage]=useState<string|null>(null);
  const [confirmDelete,setConfirmDelete]=useState(false);
  const running=useRef(false);
  const generation=useRef(0);
  const chosen=views.find(v=>v.id===selected);

  useEffect(()=>{
    if(active)return;
    generation.current++;
    setOpen(false);setViews([]);setSelected(null);setPreview(null);
    setMessage(null);setError(null);setConfirmDelete(false);
  },[active]);

  const reload=async()=>{
    if(!active||running.current)return;
    running.current=true;
    const current=++generation.current;
    setBusy(true);setError(null);
    try{
      const value=parseSmartViews(await invoke<unknown>('cmd_drive_smart_views_list'));
      if(current===generation.current){
        setViews(value);setPreview(null);
        // Never silently reuse stale revisions across reloads.
        setSelected(null);setName('');setRule({...EMPTY_SMART_RULE});
        setConfirmDelete(false);
      }
    }catch(e){if(current===generation.current)setError('Cannot load saved Smart Views: '+displayError(e));}
    finally{running.current=false;if(current===generation.current)setBusy(false);}
  };
  const pick=(item:SmartView)=>{
    setSelected(item.id);setName(item.name);setRule({...item.rule});
    setPreview(null);setConfirmDelete(false);setError(null);setMessage(null);
  };
  const clear=()=>{
    setSelected(null);setName('');setRule({...EMPTY_SMART_RULE});
    setPreview(null);setConfirmDelete(false);setError(null);setMessage(null);
  };
  const apply=async(kind:'save'|'delete'|'preview')=>{
    if(running.current||!active)return;
    running.current=true;const current=++generation.current;
    setBusy(true);setError(null);setMessage(null);
    try{
      if(kind==='preview'){
        if(!chosen)throw Error('Save or select a Smart View first.');
        const data=parseSmartPreview(await invoke<unknown>('cmd_drive_smart_views_preview',{id:chosen.id}));
        if(current===generation.current){
          if(data.revision!==chosen.revision)throw Error('Saved view revision changed; refresh the list.');
          setPreview(data);
        }
      }else if(kind==='save'){
        const id=chosen?.id??null,revision=chosen?.revision??null;
        const checkedRule = parseSmartRule(rule);
        await invoke('cmd_drive_smart_views_save',{id,name,revision,rule:checkedRule});
        const value=parseSmartViews(await invoke<unknown>('cmd_drive_smart_views_list'));
        if(current===generation.current){
          setViews(value);setSelected(null);setName('');setRule({...EMPTY_SMART_RULE});
          setPreview(null);setConfirmDelete(false);
          setMessage('Smart View saved locally. Its results update when you run it again.');
        }
      }else{
        if(!chosen||!confirmDelete)throw Error('Select a saved view and confirm its removal.');
        await invoke('cmd_drive_smart_views_delete',{id:chosen.id,revision:chosen.revision});
        const value=parseSmartViews(await invoke<unknown>('cmd_drive_smart_views_list'));
        if(current===generation.current){
          setViews(value);clear();
          setMessage('Saved view removed. No Drive files were deleted.');
        }
      }
    }catch(e){
      if(current===generation.current)setError(
        'Smart View outcome uncertain: '+displayError(e)+
        '. Refresh before repeating a change; Drive files are not modified by these commands.');
    }finally{running.current=false;if(current===generation.current)setBusy(false);}
  };
  const edit=<K extends keyof SmartRule>(key:K,value:SmartRule[K])=>
    setRule(prev=>({...prev,[key]:value}));
  return(
    <section aria-label="Drive Smart Views" className="tr-setting-row space-y-3 p-3">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-start gap-2">
          <FolderSearch className="mt-0.5 h-4 w-4 text-telegram-subtext" aria-hidden="true"/>
          <div>
            <p className="text-sm font-medium text-telegram-text">Drive Smart Views</p>
            <p className="text-xs text-telegram-subtext">Save live filename, type, size and date searches across Drive.</p>
          </div>
        </div>
        <button type="button" className={button} disabled={busy} aria-expanded={open} onClick={()=>{
          if(open){generation.current++;setOpen(false);setPreview(null);}
          else{setOpen(true);void reload();}
        }}>{open?'Hide Smart Views':'Manage Smart Views'}</button>
      </div>
      {open&&<div className="space-y-3 border-t border-telegram-border pt-3">
        <p className="text-xs text-telegram-subtext">
          Smart Views are saved on this device only, not synchronized with Telegram.
          They dynamically search visible file metadata and never move, delete or upload Drive files.
        </p>
        <div className="flex flex-wrap gap-2">
          <button type="button" className={button} disabled={busy} onClick={()=>void reload()}>
            <RefreshCw className="mr-1 inline h-3 w-3"/>Refresh saved views
          </button>
          <button type="button" className={button} disabled={busy} onClick={clear}>New view</button>
        </div>
        {views.length>0&&<div className="flex flex-wrap gap-2" aria-label="Saved Smart Views">
          {views.map(v=><button type="button" key={v.id} className={button}
            aria-pressed={selected===v.id} disabled={busy} onClick={()=>pick(v)}>
            {v.name}
          </button>)}
        </div>}
        {views.length===0&&<p className="text-xs text-telegram-subtext">No saved Smart Views yet.</p>}
        <div className="space-y-2 rounded-lg border border-telegram-border p-3">
          <p className="text-xs font-medium text-telegram-text">{chosen?'Edit saved view':'Create a Smart View'}</p>
          <label className="block space-y-1 text-xs text-telegram-subtext">View name
            <input className={field} value={name} maxLength={100} disabled={busy}
              onChange={e=>setName(e.target.value)} placeholder="Recent PDF reports"/>
          </label>
          <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
            <label className="block space-y-1 text-xs text-telegram-subtext">File name contains
              <input className={field} value={rule.name_contains??''} disabled={busy}
                onChange={e=>edit('name_contains',e.target.value||null)} maxLength={100} placeholder="report"/>
            </label>
            <label className="block space-y-1 text-xs text-telegram-subtext">Extension (without dot)
              <input className={field} value={rule.extension??''} disabled={busy}
                onChange={e=>edit('extension',e.target.value.toLowerCase()||null)} maxLength={16} placeholder="pdf"/>
            </label>
            <label className="block space-y-1 text-xs text-telegram-subtext">File type
              <select className={field} value={rule.category??''} disabled={busy}
                onChange={e=>edit('category',e.target.value as SmartRule['category']||null)}>
                <option value="">Any type</option>
                {['image','video','audio','document','archive','other'].map(t=><option value={t} key={t}>{t}</option>)}
              </select>
            </label>
            <label className="block space-y-1 text-xs text-telegram-subtext">Minimum size (MiB)
              <input type="number" className={field} min={0} step="0.1" disabled={busy}
                value={rule.min_size===null?'':rule.min_size/1048576}
                onChange={e=>edit('min_size',e.target.value===''?null:Math.round(Number(e.target.value)*1048576))}/>
            </label>
            <label className="block space-y-1 text-xs text-telegram-subtext">Maximum size (MiB)
              <input type="number" className={field} min={0} step="0.1" disabled={busy}
                value={rule.max_size===null?'':rule.max_size/1048576}
                onChange={e=>edit('max_size',e.target.value===''?null:Math.round(Number(e.target.value)*1048576))}/>
            </label>
            <label className="block space-y-1 text-xs text-telegram-subtext">Changed in the last
              <select className={field} disabled={busy}
                value={rule.updated_within_days===null?(rule.updated_after===null?'':'fixed'):String(rule.updated_within_days)}
                onChange={e=>setRule(previous=>({
                  ...previous,updated_after:null,
                  updated_within_days:e.target.value===''?null:Number(e.target.value),
                }))}>
                <option value="">Any date</option>
                <option value="1">1 day</option><option value="7">7 days</option>
                <option value="30">30 days</option><option value="90">90 days</option>
                {rule.updated_after!==null&&<option value="fixed" disabled>Saved fixed cutoff</option>}
              </select>
            </label>
          </div>
          <label className="block space-y-1 text-xs text-telegram-subtext">
            Limit to a folder and descendants (optional folder ID)
            <input className={field} disabled={busy} value={rule.folder_id??''}
              onChange={e=>edit('folder_id',e.target.value||null)} placeholder="Leave blank for all Drive"/>
          </label>
          <div className="flex flex-wrap gap-2">
            <button type="button" className={button} disabled={busy||!name.trim()} onClick={()=>void apply('save')}>
              <Save className="mr-1 inline h-3 w-3"/> {chosen?'Save updated rules':'Save new view'}
            </button>
            {chosen&&<button type="button" className={button} disabled={busy} onClick={()=>void apply('preview')}>
              <Search className="mr-1 inline h-3 w-3"/> Run live search
            </button>}
            {chosen&&<button type="button" className={button} disabled={busy}
              onClick={()=>{if(!confirmDelete)setConfirmDelete(true);else void apply('delete');}}>
              <Trash2 className="mr-1 inline h-3 w-3"/>{confirmDelete?'Confirm remove view':'Remove saved view'}
            </button>}
          </div>
          {confirmDelete&&<p className="text-xs text-telegram-subtext">
            This only deletes the saved search; it never deletes Drive files.
          </p>}
        </div>
        {preview&&<div className="space-y-2">
          <p role="status" className="text-xs text-telegram-subtext">
            {preview.total_matches} live matching file(s) · Drive revision {preview.snapshot_revision}
            {preview.truncated?' · first 200 displayed':''}
          </p>
          <ul aria-label="Smart View results" className="max-h-72 space-y-1 overflow-y-auto">
            {preview.matches.map(item=><li key={item.entry_id} className="rounded border border-telegram-border p-2 text-xs">
              <p className="break-all font-medium text-telegram-text">{item.path}</p>
              <p className="text-telegram-subtext">{item.category} · {bytes(item.size)}</p>
            </li>)}
          </ul>
        </div>}
        {message&&<p role="status" className="text-xs text-telegram-text">{message}</p>}
        {error&&<p role="alert" className="text-xs text-red-400">{error}</p>}
      </div>}
    </section>
  );
}
