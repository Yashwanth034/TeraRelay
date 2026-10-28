import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Copy, Files, Loader2, RefreshCw, ShieldCheck } from 'lucide-react';
import {
  inspectDriveDuplicates, filterDuplicateGroups, duplicateReviewCsv,
  type DuplicateFilter, type DuplicateReport, type DuplicateSort,
} from '../../../driveDuplicateExplorer';

const button = 'rounded-lg bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';
const field = 'rounded-lg border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
const bytes = (size: number): string => {
  if (size < 1024) return size + ' B';
  const unit = Math.min(Math.floor(Math.log(size) / Math.log(1024)), 4);
  return (size / 1024 ** unit).toFixed(1) + ' ' + ['B', 'KiB', 'MiB', 'GiB', 'TiB'][unit];
};

export function DriveDuplicateExplorer({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [report, setReport] = useState<DuplicateReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [copying, setCopying] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [filter, setFilter] = useState<DuplicateFilter>('all');
  const [sort, setSort] = useState<DuplicateSort>('impact');
  const [search, setSearch] = useState('');
  const [visible, setVisible] = useState(15);
  const generation = useRef(0);
  const loading = useRef(false);

  useEffect(() => {
    if (active) return;
    generation.current++;
    loading.current = false;
    setOpen(false);
    setReport(null);
    setBusy(false);
    setError(null);
    setNotice(null);
  }, [active]);

  const reload = async () => {
    if (!active || loading.current) return;
    loading.current = true;
    const id = ++generation.current;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const raw = await invoke<unknown>('cmd_drive_get_snapshot');
      const checked = inspectDriveDuplicates(raw);
      if (id !== generation.current) return;
      setReport(checked);
      setVisible(15);
    } catch (cause) {
      if (id === generation.current) setError('Could not inspect the local Drive index: ' + String(cause));
    } finally {
      loading.current = false;
      if (id === generation.current) setBusy(false);
    }
  };
  const toggle = () => {
    if (busy || copying) return;
    if (open) {
      generation.current++;
      setOpen(false);
      setReport(null);
    } else {
      setOpen(true);
      void reload();
    }
  };
  const groups = useMemo(() => filterDuplicateGroups(
    report?.groups ?? [], search, filter, sort,
  ), [report, search, filter, sort]);

  const copy = async () => {
    if (copying || busy || groups.length === 0) return;
    setCopying(true);
    setError(null);
    setNotice(null);
    try {
      await navigator.clipboard.writeText(duplicateReviewCsv(groups));
      setNotice('Review CSV copied. Nothing was renamed, moved or deleted.');
    } catch (cause) {
      setError('Could not copy review CSV: ' + String(cause));
    } finally {
      setCopying(false);
    }
  };

  return (
    <section aria-label="Drive Duplicate Explorer" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <Files className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">TeraRelay Drive · Duplicate Explorer</p>
            <p className="text-xs text-telegram-subtext">Find verified matching content across folders. Review copies safely, without downloading or deleting anything.</p>
          </div>
        </div>
        <button type="button" className={button} aria-expanded={open}
          onClick={toggle} disabled={busy || copying}>
          {open ? 'Hide duplicates' : 'Find duplicate files'}
        </button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p role="status" className="text-xs text-telegram-subtext">
              {busy ? 'Inspecting the local Drive index…' : report
                ? `${report.groups.length} matching-content groups · ${report.eligible} fingerprinted files of ${report.scanned} scanned`
                : 'No inventory loaded yet'}
            </p>
            <button type="button" className={button} disabled={busy || copying}
              onClick={() => void reload()}>
              {busy ? <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" /> :
                <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />}
              Refresh
            </button>
          </div>
          {report && (
            <>
              <div className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-subtext">
                <p><ShieldCheck className="mr-1 inline h-3 w-3" aria-hidden="true" />
                  Local read-only snapshot revision {report.revision}{report.dirty ? ' · cloud sync pending' : ''}.
                  {' '}{report.excluded} missing, invalid or unverified items excluded.
                </p>
                {report.truncated && <p role="alert">Index scan capped at 25,000 entries. Results are incomplete.</p>}
                <p className="mt-1">A matching fingerprint is evidence of identical content, not a promise of recoverable cloud space. Multiple names may reference one stored object; do not delete files based on this report alone.</p>
              </div>
              <div className="flex flex-wrap gap-2">
                <input type="search" aria-label="Search duplicate file paths" className={field + ' min-w-[150px] flex-1'}
                  value={search} onChange={event => { setSearch(event.target.value); setVisible(15); }}
                  placeholder="Filter by full path…" />
                <select className={field} aria-label="Duplicate storage relationship"
                  value={filter} onChange={event => { setFilter(event.target.value as DuplicateFilter); setVisible(15); }}>
                  <option value="all">All groups</option>
                  <option value="separate">Distinct storage IDs</option>
                  <option value="shared">Same stored object</option>
                </select>
                <select className={field} aria-label="Sort duplicate groups" value={sort}
                  onChange={event => setSort(event.target.value as DuplicateSort)}>
                  <option value="impact">Largest logical overlap</option>
                  <option value="count">Most matching paths</option>
                  <option value="path">Path A–Z</option>
                </select>
                <button type="button" className={button} disabled={copying || busy || groups.length === 0}
                  onClick={() => void copy()}>
                  <Copy className="mr-1 inline h-3 w-3" aria-hidden="true" />
                  {copying ? 'Copying…' : 'Copy review CSV'}
                </button>
              </div>
              {groups.length === 0 && (
                <p className="rounded-lg border border-telegram-border p-3 text-xs text-telegram-subtext">
                  No matching-content groups found for this filter. Files without a verified content fingerprint are not guessed from names or sizes.
                </p>
              )}
              <div className="max-h-96 space-y-2 overflow-auto" aria-label="Duplicate content groups">
                {groups.slice(0, visible).map((group, index) => (
                  <div className="rounded-lg border border-telegram-border p-3" key={group.id}>
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <p className="text-xs font-semibold text-telegram-text">
                        Group {index + 1} · {group.files.length} matching file paths
                      </p>
                      <span className="text-xs text-telegram-subtext">
                        {bytes(group.size)} each · {bytes(group.logicalExtraBytes)} additional logical bytes
                      </span>
                    </div>
                    <p className="mt-1 text-xs text-telegram-subtext">
                      {group.sharedObject
                        ? 'Shared storage: these paths reference one remote object. Deleting one name may not save any remote space.'
                        : `${group.remoteObjects} distinct storage IDs. Review carefully before removing any references; remote savings are unverified.`}
                    </p>
                    <ul className="mt-2 space-y-1">
                      {group.files.map(file => (
                        <li key={file.id} className="break-all text-xs text-telegram-text">
                          {file.path}
                        </li>
                      ))}
                    </ul>
                  </div>
                ))}
              </div>
              {groups.length > visible && <button className={button} type="button"
                onClick={() => setVisible(current => Math.min(groups.length, current + 15))}>
                Show more groups ({groups.length - visible} remaining)
              </button>}
            </>
          )}
          {error && <p role="alert" className="rounded-lg border border-red-500/30 p-2 text-xs text-red-400">{error}</p>}
          {notice && <p role="status" className="rounded-lg border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
        </div>
      )}
    </section>
  );
}
