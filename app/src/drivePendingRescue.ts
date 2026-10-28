export interface RescueCandidate {
  id: string;
  name: string;
  size: number;
}
export interface RescueResult {
  pending_id: string;
  count: number;
  bytes: number;
  restored: number;
}
export function parseRescueCandidates(value: unknown): RescueCandidate[] {
  if (!Array.isArray(value) || value.length > 100) throw new Error('Invalid rescue inventory');
  const ids = new Set<string>();
  return value.map((row: unknown) => {
    if (!row || typeof row !== 'object') throw new Error('Invalid rescue row');
    const item = row as Record<string, unknown>;
    if (typeof item.id !== 'string' || !/^[a-zA-Z0-9_-]{1,256}$/.test(item.id) ||
        ids.has(item.id) || typeof item.name !== 'string' || item.name.length < 1 ||
        item.name.length > 1024 || typeof item.size !== 'number' ||
        !Number.isSafeInteger(item.size) || item.size < 0) throw new Error('Corrupt rescue entry');
    ids.add(item.id);
    return { id: item.id, name: item.name, size: item.size };
  });
}
export function parseRescueResult(value: unknown): RescueResult {
  if (!value || typeof value !== 'object') throw new Error('Invalid rescue result');
  const item = value as Record<string, unknown>;
  if (typeof item.pending_id !== 'string' || !/^[a-zA-Z0-9_-]{1,256}$/.test(item.pending_id) ||
      typeof item.count !== 'number' || !Number.isSafeInteger(item.count) ||
      item.count < 1 || item.count > 256 ||
      typeof item.bytes !== 'number' || !Number.isSafeInteger(item.bytes) ||
      item.bytes < 0 || item.bytes > 2147483648 ||
      typeof item.restored !== 'number' || !Number.isSafeInteger(item.restored) ||
      item.restored < 0 || item.restored > item.count)
    throw new Error('Invalid rescue result');
  return item as unknown as RescueResult;
}
