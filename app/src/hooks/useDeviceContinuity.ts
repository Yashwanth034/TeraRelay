import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { validateNativeDriveMountStatus } from '../driveMountStatus';
import {
  classifyDeviceContinuity,
  validateDriveSnapshot,
  validateDriveSyncResult,
  validateDriveCryptoStatus,
  type DriveCryptoStatusLike,
  type DriveSnapshotLike,
  type DriveSyncResultLike,
} from '../deviceContinuity';

export type DeviceContinuityPhase = 'idle' | 'restoring' | 'ready' | 'degraded';

export interface DeviceContinuityState {
  phase: DeviceContinuityPhase;
  restoredFromRemote: boolean;
  directoryCount: number;
  fileCount: number;
  needsDriveUnlock: boolean;
  syncSource: string | null;
  error: string | null;
  securityError: string | null;
  /** Native desktop mount failure, tracked independently of cloud metadata sync. */
  nativeMountError: string | null;
}

interface UseDeviceContinuityOptions {
  authenticated: boolean;
  authResolved: boolean;
  isMobile: boolean;
}

const EMPTY_STATE: DeviceContinuityState = {
  phase: 'idle',
  restoredFromRemote: false,
  directoryCount: 0,
  fileCount: 0,
  needsDriveUnlock: false,
  syncSource: null,
  error: null,
  securityError: null,
  nativeMountError: null,
};

