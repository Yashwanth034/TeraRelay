/** Pure local presentation filter for Drive's recently-deleted entries.
 * Never changes restore eligibility or the server's newest-first ordering.
 */
export type DriveTrashFilterKind = 'all' | 'file' | 'folder' | 'symlink';
export type DriveTrashSortOrder = 'newest' | 'oldest' | 'name' | 'largest' | 'smallest';
export type DriveTrashAvailability = 'all' | 'restorable' | 'unavailable';
export type DriveTrashAgeRange = 'all' | '24h' | '7d' | '30d';

export interface SafeDriveTrashEntry {
    kind: 'file' | 'folder' | 'symlink';
    id: string;
    name: string;
    deleted_at: number;
    original_directory_id: string | null;
    size: number | null;
    restorable: boolean;
}

/** Fail closed on malformed provider payloads, retaining every valid row.
 * Never manufacture a restorable identity or modify the backend source.
 * A duplicate kind:id cannot be distinguished by the native restore command.
 */
export function normalizeDriveTrashEntries(value: unknown): {
    entries: SafeDriveTrashEntry[];
    skipped: number;
} {
    if (!Array.isArray(value)) throw new TypeError('Native Drive trash response was not a list');
    const entries: SafeDriveTrashEntry[] = [];
    const identities = new Set<string>();
    let skipped = 0;
    for (const candidate of value) {
        if (!candidate || typeof candidate !== 'object' || Array.isArray(candidate)) {
            skipped++;
            continue;
        }
        const row = candidate as Record<string, unknown>;
        if ((row.kind !== 'file' && row.kind !== 'folder' && row.kind !== 'symlink')
            || typeof row.id !== 'string' || row.id.trim() === '' || row.id.includes('\0')
            || typeof row.name !== 'string' || row.name.trim() === '' || row.name.includes('\0')
            || typeof row.deleted_at !== 'number' || !Number.isSafeInteger(row.deleted_at)
            || row.deleted_at < 0 || row.deleted_at > 8_640_000_000_000_000
            || typeof row.restorable !== 'boolean') {
            skipped++;
            continue;
        }
        const identity = row.kind + ':' + row.id;
        if (identities.has(identity)) {
            skipped++;
            continue;
        }
        identities.add(identity);
        entries.push({
            kind: row.kind,
            id: row.id,
            name: row.name,
            deleted_at: row.deleted_at,
            restorable: row.restorable,
            original_directory_id: typeof row.original_directory_id === 'string'
                && row.original_directory_id.trim() !== '' ? row.original_directory_id : null,
            size: typeof row.size === 'number' && Number.isSafeInteger(row.size)
                && row.size >= 0 ? row.size : null,
        });
    }
    return { entries, skipped };
}

/** Inclusive rolling time-window search over native millisecond tombstone dates.
 * Pass "now" explicitly so boundary behavior can be tested deterministically.
 * Timestamps in the future or otherwise invalid do not match a past window,
 * but remain accessible when showing all entries.
 */
export function filterDriveTrashByAge<T extends { deleted_at: number }>(
    entries: readonly T[], range: DriveTrashAgeRange, nowMs: number,
): T[] {
    if (range === 'all') return [...entries];
    if (!Number.isFinite(nowMs)) return [];
    const daysMs = 24 * 60 * 60 * 1000;
    const windowMs = range === '24h' ? daysMs : range === '7d' ? 7 * daysMs : 30 * daysMs;
    const cutoff = nowMs - windowMs;
    return entries.filter(entry => Number.isFinite(entry.deleted_at)
        && entry.deleted_at >= cutoff && entry.deleted_at <= nowMs);
}

/** Copy a bounded, sanitized text inventory for explicit user export.
 * Export only display metadata. Never expose opaque native IDs or paths.
 * Use a list prefix so pasted spreadsheet rows cannot start as formulas.
 */
