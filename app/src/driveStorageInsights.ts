import { DRIVE_FINDER_MAX_ROWS, type DriveFinderEntry } from './driveLocalFinder';

export interface DriveFolderSizeInsight {
  path: string;
  knownBytes: bigint;
  knownFiles: number;
  unknownFiles: number;
}
export interface DriveStorageInsights {
  files: number;
  folders: number;
  pendingWrites: number;
  knownFileBytes: bigint;
  unknownFileSizes: number;
  knownPendingBytes: bigint;
  unknownPendingSizes: number;
  largestFolders: DriveFolderSizeInsight[];
}

// Insights are advisory metadata *only*, never physical disk consumption or
// remote Telegram quota. The same logical object can appear under hardlinks.
const sanePath = (path: unknown): path is string =>
  typeof path === 'string' && path.startsWith('/') && path.length <= 32768
    && !/[\\\u0000-\u001f\u007f-\u009f\u2028\u2029]/.test(path)
    && !path.split('/').slice(1).some(part => !part || part === '.' || part === '..');
const saneSize = (bytes: unknown): bytes is number =>
  typeof bytes === 'number' && Number.isSafeInteger(bytes) && bytes >= 0;

/** Count each regular file once in global logical-byte totals. Subfolder
 * rollups intentionally contain descendant files, but are never summed into
 * the overall total again. Pending writes have a separate subtotal.
 */
export function summarizeDriveStorage(
  entries: readonly DriveFinderEntry[],
  maxFolders = 5,
): DriveStorageInsights {
  const folders = new Map<string, DriveFolderSizeInsight>();
  let files = 0;
  let folderCount = 0;
  let pendingWrites = 0;
  let knownFileBytes = 0n;
  let unknownFileSizes = 0;
  let knownPendingBytes = 0n;
  let unknownPendingSizes = 0;

  // Duplicate view rows must not inflate totals, even if callers bypass
  // the Finder's strict native snapshot identity normalization.
  const seen = new Set<string>();
  const safe = entries.slice(0, DRIVE_FINDER_MAX_ROWS).filter(entry => {
    if (!sanePath(entry.path) || !['file', 'folder', 'pending'].includes(entry.kind)) return false;
    if (typeof entry.id !== 'string' || entry.id.length === 0) return false;
    const key = entry.kind + ':' + entry.id;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
  for (const entry of safe) {
    if (entry.kind !== 'folder') continue;
    folderCount++;
    if (!folders.has(entry.path)) {
      folders.set(entry.path, { path: entry.path, knownBytes: 0n, knownFiles: 0, unknownFiles: 0 });
    }
  }
  for (const entry of safe) {
    if (entry.kind === 'folder') continue;
    const sizeBytes = saneSize(entry.size) ? BigInt(entry.size) : null;
    if (entry.kind === 'pending') {
      pendingWrites++;
      if (sizeBytes !== null) knownPendingBytes += sizeBytes;
      else unknownPendingSizes++;
      continue;
    }
    files++;
    if (sizeBytes !== null) knownFileBytes += sizeBytes;
    else unknownFileSizes++;

    // For a/b/c/file.ext, increment a, a/b and a/b/c if known folders.
    const parts = entry.path.split('/').slice(1, -1);
    let prefix = '';
    for (const part of parts.slice(0, 100)) {
      prefix += '/' + part;
      const folder = folders.get(prefix);
      if (!folder) continue;
      if (sizeBytes !== null) {
        folder.knownBytes += sizeBytes;
        folder.knownFiles++;
      } else {
        folder.unknownFiles++;
      }
    }
  }
  const count = Number.isSafeInteger(maxFolders)
    ? Math.min(10, Math.max(0, maxFolders)) : 5;
  const largestFolders = [...folders.values()]
    .sort((a, b) => a.knownBytes === b.knownBytes
      ? a.path.localeCompare(b.path) : a.knownBytes > b.knownBytes ? -1 : 1)
    .slice(0, count);
  return { files, folders: folderCount, pendingWrites, knownFileBytes,
    unknownFileSizes, knownPendingBytes, unknownPendingSizes, largestFolders };
}
