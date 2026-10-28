export type TransferStatus = 'queued' | 'uploading' | 'backoff' | 'paused';
export interface PendingTransfer {
  pending_id: string;
  file_name: string;
  size: number;
  status: TransferStatus;
  failed_attempts: number;
  retry_in_seconds: number;
}
export interface TransferSnapshot {
  paused: boolean;
  automatic_worker_running: boolean;
  total_pending: number;
  truncated: boolean;
  transfers: PendingTransfer[];
}
export interface TransferAction {
  attempted: boolean;
  recovered: boolean;
  remains_pending: boolean;
}
const record = (value: unknown): Record<string, unknown> | null =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
const safeCount = (v: unknown): v is number =>
  typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
const validId = (v: unknown): v is string =>
  typeof v === 'string' && v.length > 0 && v.length <= 256 && /^[a-z0-9_-]+$/i.test(v);
const statuses: TransferStatus[] = ['queued','uploading','backoff','paused'];
export function parseTransferSnapshot(value: unknown): TransferSnapshot {
  const row = record(value);
  if (!row || typeof row.paused !== 'boolean' || typeof row.automatic_worker_running !== 'boolean'
      || !safeCount(row.total_pending) || typeof row.truncated !== 'boolean'
      || !Array.isArray(row.transfers) || row.transfers.length > 100
      || row.transfers.length > row.total_pending
      || (row.total_pending > row.transfers.length && row.truncated !== true)) {
    throw new Error('Invalid native Drive transfer inventory.');
  }
  const seen = new Set<string>();
  const transfers: PendingTransfer[] = row.transfers.map((item: unknown) => {
    const r = record(item);
    if (!r || !validId(r.pending_id) || seen.has(r.pending_id)
        || typeof r.file_name !== 'string' || r.file_name.length < 1 || r.file_name.length > 1024
        || !safeCount(r.size) || !statuses.includes(r.status as TransferStatus)
        || !safeCount(r.failed_attempts) || !safeCount(r.retry_in_seconds)
        || (r.status === 'backoff' && r.retry_in_seconds === 0)) {
      throw new Error('Invalid pending Drive transfer record.');
    }
    seen.add(r.pending_id);
    return {
      pending_id: r.pending_id, file_name: r.file_name, size: r.size,
      status: r.status as TransferStatus, failed_attempts: r.failed_attempts,
      retry_in_seconds: r.retry_in_seconds,
    };
  });
  return {
    paused: row.paused, automatic_worker_running: row.automatic_worker_running,
    total_pending: row.total_pending, truncated: row.truncated, transfers,
  };
}
export function parseTransferAction(value: unknown): TransferAction {
  const row = record(value);
  if (!row || typeof row.attempted !== 'boolean' || typeof row.recovered !== 'boolean'
    || typeof row.remains_pending !== 'boolean' || (row.recovered && !row.attempted)) {
    throw new Error('Invalid Drive transfer outcome.');
  }
  return { attempted: row.attempted, recovered: row.recovered, remains_pending: row.remains_pending };
}
