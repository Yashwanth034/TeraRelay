import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { History, RotateCcw, RefreshCw, Loader2 } from 'lucide-react';
import { validateDriveSyncResult } from '../../../deviceContinuity';
import {
  parseFolderUndoHistory, parseFolderUndoPreview, parseFolderUndoApplied,
  type FolderUndoBatch, type FolderUndoPreview,
} from '../../../driveFolderUndo';

const ctl = 'rounded-lg border border-telegram-border bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const stamp = (v: number) => {
  try { return new Date(v).toLocaleString(); }
  catch { return 'Unknown timestamp'; }
};
export function DriveFolderUndo({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [history, setHistory] = useState<FolderUndoBatch[]>([]);
  const [id, setId] = useState('');
  const [preview, setPreview] = useState<FolderUndoPreview | null>(null);
  const [busy, setBusy] = useState<'load'|'preview'|'apply'|null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const inFlight = useRef(false);
  const generation = useRef(0);
  useEffect(() => {
    if (active) return;
    setOpen(false);
    setPreview(null);
    if (busy !== 'apply') {
      generation.current++;
      setId('');
      setHistory([]);
      setBusy(null);
    }
  }, [active, busy]);
  const selected = history.find(item => item.id === id);
  const load = async () => {
    if (!active || inFlight.current) return;
    inFlight.current = true;
    const ticket = ++generation.current;
    setBusy('load'); setError(''); setPreview(null);
    try {
      const list = parseFolderUndoHistory(await invoke<unknown>('cmd_drive_folder_undo_history'));
      if (ticket === generation.current) {
        setHistory(list);
        setId('');
      }
    } catch (cause) {
      if (ticket === generation.current) setError('Cannot load folder Undo history: ' + String(cause));
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };
  const inspect = async () => {
    if (!active || inFlight.current || !selected || selected.undone_at !== null) return;
    inFlight.current = true;
    const ticket = ++generation.current;
    setBusy('preview'); setError(''); setNotice(''); setPreview(null);
    try {
      const result = parseFolderUndoPreview(
        await invoke<unknown>('cmd_drive_folder_undo_preview', { id: selected.id }), selected.id);
      if (ticket === generation.current) setPreview(result);
    } catch (cause) {
      if (ticket === generation.current) setError('Undo preview failed: ' + String(cause));
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };
  const apply = async () => {
    if (!active || inFlight.current || !preview?.ready || preview.id !== id
        || !selected || selected.undone_at !== null) return;
    inFlight.current = true;
    const ticket = ++generation.current;
    const checked = preview;
    setBusy('apply'); setError(''); setNotice(''); setPreview(null);
    try {
      let restored: number;
      try {
        restored = parseFolderUndoApplied(
          await invoke<unknown>('cmd_drive_folder_undo_apply',
            { id: checked.id, revision: checked.revision }), checked).restored;
      } catch (cause) {
        if (ticket === generation.current) setError(
          'Folder Undo result is uncertain: ' + String(cause)
          + '. Refresh history and inspect Drive before retrying.');
        return;
      }
      if (ticket !== generation.current) return;
      setHistory([]);
      setId('');
      setNotice(restored + ' folder operation(s) restored locally. Checking cloud metadata sync…');
      let syncMessage = 'Cloud metadata sync is not confirmed.';
      try {
        const sync = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if (!sync.dirty && sync.source !== 'qa-local')
          syncMessage = 'Drive metadata sync returned a clean result.';
      } catch { /* Local undo remains committed even when cloud sync fails. */ }
      if (ticket === generation.current) setNotice(
        restored + ' folder operation(s) restored locally. ' + syncMessage);
    } finally {
      inFlight.current = false;
      if (ticket === generation.current) setBusy(null);
    }
  };
  return <section className="tr-setting-row p-3 space-y-3" aria-label="Drive folder batch undo">
    <div className="flex flex-wrap items-center justify-between gap-3">
      <div className="flex min-w-0 items-start gap-2">
        <History className="w-4 h-4 shrink-0 text-telegram-subtext mt-0.5" aria-hidden="true"/>
        <div>
          <h3 className="text-sm font-medium text-telegram-text">Folder Batch Undo</h3>
          <p className="text-xs text-telegram-subtext">
            Safely reverse recent folder move/rename batches. Every descendant must still match its saved state.
          </p>
        </div>
      </div>
      <button type="button" className={ctl} aria-expanded={open}
        onClick={() => setOpen(value => !value)}>{open ? 'Close undo' : 'Open folder undo'}</button>
    </div>
    {open && <div className="space-y-3">
      <div className="flex items-center gap-2">
        <button className={ctl} type="button" disabled={!active || !!busy} onClick={() => void load()}>
          <RefreshCw className="inline w-3.5 h-3.5 mr-1" aria-hidden="true"/> Refresh history
        </button>
        <span className="text-xs text-telegram-subtext">{history.length} saved batches (maximum 24)</span>
      </div>
      {history.length > 0 && <label className="block text-xs text-telegram-subtext space-y-1">
        Select an original folder operation
        <select className="w-full rounded-lg border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text"
          disabled={!!busy} value={id} onChange={e => {
            setId(e.target.value); setPreview(null); setError(''); setNotice('');
          }}>
          <option value="">Choose a batch…</option>
          {history.map(batch => <option value={batch.id} key={batch.id}>
            {stamp(batch.created_at)} · {batch.entries.length} folders
            {batch.undone_at === null ? '' : ' · already undone'}
          </option>)}
        </select>
      </label>}
      {selected && <div className="rounded-lg border border-telegram-border px-3 py-2 space-y-1">
        <p className="text-xs text-telegram-subtext">
          {selected.undone_at === null ? 'Not yet undone' : 'Undone at ' + stamp(selected.undone_at)}.
          Only original folder metadata is restored; changes to descendants block the operation.
        </p>
        <ul className="max-h-36 overflow-auto text-xs text-telegram-text space-y-1">
          {selected.entries.slice(0, 25).map(entry =>
            <li key={entry.folder_id}>{entry.current_name} → {entry.previous_name}
              {entry.current_parent_id !== entry.previous_parent_id ? ' (restore original parent)' : ''}</li>)}
        </ul>
        {selected.entries.length > 25 &&
          <p className="text-xs text-telegram-subtext">Showing the first 25 entries.</p>}
      </div>}
      <div className="flex flex-wrap gap-2">
        <button className={ctl} type="button"
          disabled={!!busy || !selected || selected.undone_at !== null} onClick={() => void inspect()}>
          Verify undo safety
        </button>
        <button className={ctl} type="button"
          disabled={!!busy || !preview?.ready || preview.id !== id || selected?.undone_at !== null}
          onClick={() => void apply()}>
          <RotateCcw className="inline w-3.5 h-3.5 mr-1" aria-hidden="true"/> Apply verified undo
        </button>
      </div>
      {preview && <div role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text space-y-1">
        <p>{preview.ready
          ? 'Ready: ' + preview.count + ' folder operation(s) can be safely undone at this Drive revision.'
          : 'Undo blocked: changed files, descendants, collisions or stale folder metadata.'}</p>
        {preview.problems.slice(0, 12).map((problem, index) => <p key={index}>{problem}</p>)}
      </div>}
      {busy && <p role="status" className="text-xs text-telegram-subtext">
        <Loader2 className="inline w-3.5 h-3.5 mr-1 animate-spin" aria-hidden="true"/>
        {busy === 'apply' ? 'Reversing the verified folder batch…'
          : busy === 'preview' ? 'Checking subtrees and collisions…' : 'Reading local folder history…'}
      </p>}
      {error && <p role="alert" className="border border-red-500/40 rounded-lg p-2 text-xs text-telegram-text">{error}</p>}
      {notice && <p role="status" className="border border-telegram-border rounded-lg p-2 text-xs text-telegram-text">{notice}</p>}
    </div>}
  </section>;
}