export function formatDriveTrashInventory(
    entries: readonly SafeDriveTrashEntry[], maxRows = 500,
): { text: string; exported: number; omitted: number } {
    const limit = Number.isSafeInteger(maxRows) ? Math.max(0, Math.min(maxRows, 500)) : 500;
    const shown = entries.slice(0, limit);
    const safeName = (name: string) => name
        .replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/g, ' ')
        .replace(/\s+/g, ' ').trim().slice(0, 220) || '(unnamed)';
    const lines = shown.map(entry => {
        const deleted = Number.isSafeInteger(entry.deleted_at)
            && entry.deleted_at >= 0 && entry.deleted_at <= 8_640_000_000_000_000
            ? new Date(entry.deleted_at).toISOString() : 'Unknown';
        const size = entry.kind === 'file' ? formatDriveTrashBytes(entry.size) : 'Not available';
        return `• ${safeName(entry.name)} | ${entry.kind} | Deleted: ${deleted} | ${size} | ${entry.restorable ? 'Restorable' : 'Unavailable'}`;
    });
    const omitted = entries.length - shown.length;
    const header = `TeraRelay Drive — Recently Deleted (filtered inventory)\n${shown.length} entries copied${omitted > 0 ? `; ${omitted} additional matches omitted (500 entry limit)` : ''}.\nNo internal identifiers or file paths included.`;
    return { text: [header, ...lines].join('\n'), exported: shown.length, omitted };
}

/** Presentation-only availability filter; never changes a native restore decision. */
export function filterDriveTrashAvailability<T extends { restorable: boolean }>(
    entries: readonly T[], availability: DriveTrashAvailability,
): T[] {
    if (availability === 'all') return [...entries];
    return entries.filter(entry => availability === 'restorable' ? entry.restorable : !entry.restorable);
}

/** Only native file byte sizes are meaningful for restore prioritization.
 * Folders, symlinks, missing values and corrupt optional sizes sort last.
 */
function knownTrashFileSize(entry: { kind?: string; size?: number | null }): number | null {
    return entry.kind === 'file' && typeof entry.size === 'number'
        && Number.isSafeInteger(entry.size) && entry.size >= 0 ? entry.size : null;
}

export function formatDriveTrashBytes(value: number | bigint | null | undefined): string {
    if (typeof value === 'number' && (!Number.isSafeInteger(value) || value < 0)) return 'Size unknown';
    if (typeof value !== 'number' && typeof value !== 'bigint') return 'Size unknown';
    const bytes = BigInt(value);
    if (bytes < 0n) return 'Size unknown';
    if (bytes < 1024n) return `${bytes} B`;
    const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'];
    let divisor = 1024n;
    let index = 0;
    while (bytes >= divisor * 1024n && index < units.length - 1) {
        divisor *= 1024n;
        index++;
    }
    // Sum large selected sets using integers, not inaccurate floating-point
    // byte counts. Round only the human-facing single-decimal display.
    const tenths = (bytes * 10n + divisor / 2n) / divisor;
    const amount = Number(tenths) / 10;
    return `${amount.toFixed(amount < 10 ? 1 : 0)} ${units[index]}`;
}

/** A selected folder's nested bytes are not known, so never invent them or
 * double-count child files. This is the subtotal of known FILE sizes only.
 */
export function summarizeSelectedDriveTrashSizes(
    entries: readonly { kind: string; size?: number | null }[],
): { knownBytes: bigint; knownFiles: number; unknownFiles: number } {
    let knownBytes = 0n;
    let knownFiles = 0;
    let unknownFiles = 0;
    for (const entry of entries) {
        if (entry.kind !== 'file') continue;
        const bytes = knownTrashFileSize(entry);
        if (bytes === null) unknownFiles++;
        else {
            knownBytes += BigInt(bytes);
            knownFiles++;
        }
    }
    return { knownBytes, knownFiles, unknownFiles };
}

/** Keep backend newest-first order unless the user chooses another order.
 * Stable sorting preserves native order for equal sizes or unknown sizes.
 */
export function sortDriveTrashEntries<T extends { name: string; deleted_at: number; kind?: string; size?: number | null }>(
    entries: readonly T[], order: DriveTrashSortOrder,
): T[] {
    const result = [...entries];
    if (order === 'oldest') {
        result.sort((a, b) => a.deleted_at - b.deleted_at);
    } else if (order === 'name') {
        result.sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }));
    } else if (order === 'largest' || order === 'smallest') {
        result.sort((a, b) => {
            const left = knownTrashFileSize(a);
            const right = knownTrashFileSize(b);
            if (left === null) return right === null ? 0 : 1;
            if (right === null) return -1;
            return order === 'largest' ? right - left : left - right;
        });
    }
    return result;
}

export function filterDriveTrashEntries<
    T extends { name: string; kind: Exclude<DriveTrashFilterKind, 'all'> },
>(entries: readonly T[], query: string, filter: DriveTrashFilterKind): T[] {
    const search = query.trim().toLocaleLowerCase();
    return entries.filter(entry =>
        (filter === 'all' || entry.kind === filter)
        && (search === '' || entry.name.toLocaleLowerCase().includes(search)),
    );
}
