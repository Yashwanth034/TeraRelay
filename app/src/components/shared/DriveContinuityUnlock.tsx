import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Cloud, KeyRound, Loader2, LockKeyhole } from 'lucide-react';
import { toast } from 'sonner';
import { validateDriveCryptoStatus } from '../../deviceContinuity';

interface DriveContinuityUnlockProps {
  open: boolean;
  restoredFromRemote: boolean;
  directoryCount: number;
  fileCount: number;
  onUnlocked: () => void;
  onDismiss: () => void;
}

export function DriveContinuityUnlock({
  open,
  restoredFromRemote,
  directoryCount,
  fileCount,
  onUnlocked,
  onDismiss,
}: DriveContinuityUnlockProps) {
  const [passphrase, setPassphrase] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestGenerationRef = useRef(0);
  const inFlightRef = useRef(false);
  const dismissedRef = useRef(false);
  const openRef = useRef(open);
  openRef.current = open;

  useEffect(() => {
    // Dismissal, sign-out and remount invalidate any older unlock response.
    requestGenerationRef.current += 1;
    if (open) {
      dismissedRef.current = false;
      // An earlier native unlock may still be running after dismissal.
      // Keep it single-flight until its promise really settles.
      setBusy(inFlightRef.current);
    }
    if (!open) {
      setPassphrase('');
      setError(null);
      setBusy(false);
    }
    return () => { requestGenerationRef.current += 1; };
  }, [open]);

  if (!open) return null;

  const dismiss = () => {
    if (dismissedRef.current) return;
    dismissedRef.current = true;
    // Native unlock cannot be cancelled. Invalidate its UI result right now
    // (before React rerenders), including when the native request is stuck.
    ++requestGenerationRef.current;
    // Preserve native in-flight ownership even after UI dismissal, so a
    // reopened prompt cannot dispatch another overlapping native unlock.
    setPassphrase('');
    setError(null);
    setBusy(false);
    onDismiss();
  };

  const unlock = async (event: React.FormEvent) => {
    event.preventDefault();
    if (dismissedRef.current || !passphrase || busy || inFlightRef.current) return;

    const generation = ++requestGenerationRef.current;
    inFlightRef.current = true;
    const submittedPassphrase = passphrase;
    // Clear the input immediately; the native request holds the submitted value.
    setPassphrase('');
    setBusy(true);
    setError(null);
    try {
      const response = await invoke<unknown>('cmd_drive_crypto_unlock', {
        passphrase: submittedPassphrase,
      });
      if (requestGenerationRef.current !== generation) return;
      // Native IPC types are only compile-time promises: never claim an
      // encrypted Drive unlocked from a truthy string or invalid state.
      const status = validateDriveCryptoStatus(response);
      if (!status.configured || !status.unlocked) {
        throw new Error('Drive remained locked after the unlock request');
      }
      toast.success('TeraRelay Drive unlocked on this device');
      onUnlocked();
    } catch (unlockError) {
      if (requestGenerationRef.current === generation) {
        setError(String(unlockError));
      }
    } finally {
      // A stale native request still owns the single-flight lease until it
      // actually finishes. The old response may release the busy indicator
      // for a reopened prompt, but never show stale success/error UI.
      inFlightRef.current = false;
      if (requestGenerationRef.current === generation
          || (openRef.current && !dismissedRef.current)) {
        setBusy(false);
      }
    }
  };

  return (
    <div className="tr-modal-backdrop fixed inset-0 z-[120] flex items-center justify-center p-4" role="presentation">
      <section
        className="tr-modal w-full max-w-[500px] space-y-5 p-6"
        role="dialog"
        aria-modal="true"
        aria-labelledby="tr-continuity-title"
        aria-describedby="tr-continuity-description"
        onKeyDown={event => {
          if (event.key === 'Escape') {
            event.preventDefault();
            dismiss();
          }
        }}
      >
        <div className="tr-modal-icon" aria-hidden="true">
          {restoredFromRemote ? <Cloud size={26} /> : <LockKeyhole size={26} />}
        </div>

        <div className="space-y-2">
          <p className="terarelay-eyebrow">DEVICE CONTINUITY</p>
          <h2 id="tr-continuity-title" className="text-xl font-semibold text-telegram-text">
            Unlock your TeraRelay Drive
          </h2>
          <p id="tr-continuity-description" className="text-sm leading-6 text-telegram-subtext">
            {restoredFromRemote
              ? `TeraRelay found your existing cloud state on Telegram${directoryCount || fileCount ? ` and restored ${directoryCount} folder${directoryCount === 1 ? '' : 's'} and ${fileCount} file${fileCount === 1 ? '' : 's'}` : ''}. Enter the same Drive passphrase to unlock encrypted file contents on this device.`
              : 'This Telegram account already has an encrypted TeraRelay Drive. Enter the existing Drive passphrase to unlock it on this device.'}
          </p>
        </div>

        <div className="tr-modal-note flex items-start gap-2">
          <KeyRound size={16} aria-hidden="true" />
          <span>Your passphrase stays on this device and is never sent to Telegram.</span>
        </div>

        <form className="space-y-3" onSubmit={unlock}>
          <label className="block space-y-2">
            <span className="text-xs font-medium text-telegram-subtext">Drive passphrase</span>
            <input
              autoFocus
              type="password"
              autoComplete="current-password"
              value={passphrase}
              disabled={busy}
              onChange={event => setPassphrase(event.target.value)}
              placeholder="Enter your existing Drive passphrase"
              className="tr-modal-input w-full px-3.5 py-2.5 text-sm"
            />
          </label>

          {error && (
            <p className="text-xs leading-5 text-red-400" role="alert">
              Could not unlock Drive. Check the passphrase and try again.
            </p>
          )}

          <div className="flex items-center justify-end gap-2 pt-1">
            <button
              type="button"
              className="tr-button tr-button--ghost"
              onClick={dismiss}
            >
              Later
            </button>
            <button
              type="submit"
              className="tr-button tr-button--primary min-w-[128px]"
              disabled={busy || !passphrase}
            >
              {busy ? <Loader2 size={16} className="animate-spin" aria-hidden="true" /> : <LockKeyhole size={16} aria-hidden="true" />}
              <span className="tr-button__label">Unlock Drive</span>
            </button>
          </div>
        </form>
      </section>
    </div>
  );
}
