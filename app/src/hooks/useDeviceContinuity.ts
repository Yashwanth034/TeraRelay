import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import {
  classifyDeviceContinuity,
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
      try {
        before = await invoke<DriveSnapshotLike>('cmd_drive_get_snapshot');
        if (disposed) return;
        initialSnapshotRef.current = before;
      } catch (error) {
        if (disposed) return;
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
          console.warn('[Continuity] Pending Drive recovery deferred:', error);
        }
      }

      let syncResult: DriveSyncResultLike | null = null;
      let syncError: unknown = null;
      try {
        syncResult = await invoke<DriveSyncResultLike>('cmd_drive_sync_metadata');
      } catch (error) {
        syncError = error;
        console.warn('[Continuity] Initial Drive metadata sync deferred:', error);
      }

      let after = before;
      try {
        after = await invoke<DriveSnapshotLike>('cmd_drive_get_snapshot');
      } catch (error) {
        console.warn('[Continuity] Could not refresh Drive snapshot:', error);
      }

      let crypto: DriveCryptoStatusLike = { configured: false, unlocked: false };
      let securityError: string | null = null;
      try {
        crypto = await invoke<DriveCryptoStatusLike>('cmd_drive_crypto_status');
        if (disposed) return;
        startupCryptoCheckPendingRef.current = false;
      } catch (error) {
        securityError = `Could not read Drive encryption status: ${String(error)}`;
        console.warn('[Continuity] Could not read Drive encryption status:', error);
      }

      if (!disposed) {
        if (syncResult) {
          const assessment = classifyDeviceContinuity(before, syncResult, after, crypto);
          initialSnapshotRef.current = after;
          setState({
            phase: securityError ? 'degraded' : 'ready',
            restoredFromRemote: assessment.restoredFromRemote,
            directoryCount: assessment.directoryCount,
            fileCount: assessment.fileCount,
            needsDriveUnlock: securityError ? false : assessment.needsDriveUnlock,
            syncSource: assessment.syncSource,
            error: null,
            securityError,
          });
        } else {
          setState({
            phase: 'degraded',
            restoredFromRemote: false,
            directoryCount: after.directories.length,
            fileCount: after.files.length,
            needsDriveUnlock: securityError ? false : crypto.configured && !crypto.unlocked,
            syncSource: null,
            error: syncError ? String(syncError) : 'Drive metadata sync is unavailable',
            securityError,
          });
        }
      }

      if (disposed) return;
      if (!isMobile) {
        try {
          await invoke('cmd_drive_mount');
          if (disposed) {
            // If auth resolved to signed-out while the native mount was still
            // starting, undo the late mount. A plain WebView teardown/reload is
            // not an auth transition and must leave the native provider alive.
            if (shouldDetachNativeDriveRef.current) {
              await invoke('cmd_drive_unmount').catch(() => {});
            }
            return;
          }
        } catch (error) {
          console.warn('[Continuity] Native Drive provider unavailable:', error);
        }
      }
    };

    const runPeriodicSync = async () => {
      if (periodicSyncInFlight) return;
      const before = initialSnapshotRef.current;
      if (!before) return;

      periodicSyncInFlight = true;
      try {
        const syncResult = await invoke<DriveSyncResultLike>('cmd_drive_sync_metadata');
        const after = await invoke<DriveSnapshotLike>('cmd_drive_get_snapshot');
        let crypto: DriveCryptoStatusLike = { configured: false, unlocked: false };
        let securityError: string | null = null;
        let allowStartupUnlock = false;
        try {
          crypto = await invoke<DriveCryptoStatusLike>('cmd_drive_crypto_status');
          if (disposed) return;
          allowStartupUnlock = startupCryptoCheckPendingRef.current;
          startupCryptoCheckPendingRef.current = false;
        } catch (error) {
          securityError = `Could not read Drive encryption status: ${String(error)}`;
          console.warn('[Continuity] Could not read Drive encryption status:', error);
        }
        if (disposed) return;
        const assessment = classifyDeviceContinuity(before, syncResult, after, crypto);
        initialSnapshotRef.current = after;
        setState(current => ({
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
