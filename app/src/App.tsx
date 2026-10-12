import React, { useState, useEffect, useRef, Suspense } from "react";
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

type AuthStatus = "loading" | "authenticated" | "unauthenticated";

function AppContent() {
  const [authStatus, setAuthStatus] = useState<AuthStatus>("loading");
  const { theme } = useTheme();
  const { isMobile } = usePlatform();
  const { settings, updateSetting, isLoaded } = useSettings();
  const { i18n } = useTranslation();
  const { state: continuity, markDriveUnlocked } = useDeviceContinuity({
    authenticated: authStatus === "authenticated",
    authResolved: authStatus !== "loading",
    isMobile,
  });
  const [continuityUnlockDismissed, setContinuityUnlockDismissed] = useState(false);
  const continuityRestoreNotifiedRef = useRef(false);
  const continuityWarningNotifiedRef = useRef(false);
  const continuitySecurityNotifiedRef = useRef(false);

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

  // On mount: check for a saved session and auto-restore it.
  // This is the SINGLE source of truth for the initial connection.
  // useTelegramConnection (inside Dashboard) no longer calls cmd_connect on mount.
  useEffect(() => {
    const checkSession = async () => {
      try {
        // Explicit isolated QA mode used only by automated/manual AI Workspace
        // testing. Production/release builds cannot enable this path.
        const qaFeatureAEnabled =
          import.meta.env.DEV && import.meta.env.VITE_TERA_QA_FEATURE_A === '1';
        (globalThis as typeof globalThis & { __TERARELAY_QA_FEATURE_A__?: boolean })
          .__TERARELAY_QA_FEATURE_A__ = qaFeatureAEnabled;
        if (qaFeatureAEnabled) {
          await invoke('cmd_qa_feature_a_seed');
          setAuthStatus("authenticated");
          return;
        }
        try {
          await invoke("cmd_migrate_legacy_api_credentials");
        } catch (migrationError) {
          console.warn("Secure credential migration was unavailable; existing login data was left unchanged.", migrationError);
        }

        const store = await load("config.json");
        const savedId = await store.get<string>("api_id");

        if (!savedId) {
          setAuthStatus("unauthenticated");
          return;
        }

        const apiId = parseInt(savedId, 10);
        if (isNaN(apiId)) {
          setAuthStatus("unauthenticated");
          return;
        }

        // Initialize the client with the saved API ID
        await invoke("cmd_connect", { apiId });

        // Verify the session is still valid with Telegram servers
        const ok = await invoke<boolean>("cmd_check_connection");
        if (ok) {
          setAuthStatus("authenticated");
        } else {
          setAuthStatus("unauthenticated");
        }
      } catch (err) {
        console.warn("Session restore failed, showing login:", err);
        // Keep the application-level Telegram API credentials. A session
        // restore can fail transiently (for example because another TeraRelay
        // process still owns the SQLite session), and deleting api_id here
        // incorrectly turns a recoverable account-session problem into a
        // credentials setup problem.
        setAuthStatus("unauthenticated");
      }
    };

    checkSession();
  }, []);

  useEffect(() => {
    if (authStatus !== "authenticated") {
      setContinuityUnlockDismissed(false);
      continuityRestoreNotifiedRef.current = false;
      continuityWarningNotifiedRef.current = false;
      continuitySecurityNotifiedRef.current = false;
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

  // Styled splash screen while verifying the session
  if (authStatus === "loading") {
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
