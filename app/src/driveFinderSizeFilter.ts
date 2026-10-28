import type { DriveFinderEntry } from './driveLocalFinder';

export type DriveFinderSizeFilter = 'all' | '10m' | '100m' | '1g' | 'empty';
const MIB = 1024 * 1024;

export function filterDriveFinderBySize<T extends DriveFinderEntry>(
  entries: readonly T[], filter: DriveFinderSizeFilter,
): T[] {
  if (filter === 'all') return [...entries];
  const minimum = filter === '10m' ? 10 * MIB :
    filter === '100m' ? 100 * MIB : filter === '1g' ? 1024 * MIB : 0;
  return entries.filter(entry => {
    // A folder's total descendants are not the file's own length. Unknown
    // sizes must never pass the filter or be treated as empty.
    if (entry.kind !== 'file' && entry.kind !== 'pending') return false;
    if (typeof entry.size !== 'number' || !Number.isSafeInteger(entry.size) || entry.size < 0) return false;
    return filter === 'empty' ? entry.size === 0 : entry.size >= minimum;
  });
}
