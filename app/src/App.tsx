import React, { useState, useEffect, useRef, Suspense } from "react";
import { MotionConfig } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import { load } from "@tauri-apps/plugin-store";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AuthWizard } from "./components/shared/AuthWizard";
import { DriveContinuityUnlock } from "./components/shared/DriveContinuityUnlock";
import { AppUpdatePrompt } from "./components/shared/AppUpdatePrompt";
import { ErrorBoundary } from "./components/shared/ErrorBoundary";
import { TeraRelayBrand } from "./components/shared/TeraRelayBrand";
import { useDeviceContinuity } from "./hooks/useDeviceContinuity";
import { usePlatform } from "./hooks/usePlatform";
import "./App.css";
import "./NeutralDesktop.css";

const DesktopDashboard = React.lazy(() => import("./components/desktop/DesktopDashboard").then(m => ({ default: m.Dashboard })));
// Vite requires a fully static import path for dynamic imports so it can
// perform static analysis and code-splitting. Template literals with
// variables prevent Vite from resolving the module at build time.
const MobileDashboard = React.lazy(() => import("./components/mobile/MobileDashboard.tsx"));

import { Toaster, toast } from "sonner";
import { ConfirmProvider } from "./context/ConfirmContext";
import { ThemeProvider, useTheme } from "./context/ThemeContext";
import { SettingsProvider } from "./context/SettingsContext";
import { AppUpdateProvider } from "./context/AppUpdateContext";
import { useSettings } from "./context/SettingsContext";
import { FastTransferAuthProvider } from "./context/FastTransferAuthContext";
import { TransferMethodProvider } from "./context/TransferMethodContext";
import { UploadSuggestionProvider } from "./context/UploadSuggestionContext";
import { useTranslation } from "react-i18next";

const queryClient = new QueryClient();

type AuthStatus = "loading" | "restoring" | "authenticated" | "unauthenticated";

