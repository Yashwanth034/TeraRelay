import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { type as osType } from '@tauri-apps/plugin-os';
import { openUrl } from '@tauri-apps/plugin-opener';
import { version as appVersion } from '../../package.json';
import { mobileManifestTargetsPlatform, type AppUpdateRelease, type AppUpdateStatus, type MobileUpdateManifest } from '../appUpdater';

interface AndroidProgressPayload {
  downloaded: number;
  total: number;
}

interface AppUpdateProgress {
  downloaded: number;
  total: number;
  percent: number | null;
}

interface MobileUpdateInfo {
  version: string;
  current_version: string;
  notes: string | null;
  published_at: string | null;
  android: MobileUpdateManifest['android'];
  ios_store_url: string | null;
}

interface AppUpdateContextValue {
  status: AppUpdateStatus;
  release: AppUpdateRelease | null;
  progress: AppUpdateProgress;
  error: string | null;
  isBusy: boolean;
  checkForUpdates: (manual?: boolean) => Promise<void>;
  installUpdate: () => Promise<void>;
  dismissUpdate: () => Promise<void>;
  retryUpdate: () => Promise<void>;
}

type DesktopUpdateHandle = {
  currentVersion: string;
  version: string;
  date?: string;
  body?: string;
  downloadAndInstall: (onEvent?: (event: {
    event: 'Started' | 'Progress' | 'Finished';
    data?: { contentLength?: number; chunkLength?: number };
  }) => void) => Promise<void>;
  close: () => Promise<void>;
};

const EMPTY_PROGRESS: AppUpdateProgress = { downloaded: 0, total: 0, percent: null };
const AppUpdateContext = createContext<AppUpdateContextValue | null>(null);

function messageFromError(error: unknown): string {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === 'string' && error.trim()) return error;
  return 'The update operation failed. Please try again.';
}

function runtimePlatform(): 'desktop' | 'android' | 'ios' {
  try {
    const value = osType();
    if (value === 'android') return 'android';
    if (value === 'ios') return 'ios';
    return 'desktop';
  } catch {
    const ua = navigator.userAgent.toLowerCase();
    if (ua.includes('android')) return 'android';
    if (ua.includes('iphone') || ua.includes('ipad')) return 'ios';
    return 'desktop';
  }
}

function progressState(downloaded: number, total: number): AppUpdateProgress {
  return {
    downloaded,
    total,
    percent: total > 0 ? Math.min(100, Math.round((downloaded / total) * 100)) : null,
  };
}

