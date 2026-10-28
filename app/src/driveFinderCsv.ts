import type { DriveFinderEntry } from './driveLocalFinder';

export interface DriveFinderCsvExport {
  csv: string;
  exported: number;
  omitted: number;
}

/** Bounded, explicit local Finder export. File names and paths can be pasted
 * into spreadsheets, so sanitize controls, separators and formula prefixes.
 * No node IDs, account metadata or file contents ever enter this report.
 */
export function formatDriveFinderCsv(
  entries: readonly DriveFinderEntry[],
  maxRows = 1000,
): DriveFinderCsvExport {
  const limit = Number.isSafeInteger(maxRows) ? Math.min(1000, Math.max(0, maxRows)) : 1000;
  const chosen = entries.slice(0, limit);
  const csvCell = (source: string): string => {
    const cleaned = source.replace(/[\u0000-\u001f\u007f-\u009f\u2028\u2029]/g, ' ')
      .replace(/\s+/g, ' ').trim().slice(0, 2048);
    // Quote every cell, including values beginning with a formula trigger.
    // The leading apostrophe is retained by generic CSV readers as literal
    // text; it prevents evaluation when pasted into Excel/LibreOffice.
    const text = /^[=+\-@]/.test(cleaned) ? "'" + cleaned : cleaned;
    return '"' + text.replace(/"/g, '""') + '"';
  };
  const lines = ['"Drive-relative path","Item type","Known size (bytes)"'];
  for (const entry of chosen) {
    const kind = entry.kind === 'file' ? 'File' : entry.kind === 'folder' ? 'Folder'
      : entry.kind === 'pending' ? 'Unfinished write' : 'Unknown';
    const bytes = entry.kind !== 'folder'
      && typeof entry.size === 'number' && Number.isSafeInteger(entry.size)
      && entry.size >= 0 ? String(entry.size) : 'Unknown';
    // Spreadsheet importers commonly round integers beyond 15 digits.
    // Preserve exact large byte sizes as a text cell rather than suggesting
    // an imprecise numeric value has been measured.
    const sizeCell = bytes !== 'Unknown' && bytes.length > 15 ? "'" + bytes : bytes;
    lines.push([csvCell(entry.path), csvCell(kind), csvCell(sizeCell)].join(','));
  }
  return { csv: lines.join('\r\n') + '\r\n',
    exported: chosen.length, omitted: entries.length - chosen.length };
}
