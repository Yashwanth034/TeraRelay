import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { FolderTree, Loader2, RefreshCw } from 'lucide-react';
import { validateDriveSyncResult } from '../../../deviceContinuity';
import {
  buildFolderPlan, buildFolderCsvPlan, parseFolderApplied, parseFolderInventory, parseFolderPreview,
  type FolderInventory, type FolderOptions, type FolderPlan, type FolderPreview,
} from '../../../driveFolderOperations';

const button = 'rounded-lg border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field = 'rounded-lg border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
const defaults: FolderOptions = { find: '', replace: '', prefix: '', suffix: '', destinationId: 'same' };

export function DriveFolderOperations({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [inventory, setInventory] = useState<FolderInventory | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [options, setOptions] = useState<FolderOptions>(defaults);
  const [search, setSearch] = useState('');
  const [csvMode, setCsvMode] = useState(false);
  const [csvText, setCsvText] = useState('source_path,new_path\n');
  const [verified, setVerified] = useState<{ key: string; preview: FolderPreview } | null>(null);
  const [busy, setBusy] = useState<'load' | 'preview' | 'apply' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const operation = useRef(false);
  const generation = useRef(0);

  useEffect(() => {
    if (active) return;
    setOpen(false);
    setVerified(null);
    if (busy !== 'apply') {
      generation.current++;
      setInventory(null);
      setSelected([]);
      setBusy(null);
    }
  }, [active, busy]);

  const matching = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    return inventory?.folders.filter(folder =>
      !query || folder.path.toLocaleLowerCase().includes(query)) ?? [];
  }, [inventory, search]);
  const shown = matching.slice(0, 80);
  const planning = useMemo(() => {
    if (!inventory) return { plan: null as FolderPlan | null, error: '' };
    try {
      const plan = csvMode
        ? buildFolderCsvPlan(inventory, csvText)
        : selected.length ? buildFolderPlan(inventory, selected, options) : null;
      return { plan, error: '' };
    } catch (cause) {
      return { plan: null, error: String(cause) };
    }
  }, [inventory, selected, options, csvMode, csvText]);
  const plan = planning.plan;
  const planKey = plan ? JSON.stringify(plan) : '';
  const canApply = !!plan && verified?.key === planKey && verified.preview.ready;

  const changeOptions = (patch: Partial<FolderOptions>) => {
    setOptions(previous => ({ ...previous, ...patch }));
    setVerified(null);
    setNotice(null);
  };
  const load = async () => {
    if (operation.current || !active) return;
    operation.current = true;
    const epoch = ++generation.current;
    setBusy('load'); setError(null); setVerified(null);
    try {
      const data = parseFolderInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
      if (generation.current === epoch) {
        setInventory(data);
        setSelected([]);
      }
    } catch (cause) {
      if (generation.current === epoch) setError('Could not read local Drive folders: ' + String(cause));
    } finally {
      operation.current = false;
      if (generation.current === epoch) setBusy(null);
    }
  };
  const check = async () => {
    if (operation.current || !active) return;
    if (!plan) {
      setError(planning.error || 'Select 1–100 folders or supply a valid CSV mapping.');
      return;
    }
    operation.current = true;
    const epoch = ++generation.current;
    setBusy('preview'); setVerified(null); setError(null); setNotice(null);
    try {
      const reply = parseFolderPreview(await invoke<unknown>('cmd_drive_folder_preview', { plan }), plan);
      if (generation.current === epoch) setVerified({ key: planKey, preview: reply });
    } catch (cause) {
      if (generation.current === epoch) setError('Folder preview failed: ' + String(cause));
    } finally {
      operation.current = false;
      if (generation.current === epoch) setBusy(null);
    }
  };
  const apply = async () => {
    if (operation.current || !active || !plan || !canApply) return;
    operation.current = true;
    const epoch = ++generation.current;
    setBusy('apply'); setVerified(null); setError(null); setNotice(null);
    try {
      let count: number;
      try {
        const outcome = parseFolderApplied(
          await invoke<unknown>('cmd_drive_folder_apply', { plan }), plan);
        count = outcome.changed;
      } catch (cause) {
        // A dropped IPC response may mean the native transaction DID commit.
        // Repeating it automatically would be unsafe: force a fresh inventory.
        if (generation.current === epoch) setError(
          'Folder apply outcome is uncertain: ' + String(cause)
          + '. Refresh folders and inspect before trying again.');
        return;
      }
      if (generation.current !== epoch) return;
      setSelected([]);
      setInventory(null);
      setNotice(count + ' folder subtree operation(s) committed locally. Checking cloud metadata sync…');
      let syncMessage = 'Cloud synchronization is not confirmed; retry sync when available.';
      try {
        const sync = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if (!sync.dirty && sync.source !== 'qa-local')
          syncMessage = 'Drive metadata sync returned a clean result.';
      } catch {
        // Local mutation remains successful when Telegram sync is unavailable.
      }
      if (generation.current === epoch)
        setNotice(count + ' folder subtree operation(s) committed locally. ' + syncMessage);
    } finally {
      operation.current = false;
      if (generation.current === epoch) setBusy(null);
    }
  };
  const selectFolder = (id: string, checked: boolean) => {
    if (operation.current) return;
    setSelected(current => checked
      ? current.includes(id) ? current : current.length >= 100 ? current : [...current, id]
      : current.filter(item => item !== id));
    setVerified(null);
    setNotice(null);
  };
  return (
    <section className="tr-setting-row p-3 space-y-3" aria-label="Drive folder operations">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-start gap-2 min-w-0">
          <FolderTree className="w-4 h-4 text-telegram-subtext mt-0.5 shrink-0" aria-hidden="true" />
          <div>
            <h3 className="text-sm font-medium text-telegram-text">Drive Folder Operations</h3>
            <p className="text-xs text-telegram-subtext">Reorganize entire folder trees in one safe, atomic operation. No file contents are downloaded.</p>
          </div>
        </div>
        <button className={button} type="button" aria-expanded={open}
          onClick={() => setOpen(value => !value)}>{open ? 'Close planner' : 'Open folder planner'}</button>
      </div>
      {open && <div className="space-y-3">
        <div className="flex items-center gap-2">
          <button className={button} type="button" disabled={!!busy || !active}
            onClick={load}><RefreshCw className="w-3.5 h-3.5 inline mr-1" aria-hidden="true" />Refresh folders</button>
          {inventory && <span className="text-xs text-telegram-subtext">{inventory.folders.length} folders · {csvMode ? (plan?.changes.length ?? 0) + ' CSV mappings' : selected.length + ' selected'}
            {inventory.skipped > 0 ? ' · ' + inventory.skipped + ' unavailable rows omitted' : ''}</span>}
        </div>
        {inventory && <>
          <label className="flex items-center gap-2 text-xs text-telegram-text">
            <input type="checkbox" checked={csvMode} disabled={!!busy} onChange={e => {
              setCsvMode(e.target.checked); setVerified(null); setError(null); setNotice(null);
            }}/>
            Use exact folder-path CSV mappings instead of find/replace
          </label>
          {csvMode ? <div className="space-y-2">
            <label className="block text-xs text-telegram-subtext space-y-1">
              CSV mappings: source_path,new_path (1–100 rows; destination parents must already exist)
              <textarea className={field + ' w-full font-mono'} rows={8}
                spellCheck={false} value={csvText} disabled={!!busy}
                aria-label="Folder move and rename CSV mappings"
                onChange={e => { setCsvText(e.target.value); setVerified(null); setError(null); setNotice(null); }}
                placeholder={'source_path,new_path\\n/Alpha,/Projects/Alpha\\n/Reports/Old,/Reports/New'}/>
            </label>
            <p className="text-xs text-telegram-subtext">
              Enter full paths starting with /. Quoted commas and escaped quotes are supported.
              These mappings use the same native preview, atomic transaction and Folder Batch Undo.
              CSV never creates missing parent folders or automatically applies changes.
            </p>
            {planning.error && <p role="alert" className="text-xs text-telegram-text">{planning.error}</p>}
          </div> : <>
          <label className="flex flex-col gap-1 text-xs text-telegram-subtext">
            Find folders
            <input className={field} value={search} onChange={e => setSearch(e.target.value)}
              placeholder="Search folder paths" disabled={!!busy}/>
          </label>
          <div className="max-h-48 overflow-auto rounded-lg border border-telegram-border p-2 space-y-1"
            role="group" aria-label="Select folders for a batch">
            {shown.map(folder =>
              <label key={folder.id} className="flex items-center gap-2 rounded-md px-1 py-1 text-xs text-telegram-text hover:bg-telegram-hover">
                <input type="checkbox" disabled={!!busy || (!selected.includes(folder.id) && selected.length >= 100)}
                  checked={selected.includes(folder.id)} onChange={e => selectFolder(folder.id, e.target.checked)}/>
                <span className="truncate" title={folder.path}>{folder.path}</span>
              </label>)}
            {matching.length === 0 && <p className="text-xs text-telegram-subtext">No matching folders.</p>}
            {matching.length > shown.length &&
              <p className="text-xs text-telegram-subtext">Showing the first 80 matches. Narrow your search for more.</p>}
          </div>
          <div className="grid grid-cols-2 gap-2">
            <label className="text-xs text-telegram-subtext space-y-1">Find name text
              <input className={field + ' w-full'} value={options.find} disabled={!!busy}
                onChange={e => changeOptions({ find: e.target.value })} placeholder="Optional exact text"/>
            </label>
            <label className="text-xs text-telegram-subtext space-y-1">Replace with
              <input className={field + ' w-full'} value={options.replace} disabled={!!busy}
                onChange={e => changeOptions({ replace: e.target.value })}/>
            </label>
            <label className="text-xs text-telegram-subtext space-y-1">Prefix
              <input className={field + ' w-full'} value={options.prefix} disabled={!!busy}
                onChange={e => changeOptions({ prefix: e.target.value })}/>
            </label>
            <label className="text-xs text-telegram-subtext space-y-1">Suffix
              <input className={field + ' w-full'} value={options.suffix} disabled={!!busy}
                onChange={e => changeOptions({ suffix: e.target.value })}/>
            </label>
          </div>
          <label className="block text-xs text-telegram-subtext space-y-1">
            Destination for all selected folders
            <select className={field + ' w-full'} value={options.destinationId ?? '__root__'}
              disabled={!!busy} onChange={e => changeOptions({
                destinationId: e.target.value === '__root__' ? null : e.target.value,
              })}>
              <option value="same">Keep each current parent</option>
              <option value="__root__">Drive root</option>
              {inventory.folders.map(folder =>
                <option value={folder.id} key={folder.id}>{folder.path}</option>)}
            </select>
          </label>
          <p className="text-xs text-telegram-subtext">Moving a folder moves all its descendants. Nested selections, ancestor destinations, occupied names and name swaps are rejected. Use a new preview after every edit.</p>
          </>}
          <div className="flex flex-wrap gap-2">
            <button className={button} type="button" disabled={!!busy || !plan}
              onClick={check}>Preview {plan?.changes.length ?? 0} folder changes</button>
            <button className={button} type="button" disabled={!!busy || !canApply}
              onClick={apply}>Apply verified folder changes</button>
          </div>
          {verified && <div role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text space-y-1">
            <p>{verified.preview.ready
              ? 'Ready: ' + verified.preview.count + ' folder operation(s), validated against the current Drive revision.'
              : 'Cannot apply: ' + verified.preview.problems.length + ' validation issue(s).'}</p>
            {verified.preview.problems.map((problem, index) => <p key={index}>{problem}</p>)}
            {verified.preview.ready && plan &&
              <ul className="max-h-44 overflow-auto space-y-1">
                {plan.changes.map(change => {
                  const source = inventory.folders.find(folder => folder.id === change.folder_id);
                  const target = change.new_parent_id === null ? '' : inventory.folders.find(folder => folder.id === change.new_parent_id)?.path ?? '[missing]';
                  return <li key={change.folder_id}>{source?.path} → {target + '/' + change.new_name}</li>;
                })}
              </ul>}
          </div>}
        </>}
        {busy && <p className="text-xs text-telegram-subtext" role="status">
          <Loader2 className="w-3.5 h-3.5 inline animate-spin mr-1" aria-hidden="true"/>
          {busy === 'apply' ? 'Applying the folder transaction…' : busy === 'preview' ? 'Checking all folder changes…' : 'Loading Drive folders…'}</p>}
        {error && <p className="text-xs text-telegram-text rounded-lg border border-red-500/40 p-2" role="alert">{error}</p>}
        {notice && <p className="text-xs text-telegram-text rounded-lg border border-telegram-border p-2" role="status">{notice}</p>}
      </div>}
    </section>
  );
}