export function AppUpdateProvider({ children }: { children: ReactNode }) {
  const [status, setStatus] = useState<AppUpdateStatus>('idle');
  const [release, setRelease] = useState<AppUpdateRelease | null>(null);
  const [progress, setProgress] = useState<AppUpdateProgress>(EMPTY_PROGRESS);
  const [error, setError] = useState<string | null>(null);
  const desktopUpdateRef = useRef<DesktopUpdateHandle | null>(null);
  const dismissedVersionRef = useRef<string | null>(null);
  const verifiedApkRef = useRef<{ version: string; path: string } | null>(null);
  const busyRef = useRef(false);

  const releaseDesktopHandle = useCallback(async () => {
    const current = desktopUpdateRef.current;
    desktopUpdateRef.current = null;
    if (current) {
      try {
        await current.close();
      } catch {
        // Releasing an updater resource is best effort; it must never block TeraRelay.
      }
    }
  }, []);

  const checkForUpdates = useCallback(async (manual = true) => {
    if (busyRef.current) return;
    busyRef.current = true;
    setStatus('checking');
    setError(null);
    setProgress(EMPTY_PROGRESS);
    try {
      const platform = runtimePlatform();
      if (platform === 'desktop') {
        const { check } = await import('@tauri-apps/plugin-updater');
        await releaseDesktopHandle();
        const update = await check();
        if (!update) {
          setRelease(null);
          setStatus(manual ? 'up-to-date' : 'idle');
          return;
        }
        const handle = update as unknown as DesktopUpdateHandle;
        if (!manual && dismissedVersionRef.current === handle.version) {
          await handle.close();
          setStatus('idle');
          return;
        }
        desktopUpdateRef.current = handle;
        setRelease({
          version: handle.version,
          currentVersion: handle.currentVersion || appVersion,
          notes: handle.body ?? null,
          publishedAt: handle.date ?? null,
          platform: 'desktop',
        });
        setStatus('available');
        return;
      }

      const update = await invoke<MobileUpdateInfo | null>('cmd_mobile_check_update');
      if (!update || !mobileManifestTargetsPlatform({ android: update.android, iosStoreUrl: update.ios_store_url }, platform)) {
        setRelease(null);
        setStatus(manual ? 'up-to-date' : 'idle');
        return;
      }
      if (!manual && dismissedVersionRef.current === update.version) {
        setStatus('idle');
        return;
      }
      setRelease({
        version: update.version,
        currentVersion: update.current_version || appVersion,
        notes: update.notes,
        publishedAt: update.published_at,
        platform,
        android: update.android,
        iosStoreUrl: update.ios_store_url,
      });
      setStatus('available');
    } catch (caught) {
      const message = messageFromError(caught);
      setError(message);
      if (manual) {
        setStatus('error');
      } else {
        console.warn('TeraRelay startup update check failed:', caught);
        setStatus('idle');
      }
    } finally {
      busyRef.current = false;
    }
  }, [releaseDesktopHandle]);

  useEffect(() => {
    // startup update check: intentionally delayed so updater I/O never slows launch/auth.
    const timer = window.setTimeout(() => {
      void checkForUpdates(false);
    }, 4500);
    return () => window.clearTimeout(timer);
  }, [checkForUpdates]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<AndroidProgressPayload>('app-update-download-progress', (event) => {
      if (disposed) return;
      setProgress(progressState(event.payload.downloaded, event.payload.total));
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => {
      // Desktop/iOS never emit this event; listening failure is non-fatal.
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => () => {
    const current = desktopUpdateRef.current;
    desktopUpdateRef.current = null;
    if (current) void current.close().catch(() => {});
  }, []);

  const installUpdate = useCallback(async () => {
    if (!release || busyRef.current) return;
    busyRef.current = true;
    setError(null);
    try {
      if (release.platform === 'desktop') {
        const update = desktopUpdateRef.current;
        if (!update) throw new Error('Update metadata expired. Check for updates again.');
        let downloaded = 0;
        let total = 0;
        setProgress(EMPTY_PROGRESS);
        setStatus('downloading');
        await update.downloadAndInstall((event) => {
          if (event.event === 'Started') {
            total = event.data?.contentLength ?? 0;
            setProgress(progressState(downloaded, total));
          } else if (event.event === 'Progress') {
            downloaded += event.data?.chunkLength ?? 0;
            setProgress(progressState(downloaded, total));
          } else if (event.event === 'Finished') {
            setProgress(progressState(total || downloaded, total || downloaded));
            setStatus('installing');
          }
        });
        if (runtimePlatform() === 'desktop') {
          const { relaunch } = await import('@tauri-apps/plugin-process');
          await relaunch();
        }
        return;
      }

      if (release.platform === 'ios') {
        if (!release.iosStoreUrl) {
          throw new Error('This iOS build does not have a configured App Store or TestFlight update channel yet.');
        }
        setStatus('installing');
        await openUrl(release.iosStoreUrl);
        setStatus('available');
        return;
      }

      const asset = release.android;
      if (!asset) throw new Error('This release does not contain an Android APK update.');
      let apkPath = verifiedApkRef.current?.version === release.version
        ? verifiedApkRef.current.path
        : null;
      if (!apkPath) {
        setStatus('downloading');
        setProgress(EMPTY_PROGRESS);
        apkPath = await invoke<string>('cmd_android_download_update', {
          version: release.version,
          url: asset.url,
          sha256: asset.sha256,
        });
        verifiedApkRef.current = { version: release.version, path: apkPath };
        setStatus('ready-to-install');
      }
      const allowed = await invoke<boolean>('cmd_android_can_install_packages');
      if (!allowed) {
        setStatus('permission-required');
        await invoke('cmd_android_open_install_permission');
        return;
      }
      setStatus('installing');
      await invoke('cmd_android_install_update', { apkPath, sha256: asset.sha256 });
      // Android's package installer owns the final user confirmation. If the
      // user cancels or returns to TeraRelay, keep the verified APK retryable.
      setStatus('ready-to-install');
    } catch (caught) {
      setError(messageFromError(caught));
      setStatus('error');
    } finally {
      busyRef.current = false;
    }
  }, [release]);

  const dismissUpdate = useCallback(async () => {
    if (release) dismissedVersionRef.current = release.version;
    setError(null);
    setProgress(EMPTY_PROGRESS);
    setRelease(null);
    setStatus('idle');
    await releaseDesktopHandle();
  }, [release, releaseDesktopHandle]);

  const retryUpdate = useCallback(async () => {
    if (release) {
      await installUpdate();
    } else {
      await checkForUpdates(true);
    }
  }, [release, installUpdate, checkForUpdates]);

  const isBusy = status === 'checking' || status === 'downloading' || status === 'installing';

  return (
    <AppUpdateContext.Provider value={{
      status,
      release,
      progress,
      error,
      isBusy,
      checkForUpdates,
      installUpdate,
      dismissUpdate,
      retryUpdate,
    }}>
      {children}
    </AppUpdateContext.Provider>
  );
}

export function useAppUpdate(): AppUpdateContextValue {
  const context = useContext(AppUpdateContext);
  if (!context) throw new Error('useAppUpdate must be used within AppUpdateProvider');
  return context;
}
