import { validateDriveSnapshot } from './deviceContinuity';

export type DriveFinderKind = 'all' | 'folder' | 'file' | 'pending';
export interface DriveFinderEntry {
  id: string;
  kind: Exclude<DriveFinderKind, 'all'>;
  name: string;
  path: string;
  size: number | null;
}
export const DRIVE_FINDER_MAX_ROWS = 25000;

const rowRecord = (value: unknown): Record<string, unknown> | null =>
  value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
// Internal metadata identities are not paths. Even if a corrupt row would not
// appear verbatim in the UI, reject path separators, traversal-only IDs and
// control characters before using them to resolve parent relationships.
const validId = (id: unknown): id is string =>
  typeof id === 'string' && id.length > 0 && id.length <= 256
    && id.trim() === id && id !== '.' && id !== '..'
    && !/[\/\\\u0000-\u001f\u007f-\u009f\u2028\u2029]/.test(id);
const validName = (name: unknown): name is string =>
  typeof name === 'string' && name.trim().length > 0 && name.length <= 255
    && !/[\/\\\u0000-\u001f\u007f]/.test(name)
    && name !== '.' && name !== '..';
const parentId = (value: unknown): string | null | undefined =>
  value === null || value === undefined ? null : validId(value) ? value : undefined;
const knownSize = (value: unknown): number | null =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;

/** Read-only, bounded and fail-closed view of native local Drive metadata.
 * Bad ancestry never becomes a guessed user-visible path. No network access.
 */
export function normalizeDriveFinderSnapshot(value: unknown): {
  entries: DriveFinderEntry[];
  skipped: number;
  truncated: number;
} {
  const snapshot = validateDriveSnapshot(value);
  let skipped = 0;
  let truncated = 0;
  const folders = new Map<string, { name: string; parent: string | null }>();
  const all: Array<{ kind: Exclude<DriveFinderKind, 'all'>; id: string; name: string;
    parent: string | null; size: number | null }> = [];
  const seen = new Set<string>();
  const add = (raw: unknown, kind: Exclude<DriveFinderKind, 'all'>) => {
    const row = rowRecord(raw);
    const id = kind === 'folder' || kind === 'pending' ? row?.id : row?.entry_id;
    const name = kind === 'folder' ? row?.name : row?.display_name;
    const parent = parentId(kind === 'folder' ? row?.parent_id : row?.directory_id);
    const identity = kind + ':' + id;
    if (!row || !validId(id) || !validName(name) || parent === undefined
        || seen.has(identity) || (kind === 'folder' && row.trashed_at != null)) {
      skipped++;
      return;
    }
    seen.add(identity);
    if (all.length >= DRIVE_FINDER_MAX_ROWS) {
      truncated++;
      return;
    }
    const size = kind === 'file' ? knownSize(row.plaintext_size ?? row.total_size)
      : kind === 'pending' ? knownSize(row.size) : null;
    all.push({ kind, id, name, parent, size });
    if (kind === 'folder') folders.set(id, { name, parent });
  };
  // Directories before children. Pending writes can be visible before upload.
  snapshot.directories.forEach(row => add(row, 'folder'));
  snapshot.files.forEach(row => add(row, 'file'));
  snapshot.pending.forEach(row => add(row, 'pending'));

  const resolved = new Map<string, string | null>();
  const resolving = new Set<string>();
  const resolve = (id: string, depth = 0): string | null => {
    if (resolved.has(id)) return resolved.get(id)!;
    const folder = folders.get(id);
    if (!folder || resolving.has(id) || depth >= 100) return null;
    resolving.add(id);
    const prefix = folder.parent === null ? '' : resolve(folder.parent, depth + 1);
    resolving.delete(id);
    const path = prefix === null ? null : prefix + '/' + folder.name;
    resolved.set(id, path);
    return path;
  };
  const entries: DriveFinderEntry[] = [];
  for (const item of all) {
    const parentPath = item.parent === null ? '' : resolve(item.parent);
    const path = item.kind === 'folder' ? resolve(item.id)
      : parentPath === null ? null : parentPath + '/' + item.name;
    if (path === null) {
      skipped++;
      continue;
    }
    entries.push({ id: item.id, kind: item.kind, name: item.name, path, size: item.size });
  }
  entries.sort((a, b) => a.path.localeCompare(b.path, undefined, { sensitivity: 'base' })
    || a.kind.localeCompare(b.kind) || a.id.localeCompare(b.id));
  return { entries, skipped, truncated };
}

export function filterDriveFinderEntries(
  entries: readonly DriveFinderEntry[], query: string, kind: DriveFinderKind,
): DriveFinderEntry[] {
  const needle = query.trim().toLocaleLowerCase();
  return entries.filter(entry => (kind === 'all' || entry.kind === kind)
    && (needle === '' || entry.path.toLocaleLowerCase().includes(needle)));
}
