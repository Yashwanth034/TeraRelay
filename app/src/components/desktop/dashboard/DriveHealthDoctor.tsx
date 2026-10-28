import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Activity, AlertTriangle, CheckCircle2, RefreshCw, ShieldAlert } from 'lucide-react';
import { parseDriveHealthReport, type DriveHealthReport } from '../../../driveHealthDoctor';

/** Explicit, read-only Drive staging diagnosis. Never scans on a timer or repairs data. */
export function DriveHealthDoctor({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [running, setRunning] = useState(false);
  const [report, setReport] = useState<DriveHealthReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const generation = useRef(0);

  useEffect(() => {
    if (!active) {
      setOpen(false);
      setReport(null);
      setError(null);
      setRunning(false);
      generation.current += 1;
    }
  }, [active]);

  const scan = async () => {
    if (running || !active) return;
    const scanId = ++generation.current;
    setRunning(true);
    setReport(null);
    setError(null);
    try {
      const result = parseDriveHealthReport(await invoke<unknown>('cmd_drive_health_inspect'));
      // Do not display stale results after Settings closes or a new run begins.
      if (generation.current === scanId) setReport(result);
    } catch (cause) {
      if (generation.current === scanId) setError('Read-only Drive health inspection failed: ' + String(cause));
    } finally {
      if (generation.current === scanId) setRunning(false);
    }
  };

  const critical = report?.issues.filter(item => item.severity === 'critical') ?? [];
  const warnings = report?.issues.filter(item => item.severity === 'warning') ?? [];
  const ctl = 'rounded-md border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';

  return (
    <section aria-label="Drive integrity doctor" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-2">
          <Activity className="h-4 w-4 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">Drive Integrity Doctor</p>
            <p className="text-xs text-telegram-subtext">
              Inspect private pending-write staging for unsafe paths, missing local ranges and permissions.
              No uploads, repairs, deletions or cloud access.
            </p>
          </div>
        </div>
        <button className={ctl} type="button" aria-expanded={open}
          disabled={running}
          onClick={() => { if (open) { generation.current++; setReport(null); } setOpen(!open); setError(null); }}>
          {open ? 'Hide diagnostics' : 'Inspect staging health'}
        </button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <div className="flex items-center justify-between gap-2">
            <p className="text-xs text-telegram-subtext">Manual, bounded and read-only. A clean result does not prove cloud integrity.</p>
            <button className={ctl} disabled={running} type="button" onClick={() => void scan()}>
              <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />
              {running ? 'Checking…' : report ? 'Run again' : 'Run diagnosis'}
            </button>
          </div>
          {error && <p role="alert" className="rounded border border-red-500/40 p-2 text-xs text-red-400">{error}</p>}
          {report && (
            <div className="space-y-3" aria-live="polite">
              <div className="flex flex-wrap gap-3 text-xs text-telegram-text">
                <span>{report.scanned} pending records inspected</span>
                <span>{report.closed} closed uploads</span>
                <span>{report.open_writers} active writers skipped</span>
              </div>
              {report.issues.length === 0 && !report.truncated ? (
                <p className="flex items-center gap-2 text-xs text-telegram-text">
                  <CheckCircle2 className="h-4 w-4" aria-hidden="true" />
                  No staging problems detected in the scanned pending records.
                </p>
              ) : (
                <>
                  <p className="flex items-center gap-2 text-xs text-telegram-text">
                    <ShieldAlert className="h-4 w-4" aria-hidden="true" />
                    {critical.length} critical issue(s) · {warnings.length} warning(s)
                  </p>
                  <ul className="max-h-64 space-y-2 overflow-auto" aria-label="Drive staging diagnostic findings">
                    {report.issues.map((item, index) => (
                      <li key={item.pending_id + ':' + item.code + ':' + index}
                        className="rounded border border-telegram-border p-2 text-xs text-telegram-text">
                        <p className="flex gap-1.5 font-medium break-all">
                          {item.severity === 'critical' && <AlertTriangle className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />}
                          {item.name} · {item.code.replace(/_/g, ' ')}
                        </p>
                        <p className="mt-1 text-telegram-subtext">{item.advice}</p>
                      </li>
                    ))}
                  </ul>
                </>
              )}
              {report.truncated && <p role="alert" className="text-xs text-telegram-text">
                The bounded inspection was incomplete. Do not interpret omitted records as healthy.
              </p>}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
