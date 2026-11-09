// Development-only visual QA entry: actual TeraRelay Sidebar, disposable mock data.
// This never authenticates, uploads, downloads, or writes to Telegram.
import React, { useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { toast, Toaster } from 'sonner';
import { Database, FileVideo, Info, Plus, Trash2, FlaskConical, ArrowRightLeft } from 'lucide-react';
import { Sidebar } from '../components/desktop/dashboard/Sidebar';
import { SettingsProvider } from '../context/SettingsContext';
import type { TelegramFolder, FolderGroup } from '../types';
import '../i18n';
import '../App.css';
import '../NeutralDesktop.css';

const mock = globalThis as typeof globalThis & {
  __TAURI_INTERNALS__?: { invoke: (cmd: string) => Promise<unknown> }
};
mock.__TAURI_INTERNALS__ = {
  invoke: async (cmd: string) => {
    if (cmd === 'cmd_telegram_account_tier')
      return { is_premium: true, max_part_size_bytes: 4294967296 };
    if (cmd === 'cmd_get_bandwidth')
      return { up_bytes: 0, down_bytes: 0 };
    throw new Error('Mock-only preview: native backend disabled (' + cmd + ')');
  }
};
type MockFile = { id: number; name: string; size: string };
const initial: TelegramFolder[] = [
  { id: 101, name: 'Main Telegram Storage', role: 'owner', group_id: null, display_order: 1 },
  { id: 102, name: 'Personal Media', role: 'owner', group_id: null, display_order: 2 },
  { id: -201, name: 'Mahesh Babu Filmography', role: 'owner', group_id: null, display_order: 3 },
  { id: -202, name: 'Allu Arjun Filmography', role: 'owner', group_id: null, display_order: 4 },
];
function Preview() {
  const [folders, setFolders] = useState(initial);
  const [backing, setBacking] = useState<Record<number, number>>({[-201]:101, [-202]:101});
  const [items, setItems] = useState<Record<number, MockFile[]>>({
    [-201]: [{id:1,name:'Okkadu (2003).mkv',size:'3.2 GB'}, {id:2,name:'Pokiri (2006).mkv',size:'4.2 GB'}],
    [-202]: [{id:3,name:'Arya (2004).mkv',size:'2.7 GB'}],
    [101]: [{id:4,name:'Independent storage sample.mkv',size:'1.2 GB'}],
    [102]: []
  });
  const [groups, setGroups] = useState<FolderGroup[]>([]);
  const [active, setActive] = useState<number|null>(-201);
  const [selected, setSelected] = useState<number|null>(null);
  const [destination, setDestination] = useState('');
  const nextReal = useRef(103), nextShared = useRef(-203), nextFile = useRef(5);
  const input = useRef<HTMLInputElement>(null);
  const current = folders.find(f=>f.id===active);
  const physicalId = active === null ? null : active > 0 ? active : backing[active];
  const physical = folders.find(f=>f.id===physicalId);
  const files = active===null?[]:items[active] || [];

  const create = async (name:string, storageFolderId?:number) => {
    const trimmed=name.trim();
    if (!trimmed || folders.some(f=>f.name.toLowerCase()===trimmed.toLowerCase())) {
      toast.error('Name missing or already used'); throw new Error('Invalid name');
    }
    if (storageFolderId!==undefined && !folders.some(f=>f.id===storageFolderId && f.id>0 && f.role==='owner')) {
      toast.error('Choose owned dedicated storage'); throw new Error('Invalid backing channel');
    }
    const id=storageFolderId===undefined?nextReal.current++:nextShared.current--;
    setFolders(prev=>[...prev,{id,name:trimmed,role:'owner',group_id:null,display_order:prev.length+1}]);
    if(storageFolderId!==undefined)setBacking(prev=>({...prev,[id]:storageFolderId}));
    setItems(prev=>({...prev,[id]:[]}));
    setActive(id);setSelected(null);
    toast.success(storageFolderId===undefined?'Dedicated channel created (mock)':'Shared channel created (mock)');
  };
  const erase=(id:number,name:string)=>{
    if(id>0 && Object.values(backing).includes(id))return toast.error('This real storage is used by shared channels');
    if((items[id]||[]).length)return toast.error('Remove the sample files before removing this channel');
    if(!confirm('Remove mock channel '+name+'? No Telegram data will be affected.'))return;
    setFolders(prev=>prev.filter(f=>f.id!==id));
    setBacking(prev=>{const next={...prev};delete next[id];return next});
    if(active===id)setActive(null);
    toast.success('Mock channel removed');
  };
  const rename=(id:number,oldName:string)=>{
    const name=prompt('Rename mock channel',oldName)?.trim();
    if(!name || name===oldName)return;
    if(folders.some(f=>f.id!==id && f.name.toLowerCase()===name.toLowerCase()))
      return toast.error('Duplicate channel name');
    setFolders(prev=>prev.map(f=>f.id===id?{...f,name}:f));toast.success('Mock channel renamed');
  };
  const add=(list:FileList|null, folderId=active)=>{
    if(folderId===null || !list?.length)return;
    const adds=Array.from(list).map(f=>({id:nextFile.current++,name:f.name,size:(f.size/1024/1024).toFixed(1)+' MB'}));
    setItems(prev=>({...prev,[folderId]:[...(prev[folderId]||[]),...adds]}));
    toast.message('Sample list updated — no files uploaded');
  };
  const move=()=>{
    if(active===null||selected===null||!destination)return;
    const target=Number(destination);
    const file=(items[active]||[]).find(f=>f.id===selected);
    if(!file || !folders.some(f=>f.id===target) || target===active)return;
    setItems(prev=>({...prev,[active]:(prev[active]||[]).filter(f=>f.id!==selected),[target]:[...(prev[target]||[]),file]}));
    setSelected(null);setDestination('');toast.success('Mock file reassigned — Telegram unchanged');
  };
  return <div data-testid="channel-qa-preview" className="terarelay-shell flex h-screen w-screen gap-2 overflow-hidden">
    <Sidebar
      folders={folders} groups={groups} activeFolderId={active}
      setActiveFolderId={id=>{setActive(id);setSelected(null);setDestination('')}}
      onDrop={(e,id)=>{e.preventDefault();add(e.dataTransfer.files,id)}}
      onDelete={erase} onRename={rename} onToggleVisibility={()=>toast.info('Visibility requires live Telegram')}
      onCreate={create} onJoinChannel={()=>toast.info('No real Telegram connection in this browser preview')}
      isSyncing={false} isConnected={true}
      onSync={()=>toast.info('Mock preview: no Telegram sync')}
      onLogout={()=>toast.info('No Telegram login exists in this preview')}
      onAssignFolderToGroup={async(id,groupId)=>setFolders(prev=>prev.map(f=>f.id===id?{...f,group_id:groupId}:f))}
      onReorderFolders={async next=>setFolders(next)}
      onUpdateGroupOrder={async next=>setGroups(next)}
      onCreateGroup={async(name,color_hex)=>setGroups(prev=>[...prev,{id:Math.max(0,...prev.map(g=>g.id))+1,name,color_hex,display_order:prev.length}])}
      onUpdateGroup={async(id,name,color_hex)=>setGroups(prev=>prev.map(g=>g.id===id?{...g,name,color_hex}:g))}
      onDeleteGroup={async id=>setGroups(prev=>prev.filter(g=>g.id!==id))}
    />
    <main className="tr-main-panel flex-1 min-w-0 flex flex-col overflow-hidden border border-telegram-border">
      <header className="tr-topbar flex items-center flex-wrap justify-between gap-3 px-6 py-4 border-b border-telegram-border">
        <div><div className="font-semibold">{current?.name||'Personal Vault'}</div><div className="text-xs text-telegram-subtext">{files.length} sample file(s)</div></div>
        <span className="flex items-center gap-2 rounded-full text-[11px] font-semibold bg-amber-500/10 text-amber-300 border border-amber-500/30 px-3 py-1.5"><FlaskConical size={15}/> BROWSER QA — NO REAL LOGIN</span>
      </header>
      <div className="flex-1 overflow-auto p-7 space-y-6">
        <div className="p-4 flex gap-3 rounded-xl bg-amber-500/5 border border-amber-500/30 text-sm">
          <Info size={20} className="text-amber-300 shrink-0"/>
          <div><strong>Real TeraRelay channel sidebar, simulated backend.</strong> Test the creation UI and channel organization without a Telegram login, cloud writes, or DEB installation. Refreshing resets the sample data. Native upload/download behavior cannot be verified in Chrome.</div>
        </div>
        {active!==null && <section className="border border-telegram-border bg-telegram-bg/70 p-4 rounded-xl">
          <div className="font-semibold text-sm flex items-center gap-2 mb-3"><Database size={17}/> Channel storage mapping</div>
          <div className="grid grid-cols-2 gap-3 text-xs">
            <div className="rounded-lg p-3 bg-white/5"><div className="text-telegram-subtext mb-1">TeraRelay channel</div><div className="font-semibold">{current?.name}</div><div className="text-telegram-subtext mt-1">View ID: {active}</div></div>
            <div className="rounded-lg p-3 bg-white/5"><div className="text-telegram-subtext mb-1">Underlying Telegram channel (mock)</div><div className="font-semibold">{physical?.name}</div><div className="text-telegram-subtext mt-1">Storage ID: {physicalId}</div></div>
          </div>
          <p className="text-xs text-telegram-subtext mt-3">{active<0?'Shared-storage mode: separate file list with common Telegram storage':'Dedicated Telegram channel mode'}</p>
        </section>}
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-sm font-semibold flex items-center gap-2"><FileVideo size={18}/> Files in this channel <span className="text-xs text-telegram-subtext font-normal">(mock)</span></h2>
          <button disabled={active===null} onClick={()=>input.current?.click()} className="rounded-lg px-3.5 py-2 bg-telegram-primary text-telegram-bg disabled:opacity-40 text-xs font-semibold flex items-center gap-2"><Plus size={15}/> Add mock files</button>
          <input ref={input} type="file" className="hidden" multiple onChange={e=>{add(e.target.files);e.target.value=''}}/>
        </div>
        <div className="space-y-2">
          {files.map(f=><button key={f.id} data-testid={'file-'+f.id} onClick={()=>setSelected(selected===f.id?null:f.id)} className={'w-full text-left flex gap-4 items-center border p-4 rounded-xl transition '+(selected===f.id?'border-telegram-primary bg-white/10':'border-telegram-border bg-white/5 hover:bg-white/10')}>
            <FileVideo size={20} className="text-telegram-subtext"/><span className="flex-1 min-w-0"><span className="block text-sm font-medium truncate">{f.name}</span><span className="text-xs text-telegram-subtext">{f.size}</span></span>
          </button>)}
          {!files.length&&<div className="border border-dashed border-telegram-border rounded-xl p-9 text-center text-telegram-subtext text-sm">This mock channel is empty. Try adding sample files.</div>}
        </div>
        {selected!==null&&<div className="border border-telegram-border rounded-xl p-4 flex flex-wrap gap-3 items-center text-xs">
          <ArrowRightLeft size={16}/><label htmlFor="mock-target">Move sample file to</label>
          <select id="mock-target" aria-label="Destination channel" className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2" value={destination} onChange={e=>setDestination(e.target.value)}>
            <option value="">Choose channel</option>{folders.filter(f=>f.id!==active).map(f=><option key={f.id} value={f.id}>{f.name}</option>)}
          </select>
          <button disabled={!destination} onClick={move} className="rounded-lg px-3 py-2 bg-white/15 disabled:opacity-40">Move</button>
          <button onClick={()=>{if(active!==null){setItems(prev=>({...prev,[active]:(prev[active]||[]).filter(f=>f.id!==selected)}));setSelected(null);toast.message('Mock file removed — Telegram unchanged')}}} className="rounded-lg px-3 py-2 bg-red-500/10 text-red-300 flex items-center gap-1"><Trash2 size={14}/> Remove mock file</button>
        </div>}
        <div className="border border-telegram-border rounded-xl p-4 text-xs text-telegram-subtext">
          <strong className="text-telegram-text">Try this:</strong> Create Channel → <strong>Share existing Telegram storage</strong> → select <strong>Main Telegram Storage</strong> → give it a new name → create. Repeat. Each view will show separately while its storage mapping points to the same physical channel.
        </div>
      </div>
      <footer className="border-t border-telegram-border py-3 px-5 text-xs text-telegram-subtext">Mock fixture · No Telegram session · No real file operations</footer>
    </main>
    <Toaster richColors position="bottom-right"/>
  </div>;
}

createRoot(document.getElementById('root')!).render(
  <React.StrictMode><QueryClientProvider client={new QueryClient({defaultOptions:{queries:{retry:false}}})}>
    <SettingsProvider><Preview/></SettingsProvider>
  </QueryClientProvider></React.StrictMode>
);
