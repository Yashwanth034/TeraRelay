export interface DriveHealthIssue {
  pending_id: string;
  name: string;
  severity: 'warning' | 'critical';
  code: string;
  advice: string;
}
export interface DriveHealthReport {
  scanned: number;
  closed: number;
  open_writers: number;
  truncated: boolean;
  read_only: true;
  issues: DriveHealthIssue[];
}
const obj = (value: unknown): Record<string, unknown> | null =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const count = (v: unknown): v is number =>
  Number.isSafeInteger(v) && (v as number) >= 0 && (v as number) <= 1_000_000;

/** Fail closed on a malformed IPC payload: never show a fictitious healthy Drive. */
export function parseDriveHealthReport(input: unknown): DriveHealthReport {
  const row = obj(input);
  if (!row || !count(row.scanned) || !count(row.closed)
      || !count(row.open_writers) || row.closed + row.open_writers > row.scanned
      || typeof row.truncated !== 'boolean' || row.read_only !== true
      || !Array.isArray(row.issues) || row.issues.length > 200) {
    throw new Error('Drive staging health returned invalid summary metadata.');
  }
  const issues: DriveHealthIssue[] = row.issues.map((value: unknown) => {
    const item = obj(value);
    if (!item || typeof item.pending_id !== 'string' || !item.pending_id
        || item.pending_id.length > 512 || typeof item.name !== 'string'
        || item.name.length > 1024 || (item.severity !== 'warning' && item.severity !== 'critical')
        || typeof item.code !== 'string' || !/^[a-z_]{1,64}$/.test(item.code)
        || typeof item.advice !== 'string' || item.advice.length > 1500) {
      throw new Error('Drive staging health returned an invalid issue.');
    }
    return { pending_id: item.pending_id, name: item.name, severity: item.severity,
      code: item.code, advice: item.advice };
  });
  return { scanned: row.scanned, closed: row.closed, open_writers: row.open_writers,
    truncated: row.truncated, read_only: true, issues };
}
