import { validateDriveSnapshot } from './deviceContinuity';
import { normalizeDriveFinderSnapshot } from './driveLocalFinder';

export interface FolderItem {
  id: string;
  name: string;
  path: string;
  parentId: string | null;
}
export interface FolderInventory {
  revision: number;
  folders: FolderItem[];
  skipped: number;
}
export interface FolderOptions {
  find: string;
  replace: string;
  prefix: string;
  suffix: string;
  destinationId: string | null | 'same';
}
export interface FolderMove {
  folder_id: string;
  expected_name: string;
  expected_parent_id: string | null;
  new_name: string;
  new_parent_id: string | null;
}
export interface FolderPlan {
  revision: number;
  changes: FolderMove[];
}
export interface FolderPreview {
  revision: number;
  count: number;
  ready: boolean;
  problems: string[];
}
export interface FolderApplied {
  changed: number;
  revision: number;
  dirty: boolean;
}
const record = (value: unknown): Record<string, unknown> | null =>
  !!value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const validParent = (id: unknown): id is string | null =>
  id === null || typeof id === 'string' && id.length > 0 && id.length <= 256;
const validName = (value: string): boolean =>
  value.length > 0 && new TextEncoder().encode(value).length <= 255
  && value.trim() === value && value !== '.' && value !== '..' && !value.endsWith('.')
  && !/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(value)
  && !/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(value);

export function parseFolderInventory(value: unknown): FolderInventory {
  const native = validateDriveSnapshot(value);
  const finder = normalizeDriveFinderSnapshot(value);
  const paths = new Map(finder.entries.filter(x => x.kind === 'folder')
    .map(x => [x.id, x.path]));
  const folders: FolderItem[] = [];
  const seen = new Set<string>();
  let skipped = finder.skipped + finder.truncated;
  for (const input of native.directories) {
    const row = record(input);
    const id = row?.id;
    const name = row?.name;
    const parent = row?.parent_id;
    if (row?.trashed_at !== null && row?.trashed_at !== undefined) continue;
    if (typeof id !== 'string' || typeof name !== 'string' || !validParent(parent)
        || !validName(name) || seen.has(id) || !paths.has(id)) {
      skipped++;
      continue;
    }
    folders.push({ id, name, parentId: parent, path: paths.get(id)! });
    seen.add(id);
  }
  folders.sort((a, b) => a.path.localeCompare(b.path, undefined, { sensitivity: 'base' }));
  return { revision: native.revision, folders, skipped };
}
export function buildFolderPlan(
  inventory: FolderInventory, selected: readonly string[], options: FolderOptions,
): FolderPlan {
  if (!selected.length || selected.length > 100 || selected.length !== new Set(selected).size) {
    throw Error('Choose 1–100 distinct folders.');
  }
  const folders = new Map(inventory.folders.map(f => [f.id, f]));
  if (options.destinationId !== 'same' && options.destinationId !== null
      && !folders.has(options.destinationId)) {
    throw Error('Destination folder is not visible in the latest Drive snapshot.');
  }
  return {
    revision: inventory.revision,
    changes: selected.map(id => {
      const folder = folders.get(id);
      if (!folder) throw Error('A selected folder is no longer visible.');
      const transformed = options.find
        ? folder.name.split(options.find).join(options.replace) : folder.name;
      const name = options.prefix + transformed + options.suffix;
      if (!validName(name)) throw Error('Invalid portable folder name: ' + name.slice(0, 100));
      return {
        folder_id: id, expected_name: folder.name, expected_parent_id: folder.parentId,
        new_name: name,
        new_parent_id: options.destinationId === 'same' ? folder.parentId : options.destinationId,
      };
    }),
  };
}
/** Read only a strict two-column CSV. Quoted commas, quotes and CRLF are
 * supported; ambiguous headers, ragged rows and oversized input fail closed.
 * This is a local preview plan, never an instruction to create missing folders.
 */
