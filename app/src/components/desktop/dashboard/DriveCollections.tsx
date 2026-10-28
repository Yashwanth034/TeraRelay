import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Bookmark, Copy, Loader2, RefreshCw, ShieldCheck } from 'lucide-react';
import {
  collectionInventory, parseCollections, resolveCollection, buildCollectionSave,
  collectionMembersCsv, type CollectionInventory, type SavedCollection,
} from '../../../driveCollections';

const ctl = 'rounded-lg bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field = 'rounded-lg border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
const MAX_VISIBLE = 80;

export function DriveCollections({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [inventory, setInventory] = useState<CollectionInventory | null>(null);
  const [collections, setCollections] = useState<SavedCollection[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draftName, setDraftName] = useState('');
  const [draftMembers, setDraftMembers] = useState<string[]>([]);
  const [search, setSearch] = useState('');
  const [busy, setBusy] = useState<'load' | 'save' | 'delete' | null>(null);
  const [copying, setCopying] = useState(false);
  const [deleteArmed, setDeleteArmed] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const generation = useRef(0);
  const operationBusy = useRef(false);

  useEffect(() => {
    if (active) return;
    // Settings may close while the native SQLite operation is still in flight.
    // Keep single-flight ownership and its outcome rather than allowing a new
    // Create request whose first IPC result may have been lost.
    if (operationBusy.current) {
      setOpen(false);
      return;
    }
    generation.current++;
    setOpen(false);
    setInventory(null);
    setCollections([]);
    setBusy(null);
    setDeleteArmed(false);
    setError(null);
    setNotice(null);
  }, [active]);

  const selected = collections.find(collection => collection.id === selectedId) ?? null;
  const known = useMemo(() => new Set(inventory?.files.map(file => file.id) ?? []), [inventory]);
  const missingCount = selected?.entry_ids.filter(id => !known.has(id)).length ?? 0;
  const matches = useMemo(() => inventory?.files.filter(file =>
    file.path.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase())) ?? [],
    [inventory, search]);
  const shown = matches.slice(0, MAX_VISIBLE);

  const choose = (item: SavedCollection | null) => {
    if (operationBusy.current) return;
    setSelectedId(item?.id ?? null);
    setDraftName(item?.name ?? '');
    setDraftMembers(item?.entry_ids.filter(id => known.has(id)) ?? []);
    setDeleteArmed(false);
    setError(null);
    setNotice(null);
  };

  const refresh = async () => {
    if (!active || operationBusy.current) return;
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('load');
    setError(null);
    try {
      const [snapshot, raw] = await Promise.all([
        invoke<unknown>('cmd_drive_get_snapshot'),
        invoke<unknown>('cmd_drive_collections_list'),
      ]);
      const files = collectionInventory(snapshot);
      const saved = parseCollections(raw);
      if (generation.current !== id) return;
      setInventory(files);
      setCollections(saved);
      setSelectedId(null);
      setDraftName('');
      setDraftMembers([]);
      setDeleteArmed(false);
      setNotice(null);
    } catch (cause) {
      if (generation.current === id) setError('Cannot safely load local collections: ' + String(cause));
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const save = async () => {
    if (!active || operationBusy.current || !inventory) return;
    let request;
    try {
      request = buildCollectionSave(inventory, selected, draftName, draftMembers);
    } catch (cause) {
      setError(String(cause));
      return;
    }
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('save');
    setError(null);
    setNotice(null);
    try {
      // The native command rejects stale collection revisions and unavailable
      // Drive identities. An uncertain IPC response is NEVER blindly retried.
      const savedRaw = await invoke<unknown>('cmd_drive_collections_save', {
        collectionId: request.collectionId,
        expectedRevision: request.expectedRevision,
        name: request.name,
        entryIds: request.entryIds,
      });
      const updated = parseCollections([savedRaw])[0];
      if (generation.current !== id) return;
      const list = parseCollections(await invoke<unknown>('cmd_drive_collections_list'));
      if (generation.current !== id) return;
      setCollections(list);
      setSelectedId(updated.id);
      setDraftName(updated.name);
      setDraftMembers(updated.entry_ids);
      setDeleteArmed(false);
      setNotice('Collection saved locally. Drive files and Telegram data were not changed.');
    } catch (cause) {
      if (generation.current === id) {
        setError('Save outcome may be uncertain: ' + String(cause) + '. Refresh collections before trying again.');
      }
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const remove = async () => {
    if (!selected || !active || operationBusy.current) return;
    if (!deleteArmed) {
      setDeleteArmed(true);
      setError(null);
      return;
    }
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('delete');
    setDeleteArmed(false);
    setError(null);
    try {
      await invoke('cmd_drive_collections_delete', {
        collectionId: selected.id, expectedRevision: selected.revision,
      });
      if (generation.current !== id) return;
      setCollections(current => current.filter(item => item.id !== selected.id));
      setSelectedId(null);
      setDraftName('');
      setDraftMembers([]);
      setNotice('Only this saved collection was removed. Its Drive files remain untouched.');
    } catch (cause) {
      if (generation.current === id) setError('Deletion outcome is uncertain: ' + String(cause)
        + '. Refresh the collections; no Drive files were targeted.');
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const copy = async () => {
    if (!selected || !inventory || copying || operationBusy.current) return;
    setCopying(true);
    setError(null);
    try {
      await navigator.clipboard.writeText(collectionMembersCsv(resolveCollection(selected, inventory)));
      setNotice('Collection file list copied as CSV. No file content was accessed.');
    } catch (cause) {
      setError('Could not copy collection CSV: ' + String(cause));
    } finally {
      setCopying(false);
    }
  };

  const toggleMember = (id: string) => {
    if (operationBusy.current) return;
    setDraftMembers(current => current.includes(id) ? current.filter(value => value !== id)
      : current.length >= 200 ? current : [...current, id]);
    setDeleteArmed(false);
    setNotice(null);
  };

  return (
    <section aria-label="Drive Virtual Collections" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <Bookmark className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">TeraRelay Drive · Collections</p>
            <p className="text-xs text-telegram-subtext">
              Organize file references from different Drive folders into persistent local projects without moving the actual files.
            </p>
          </div>
        </div>
        <button type="button" className={ctl} disabled={busy !== null || copying}
          aria-expanded={open} onClick={() => {
            if (open) { generation.current++; setOpen(false); setInventory(null); }
            else { setOpen(true); void refresh(); }
          }}>{open ? 'Hide collections' : 'Open collections'}</button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <p className="text-xs text-telegram-subtext">
            <ShieldCheck className="mr-1 inline h-3 w-3" aria-hidden="true" />
            Collections are saved on this device only; they are not backed up or synced through Telegram.
            Deleting a collection never deletes any Drive file.
          </p>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p role="status" className="text-xs text-telegram-subtext">
              {busy === 'load' ? 'Loading local collections…'
                : `${collections.length} saved collections · ${inventory?.files.length ?? 0} available files`}
            </p>
            <button type="button" className={ctl} disabled={busy !== null || copying} onClick={() => void refresh()}>
              {busy === 'load' ? <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" />
                : <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />}
              Refresh
            </button>
          </div>
          {inventory && <>
            {inventory.truncated > 0 && <p role="alert" className="text-xs text-telegram-subtext">The local inventory exceeded its 25,000-entry safety cap. Some files may not be selectable.</p>}
            <div className="flex flex-wrap gap-2">
              <select aria-label="Choose saved Drive collection" className={field + ' min-w-[200px] flex-1'}
                value={selectedId ?? ''} disabled={busy !== null} onChange={event => {
                  choose(collections.find(item => item.id === event.target.value) ?? null);
                }}>
                <option value="">New collection</option>
                {collections.map(item => <option value={item.id} key={item.id}>{item.name} ({item.entry_ids.length})</option>)}
              </select>
              <button type="button" className={ctl} disabled={busy !== null}
                onClick={() => choose(null)}>New collection</button>
              <button type="button" className={ctl} disabled={!selected || busy !== null || copying}
                onClick={() => void copy()}><Copy className="mr-1 inline h-3 w-3" aria-hidden="true" />Copy CSV</button>
            </div>
            <label className="block text-xs text-telegram-subtext">Collection name
              <input aria-label="Drive collection name" className={field + ' mt-1 w-full'} value={draftName}
                disabled={busy !== null} maxLength={100} onChange={event => { setDraftName(event.target.value); setDeleteArmed(false); }} />
            </label>
            {missingCount > 0 && <p role="alert" className="text-xs text-telegram-subtext">
              {missingCount} saved file reference(s) are no longer available in this local index. Saving this collection will omit those references.
            </p>}
            <div className="flex flex-wrap items-center gap-2">
              <input type="search" className={field + ' min-w-[160px] flex-1'}
                aria-label="Find Drive files for collection" value={search}
                onChange={event => setSearch(event.target.value)} placeholder="Search Drive file paths…" />
              <span className="text-xs text-telegram-subtext">{draftMembers.length}/200 selected</span>
            </div>
            <div className="max-h-52 space-y-1 overflow-auto rounded-lg border border-telegram-border p-2"
              aria-label="Drive files to add to collection">
              {shown.map(file => <label key={file.id}
                className="flex items-start gap-2 rounded p-1 text-xs text-telegram-text hover:bg-telegram-hover/50">
                <input type="checkbox" className="mt-0.5" disabled={busy !== null}
                  checked={draftMembers.includes(file.id)} onChange={() => toggleMember(file.id)} />
                <span className="break-all">{file.path}</span>
              </label>)}
              {matches.length > MAX_VISIBLE && <p className="text-xs text-telegram-subtext">
                Showing first {MAX_VISIBLE} of {matches.length} matching files. Narrow the search to choose others.
              </p>}
              {matches.length === 0 && <p className="text-xs text-telegram-subtext">No matching completed Drive files.</p>}
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <button type="button" className={ctl + ' font-semibold'} disabled={busy !== null || copying}
                onClick={() => void save()}>
                {busy === 'save' && <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" />}
                {selected ? 'Save collection changes' : 'Create collection'}
              </button>
              {selected && <button type="button" className={ctl} disabled={busy !== null || copying}
                onClick={() => void remove()}>
                {deleteArmed ? 'Confirm remove collection only' : 'Remove collection'}
              </button>}
              {deleteArmed && <button type="button" className={ctl} onClick={() => setDeleteArmed(false)}>
                Cancel
              </button>}
            </div>
          </>}
          {error && <p role="alert" className="rounded-lg border border-red-500/30 p-2 text-xs text-red-400">{error}</p>}
          {notice && <p role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
        </div>
      )}
    </section>
  );
}
