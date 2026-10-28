import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { History, RotateCcw, RefreshCw } from 'lucide-react';
import { readOrganizerSnapshot } from '../../../driveBulkOrganizer';

type UndoFile = {
  entry_id: string;
  previous_name: string;
  previous_directory_id: string | null;
  current_name: string;
  current_directory_id: string | null;
};
type UndoHistory = {
  id: string;
  created_at: number;
  applied_revision: number;
  undone_at: number | null;
  files: UndoFile[];
};
type UndoPreview = {
  revision: number;
  ready: boolean;
  problems: string[];
  count: number;
};
const action = 'rounded-lg border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
function object(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
function historyResponse(value: unknown): UndoHistory[] {
  if (!Array.isArray(value) || value.length > 24) throw new Error('Invalid undo history.');
  const seen = new Set<string>();
  return value.map(value => {
    const row = object(value);
    if (!row || typeof row.id !== 'string' || !/^bulk-[0-9a-f]{32}$/.test(row.id)
        || seen.has(row.id) || !Number.isSafeInteger(row.created_at)
        || !Number.isSafeInteger(row.applied_revision)
        || !(row.undone_at === null || Number.isSafeInteger(row.undone_at))
        || !Array.isArray(row.files) || row.files.length === 0 || row.files.length > 100) {
      throw new Error('Invalid saved undo operation.');
    }
    seen.add(row.id);
    const files: UndoFile[] = row.files.map(raw => {
      const file = object(raw);
      if (!file || typeof file.entry_id !== 'string'
          || typeof file.previous_name !== 'string' || typeof file.current_name !== 'string'
          || !file.entry_id || !file.previous_name || !file.current_name
          || file.entry_id.length > 256
          || file.previous_name.length > 255 || file.current_name.length > 255
          || !(file.previous_directory_id === null || typeof file.previous_directory_id === 'string')
          || !(file.current_directory_id === null || typeof file.current_directory_id === 'string')) {
        throw new Error('Invalid saved undo file identity.');
      }
      return {
        entry_id: file.entry_id,
        previous_name: file.previous_name,
        current_name: file.current_name,
        previous_directory_id: file.previous_directory_id as string | null,
        current_directory_id: file.current_directory_id as string | null,
      };
    });
    return {
      id: row.id, created_at: row.created_at as number,
      applied_revision: row.applied_revision as number,
      undone_at: row.undone_at as number | null, files,
    };
  });
}
function previewResponse(value: unknown): UndoPreview {
  const row = object(value);
  if (!row || !Number.isSafeInteger(row.revision) || typeof row.ready !== 'boolean'
      || !Number.isSafeInteger(row.count) || !Array.isArray(row.problems)
      || row.problems.length > 100 || row.problems.some(
        item => typeof item !== 'string' || item.length > 400
      )) throw new Error('Invalid undo preview.');
  return {
    revision: row.revision as number, ready: row.ready,
    count: row.count as number, problems: row.problems as string[],
  };
}
export function DriveBulkUndo({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [history, setHistory] = useState<UndoHistory[]>([]);
  const [folderPaths, setFolderPaths] = useState<Map<string, string>>(new Map());
  const [selectedId, setSelectedId] = useState('');
  const [preview, setPreview] = useState<UndoPreview | null>(null);
  const [busy, setBusy] = useState<'history' | 'preview' | 'undo' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const working = useRef(false);
  const generation = useRef(0);
  const selected = history.find(item => item.id === selectedId);
  useEffect(() => {
    if (active) return;
    if (working.current) { setOpen(false); return; }
    generation.current++;
    setOpen(false);
    setPreview(null);
    setHistory([]);
    setFolderPaths(new Map());
  }, [active]);
  const load = async () => {
    if (!active || working.current) return;
    working.current = true;
    const requestId = ++generation.current;
    setBusy('history'); setError(null); setPreview(null);
    try {
      const [rawHistory, rawSnapshot] = await Promise.all([
        invoke<unknown>('cmd_drive_bulk_history'),
        invoke<unknown>('cmd_drive_get_snapshot'),
      ]);
      const items = historyResponse(rawHistory);
      const index = readOrganizerSnapshot(rawSnapshot);
      if (requestId !== generation.current) return;
      setFolderPaths(new Map(index.folders.map(folder => [folder.id, folder.path])));
      setHistory(items);
      setSelectedId(items.find(item => item.undone_at === null)?.id ?? items[0]?.id ?? '');
    } catch (cause) {
      if (requestId === generation.current) setError('Could not load undo history: ' + String(cause));
    } finally {
      working.current = false;
      if (requestId === generation.current) setBusy(null);
    }
  };
  const inspect = async () => {
    if (!selected || selected.undone_at !== null || working.current) return;
    working.current = true;
    const requestId = ++generation.current;
    setBusy('preview'); setError(null); setPreview(null);
    try {
      const result = previewResponse(await invoke<unknown>(
        'cmd_drive_bulk_undo_preview', { batchId: selected.id },
      ));
      if (requestId === generation.current) setPreview(result);
    } catch (cause) {
      if (requestId === generation.current) setError('Undo is unsafe: ' + String(cause));
    } finally {
      working.current = false;
      if (requestId === generation.current) setBusy(null);
    }
  };
  const applyUndo = async () => {
    if (!selected || !preview?.ready || working.current) return;
    working.current = true;
    const requestId = ++generation.current;
    setBusy('undo'); setError(null); setNotice(null);
    try {
      const result = object(await invoke<unknown>('cmd_drive_bulk_undo', {
        batchId: selected.id, expectedRevision: preview.revision,
      }));
      if (!result || !Number.isSafeInteger(result.changed)
          || !Number.isSafeInteger(result.revision)) {
        throw new Error('Undo result could not be confirmed.');
      }
      const updated = historyResponse(await invoke<unknown>('cmd_drive_bulk_history'));
      if (requestId !== generation.current) return;
      setHistory(updated);
      setPreview(null);
      setNotice(String(result.changed) + ' changes reversed. No files were overwritten.');
    } catch (cause) {
      if (requestId === generation.current) {
        setError('Undo outcome may be uncertain: ' + String(cause)
          + '. Refresh history before retrying.');
        setPreview(null);
      }
    } finally {
      working.current = false;
      if (requestId === generation.current) setBusy(null);
    }
  };
  const displayPath = (directoryId: string | null, fileName: string) => {
    if (directoryId === null) return '/' + fileName;
    const folderPath = folderPaths.get(directoryId);
    return folderPath === undefined ? '[Folder unavailable]/' + fileName : folderPath + '/' + fileName;
  };
  return (
    <section className="tr-setting-row space-y-3 p-3" aria-label="Drive Bulk Organizer Undo">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <p className="text-sm font-medium text-telegram-text">
            <History className="mr-1 inline h-4 w-4" aria-hidden="true" />
            Drive · Undo recent bulk changes
          </p>
          <p className="text-xs text-telegram-subtext">Restore previous file names and folders after an Organizer batch, without replacing newer edits.</p>
        </div>
        <button type="button" className={action} disabled={busy !== null} aria-expanded={open}
          onClick={() => {
            if (working.current) return;
            if (open) { generation.current++; setOpen(false); setPreview(null); }
            else { setOpen(true); void load(); }
          }}>{open ? 'Hide undo history' : 'Review undo history'}</button>
      </div>
      {open && <div className="space-y-3 border-t border-telegram-border pt-3">
        <p className="text-xs text-telegram-subtext">The latest 24 Organizer batches are stored only on this device. Undo is all-or-none, checks for changed files and occupied destinations, and is not a cloud-version restore.</p>
        <div className="flex items-center justify-between gap-2">
          <span role="status" className="text-xs text-telegram-subtext">{history.length} saved batches</span>
          <button className={action} disabled={busy !== null} onClick={() => void load()}>
            <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />Refresh history
          </button>
        </div>
        {history.length > 0 && <>
          <select aria-label="Select Drive batch to undo" className="w-full rounded-lg border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text"
            disabled={busy !== null} value={selectedId}
            onChange={event => { setSelectedId(event.target.value); setPreview(null); }}>
            {history.map(item => <option key={item.id} value={item.id}>
              {new Date(item.created_at).toLocaleString()} · {item.files.length} files
              {item.undone_at !== null ? ' (already undone)' : ''}
            </option>)}
          </select>
          {selected && <>
            <div className="max-h-52 overflow-auto rounded-lg border border-telegram-border p-2">
              {selected.files.map(file => <p className="mb-1 break-all text-xs text-telegram-text" key={file.entry_id}>
                <span className="block text-telegram-subtext">Current: {displayPath(file.current_directory_id, file.current_name)}</span>
                <span className="block text-telegram-text">Restore to: {displayPath(file.previous_directory_id, file.previous_name)}</span>
              </p>)}
            </div>
            <button className={action} disabled={busy !== null || selected.undone_at !== null}
              onClick={() => void inspect()}>Preview safe undo</button>
            {preview && <div className="space-y-2 text-xs text-telegram-subtext">
              <p role="status">{preview.ready
                ? 'Ready to reverse ' + preview.count + ' changes; no collisions detected.'
                : 'Undo blocked by changes to the current Drive.'}</p>
              {preview.problems.map((problem, i) => <p role="alert" key={i}>{problem}</p>)}
              <button className={action} disabled={!preview.ready || busy !== null}
                onClick={() => void applyUndo()}>
                <RotateCcw className="mr-1 inline h-3 w-3" aria-hidden="true" />
                Confirm undo of this batch
              </button>
            </div>}
          </>}
        </>}
        {history.length === 0 && <p className="text-xs text-telegram-subtext">No local Organizer batches are saved yet.</p>}
        {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
        {notice && <p role="status" className="text-xs text-telegram-text">{notice}</p>}
      </div>}
    </section>
  );
}
