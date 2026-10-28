import { parseFolderInventory, type FolderInventory } from './driveFolderOperations';

export type CloneInventory = FolderInventory;
export { parseFolderInventory as readCloneInventory };

export interface FolderCloneRequest {
  revision: number;
  source_id: string;
  expected_source_name: string;
  expected_source_parent_id: string | null;
  destination_parent_id: string | null;
  new_root_name: string;
}
export interface FolderClonePreview {
  revision: number;
  ready: boolean;
  count: number;
  not_copied_files: number;
  not_copied_pending: number;
  paths: string[];
  truncated: boolean;
  problems: string[];
}
export interface FolderCloneApplied {
  created: number;
  root_id: string;
  revision: number;
  dirty: boolean;
}
function obj(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
export function validCloneName(value: string): boolean {
  if (!value || value.trim() !== value || value === '.' || value === '..'
      || value.endsWith('.') || new TextEncoder().encode(value).length > 255
      || /[<>:"/\\|?*\u0000-\u001f\u007f]/.test(value)) return false;
  const stem = value.split('.')[0].toUpperCase();
  return !/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9]|COM[¹²³]|LPT[¹²³])$/.test(stem);
}
export function buildCloneRequest(
  inventory: CloneInventory,
  sourceId: string,
  destinationId: string | null,
  name: string,
): FolderCloneRequest {
  const source = inventory.folders.find(item => item.id === sourceId);
  if (!source) throw Error('Source folder is not in the current visible Drive index.');
  if (destinationId !== null && !inventory.folders.some(item => item.id === destinationId)) {
    throw Error('Destination folder is not in the current visible Drive index.');
  }
  if (!validCloneName(name)) throw Error('Choose a valid portable title for the new top folder.');
  return {
    revision: inventory.revision,
    source_id: source.id,
    expected_source_name: source.name,
    expected_source_parent_id: source.parentId,
    destination_parent_id: destinationId,
    new_root_name: name,
  };
}
export function decodeClonePreview(value: unknown, request: FolderCloneRequest): FolderClonePreview {
  const row = obj(value);
  if (!row || !Number.isSafeInteger(row.revision) || row.revision !== request.revision
      || typeof row.ready !== 'boolean' || !Number.isSafeInteger(row.count)
      || (row.count as number) < 0 || (row.count as number) > 512
      || !Number.isSafeInteger(row.not_copied_files) || (row.not_copied_files as number) < 0
      || !Number.isSafeInteger(row.not_copied_pending) || (row.not_copied_pending as number) < 0
      || !Array.isArray(row.paths) || row.paths.length > 80
      || row.paths.some(path => typeof path !== 'string' || path.length > 4096)
      || typeof row.truncated !== 'boolean'
      || !Array.isArray(row.problems) || row.problems.length > 256
      || row.problems.some(error => typeof error !== 'string' || error.length > 2000)
      || (row.ready && (row.problems.length > 0 || (row.count as number) < 1))
      || (!row.ready && (row.count !== 0 || row.paths.length !== 0))) {
    throw Error('Invalid or stale native folder clone preview.');
  }
  return {
    revision: row.revision as number, ready: row.ready,
    count: row.count as number,
    not_copied_files: row.not_copied_files as number,
    not_copied_pending: row.not_copied_pending as number,
    paths: row.paths as string[],
    truncated: row.truncated, problems: row.problems as string[],
  };
}
export function decodeCloneApplied(value: unknown, request: FolderCloneRequest,
  preview: FolderClonePreview): FolderCloneApplied {
  const row = obj(value);
  if (!row || !Number.isSafeInteger(row.created) || row.created !== preview.count
      || typeof row.root_id !== 'string' || row.root_id.length < 1 || row.root_id.length > 256
      || row.root_id === request.source_id
      || !Number.isSafeInteger(row.revision) || (row.revision as number) <= request.revision
      || typeof row.dirty !== 'boolean' || row.dirty !== true) {
    throw Error('Folder tree copy response cannot be verified; refresh Drive before retrying.');
  }
  return {
    created: row.created as number, root_id: row.root_id,
    revision: row.revision as number, dirty: row.dirty,
  };
}