export function useDeviceContinuity({ authenticated, authResolved, isMobile }: UseDeviceContinuityOptions) {
  const [state, setState] = useState<DeviceContinuityState>(EMPTY_STATE);
  const initialSnapshotRef = useRef<DriveSnapshotLike | null>(null);
  const startupCryptoCheckPendingRef = useRef(true);
  const shouldDetachNativeDriveRef = useRef(false);
  shouldDetachNativeDriveRef.current = authResolved && !authenticated && !isMobile;

  const markDriveUnlocked = useCallback(() => {
    startupCryptoCheckPendingRef.current = false;
    setState(current => ({ ...current, needsDriveUnlock: false, securityError: null }));
  }, []);

  // Provider recovery must not wait for Telegram metadata sync to finish.
  useEffect(() => {
    if (!authenticated || isMobile) return;
    let disposed = false;
    let inFlight = false;
    let pendingRecoveryTicks = 0;
    let lastError: string | null = null;
    // After a successful mount, native Drive owns recovery and explicit
    // unmount intent. JavaScript must not silently undo a user unmount.
    let mountHandedToNative = false;

    const recoverNativeDrive = async () => {
      if (disposed || inFlight) return;
      inFlight = true;
      pendingRecoveryTicks = 0;
      try {
        const status = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
        if (disposed) return;
        if (!status.supported) {
          // A confirmed unsupported provider is not a failed mount attempt.
          // Clear an earlier transient probe/mount warning instead of leaving
          // the dashboard reporting a failure on a platform with no provider.
          lastError = null;
          setState(current => current.nativeMountError === null
            ? current : { ...current, nativeMountError: null });
          return;
        }
        // Never race the native recovery worker. Once a mount has been
        // handed to native, a later explicit unmount must remain respected.
        // A successfully validated status also clears stale malformed-probe
        // warnings even when the native worker is still recovering.
        if (status.mounted || status.native_recovery || mountHandedToNative) {
          mountHandedToNative = true;
          lastError = null;
          setState(current => current.nativeMountError === null
            ? current : { ...current, nativeMountError: null });
          return;
        }
        await invoke('cmd_drive_mount');
        mountHandedToNative = true;
        if (disposed && shouldDetachNativeDriveRef.current) {
          await invoke('cmd_drive_unmount').catch(() => {});
        }
        if (!disposed) {
          setState(current => current.nativeMountError === null
            ? current : { ...current, nativeMountError: null });
        }
        lastError = null;
      } catch (error) {
        const message = String(error);
        if (!disposed) {
          setState(current => current.nativeMountError === message
            ? current : { ...current, nativeMountError: message });
          if (message !== lastError) {
            console.warn('[Continuity] Native Drive recovery deferred:', error);
            lastError = message;
          }
        }
      } finally {
        inFlight = false;
        pendingRecoveryTicks = 0;
      }
    };

    void recoverNativeDrive();
    const timer = window.setInterval(() => {
      // Do not race another mount/status command while native is still busy.
      // After four 5-second ticks, surface the stall independently of cloud
      // metadata sync; a late valid response may still safely clear it.
      if (!disposed && inFlight && ++pendingRecoveryTicks === 4) {
        const message = 'Native Drive recovery is taking longer than expected. TeraRelay is waiting for the current request.';
        lastError = message;
        setState(current => current.nativeMountError === message
          ? current : { ...current, nativeMountError: message });
      }
      void recoverNativeDrive();
    }, 5_000);
    const onFocus = () => { void recoverNativeDrive(); };
    window.addEventListener('focus', onFocus);
    return () => {
      disposed = true;
      window.clearInterval(timer);
      window.removeEventListener('focus', onFocus);
    };
  }, [authenticated, isMobile]);

  useEffect(() => {
    if (!authenticated) {
      initialSnapshotRef.current = null;
      startupCryptoCheckPendingRef.current = true;
      setState(EMPTY_STATE);
      // Native Drive is owned by the native application lifecycle, not by the
      // WebView. Only a resolved signed-out state should detach it; transient
      // loading/WebView reload must leave the provider alive.
      if (authResolved && !isMobile) {
        void invoke('cmd_drive_unmount').catch(() => {});
      }
      return;
    }

    let disposed = false;
    let syncTimer: number | undefined;
    let periodicSyncInFlight = false;

    const runInitialContinuity = async () => {
      setState(current => ({ ...current, phase: 'restoring', error: null, securityError: null }));

      let before: DriveSnapshotLike;
      let initialSnapshotError: unknown = null;
      try {
        before = validateDriveSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
        if (disposed) return;
        initialSnapshotRef.current = before;
      } catch (error) {
        if (disposed) return;
        initialSnapshotError = error;
        setState(current => ({
          ...current,
          phase: 'degraded',
          error: String(error),
        }));
        // A failed local snapshot must never look like a brand-new pristine
        // device. The sentinel revision/dirty state makes restore
        // classification fail closed while allowing remote reconciliation to
        // continue and retry safely.
        before = { revision: -1, dirty: true, directories: [], files: [], pending: [] };
        initialSnapshotRef.current = before;
      }

      if (!isMobile) {
        try {
          await invoke('cmd_drive_recover_pending');
        } catch (error) {
          if (!disposed) console.warn('[Continuity] Pending Drive recovery deferred:', error);
        }
        // A slow recovery command may have completed after sign-out. Do not
        // start another account-bound Drive operation for the stale session.
        if (disposed) return;
      }

      let syncResult: DriveSyncResultLike | null = null;
      let syncError: unknown = null;
      try {
        syncResult = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
      } catch (error) {
        syncError = error;
        if (!disposed) console.warn('[Continuity] Initial Drive metadata sync deferred:', error);
      }
      if (disposed) return;

      let after = before;
      let afterSnapshotError: unknown = null;
      try {
        after = validateDriveSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
      } catch (error) {
        afterSnapshotError = error;
        if (!disposed) console.warn('[Continuity] Could not refresh Drive snapshot:', error);
      }
      if (disposed) return;

      let crypto: DriveCryptoStatusLike = { configured: false, unlocked: false };
      let securityError: string | null = null;
      try {
        crypto = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_status'));
        if (disposed) return;
        startupCryptoCheckPendingRef.current = false;
      } catch (error) {
        securityError = `Could not read Drive encryption status: ${String(error)}`;
        if (!disposed) console.warn('[Continuity] Could not read Drive encryption status:', error);
      }

      if (!disposed) {
        if (syncResult && !afterSnapshotError) {
          const assessment = classifyDeviceContinuity(before, syncResult, after, crypto);
          initialSnapshotRef.current = after;
          setState(current => ({
            ...current,
            phase: securityError || initialSnapshotError ? 'degraded' : 'ready',
            restoredFromRemote: assessment.restoredFromRemote,
            directoryCount: assessment.directoryCount,
            fileCount: assessment.fileCount,
            needsDriveUnlock: securityError ? false : assessment.needsDriveUnlock,
            syncSource: assessment.syncSource,
            error: initialSnapshotError ? `Could not validate initial Drive snapshot: ${String(initialSnapshotError)}` : null,
            securityError,
          }));
        } else {
          setState(current => ({
            ...current,
            phase: 'degraded',
            restoredFromRemote: false,
            directoryCount: after.directories.length,
            fileCount: after.files.length,
            needsDriveUnlock: securityError ? false : crypto.configured && !crypto.unlocked,
            syncSource: null,
            error: afterSnapshotError ? `Could not validate refreshed Drive snapshot: ${String(afterSnapshotError)}`
              : syncError ? String(syncError) : 'Drive metadata sync is unavailable',
            securityError,
          }));
        }
      }

      // The independent recovery effect owns the initial mount request.
      // A delayed metadata sync must not revive an explicitly unmounted Drive.
    };

    const runPeriodicSync = async () => {
      if (periodicSyncInFlight) return;
      const before = initialSnapshotRef.current;
      if (!before) return;

      periodicSyncInFlight = true;
      try {
        const syncResult = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if (disposed) return;
        const after = validateDriveSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
        if (disposed) return;
        let crypto: DriveCryptoStatusLike = { configured: false, unlocked: false };
        let securityError: string | null = null;
        let allowStartupUnlock = false;
        try {
          crypto = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_status'));
          if (disposed) return;
          allowStartupUnlock = startupCryptoCheckPendingRef.current;
          startupCryptoCheckPendingRef.current = false;
        } catch (error) {
          securityError = `Could not read Drive encryption status: ${String(error)}`;
          if (!disposed) console.warn('[Continuity] Could not read Drive encryption status:', error);
        }
        if (disposed) return;
        const assessment = classifyDeviceContinuity(before, syncResult, after, crypto);
        initialSnapshotRef.current = after;
        setState(current => ({
          ...current,
          phase: securityError ? 'degraded' : 'ready',
          restoredFromRemote: current.restoredFromRemote || assessment.restoredFromRemote,
          directoryCount: assessment.directoryCount,
          fileCount: assessment.fileCount,
          // Only a true remote restore may introduce a new automatic unlock prompt
          // after startup. This avoids re-prompting when a user intentionally locks
          // Drive from Settings during the same session. If the OS credential store
          // cannot be checked, fail closed instead of assuming Drive is unencrypted.
          needsDriveUnlock: securityError
            ? current.needsDriveUnlock
            : current.needsDriveUnlock
              || (allowStartupUnlock && assessment.needsDriveUnlock)
              || (assessment.restoredFromRemote && assessment.needsDriveUnlock),
          syncSource: assessment.syncSource,
          error: null,
          securityError,
        }));
      } catch (error) {
        if (!disposed) {
          setState(current => ({ ...current, phase: 'degraded', error: String(error) }));
        }
      } finally {
        periodicSyncInFlight = false;
      }
    };

    void runInitialContinuity().finally(() => {
      if (disposed) return;
      syncTimer = window.setInterval(() => {
        void runPeriodicSync();
      }, 15_000);
    });

    return () => {
      disposed = true;
      if (syncTimer !== undefined) window.clearInterval(syncTimer);
      // Do not unmount here. Effect/WebView teardown is not equivalent to
      // signing out; the native application owns Drive lifetime. The resolved
      // unauthenticated branch above performs the security detach instead.
    };
  }, [authenticated, authResolved, isMobile]);

  return { state, markDriveUnlocked };
}
