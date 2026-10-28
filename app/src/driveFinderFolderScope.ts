import type { DriveFinderEntry } from './driveLocalFinder';

/** Empty scope means the entire indexed Drive; all other scopes are strict,
 * canonical Drive-relative directory paths (never OS mount paths). */
function safeDriveFolderPath(path: unknown): path is string {
  return typeof path === 'string' && path.startsWith('/') && path.length <= 32768
    && !/[\\\u0000-\u001f\u007f-\u009f\u2028\u2029]/.test(path)
    && !path.split('/').slice(1).some(part => !part || part === '.' || part === '..');
}

export function filterDriveFinderWithinFolder<T extends DriveFinderEntry>(
  entries: readonly T[],
  folderPath: string,
): T[] {
  if (folderPath === '') return [...entries];
  if (!safeDriveFolderPath(folderPath)) return [];
  const prefix = folderPath + '/';
  return entries.filter(entry => safeDriveFolderPath(entry.path)
    && entry.path.startsWith(prefix));
}

export function parentDriveFinderFolder(folderPath: string): string {
  if (!safeDriveFolderPath(folderPath)) return '';
  const lastSlash = folderPath.lastIndexOf('/');
  return lastSlash <= 0 ? '' : folderPath.slice(0, lastSlash);
}
