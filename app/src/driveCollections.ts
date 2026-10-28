import { normalizeDriveFinderSnapshot } from './driveLocalFinder';
import { validateDriveSnapshot } from './deviceContinuity';

export interface CollectionItem { id: string; name: string; path: string; size: number | null }
export interface CollectionInventory {
  revision: number;
  files: CollectionItem[];
  skipped: number;
  truncated: number;
}
export interface SavedCollection {
  id: string;
  name: string;
  revision: number;
  entry_ids: string[];
}
export interface ResolvedCollection {
  collection: SavedCollection;
  available: CollectionItem[];
  missingCount: number;
}
export interface CollectionSaveRequest {
  collectionId: string | null;
  expectedRevision: number | null;
  name: string;
  entryIds: string[];
}
const obj = (value: unknown): Record<string, unknown> | null =>
  value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const idOkay = (id: unknown): id is string =>
  typeof id === 'string' && id.length > 0 && id.length <= 256
    && id.trim() === id && !/[\/\\\u0000-\u001f\u007f]/.test(id);
const nameOkay = (name: unknown): name is string =>
  typeof name === 'string' && name.length > 0 && name.length <= 100
    && name.trim() === name && [...name].length <= 80
    && !/[\/\\\u0000-\u001f\u007f]/.test(name);

/** Only the Finder-vetted, path-resolved, completed metadata becomes selectable. */
export function collectionInventory(value: unknown): CollectionInventory {
  const native = validateDriveSnapshot(value);
  const finder = normalizeDriveFinderSnapshot(value);
  return {
    revision: native.revision,
    files: finder.entries.filter(entry => entry.kind === 'file')
      .map(entry => ({ id: entry.id, name: entry.name, path: entry.path, size: entry.size })),
    skipped: finder.skipped,
    truncated: finder.truncated,
  };
}

export function parseCollections(value: unknown): SavedCollection[] {
  if (!Array.isArray(value) || value.length > 100) throw new TypeError('Invalid saved collection inventory.');
  const seen = new Set<string>();
  return value.map(raw => {
    const item = obj(raw);
    const ids = item?.entry_ids;
    if (!item || !idOkay(item.id) || !nameOkay(item.name)
        || !Number.isSafeInteger(item.revision) || (item.revision as number) < 1
        || !Array.isArray(ids) || ids.length > 200 || seen.has(item.id)
        || ids.some(id => !idOkay(id)) || new Set(ids).size !== ids.length) {
      throw new TypeError('Saved collection metadata is invalid; no changes were made.');
    }
    seen.add(item.id);
    return { id: item.id, name: item.name, revision: item.revision as number, entry_ids: [...ids] as string[] };
  });
}

export function resolveCollection(
  collection: SavedCollection, inventory: CollectionInventory,
): ResolvedCollection {
  const present = new Map(inventory.files.map(file => [file.id, file]));
  const available: CollectionItem[] = [];
  let missingCount = 0;
  for (const id of collection.entry_ids) {
    const found = present.get(id);
    if (found) available.push(found);
    else missingCount++;
  }
  return { collection, available, missingCount };
}

export function buildCollectionSave(
  inventory: CollectionInventory, source: SavedCollection | null, name: string, ids: readonly string[],
): CollectionSaveRequest {
  if (!nameOkay(name)) throw new Error('Name must be 1–80 clean characters.');
  if (ids.length > 200 || new Set(ids).size !== ids.length) {
    throw new Error('Select at most 200 different Drive files.');
  }
  const allowed = new Set(inventory.files.map(file => file.id));
  if (ids.some(id => !allowed.has(id))) {
    throw new Error('One selected file is missing. Refresh and remove unavailable entries.');
  }
  return {
    collectionId: source?.id ?? null,
    expectedRevision: source?.revision ?? null,
    name,
    entryIds: [...ids],
  };
}

const csvCell = (value: string) => '"' +
  (/^[=+@\-\t\r]/.test(value) ? "'" + value : value).replace(/"/g, '""') + '"';

/** Copy-only portability report. Does not include saved local DB internals. */
export function collectionMembersCsv(resolved: ResolvedCollection): string {
  const rows = ['Collection,Path,Size bytes,Availability'];
  for (const file of resolved.available) {
    rows.push([resolved.collection.name, file.path, file.size?.toString() ?? '', 'Available']
      .map(csvCell).join(','));
  }
  if (resolved.missingCount > 0) {
    rows.push([resolved.collection.name, '', '', `${resolved.missingCount} missing references excluded`]
      .map(csvCell).join(','));
  }
  return rows.join('\r\n');
}
