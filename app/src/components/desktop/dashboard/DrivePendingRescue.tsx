import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open, save } from '@tauri-apps/plugin-dialog';
import { ArchiveRestore, HardDriveDownload, RefreshCw, ShieldCheck } from 'lucide-react';
import { parseRescueCandidates, parseRescueResult, type RescueCandidate } from '../../../drivePendingRescue';

const ctl = 'rounded-md border border-telegram-border px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-hover disabled:opacity-50';

export function DrivePendingRescue({ active }: { active: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const [list, setList] = useState<RescueCandidate[] | null>(null);
  const [selected, setSelected] = useState('');
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const [error, setError] = useState('');
  const generation = useRef(0);
  const operation = useRef(false);

  useEffect(() => {
    if (active) return;
    generation.current++;
    setExpanded(false); setList(null); setSelected(''); setBusy(false);
    setNotice(''); setError('');
  }, [active]);

  const refresh = async () => {
    if (!active || operation.current) return;
    operation.current = true;
    const tick = ++generation.current;
    setBusy(true); setError('');
    try {
      const items = parseRescueCandidates(await invoke<unknown>('cmd_drive_rescue_candidates'));
      if (tick === generation.current) {
        setList(items);
        setSelected(current => items.some(item => item.id === current) ? current : (items[0]?.id ?? ''));
      }
    } catch (cause) {
      if (tick === generation.current) setError('Cannot read staged Drive uploads: ' + String(cause));
    } finally {
      operation.current = false;
      if (tick === generation.current) setBusy(false);
    }
  };

  const run = async (kind: 'export' | 'restore') => {
    if (operation.current || !active || !list?.some(item => item.id === selected)) return;
    operation.current = true;
    const tick = ++generation.current;
    setBusy(true); setError(''); setNotice('');
    try {
      // Pickers are explicit user gestures. Export never overwrites an existing
      // destination; restore only adds missing files to the matching local stage.
      const path = kind === 'export'
        ? await save({ title: 'Save local Drive rescue ZIP', defaultPath: 'terarelay-drive-rescue.zip',
            filters: [{ name: 'ZIP archive', extensions: ['zip'] }] })
        : await open({ title: 'Open existing Drive rescue ZIP', multiple: false,
            filters: [{ name: 'ZIP archive', extensions: ['zip'] }] });
      if (!path || tick !== generation.current) return;
      const raw = await invoke<unknown>(
        kind === 'export' ? 'cmd_drive_rescue_export' : 'cmd_drive_rescue_restore',
        kind === 'export' ? { pendingId: selected, destinationPath: path } : { pendingId: selected, sourcePath: path }
      );
      const result = parseRescueResult(raw);
      if (result.pending_id !== selected) throw new Error('Rescue result identifies a different pending file');
      if (tick === generation.current) setNotice(
        kind === 'export'
          ? 'Created a checked local rescue ZIP (' + result.count + ' staged entries). Keep it private: it contains unencrypted staged bytes.'
          : 'Verified rescue archive. Restored ' + result.restored + ' missing staged entries; no Telegram upload was attempted.'
      );
    } catch (cause) {
      if (tick === generation.current) setError(
        'Rescue ' + kind + ' did not confirm success: ' + String(cause) +
        '. Inspect staging before retrying; existing data is never intentionally overwritten.'
      );
    } finally {
      operation.current = false;
      if (tick === generation.current) setBusy(false);
    }
  };

  return (
    <section aria-label="Drive Pending Rescue Packs" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2">
          <ArchiveRestore className="h-4 w-4 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">Drive emergency rescue</p>
            <p className="text-xs text-telegram-subtext">
              Back up locally retained bytes of unfinished Drive uploads before recovery.
            </p>
          </div>
        </div>
        <button type="button" className={ctl} disabled={busy} aria-expanded={expanded} onClick={() => {
          if (expanded) { generation.current++; setExpanded(false); setList(null); setError(''); setNotice(''); }
          else { setExpanded(true); void refresh(); }
        }}>{expanded ? 'Close rescue' : 'Open rescue tools'}</button>
      </div>
      {expanded && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <p className="text-xs text-telegram-subtext">
            Rescue ZIPs may contain <strong>plaintext</strong>. Keep backups private and encrypted when stored externally.
            A pack contains staged local chunks only, not remotely uploaded pieces, encryption keys, or the Drive database.
            Restore requires the original matching pending record and its private staging directory; it does not recover a deleted account.
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <button type="button" className={ctl} disabled={busy} onClick={() => void refresh()}>
              <RefreshCw className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" /> Refresh pending
            </button>
            {list && <span className="text-xs text-telegram-subtext">{list.length} closed pending upload(s)</span>}
          </div>
          {list && list.length > 0 && (
            <>
              <label htmlFor="drive-rescue-file" className="block text-xs text-telegram-text">Choose closed upload</label>
              <select id="drive-rescue-file" className="w-full rounded-md border border-telegram-border bg-telegram-bg p-2 text-xs text-telegram-text"
                disabled={busy} value={selected} onChange={event => { setSelected(event.target.value); setNotice(''); setError(''); }}>
                {list.map(item => <option key={item.id} value={item.id}>
                  {item.name} ({(item.size / 1048576).toFixed(1)} MiB logical)
                </option>)}
              </select>
              <div className="flex flex-wrap gap-2">
                <button className={ctl} type="button" disabled={busy || !selected} onClick={() => void run('export')}>
                  <HardDriveDownload className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" /> Export rescue ZIP
                </button>
                <button className={ctl} type="button" disabled={busy || !selected} onClick={() => void run('restore')}>
                  <ShieldCheck className="mr-1 inline h-3.5 w-3.5" aria-hidden="true" /> Verify and restore missing chunks
                </button>
              </div>
            </>
          )}
          {list?.length === 0 && <p className="text-xs text-telegram-subtext">No closed unfinished uploads available for rescue.</p>}
          {busy && <p role="status" className="text-xs text-telegram-subtext">Rescue operation running…</p>}
          {notice && <p role="status" className="text-xs text-telegram-text">{notice}</p>}
          {error && <p role="alert" className="text-xs text-red-400">{error}</p>}
        </div>
      )}
    </section>
  );
}
