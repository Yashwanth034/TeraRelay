import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { FolderPlus, Loader2, RefreshCw } from 'lucide-react';
import { validateDriveSyncResult } from '../../../deviceContinuity';
import {
  buildCloneRequest, decodeCloneApplied, decodeClonePreview, readCloneInventory,
  type CloneInventory, type FolderClonePreview, type FolderCloneRequest,
} from '../../../driveFolderClone';

const control = 'rounded-lg border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field = 'w-full rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
export function DriveFolderClone({ active }: { active: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const [inventory, setInventory] = useState<CloneInventory | null>(null);
  const [sourceId, setSourceId] = useState('');
  const [destinationId, setDestinationId] = useState<string | null>(null);
  const [newName, setNewName] = useState('');
  const [search, setSearch] = useState('');
  const [preview, setPreview] = useState<{ key: string; data: FolderClonePreview } | null>(null);
  const [busy, setBusy] = useState<'load' | 'preview' | 'apply' | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const inFlight = useRef(false);
  const generation = useRef(0);

  useEffect(() => {
    if (active) return;
    setExpanded(false);
    setPreview(null);
    // Keep the mutation acknowledgement intact after Settings closes.
    if (busy !== 'apply') {
      generation.current++;
      setInventory(null);
      setSourceId('');
      setBusy(null);
    }
  }, [active, busy]);

  const source = inventory?.folders.find(folder => folder.id === sourceId);
  const possible = useMemo(() => {
    const needle = search.trim().toLocaleLowerCase();
    return (inventory?.folders ?? []).filter(folder =>
      !needle || folder.path.toLocaleLowerCase().includes(needle));
  }, [inventory, search]);
  const request = useMemo<FolderCloneRequest | null>(() => {
    if (!inventory || !sourceId) return null;
    try { return buildCloneRequest(inventory, sourceId, destinationId, newName); }
    catch { return null; }
  }, [inventory, sourceId, destinationId, newName]);
  const key = request ? JSON.stringify(request) : '';
  const canApply = !!request && preview?.key === key && preview.data.ready;

  const invalidate = () => { setPreview(null); setError(''); setNotice(''); };
  const refresh = async () => {
    if (!active || inFlight.current) return;
    inFlight.current = true;
    const ticket = ++generation.current;
    setBusy('load'); setError(''); setPreview(null);
    try {
      const fresh = readCloneInventory(await invoke<unknown>('cmd_drive_get_snapshot'));
      if (ticket === generation.current) {
        setInventory(fresh);
        setSourceId('');
        setDestinationId(null);
        setNewName('');
      }
    } catch (cause) {
      if (ticket === generation.current) setError('Cannot load the current folder hierarchy: ' + String(cause));
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };
  const check = async () => {
    if (!active || inFlight.current) return;
    if (!request) {
      setError('Select a source folder, destination, and valid name for its new empty copy.');
      return;
    }
    inFlight.current = true;
    const ticket = ++generation.current;
    setBusy('preview'); setError(''); setPreview(null); setNotice('');
    try {
      const result = decodeClonePreview(
        await invoke<unknown>('cmd_drive_folder_clone_preview', { request }), request);
      if (ticket === generation.current) setPreview({ key, data: result });
    } catch (cause) {
      if (ticket === generation.current) setError('Cannot preview empty folder copy: ' + String(cause));
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };
  const apply = async () => {
    if (!active || inFlight.current || !request || !canApply || !preview) return;
    inFlight.current = true;
    const ticket = ++generation.current;
    const checkedPreview = preview.data;
    setBusy('apply'); setError(''); setPreview(null); setNotice('');
    try {
      let copied: number;
      try {
        const result = decodeCloneApplied(
          await invoke<unknown>('cmd_drive_folder_clone_apply', { request }),
          request, checkedPreview);
        copied = result.created;
      } catch (cause) {
        if (ticket === generation.current) setError(
          'Copy result is uncertain: ' + String(cause)
          + '. Refresh the folder inventory and inspect the destination before retrying.');
        return;
      }
      if (ticket !== generation.current) return;
      setInventory(null);
      setSourceId('');
      setNotice(copied + ' empty folders created locally. Checking Drive metadata sync…');
      let syncMessage = 'Cloud metadata synchronization remains unverified.';
      try {
        const sync = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if (!sync.dirty && sync.source !== 'qa-local')
          syncMessage = 'Drive metadata sync returned a clean result.';
      } catch { /* Do not misrepresent a committed local copy as rolled back. */ }
      if (ticket === generation.current) setNotice(
        copied + ' empty folders created locally. Original directories and all files were untouched. ' + syncMessage);
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };

  return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive folder structure cloner">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex items-start gap-2">
        <FolderPlus className="h-4 w-4 mt-0.5 text-telegram-subtext shrink-0" aria-hidden="true" />
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Drive Folder Structure Cloner</h3>
          <p className="text-xs text-telegram-subtext">Reuse a project folder layout without moving the original folders or copying file contents.</p>
        </div>
      </div>
      <button className={control} type="button" aria-expanded={expanded}
        onClick={() => setExpanded(value => !value)}>
        {expanded ? 'Close cloner' : 'Open folder cloner'}
      </button>
    </div>
    {expanded && <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <button className={control} type="button" disabled={!active || !!busy}
          onClick={() => void refresh()}>
          <RefreshCw className="w-3.5 h-3.5 inline mr-1" aria-hidden="true" /> Refresh folder index
        </button>
        {inventory && <span className="text-xs text-telegram-subtext">
          {inventory.folders.length} available folders · {inventory.skipped} invalid/hidden rows excluded
        </span>}
      </div>
      {inventory && <>
        <label className="block text-xs text-telegram-subtext space-y-1">
          Search source folder paths
          <input className={field} disabled={!!busy} value={search} placeholder="Search folder paths"
            onChange={e => setSearch(e.target.value)} />
        </label>
        <label className="block text-xs text-telegram-subtext space-y-1">
          Existing source folder (structure only)
          <select className={field} disabled={!!busy} value={sourceId}
            onChange={e => {
              const id = e.target.value;
              const folder = inventory.folders.find(row => row.id === id);
              setSourceId(id);
              setNewName(folder ? folder.name + ' copy' : '');
              invalidate();
            }}>
            <option value="">Choose a source folder…</option>
            {possible.slice(0, 150).map(folder =>
              <option key={folder.id} value={folder.id}>{folder.path}</option>)}
          </select>
        </label>
        {possible.length > 150 &&
          <p className="text-xs text-telegram-subtext">Showing the first 150 matching folders. Narrow your search for more.</p>}
        <label className="block text-xs text-telegram-subtext space-y-1">
          Destination of the new copy
          <select className={field} disabled={!!busy} value={destinationId ?? '__root__'}
            onChange={e => { setDestinationId(e.target.value === '__root__' ? null : e.target.value); invalidate(); }}>
            <option value="__root__">Drive root</option>
            {inventory.folders.map(folder =>
              <option key={folder.id} value={folder.id}>{folder.path}</option>)}
          </select>
        </label>
        <label className="block text-xs text-telegram-subtext space-y-1">
          New top folder name
          <input className={field} disabled={!!busy} value={newName}
            onChange={e => { setNewName(e.target.value); invalidate(); }}
            placeholder="Example: Client project 2027" />
        </label>
        {source && <p className="text-xs text-telegram-subtext">
          Source: {source.path}. A new, empty directory tree will be created; files, uploads, permissions and data are not copied.
          Maximum 512 folders and 64 levels. The destination cannot be inside the source tree.
        </p>}
        <div className="flex flex-wrap gap-2">
          <button className={control} type="button" disabled={!!busy || !sourceId}
            onClick={() => void check()}>Preview folder structure</button>
          <button className={control} type="button" disabled={!!busy || !canApply}
            onClick={() => void apply()}>Create verified empty tree</button>
        </div>
        {preview && <div className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text space-y-1" role="status">
          <p>{preview.data.ready
            ? 'Ready: ' + preview.data.count + ' new empty folders.'
            : 'Cannot create this folder tree:'}</p>
          {preview.data.problems.map((p, i) => <p key={i}>{p}</p>)}
          {preview.data.ready && <p className="font-medium">
            Not copied: {preview.data.not_copied_files} completed file(s) and {preview.data.not_copied_pending} unfinished upload(s). This duplicates directories only.
          </p>}
          {preview.data.ready && <ul className="max-h-44 overflow-auto space-y-1">
            {preview.data.paths.map((path, i) => <li key={i}>{path}</li>)}
          </ul>}
          {preview.data.truncated &&
            <p>Only the first 80 new paths are listed; all {preview.data.count} were validated.</p>}
        </div>}
      </>}
      {busy && <p className="text-xs text-telegram-subtext" role="status">
        <Loader2 className="h-3.5 w-3.5 animate-spin inline mr-1" aria-hidden="true"/>
        {busy === 'apply' ? 'Creating the folder tree…' :
          busy === 'preview' ? 'Validating all folder paths…' : 'Loading folder hierarchy…'}</p>}
      {error && <p role="alert" className="rounded-lg border border-red-500/40 p-2 text-xs text-telegram-text">{error}</p>}
      {notice && <p role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
    </div>}
  </section>;
}
