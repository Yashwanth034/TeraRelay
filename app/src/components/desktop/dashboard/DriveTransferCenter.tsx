import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { type as osType } from '@tauri-apps/plugin-os';
import { CloudUpload, RefreshCw, PauseCircle, PlayCircle } from 'lucide-react';
import { parseTransferSnapshot, parseTransferAction, type TransferSnapshot } from '../../../driveTransferCenter';

const button = 'rounded-lg border border-telegram-border px-2.5 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';
const sizeText = (bytes: number) => bytes < 1024 ? bytes + ' B' : (bytes / 1024 / 1024).toFixed(1) + ' MiB';

export function DriveTransferCenter({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [snapshot, setSnapshot] = useState<TransferSnapshot | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const generation = useRef(0);
  const operation = useRef(false);
  useEffect(() => {
    if (active) return;
    generation.current++;
    setOpen(false); setSnapshot(null); setBusy(false); setError(null); setNotice(null);
  }, [active]);

  const refresh = async () => {
    if (!active || operation.current) return;
    operation.current = true;
    const id = ++generation.current;
    setBusy(true); setError(null);
    try {
      const next = parseTransferSnapshot(await invoke<unknown>('cmd_drive_transfer_snapshot'));
      if (generation.current === id) setSnapshot(next);
    } catch (cause) {
      if (generation.current === id) setError('Could not inspect queued Drive uploads: ' + String(cause));
    } finally {
      operation.current = false;
      if (generation.current === id) setBusy(false);
    }
  };
  const pause = async () => {
    if (!snapshot || operation.current || !active) return;
    operation.current = true;
    const id = ++generation.current;
    setBusy(true); setError(null); setNotice(null);
    try {
      const nextValue = !snapshot.paused;
      const confirmed = await invoke<unknown>('cmd_drive_transfer_pause', { paused: nextValue });
      if (confirmed !== nextValue) throw new Error('Native pause state was not confirmed.');
      const next = parseTransferSnapshot(await invoke<unknown>('cmd_drive_transfer_snapshot'));
      if (generation.current === id) {
        setSnapshot(next);
        setNotice(nextValue
          ? 'Automatic retry paused for this app session; an upload already running may finish.'
          : 'Automatic retry resumed. Failed files still honor their retry backoff.');
      }
    } catch (cause) {
      if (generation.current === id) setError('Pause/resume result uncertain: ' + String(cause) +
        '. Refresh before changing the setting again.');
    } finally {
      operation.current = false;
      if (generation.current === id) setBusy(false);
    }
  };
  const retry = async (pendingId: string) => {
    if (operation.current || !active || !snapshot?.transfers.some(item => item.pending_id === pendingId)) return;
    operation.current = true;
    const id = ++generation.current;
    setBusy(true); setError(null); setNotice(null);
    try {
      const outcome = parseTransferAction(await invoke<unknown>('cmd_drive_transfer_retry', { pendingId }));
      if (generation.current !== id) return;
      if (!outcome.attempted) setNotice('Upload was skipped because it is busy or no longer available.');
      else if (outcome.recovered) setNotice('Selected upload recovered. Cloud metadata sync still needs verification.');
      else setNotice('Selected upload failed. Staged data was retained for a later retry.');
      try {
        const next = parseTransferSnapshot(await invoke<unknown>('cmd_drive_transfer_snapshot'));
        if (generation.current === id) setSnapshot(next);
      } catch {
        if (generation.current === id) setNotice('Retry finished but inventory refresh failed. Refresh before retrying.');
      }
    } catch (cause) {
      if (generation.current === id) setError('Transfer outcome uncertain: ' + String(cause) +
        '. Do not blindly resend; refresh and review the file first.');
    } finally {
      operation.current = false;
      if (generation.current === id) setBusy(false);
    }
  };
  // Automatic staged-upload retry is currently a Linux native FUSE feature.
  // Other desktop platforms must not display controls without native IPC.
  try { if (osType() !== 'linux') return null; } catch { return null; }
  return (
    <section aria-label="Drive Transfer Control Center" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2">
          <CloudUpload className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">Drive Transfer Control Center</p>
            <p className="text-xs text-telegram-subtext">
              Review pending uploads, pause background retries, or retry a selected file safely.
            </p>
          </div>
        </div>
        <button type="button" className={button} disabled={busy} aria-expanded={open}
          onClick={() => { if (!open) { setOpen(true); void refresh(); } else { generation.current++; setOpen(false); setSnapshot(null); } }}>
          {open ? 'Hide transfers' : 'Manage transfers'}
        </button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <p className="text-xs text-telegram-subtext">
            Pause applies to automatic retries for this app session only. It does not cancel a Telegram send already in flight or remove staged data. This list does not prove cloud synchronization.
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <button type="button" className={button} disabled={busy} onClick={() => void refresh()}>
              <RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true"/>Refresh queue
            </button>
            {snapshot && <button type="button" className={button} disabled={busy} onClick={() => void pause()}>
              {snapshot.paused
                ? <PlayCircle className="mr-1 inline h-3 w-3" aria-hidden="true"/>
                : <PauseCircle className="mr-1 inline h-3 w-3" aria-hidden="true"/>}
              {snapshot.paused ? 'Resume automatic retries' : 'Pause automatic retries'}
            </button>}
          </div>
          {snapshot && (
            <div className="space-y-2">
              <p role="status" className="text-xs text-telegram-subtext">
                {snapshot.total_pending} closed upload(s) pending · automatic worker
                {snapshot.automatic_worker_running ? ' running' : ' not running'} ·
                {snapshot.paused ? ' retries paused' : ' retries enabled'}
              </p>
              {snapshot.truncated && <p role="alert" className="text-xs text-telegram-text">
                Showing the first 100 pending uploads. Additional items are not shown.
              </p>}
              {snapshot.transfers.length === 0 &&
                <p className="rounded border border-telegram-border p-2 text-xs text-telegram-subtext">
                  No closed pending uploads found. Files still open for writing are excluded.
                </p>}
              <ul className="max-h-80 space-y-2 overflow-auto" aria-label="Queued Drive uploads">
                {snapshot.transfers.map(item => (
                  <li key={item.pending_id} className="flex flex-wrap items-center justify-between gap-2 rounded border border-telegram-border p-2 text-xs">
                    <div className="min-w-0 text-telegram-text">
                      <p className="break-all font-medium">{item.file_name}</p>
                      <p className="text-telegram-subtext">
                        {sizeText(item.size)} · {item.status} · failed attempts {item.failed_attempts}
                        {item.retry_in_seconds > 0 ? ' · retry in about ' + item.retry_in_seconds + 's' : ''}
                      </p>
                    </div>
                    <button type="button" className={button}
                      disabled={busy || item.status === 'uploading'}
                      onClick={() => void retry(item.pending_id)}>Retry this file</button>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {error && <p role="alert" className="rounded border border-red-500/30 p-2 text-xs text-red-400">{error}</p>}
          {notice && <p role="status" className="rounded border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
        </div>
      )}
    </section>
  );
}
