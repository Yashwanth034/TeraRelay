import { validateDriveSnapshot } from './deviceContinuity';
import { normalizeDriveFinderSnapshot, DRIVE_FINDER_MAX_ROWS } from './driveLocalFinder';

export interface DuplicateFile {
  id: string;
  path: string;
  size: number;
  fileId: string;
  storageVersion: number;
}
export interface DuplicateGroup {
  id: string;
  size: number;
  files: DuplicateFile[];
  remoteObjects: number;
  sharedObject: boolean;
  logicalExtraBytes: number;
}
export interface DuplicateReport {
  revision: number;
  dirty: boolean;
  scanned: number;
  eligible: number;
  excluded: number;
  truncated: boolean;
  groups: DuplicateGroup[];
}
export type DuplicateFilter = 'all' | 'separate' | 'shared';
export type DuplicateSort = 'impact' | 'count' | 'path';

const record = (value: unknown): Record<string, unknown> | null =>
  value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const exactHash = (value: unknown): value is string =>
  typeof value === 'string' && /^[0-9a-fA-F]{64}$/.test(value);
const safeBytes = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
const validId = (value: unknown): value is string =>
  typeof value === 'string' && value.length > 0 && value.length <= 256
    && value.trim() === value && !/[\/\\\u0000-\u001f\u007f]/.test(value);

function safeProduct(bytes: number, copies: number): number | null {
  const amount = bytes * copies;
  return Number.isSafeInteger(amount) && amount >= 0 ? amount : null;
}

/** Matches only content fingerprints whose encryption identities are compatible.
 * No bytes are read, and a matching size alone can NEVER imply identical content.
 * Remote storage reclamation is intentionally NOT inferred from logical aliases.
 */
export function inspectDriveDuplicates(value: unknown): DuplicateReport {
  const native = validateDriveSnapshot(value);
  const finder = normalizeDriveFinderSnapshot(value);
  const entries = new Map(finder.entries.filter(row => row.kind === 'file')
    .map(row => [row.id, row]));
  const groups = new Map<string, DuplicateFile[]>();
  // Finder failures are rechecked per file below; do not count them twice.
  // Excluded refers only to files among this bounded native scan.
  let excluded = 0;
  let eligible = 0;
  const seen = new Set<string>();
  const bounded = native.files.slice(0, DRIVE_FINDER_MAX_ROWS);
  for (const item of bounded) {
    const row = record(item);
    const id = row?.entry_id;
    if (!validId(id) || seen.has(id)) {
      excluded++;
      continue;
    }
    seen.add(id); // Ambiguous duplicate identities never fall through to another row.
    const fileId = row?.file_id;
    const match = typeof id === 'string' ? entries.get(id) : undefined;
    const fp = row?.content_fingerprint;
    const version = row?.encryption_version;
    const crypto = row?.crypto_id;
    const storageVersion = row?.storage_version;
    const size = row?.plaintext_size ?? row?.total_size;
    if (!match || row?.display_name !== match.name || !validId(fileId)
        || !exactHash(fp) || !safeBytes(size) || match.size !== size
        || !Number.isSafeInteger(version) || (version as number) < 0
        || (version as number) > 128 || !Number.isSafeInteger(storageVersion)
        || (storageVersion as number) < 1 || (storageVersion as number) > 2
        || !(crypto === null || crypto === undefined || validId(crypto))
        || ((version as number) > 0 && !validId(crypto))) {
      excluded++;
      continue;
    }
    eligible++;
    // Different encryption keys cannot be considered interchangeable.
    const key = JSON.stringify([fp.toLowerCase(), size, version, crypto ?? null]);
    const file = { id, path: match.path, size, fileId, storageVersion: storageVersion as number };
    const list = groups.get(key);
    if (list) list.push(file);
    else groups.set(key, [file]);
  }
  const duplicates: DuplicateGroup[] = [];
  for (const [key, files] of groups) {
    if (files.length < 2) continue;
    // A V1 logical file and a V2 pool object live in different namespaces.
    // Equal ID strings across storage versions are NOT the same remote object.
    const storageIds = new Set(files.map(file => JSON.stringify([file.storageVersion, file.fileId])));
    const logicalExtraBytes = safeProduct(files[0].size, files.length - 1);
    if (logicalExtraBytes === null) {
      excluded += files.length;
      continue;
    }
    files.sort((a, b) => a.path.localeCompare(b.path) || a.id.localeCompare(b.id));
    duplicates.push({
      id: key, size: files[0].size, files,
      remoteObjects: storageIds.size,
      sharedObject: storageIds.size === 1,
      logicalExtraBytes,
    });
  }
  duplicates.sort((a, b) => b.logicalExtraBytes - a.logicalExtraBytes
    || b.files.length - a.files.length
    || a.files[0].path.localeCompare(b.files[0].path));
  return {
    revision: native.revision,
    dirty: native.dirty,
    scanned: bounded.length,
    eligible,
    excluded,
    truncated: native.files.length > DRIVE_FINDER_MAX_ROWS || finder.truncated > 0,
    groups: duplicates,
  };
}

export function filterDuplicateGroups(
  groups: readonly DuplicateGroup[], query: string,
  kind: DuplicateFilter, order: DuplicateSort,
): DuplicateGroup[] {
  const needle = query.trim().toLocaleLowerCase();
  return groups.filter(group =>
    (kind === 'all' || (kind === 'shared') === group.sharedObject)
    && (!needle || group.files.some(file => file.path.toLocaleLowerCase().includes(needle))))
    .sort((a, b) => order === 'count'
      ? b.files.length - a.files.length || b.logicalExtraBytes - a.logicalExtraBytes
      : order === 'path'
        ? a.files[0].path.localeCompare(b.files[0].path)
        : b.logicalExtraBytes - a.logicalExtraBytes || b.files.length - a.files.length);
}

const csvCell = (value: string): string => {
  // Prevent spreadsheet formula evaluation when the user opens an exported CSV.
  const safe = /^[=+@\-\t\r]/.test(value) ? "'" + value : value;
  return '"' + safe.replace(/"/g, '""') + '"';
};

/** Advisory export only. Rows are NOT deletion instructions or space forecasts. */
export function duplicateReviewCsv(groups: readonly DuplicateGroup[]): string {
  const rows = ['Group,Path,Size bytes,Underlying objects,Storage relationship,Review action'];
  groups.forEach((group, index) => {
    for (const file of group.files) {
      rows.push([
        String(index + 1), file.path, String(file.size),
        String(group.remoteObjects), group.sharedObject ? 'Shared remote object' : 'Distinct storage IDs',
        'Inspect manually; no deletion scheduled',
      ].map(csvCell).join(','));
    }
  });
  return rows.join('\r\n');
}
