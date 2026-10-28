import type { DriveFinderEntry } from './driveLocalFinder';

export type DriveFinderSortOrder = 'path' | 'largest' | 'smallest';

function knownFileSize(row: DriveFinderEntry): number | null {
  // Folder sizes are intentionally unknown: their descendant logical totals
  // are a different metric in the storage-insights breakdown.
  return (row.kind === 'file' || row.kind === 'pending')
    && typeof row.size === 'number'
    && Number.isSafeInteger(row.size) && row.size >= 0 ? row.size : null;
}

/** Sort only a presentation copy. Unknown file sizes and all folders sort
 * last in size modes, without inventing folder size or modifying backend data.
 * Equal sizes retain native/path ordering for repeatable browsing.
 */
export function sortDriveFinderEntries<T extends DriveFinderEntry>(
  entries: readonly T[], order: DriveFinderSortOrder,
): T[] {
  const rows = [...entries];
  if (order === 'path') {
    rows.sort((a, b) => a.path.localeCompare(b.path, undefined, { sensitivity: 'base' }));
  } else if (order === 'largest' || order === 'smallest') {
    rows.sort((a, b) => {
      const first = knownFileSize(a);
      const second = knownFileSize(b);
      if (first === null) return second === null ? 0 : 1;
      if (second === null) return -1;
      return order === 'largest' ? second - first : first - second;
    });
  }
  return rows;
}
