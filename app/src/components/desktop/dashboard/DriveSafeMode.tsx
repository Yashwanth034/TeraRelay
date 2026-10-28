import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { type as osType } from '@tauri-apps/plugin-os';
import { LockKeyhole, UnlockKeyhole, RefreshCw, ShieldAlert } from 'lucide-react';
import {
  parseDriveSafeMountStatus, type DriveSafeMountStatus,
} from '../../../driveSafeMode';

const control = 'rounded-lg border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';

/** Protects *native Linux FUSE writes*, not metadata mutation elsewhere in Settings. */
export function DriveSafeMode({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [status, setStatus] = useState<DriveSafeMountStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const running = useRef(false);
  const generation = useRef(0);

  useEffect(() => {
    if (active) return;
    if (running.current) {
      setOpen(false);
      return;
    }
    generation.current++;
    setOpen(false);
    setStatus(null);
    setError(null);
    setNotice(null);
  }, [active]);

  const refresh = async () => {
    if (!active || running.current) return;
    running.current = true;
    const epoch = ++generation.current;
    setBusy(true);
    setError(null);
    try {
      const next = parseDriveSafeMountStatus(await invoke<unknown>('cmd_drive_safe_mount_status'));
      if (epoch === generation.current) setStatus(next);
    } catch (cause) {
      if (epoch === generation.current) setError('Cannot confirm the current mounted Drive safety state: ' + String(cause));
    } finally {
      running.current = false;
      if (epoch === generation.current) setBusy(false);
    }
  };

  const toggle = async () => {
    if (!active || !status || running.current) return;
    running.current = true;
    const epoch = ++generation.current;
    setBusy(true);
    setError(null);
    setNotice(null);
    const nextValue = !status.read_only;
    try {
      const next = parseDriveSafeMountStatus(
        await invoke<unknown>('cmd_drive_safe_mount_set', { readOnly: nextValue }),
      );
      if (next.read_only !== nextValue) throw new Error('Native read-only state was not confirmed.');
      if (epoch === generation.current) {
        setStatus(next);
        setNotice(nextValue
          ? 'Safe Read-Only Mode enabled. New mounted filesystem mutations are blocked; previously started transfers may finish.'
          : 'Mounted filesystem write operations are enabled again.');
      }
    } catch (cause) {
      if (epoch === generation.current) setError(
        'Safe Mode switch outcome uncertain: ' + String(cause) +
        '. Refresh the native state before attempting another switch.',
      );
    } finally {
      running.current = false;
      if (epoch === generation.current) setBusy(false);
    }
  };

  let supported = false;
  try { supported = osType() === 'linux'; } catch { /* unsupported platform */ }
  if (!supported) return null;

  return (
    <section aria-label="Drive Safe Read-Only Mount" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <ShieldAlert className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">Drive Safe Read-Only Mount</p>
            <p className="text-xs text-telegram-subtext">
              Protect your mounted Drive from accidental filesystem edits while inspecting or recovering files.
            </p>
          </div>
        </div>
        <button type="button" className={control} aria-expanded={open} disabled={busy}
          onClick={() => {
            if (open) { generation.current++; setOpen(false); setNotice(null); }
            else { setOpen(true); void refresh(); }
          }}>
          {open ? 'Hide safety controls' : 'Mount safety controls'}
        </button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <p className="text-xs text-telegram-subtext">
            Applies only to writes through this app's Linux FUSE mount. It does not lock all TeraRelay
            Settings commands, change Telegram permissions, or create a cloud snapshot.
            Already-running uploads may finish after enabling protection.
          </p>
          <div className="flex flex-wrap items-center justify-between gap-2">
            <button type="button" className={control} disabled={busy} onClick={() => void refresh()}>
              <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" /> Refresh status
            </button>
            {status && <button type="button" className={control} disabled={busy}
              onClick={() => void toggle()}>
              {status.read_only
                ? <UnlockKeyhole className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" />
                : <LockKeyhole className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" />}
              {status.read_only ? 'Enable mounted writes' : 'Protect mount: block writes'}
            </button>}
          </div>
          {status && <p role="status" className="text-xs text-telegram-text">
            {status.mounted ? 'Drive is mounted' : 'Drive is not currently mounted'} ·
            {status.read_only ? ' Safe Read-Only Mode ON' : ' Mounted writes permitted'}
            {' · '}session-only setting
          </p>}
          {status?.read_only && <p className="text-xs text-telegram-subtext">
            FUSE create, write, truncate, rename, folder changes and deletion now return read-only filesystem errors.
            Background staged-upload retries are deferred until you restore writable mode.
          </p>}
          {notice && <p role="status" className="rounded border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
          {error && <p role="alert" className="rounded border border-red-500/40 p-2 text-xs text-red-400">{error}</p>}
        </div>
      )}
    </section>
  );
}