function AppContent() {
  const [authStatus, setAuthStatus] = useState<AuthStatus>("loading");
  const [sessionRecoveryDetail, setSessionRecoveryDetail] = useState("");
  const [sessionRetryGeneration, setSessionRetryGeneration] = useState(0);
  const [manualSignIn, setManualSignIn] = useState(false);
  const { theme } = useTheme();
  const { isMobile } = usePlatform();
  const { settings, updateSetting, isLoaded } = useSettings();
  const { i18n } = useTranslation();
  const { state: continuity, markDriveUnlocked } = useDeviceContinuity({
    authenticated: authStatus === "authenticated",
    authResolved: authStatus === "authenticated" || authStatus === "unauthenticated",
    isMobile,
  });
  const [continuityUnlockDismissed, setContinuityUnlockDismissed] = useState(false);
  const continuityRestoreNotifiedRef = useRef(false);
  const continuityWarningNotifiedRef = useRef(false);
  const continuitySecurityNotifiedRef = useRef(false);
  const continuityMountWarningRef = useRef(false);

  const handleLogout = () => {
    // Cancel in-flight queries and discard account data before another login.
    queryClient.clear();
    setAuthStatus("unauthenticated");
  };

  // Handle active language and RTL direction changes
  useEffect(() => {
    if (!isLoaded) return;
    i18n.changeLanguage(settings.language);
    document.documentElement.lang = settings.language;
    document.documentElement.dir = settings.language === 'ar' ? 'rtl' : 'ltr';
  }, [settings.language, isLoaded, i18n]);

  // Performance mode: auto-enable when user has prefers-reduced-motion
  useEffect(() => {
    const mediaQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    if (mediaQuery.matches && !settings.performanceMode) {
      updateSetting('performanceMode', true);
    }
    const handler = (e: MediaQueryListEvent) => {
      if (e.matches && !settings.performanceMode) {
        updateSetting('performanceMode', true);
      }
    };
    mediaQuery.addEventListener('change', handler);
    return () => mediaQuery.removeEventListener('change', handler);
  }, []);

  // Apply performance-mode class to body (guarded by settings load to avoid flicker)
  useEffect(() => {
    if (!isLoaded) return;
    if (settings.performanceMode) {
      document.body.classList.add('performance-mode');
    } else {
      document.body.classList.remove('performance-mode');
    }
  }, [settings.performanceMode, isLoaded]);

  // A saved session is authoritative until the user explicitly signs out.
  // Native background startup and the frontend share one locked initializer.
  // Transient network failures must never be interpreted as a fresh login.
  useEffect(() => {
    if (manualSignIn) {
      setAuthStatus("unauthenticated");
      return;
    }
    let cancelled = false;
    let wake: (() => void) | undefined;
    const delay = (ms: number) => new Promise<void>(resolve => {
      const timer = setTimeout(resolve, ms);
      wake = () => { clearTimeout(timer); resolve(); };
    });
    const restoreSession = async () => {
      // QA-only bypass remains isolated from real saved-account restoration.
      const qaFeatureAEnabled =
        import.meta.env.DEV && import.meta.env.VITE_TERA_QA_FEATURE_A === '1';
      (globalThis as typeof globalThis & { __TERARELAY_QA_FEATURE_A__?: boolean })
        .__TERARELAY_QA_FEATURE_A__ = qaFeatureAEnabled;
      if (qaFeatureAEnabled) {
        try {
          await invoke('cmd_qa_feature_a_seed');
          if (!cancelled) setAuthStatus("authenticated");
        } catch (error) {
          if (!cancelled) {
            setSessionRecoveryDetail(String(error));
            setAuthStatus("restoring");
          }
        }
        return;
      }

      setAuthStatus("restoring");
      let attempt = 0;
      while (!cancelled) {
        try {
          // Read-only existence check avoids creating an empty SQLite session.
          const hasSession = await invoke<boolean>("cmd_saved_telegram_session_exists");
          if (cancelled) return;
          if (!hasSession) {
            setAuthStatus("unauthenticated");
            return;
          }

          try {
            await invoke("cmd_migrate_legacy_api_credentials");
          } catch (error) {
            console.warn("Credential migration unavailable; the saved session was preserved.", error);
          }
          const store = await load("config.json");
          const savedId = await store.get<string | number>("api_id");
          const apiId = typeof savedId === "number" ? savedId : Number(savedId);
          if (!Number.isSafeInteger(apiId) || apiId <= 0) {
            setSessionRecoveryDetail("Your saved Telegram session exists, but its API ID is unavailable. The session has not been deleted.");
          } else {
            const connected = await invoke<boolean>("cmd_restore_saved_telegram_session", { apiId });
            if (cancelled) return;
            if (connected) {
              setAuthStatus("authenticated");
              return;
            }
            // The user may have explicitly signed out while we were waiting.
            setAuthStatus("unauthenticated");
            return;
          }
        } catch (error) {
          if (cancelled) return;
          console.warn("Saved session reconnect unavailable; preserving the session.", error);
          setSessionRecoveryDetail("Telegram is not responding yet. TeraRelay is keeping your saved login and retrying automatically.");
        }
        if (cancelled) return;
        attempt += 1;
        await delay(Math.min(30_000, 5_000 * Math.min(attempt, 6)));
      }
    };
    void restoreSession();
    return () => {
      cancelled = true;
      wake?.();
    };
  }, [manualSignIn, sessionRetryGeneration]);

  // Give routine restoration time to finish without flashing recovery controls.
  // A slow native request must still leave a way to retry or choose phone sign-in.
  useEffect(() => {
    if (authStatus !== "restoring" || sessionRecoveryDetail) return;
    const timer = window.setTimeout(() => {
      setSessionRecoveryDetail("Connecting is taking longer than usual. Your saved login is preserved; you can keep waiting or retry.");
    }, 8_000);
    return () => window.clearTimeout(timer);
  }, [authStatus, sessionRecoveryDetail, sessionRetryGeneration]);

  useEffect(() => {
    if (authStatus !== "authenticated") {
      setContinuityUnlockDismissed(false);
      continuityRestoreNotifiedRef.current = false;
      continuityWarningNotifiedRef.current = false;
      continuitySecurityNotifiedRef.current = false;
      continuityMountWarningRef.current = false;
      return;
    }

    if (continuity.restoredFromRemote && !continuityRestoreNotifiedRef.current) {
      continuityRestoreNotifiedRef.current = true;
      const restoredItems = continuity.directoryCount + continuity.fileCount;
      const detail = restoredItems > 0
        ? `${continuity.directoryCount} folder${continuity.directoryCount === 1 ? '' : 's'} and ${continuity.fileCount} file${continuity.fileCount === 1 ? '' : 's'}`
        : 'your existing TeraRelay cloud state';
      toast.success(`TeraRelay restored ${detail} from Telegram`);
    }

    if (continuity.nativeMountError && !continuityMountWarningRef.current) {
      continuityMountWarningRef.current = true;
      toast.warning('TeraRelay Drive could not mount. Your files were not changed; Drive will retry automatically.');
    } else if (!continuity.nativeMountError && continuityMountWarningRef.current) {
      continuityMountWarningRef.current = false;
      toast.success('TeraRelay Drive is available again.');
    }

    if (continuity.securityError && !continuitySecurityNotifiedRef.current) {
      continuitySecurityNotifiedRef.current = true;
      toast.warning('TeraRelay cloud metadata is available, but secure Drive key storage could not be checked on this device. Encrypted Drive contents stay locked until secure storage is available.');
    }

    if (
      continuity.phase === 'degraded'
      && continuity.error
      && continuity.directoryCount === 0
      && continuity.fileCount === 0
      && !continuityWarningNotifiedRef.current
    ) {
      continuityWarningNotifiedRef.current = true;
      toast.warning('Could not restore TeraRelay cloud data yet. Nothing was changed; TeraRelay will retry automatically.');
    }
  }, [
    authStatus,
    continuity.phase,
    continuity.error,
    continuity.securityError,
    continuity.nativeMountError,
    continuity.restoredFromRemote,
    continuity.directoryCount,
    continuity.fileCount,
  ]);

  // Clean up PDF preview cache files on close/beforeunload
  useEffect(() => {
    const handleClose = () => {
      invoke("cmd_clean_preview_cache").catch(() => {});
    };

    window.addEventListener("beforeunload", handleClose);
    return () => {
      window.removeEventListener("beforeunload", handleClose);
      handleClose();
    };
  }, []);

  // Recovery actions are for a reported problem, not a routine saved-session connection.
  // Keep the saved account intact when Telegram is temporarily offline.
  if (authStatus === "restoring" && sessionRecoveryDetail) {
    return (
      <main className="h-screen w-screen auth-gradient terarelay-auth-stage flex flex-col items-center justify-center gap-4 px-6 text-center">
        <TeraRelayBrand size="lg" showName={false} />
        <p className="text-telegram-text font-medium">Restoring your saved Telegram session</p>
        <p className="text-sm text-telegram-subtext max-w-md" role="status">
          {sessionRecoveryDetail || "Connecting to Telegram using your existing login. No code is needed."}
        </p>
        <div className="flex flex-wrap items-center justify-center gap-3">
          <button type="button" className="rounded-lg bg-telegram-primary px-4 py-2 text-sm text-white"
            onClick={() => { setSessionRecoveryDetail(""); setSessionRetryGeneration(n => n + 1); }}>
            Retry now
          </button>
          <button type="button" className="rounded-lg bg-telegram-hover px-4 py-2 text-sm text-telegram-text"
            onClick={() => setManualSignIn(true)}>
            Use phone sign-in instead
          </button>
        </div>
        <p className="text-xs text-telegram-subtext max-w-md">
          Manual sign-in is optional. Your saved session and its backups have not been cleared.
        </p>
      </main>
    );
  }

  // Normal restoration has no recovery warning until an actual issue is reported.
  if (authStatus === "loading" || authStatus === "restoring") {
    return (
      <main className="h-screen w-screen auth-gradient terarelay-auth-stage flex items-center justify-center">
        <div className="flex flex-col items-center gap-3">
          <TeraRelayBrand size="lg" showName={false} />
          <p className="terarelay-eyebrow">TERARELAY</p>
          <p className="text-sm text-telegram-subtext tracking-wide">Opening your private vault…</p>
        </div>
      </main>
    );
  }

  return (
    <MotionConfig reducedMotion={settings.performanceMode ? "always" : "user"}>
    <main className="absolute inset-0 text-telegram-text overflow-hidden selection:bg-telegram-primary/30">
      <Toaster theme={theme} position="bottom-center" closeButton />
      <AppUpdatePrompt />
      {authStatus === "authenticated" && (
        <Suspense fallback={
          <div className="h-screen w-screen flex flex-col items-center justify-center bg-telegram-bg">
            <div className="animate-spin rounded-full h-8 w-8 border-t-2 border-b-2 border-telegram-primary"></div>
          </div>
        }>
          {isMobile ? (
            <ErrorBoundary>
              <MobileDashboard onLogout={handleLogout} />
            </ErrorBoundary>
          ) : (
            <ErrorBoundary>
              <DesktopDashboard onLogout={handleLogout} />
            </ErrorBoundary>
          )}
        </Suspense>
      )}
      {authStatus === "authenticated" && (
        <DriveContinuityUnlock
          open={continuity.needsDriveUnlock && !continuityUnlockDismissed}
          restoredFromRemote={continuity.restoredFromRemote}
          directoryCount={continuity.directoryCount}
          fileCount={continuity.fileCount}
          onUnlocked={() => {
            markDriveUnlocked();
            setContinuityUnlockDismissed(true);
          }}
          onDismiss={() => setContinuityUnlockDismissed(true)}
        />
      )}
      {authStatus === "unauthenticated" && (
        <AuthWizard onLogin={() => setAuthStatus("authenticated")} />
      )}
    </main>
    </MotionConfig>
  );
}


function App() {
  return (
    <ErrorBoundary>
      <ThemeProvider>
        <QueryClientProvider client={queryClient}>
          <ConfirmProvider>
            <SettingsProvider>
              <AppUpdateProvider>
                <FastTransferAuthProvider>
                  <TransferMethodProvider>
                    <UploadSuggestionProvider>
                      <AppContent />
                    </UploadSuggestionProvider>
                  </TransferMethodProvider>
                </FastTransferAuthProvider>
              </AppUpdateProvider>
            </SettingsProvider>
          </ConfirmProvider>
        </QueryClientProvider>
      </ThemeProvider>
    </ErrorBoundary>
  );
}

export default App;
