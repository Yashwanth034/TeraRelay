import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { History, RefreshCw, Copy, ShieldCheck, Loader2 } from 'lucide-react';
import {
  parseDriveChangeReview, filterDriveChanges, driveChangesCsv,
  type DriveChangeReview, type DriveChangeFilter,
} from '../../../driveChangeReview';

const buttonStyle = 'rounded-lg bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const inputStyle = 'rounded-lg border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
const statusLabel: Record<Exclude<DriveChangeFilter, 'all'>, string> = {
  added: 'Added to index',
  missing: 'Missing from index',
  moved: 'Moved path',
  renamed: 'Renamed',
  version_changed: 'Content / storage revision',
};

export function DriveChangeReviewPanel({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [report, setReport] = useState<DriveChangeReview | null>(null);
  const [busy, setBusy] = useState<'inspect' | 'capture' | null>(null);
  const [copying, setCopying] = useState(false);
  const [armed, setArmed] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [kind, setKind] = useState<DriveChangeFilter>('all');
  const [query, setQuery] = useState('');
  const [visible, setVisible] = useState(60);
  const generation = useRef(0);
  const inFlight = useRef(false);

  useEffect(() => {
    if (active) return;
    if (inFlight.current) {
      setOpen(false);
      return;
    }
    generation.current++;
    setOpen(false);
    setReport(null);
    setArmed(false);
    setBusy(null);
  }, [active]);

  const inspect = async () => {
    if (!active || inFlight.current) return;
    inFlight.current = true;
    const id = ++generation.current;
    setBusy('inspect');
    setError(null);
    try {
      const result = parseDriveChangeReview(
        await invoke<unknown>('cmd_drive_change_review_inspect'),
      );
      if (id !== generation.current) return;
      setReport(result);
      setArmed(false);
      setVisible(60);
    } catch (cause) {
      if (id === generation.current) {
        setError('Could not verify current local Drive metadata: ' + String(cause));
      }
    } finally {
      inFlight.current = false;
      if (id === generation.current) setBusy(null);
    }
  };

  const capture = async () => {
    if (!active || !report || inFlight.current) return;
    if (!armed) {
      setArmed(true);
      setNotice(null);
      return;
    }
    setArmed(false);
    inFlight.current = true;
    const id = ++generation.current;
    setBusy('capture');
    setError(null);
    setNotice(null);
    try {
      const response = parseDriveChangeReview(
        await invoke<unknown>('cmd_drive_change_review_capture', {
          expectedGeneration: report.generation,
        }),
      );
      if (id !== generation.current) return;
      setReport(response);
      setNotice('New checkpoint saved on this device. No Drive files or cloud metadata were changed.');
      setVisible(60);
    } catch (cause) {
      if (id === generation.current) {
        setError('Checkpoint outcome is uncertain: ' + String(cause)
          + '. Inspect the current checkpoint before retrying.');
      }
    } finally {
      inFlight.current = false;
      if (id === generation.current) setBusy(null);
    }
  };

  const changes = useMemo(() => filterDriveChanges(
    report?.changes ?? [], kind, query,
  ), [report, kind, query]);

  const copy = async () => {
    if (!report || copying || inFlight.current || changes.length === 0) return;
    setCopying(true);
    setError(null);
    try {
      await navigator.clipboard.writeText(driveChangesCsv(changes));
      setNotice('Change review copied as CSV. This is not a deletion or recovery instruction.');
    } catch (cause) {
      setError('Could not copy the change review: ' + String(cause));
    } finally {
      setCopying(false);
    }
  };

  return (
    <section aria-label="Drive Change Review" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <History className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">TeraRelay Drive · Change Review</p>
            <p className="text-xs text-telegram-subtext">
              Compare the current local Drive index with a saved checkpoint and see added, missing, moved, renamed or replaced files and folders.
            </p>
          </div>
        </div>
        <button type="button" className={buttonStyle} disabled={busy !== null || copying}
          aria-expanded={open} onClick={() => {
            if (inFlight.current) return;
            if (open) { generation.current++; setOpen(false); setArmed(false); }
            else { setOpen(true); void inspect(); }
          }}>
          {open ? 'Hide change review' : 'Review changes'}
        </button>
      </div>
      {open && <div className="space-y-3 border-t border-telegram-border pt-3">
        <div className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-subtext">
          <ShieldCheck className="mr-1 inline h-3 w-3" aria-hidden="true" />
          A checkpoint is saved only on this device. This compares local metadata, not a continuous activity log or verified cloud history. Sync Drive metadata separately before comparing changes from another device. Missing entries may be trashed or absent, not necessarily permanently deleted.
        </div>
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-telegram-subtext" role="status">
            {busy ? 'Verifying local Drive index…' : report
              ? `${report.tracked_files} files · ${report.tracked_folders} folders · index revision ${report.current_revision}`
              : 'No verified index loaded'}
          </p>
          <button type="button" className={buttonStyle} disabled={busy !== null || copying}
            onClick={() => void inspect()}>
            {busy === 'inspect' ? <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" />
              : <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />}
            Refresh comparison
          </button>
        </div>
        {report && <>
          <p className="text-xs text-telegram-subtext">
            {report.generation === 0 ? 'No saved checkpoint yet.' :
              `Checkpoint #${report.generation} · ${report.baseline_count} items · saved ${new Date(report.captured_at!).toLocaleString()} · baseline revision ${report.baseline_revision}`}
            {report.current_dirty && ' · local metadata not yet synced to cloud'}
          </p>
          {report.current_pending > 0 && <p role="alert" className="text-xs text-telegram-subtext">
            {report.current_pending} pending write(s) are unfinished. Checkpoint capture is blocked until they are recovered; comparison may show temporary differences.
          </p>}
          <div className="flex flex-wrap items-center gap-2">
            <button type="button" className={buttonStyle + ' font-semibold'}
              disabled={busy !== null || copying || report.current_pending > 0}
              onClick={() => void capture()}>
              {busy === 'capture' ? 'Saving checkpoint…' :
                armed ? 'Confirm replace saved checkpoint' :
                  report.generation > 0 ? 'Replace saved checkpoint' : 'Save first checkpoint'}
            </button>
            {armed && <button type="button" className={buttonStyle}
              onClick={() => setArmed(false)}>Cancel</button>}
          </div>
          <p className="text-xs text-telegram-subtext">
            Saving replaces your previous local checkpoint. Preserve this comparison as CSV first if you need to keep it.
          </p>
          {report.generation > 0 && <>
            <div className="flex flex-wrap items-center gap-2">
              <p className="flex-1 text-xs font-medium text-telegram-text">
                {report.change_count} metadata change events
                {report.truncated ? ' · only first 5,000 shown' : ''}
              </p>
              <input type="search" aria-label="Filter changed Drive paths"
                placeholder="Search changed paths…" className={inputStyle + ' min-w-[120px] flex-1'}
                value={query} onChange={e => { setQuery(e.target.value); setVisible(60); }} />
              <select aria-label="Filter Drive changes by type" className={inputStyle}
                value={kind} onChange={e => { setKind(e.target.value as DriveChangeFilter); setVisible(60); }}>
                <option value="all">All changes</option>
                {Object.entries(statusLabel).map(([key, label]) => <option key={key} value={key}>{label}</option>)}
              </select>
              <button type="button" className={buttonStyle} onClick={() => void copy()}
                disabled={copying || changes.length === 0 || busy !== null}>
                <Copy className="mr-1 inline h-3 w-3" aria-hidden="true" />
                Copy review CSV
              </button>
            </div>
            {changes.length === 0 && <p className="text-xs text-telegram-subtext">
              No changes match this review. This does not independently verify cloud file contents.
            </p>}
            <ul aria-label="Drive metadata changes" className="max-h-80 space-y-2 overflow-auto">
              {changes.slice(0, visible).map((change, index) => (
                <li className="rounded-lg border border-telegram-border p-2 text-xs" key={index}>
                  <p className="font-medium text-telegram-text">{statusLabel[change.status]} · {change.kind}</p>
                  {change.before_path && <p className="break-all text-telegram-subtext">Before: {change.before_path}</p>}
                  {change.after_path && <p className="break-all text-telegram-text">After: {change.after_path}</p>}
                  <p className="mt-1 text-telegram-subtext">{change.content_evidence}</p>
                </li>
              ))}
            </ul>
            {changes.length > visible && <button type="button" className={buttonStyle}
              onClick={() => setVisible(count => Math.min(changes.length, count + 60))}>
              Show more ({changes.length - visible} remaining)
            </button>}
          </>}
        </>}
        {error && <p role="alert" className="rounded-lg border border-red-500/30 p-2 text-xs text-red-400">{error}</p>}
        {notice && <p role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
      </div>}
    </section>
  );
}
