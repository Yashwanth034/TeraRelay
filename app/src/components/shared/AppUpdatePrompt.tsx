import { Download, RefreshCw, ShieldCheck, Smartphone, X } from 'lucide-react';
import { useAppUpdate } from '../../context/AppUpdateContext';

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  const index = Math.min(units.length - 1, Math.floor(Math.log(value) / Math.log(1024)));
  const amount = value / (1024 ** index);
  return `${amount >= 10 || index === 0 ? amount.toFixed(0) : amount.toFixed(1)} ${units[index]}`;
}

export function AppUpdatePrompt() {
  const { status, release, progress, error, isBusy, installUpdate, dismissUpdate, retryUpdate } = useAppUpdate();

  if (!release) return null;

  const downloading = status === 'downloading';
  const installing = status === 'installing';
  const permissionRequired = status === 'permission-required';
  const failed = status === 'error';
  const platformLabel = release.platform === 'android'
    ? 'Android'
    : release.platform === 'ios'
      ? 'iPhone / iPad'
      : 'Desktop';
  const primaryLabel = failed
    ? 'Retry'
    : permissionRequired
      ? 'Continue installation'
      : release.platform === 'ios'
        ? 'Open update page'
        : status === 'ready-to-install' && release.platform === 'android'
          ? 'Open installer'
          : downloading
            ? 'Downloading…'
            : installing
              ? 'Installing…'
              : 'Update now';

  return (
    <div className="fixed inset-0 z-[220] flex items-center justify-center p-4 bg-black/55 backdrop-blur-sm">
      <div role="dialog" aria-modal="true" aria-labelledby="app-update-title" className="w-full max-w-md rounded-2xl border border-telegram-border bg-telegram-surface shadow-2xl overflow-hidden">
        <div className="flex items-start justify-between gap-4 p-5 border-b border-telegram-border/70">
          <div className="flex items-start gap-3 min-w-0">
            <div className="mt-0.5 p-2 rounded-xl bg-telegram-primary/10 text-telegram-primary border border-telegram-primary/20">
              <Download className="w-5 h-5" />
            </div>
            <div className="min-w-0">
              <h2 id="app-update-title" className="text-base font-bold text-telegram-text">TeraRelay update available</h2>
              <p className="mt-1 text-xs text-telegram-subtext">
                {release.currentVersion} → <span className="font-semibold text-telegram-primary">{release.version}</span> · {platformLabel}
              </p>
            </div>
          </div>
          {!installing && (
            <button onClick={() => void dismissUpdate()} disabled={isBusy} aria-label="Later" className="p-1.5 rounded-lg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-hover disabled:opacity-40 transition">
              <X className="w-4 h-4" />
            </button>
          )}
        </div>

        <div className="p-5 space-y-4">
          {release.notes && (
            <div className="rounded-xl border border-telegram-border/70 bg-telegram-bg/40 p-3">
              <p className="text-[10px] uppercase tracking-wide font-bold text-telegram-subtext mb-1.5">Release notes</p>
              <p className="text-xs text-telegram-text/90 whitespace-pre-wrap max-h-32 overflow-y-auto leading-relaxed">{release.notes}</p>
            </div>
          )}

          {permissionRequired && (
            <div className="flex gap-2.5 rounded-xl border border-amber-500/25 bg-amber-500/5 p-3">
              <Smartphone className="w-4 h-4 text-amber-400 mt-0.5 flex-shrink-0" />
              <p className="text-xs text-amber-200/90 leading-relaxed">
                The APK is already downloaded and verified. Android requires permission for TeraRelay to request app installs. Enable it in the system screen, return here, then continue installation.
              </p>
            </div>
          )}

          {(downloading || installing || status === 'ready-to-install') && (
            <div className="space-y-2">
              <div className="flex items-center justify-between text-[11px] text-telegram-subtext">
                <span>{installing ? 'Installing verified update…' : status === 'ready-to-install' ? 'Verified update ready to install' : 'Downloading verified update…'}</span>
                {progress.percent !== null && <span>{progress.percent}%</span>}
              </div>
              <div className="h-2 rounded-full bg-telegram-border/40 overflow-hidden">
                {progress.percent !== null ? (
                  <div className="h-full bg-telegram-primary transition-[width] duration-200" style={{ width: `${progress.percent}%` }} />
                ) : (
                  <div className="h-full w-1/3 bg-telegram-primary animate-pulse rounded-full" />
                )}
              </div>
              {progress.downloaded > 0 && (
                <p className="text-[10px] text-telegram-subtext">
                  {formatBytes(progress.downloaded)}{progress.total > 0 ? ` / ${formatBytes(progress.total)}` : ''}
                </p>
              )}
            </div>
          )}

          {error && <div className="rounded-xl border border-red-500/25 bg-red-500/5 p-3 text-xs text-red-300 leading-relaxed">{error}</div>}

          <div className="flex items-center gap-2 text-[10px] text-telegram-subtext/80">
            <ShieldCheck className="w-3.5 h-3.5 text-emerald-400" />
            <span>{release.platform === 'desktop' ? 'Desktop packages are cryptographically signed before installation.' : release.platform === 'android' ? 'The APK is SHA-256 verified before Android is allowed to install it.' : 'Installation is handled by Apple’s update channel.'}</span>
          </div>
        </div>

        <div className="flex items-center justify-end gap-2 px-5 py-4 border-t border-telegram-border/70 bg-telegram-bg/20">
          {!installing && (
            <button onClick={() => void dismissUpdate()} disabled={isBusy} className="px-3.5 py-2 rounded-xl text-xs font-semibold text-telegram-subtext hover:text-telegram-text hover:bg-telegram-hover disabled:opacity-40 transition">
              Later
            </button>
          )}
          <button onClick={() => void (failed ? retryUpdate() : installUpdate())} disabled={isBusy} className="flex items-center gap-1.5 px-4 py-2 rounded-xl text-xs font-semibold bg-telegram-primary text-white hover:bg-telegram-primary/90 disabled:opacity-50 transition">
            {(isBusy || installing) && <RefreshCw className="w-3.5 h-3.5 animate-spin" />}
            {primaryLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
