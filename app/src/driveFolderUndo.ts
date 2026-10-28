export interface FolderUndoEntry {
  folder_id: string;
  previous_name: string;
  previous_parent_id: string | null;
  current_name: string;
  current_parent_id: string | null;
  subtree_sha256: string;
}
export interface FolderUndoBatch {
  id: string;
  created_at: number;
  applied_revision: number;
  undone_at: number | null;
  entries: FolderUndoEntry[];
}
export interface FolderUndoPreview {
  id: string;
  revision: number;
  ready: boolean;
  count: number;
  problems: string[];
  entries: FolderUndoEntry[];
}
export interface FolderUndoApplied {
  restored: number;
  revision: number;
  dirty: boolean;
}
const row = (v: unknown): Record<string, unknown> | null =>
  v !== null && typeof v === 'object' && !Array.isArray(v) ? v as Record<string, unknown> : null;
const safeInteger = (v: unknown): v is number => Number.isSafeInteger(v) && (v as number) >= 0;
const safeId = (v: unknown): v is string => typeof v === 'string'
  && v.length > 0 && v.length <= 256 && v.trim() === v
  && v !== '.' && v !== '..' && !/[\\/\u0000-\u001f\u007f]/.test(v);
const safeParent = (v: unknown): v is string | null => v === null || safeId(v);
const safeName = (v: unknown): v is string => typeof v === 'string'
  && v.length > 0 && new TextEncoder().encode(v).length <= 255 && v.trim() === v
  && !/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(v) && !v.endsWith('.');
const safeBatchId = (v: unknown): v is string => typeof v === 'string'
  && /^folder-[a-fA-F0-9]{32}$/.test(v);
function readEntries(v: unknown): FolderUndoEntry[] {
  if (!Array.isArray(v) || v.length < 1 || v.length > 100) throw Error('Invalid saved folder undo entries.');
  const seen = new Set<string>();
  return v.map(item => {
    const e = row(item);
    if (!e || !safeId(e.folder_id) || seen.has(e.folder_id)
        || !safeName(e.previous_name) || !safeName(e.current_name)
        || !safeParent(e.previous_parent_id) || !safeParent(e.current_parent_id)
        || typeof e.subtree_sha256 !== 'string' || !/^[a-fA-F0-9]{64}$/.test(e.subtree_sha256)
        || (e.previous_name === e.current_name && e.previous_parent_id === e.current_parent_id)) {
      throw Error('Saved folder undo entries are damaged.');
    }
    seen.add(e.folder_id);
    return {
      folder_id: e.folder_id, previous_name: e.previous_name,
      previous_parent_id: e.previous_parent_id,
      current_name: e.current_name, current_parent_id: e.current_parent_id,
      subtree_sha256: e.subtree_sha256,
    } as FolderUndoEntry;
  });
}
export function parseFolderUndoHistory(v: unknown): FolderUndoBatch[] {
  if (!Array.isArray(v) || v.length > 24) throw Error('Invalid folder undo history.');
  const ids = new Set<string>();
  return v.map(value => {
    const x = row(value);
    if (!x || !safeBatchId(x.id) || ids.has(x.id)
        || !safeInteger(x.created_at) || !safeInteger(x.applied_revision)
        || !(x.undone_at === null || safeInteger(x.undone_at))) {
      throw Error('Saved folder undo history is invalid.');
    }
    ids.add(x.id);
    return {
      id: x.id, created_at: x.created_at, applied_revision: x.applied_revision,
      undone_at: x.undone_at, entries: readEntries(x.entries),
    } as FolderUndoBatch;
  });
}
export function parseFolderUndoPreview(v: unknown, id: string): FolderUndoPreview {
  const x = row(v);
  if (!x || !safeBatchId(id) || x.id !== id
      || !safeInteger(x.revision) || typeof x.ready !== 'boolean'
      || !safeInteger(x.count) || !Array.isArray(x.problems)
      || x.problems.length > 200 || x.problems.some(p => typeof p !== 'string' || p.length > 2000)) {
    throw Error('Folder undo preview response was invalid.');
  }
  const entries = readEntries(x.entries);
  if (x.count !== entries.length || (x.ready && x.problems.length > 0)) {
    throw Error('Folder undo preview is inconsistent.');
  }
  return { id, revision: x.revision, ready: x.ready,
    count: x.count, problems: x.problems as string[], entries };
}
export function parseFolderUndoApplied(v: unknown, p: FolderUndoPreview): FolderUndoApplied {
  const x = row(v);
  if (!x || !safeInteger(x.restored) || x.restored !== p.count
      || !safeInteger(x.revision) || (x.revision as number) <= p.revision
      || x.dirty !== true) {
    throw Error('Folder undo outcome is uncertain. Refresh Drive before retrying.');
  }
  return { restored: x.restored, revision: x.revision, dirty: x.dirty };
}
