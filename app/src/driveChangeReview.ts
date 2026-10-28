export type DriveChangeStatus = 'added' | 'missing' | 'renamed' | 'moved' | 'version_changed';
export type DriveChangeKind = 'file' | 'folder';
export interface DriveChange {
  kind: DriveChangeKind;
  status: DriveChangeStatus;
  before_path: string | null;
  after_path: string | null;
  content_evidence: string;
}
export interface DriveChangeReview {
  generation: number;
  captured_at: number | null;
  baseline_revision: number | null;
  current_revision: number;
  current_dirty: boolean;
  current_pending: number;
  tracked_files: number;
  tracked_folders: number;
  baseline_count: number;
  change_count: number;
  truncated: boolean;
  changes: DriveChange[];
}
export type DriveChangeFilter = 'all' | DriveChangeStatus;

const record = (value: unknown): Record<string, unknown> | null =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const nonnegative = (value: unknown): value is number =>
  typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
const pathOkay = (value: unknown): value is string =>
  typeof value === 'string' && value.startsWith('/')
    && value.length <= 26_000
    && !/[\u0000-\u001f\u007f]/.test(value);
const statuses = new Set<DriveChangeStatus>(['added', 'missing', 'renamed', 'moved', 'version_changed']);

export function parseDriveChangeReview(value: unknown): DriveChangeReview {
  const row = record(value);
  if (!row || !nonnegative(row.generation) || !nonnegative(row.current_revision)
      || !nonnegative(row.current_pending) || !nonnegative(row.tracked_files)
      || !nonnegative(row.tracked_folders) || !nonnegative(row.baseline_count)
      || !nonnegative(row.change_count) || row.change_count > 50_000
      || typeof row.current_dirty !== 'boolean' || typeof row.truncated !== 'boolean'
      || !(row.captured_at === null || nonnegative(row.captured_at))
      || !(row.baseline_revision === null || nonnegative(row.baseline_revision))
      || !Array.isArray(row.changes) || row.changes.length > 5_000
      || row.changes.length > row.change_count
      || (row.truncated !== (row.change_count > row.changes.length))
      || (row.generation === 0 && (row.captured_at !== null || row.baseline_revision !== null
        || row.changes.length !== 0 || row.baseline_count !== 0))
      || (row.generation > 0 && (row.captured_at === null || row.baseline_revision === null))) {
    throw new TypeError('Untrusted Drive Change Review response. No checkpoint was changed.');
  }
  const changes: DriveChange[] = row.changes.map(raw => {
    const entry = record(raw);
    if (!entry || (entry.kind !== 'file' && entry.kind !== 'folder')
        || !statuses.has(entry.status as DriveChangeStatus)
        || !(entry.before_path === null || pathOkay(entry.before_path))
        || !(entry.after_path === null || pathOkay(entry.after_path))
        || typeof entry.content_evidence !== 'string' || entry.content_evidence.length > 400
        || (entry.status === 'added' && (entry.before_path !== null || entry.after_path === null))
        || (entry.status === 'missing' && (entry.before_path === null || entry.after_path !== null))
        || (!['added', 'missing'].includes(entry.status as string)
            && (entry.before_path === null || entry.after_path === null))) {
      throw new TypeError('Untrusted Drive Change Review entry. No checkpoint was changed.');
    }
    return {
      kind: entry.kind as DriveChangeKind,
      status: entry.status as DriveChangeStatus,
      before_path: entry.before_path as string | null,
      after_path: entry.after_path as string | null,
      content_evidence: entry.content_evidence as string,
    };
  });
  return {
    generation: row.generation,
    captured_at: row.captured_at as number | null,
    baseline_revision: row.baseline_revision as number | null,
    current_revision: row.current_revision,
    current_dirty: row.current_dirty,
    current_pending: row.current_pending,
    tracked_files: row.tracked_files,
    tracked_folders: row.tracked_folders,
    baseline_count: row.baseline_count,
    change_count: row.change_count,
    truncated: row.truncated,
    changes,
  };
}

export function filterDriveChanges(
  rows: readonly DriveChange[], filter: DriveChangeFilter, query: string,
): DriveChange[] {
  const needle = query.trim().toLocaleLowerCase();
  return rows.filter(change => (filter === 'all' || change.status === filter)
    && (!needle || (change.before_path ?? '').toLocaleLowerCase().includes(needle)
      || (change.after_path ?? '').toLocaleLowerCase().includes(needle)));
}

const csvCell = (value: string): string => {
  const safe = /^[=+@\-\t\r]/.test(value) ? "'" + value : value;
  return '"' + safe.replace(/"/g, '""') + '"';
};

/** Advisory list of local index changes; NEVER a restore or deletion plan. */
export function driveChangesCsv(rows: readonly DriveChange[]): string {
  const output = ['Type,Status,Before path,After path,Evidence'];
  for (const change of rows) {
    output.push([
      change.kind, change.status, change.before_path ?? '', change.after_path ?? '',
      change.content_evidence,
    ].map(csvCell).join(','));
  }
  return output.join('\r\n');
}