export function buildFolderCsvPlan(inventory: FolderInventory, csv: string): FolderPlan {
  if (typeof csv !== 'string' || new TextEncoder().encode(csv).length > 128 * 1024) {
    throw Error('Folder CSV exceeds the 128 KiB safety limit.');
  }
  const input = csv.replace(/^\uFEFF/, '');
  const rows: string[][] = [];
  let columns: string[] = [];
  let cell = '';
  let quoted = false;
  let closedQuote = false;
  let atStart = true;
  const finishCell = () => {
    columns.push(cell);
    cell = '';
    closedQuote = false;
    atStart = true;
    if (columns.length > 2) throw Error('Folder CSV must contain exactly two columns.');
  };
  const finishRow = () => {
    finishCell();
    if (columns.length !== 2) throw Error('Folder CSV must contain exactly two columns.');
    rows.push(columns);
    if (rows.length > 101) throw Error('Folder CSV allows at most 100 mappings.');
    columns = [];
  };
  for (let i = 0; i < input.length; i++) {
    const char = input[i];
    if (quoted) {
      if (char === '"') {
        if (input[i + 1] === '"') { cell += '"'; i++; }
        else { quoted = false; closedQuote = true; }
      } else {
        cell += char;
      }
      continue;
    }
    if (char === '"' && atStart) { quoted = true; atStart = false; continue; }
    if (char === '"' || (closedQuote && char !== ',' && char !== '\r' && char !== '\n')) {
      throw Error('Malformed CSV quotes in folder mapping.');
    }
    if (char === ',') { finishCell(); continue; }
    if (char === '\r' || char === '\n') {
      finishRow();
      if (char === '\r' && input[i + 1] === '\n') i++;
      continue;
    }
    cell += char;
    atStart = false;
  }
  if (quoted) throw Error('Unterminated quoted CSV field.');
  if (cell.length || columns.length || !atStart || closedQuote) finishRow();
  if (!rows.length || rows[0][0] !== 'source_path' || rows[0][1] !== 'new_path') {
    throw Error('Folder CSV header must be source_path,new_path.');
  }
  if (rows.length < 2) throw Error('Folder CSV must include 1–100 mapping rows.');
  const folders = new Map<string, FolderItem>();
  const ambiguous = new Set<string>();
  for (const folder of inventory.folders) {
    if (folders.has(folder.path)) ambiguous.add(folder.path);
    folders.set(folder.path, folder);
  }
  const seenSources = new Set<string>();
  const seenTargets = new Set<string>();
  const changes = rows.slice(1).map(([sourcePath, newPath], index): FolderMove => {
    const row = index + 2;
    if (!sourcePath.startsWith('/') || sourcePath.endsWith('/')
        || !newPath.startsWith('/') || newPath.endsWith('/')
        || newPath.includes('//') || sourcePath.includes('//')) {
      throw Error('Folder CSV row ' + row + ': use full absolute folder paths.');
    }
    if (ambiguous.has(sourcePath)) throw Error('Folder CSV row ' + row + ': ambiguous source folder.');
    const source = folders.get(sourcePath);
    if (!source) throw Error('Folder CSV row ' + row + ': source is not in the current Drive snapshot.');
    if (seenSources.has(source.id)) throw Error('Folder CSV row ' + row + ': duplicate source folder.');
    seenSources.add(source.id);
    const lastSlash = newPath.lastIndexOf('/');
    const parentPath = lastSlash === 0 ? '/' : newPath.slice(0, lastSlash);
    const newName = newPath.slice(lastSlash + 1);
    if (!validName(newName)) throw Error('Folder CSV row ' + row + ': unsafe destination folder name.');
    const parent = parentPath === '/' ? null : folders.get(parentPath);
    if (parent === undefined && parentPath !== '/') {
      throw Error('Folder CSV row ' + row + ': target parent must already exist.');
    }
    if (ambiguous.has(parentPath)) throw Error('Folder CSV row ' + row + ': ambiguous target parent.');
    const newParentId = parent === null ? null : parent?.id ?? null;
    if (source.name === newName && source.parentId === newParentId) {
      throw Error('Folder CSV row ' + row + ': no folder change requested.');
    }
    const portableTarget = newPath.normalize('NFC').toLocaleLowerCase();
    if (seenTargets.has(portableTarget)) throw Error('Folder CSV row ' + row + ': repeated target path.');
    seenTargets.add(portableTarget);
    return {
      folder_id: source.id, expected_name: source.name, expected_parent_id: source.parentId,
      new_name: newName, new_parent_id: newParentId,
    };
  });
  return { revision: inventory.revision, changes };
}

export function parseFolderPreview(value: unknown, plan: FolderPlan): FolderPreview {
  const row = record(value);
  if (!row || !Number.isSafeInteger(row.revision) || row.revision !== plan.revision
      || !Number.isSafeInteger(row.count) || row.count !== plan.changes.length
      || typeof row.ready !== 'boolean' || !Array.isArray(row.problems)
      || row.problems.length > 300 || row.problems.some(x => typeof x !== 'string')) {
    throw Error('Invalid or stale native folder preview.');
  }
  return { revision: row.revision as number, count: row.count as number,
    ready: row.ready, problems: row.problems as string[] };
}
export function parseFolderApplied(value: unknown, plan: FolderPlan): FolderApplied {
  const row = record(value);
  if (!row || !Number.isSafeInteger(row.changed) || row.changed !== plan.changes.length
      || !Number.isSafeInteger(row.revision) || (row.revision as number) <= plan.revision
      || typeof row.dirty !== 'boolean') {
    throw Error('Folder operation completed with an unexpected native result. Refresh.');
  }
  return { changed: row.changed as number, revision: row.revision as number,
    dirty: row.dirty };
}
