import { useState, useCallback, useMemo, useEffect } from 'react';
import { Folder, Download, Menu, LogOut, RefreshCw, UploadCloud, MoreVertical, Trash2, Pencil, Globe, Shield, Lock, ChevronDown, Wifi, Activity, Zap, Eye, EyeOff } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { openUrl } from '@tauri-apps/plugin-opener';
import { useQuery } from '@tanstack/react-query';
import { toast } from 'sonner';
import { BottomNavBar } from './BottomNavBar';
import { TouchFileList } from './TouchFileList';
import { ThemeToggle } from '../shared/ThemeToggle';
import { TeraRelayBrand } from '../shared/TeraRelayBrand';
import { ChannelInfoPanel } from '../desktop/dashboard/ChannelInfoPanel';
import { ActionPopover, ActionItem } from './ActionPopover';
import { RenameFolderSheet } from './RenameFolderSheet';
import { usePlatform } from '../../hooks/usePlatform';
import { useTelegramConnection } from '../../hooks/useTelegramConnection';
import { useFileUpload } from '../../hooks/useFileUpload';
import { useFileDownload } from '../../hooks/useFileDownload';
import { useFileOperations } from '../../hooks/useFileOperations';
import { formatBytes, getFileCategory, isImageFile, copyToClipboard, type FileCategory } from '../../utils';
import { PreviewModal } from '../desktop/dashboard/PreviewModal';
import { TelegramFile, TelegramFolder, BandwidthStats } from '../../types';
import { useSettings } from '../../context/SettingsContext';
import { version as appVersion } from '../../../package.json';
import { LANGUAGES } from '../../i18n/languages';
import { useTranslation } from 'react-i18next';

export default function MobileDashboard({ onLogout }: { onLogout?: () => void }) {
  const { t } = useTranslation();
  const [activeTab, setActiveTab] = useState<'files' | 'downloads' | 'settings'>('files');
  const [isSidebarOpen, setIsSidebarOpen] = useState(false);
  const { isAndroid } = usePlatform();
  const { settings, updateSetting } = useSettings();

  // Sync proxy settings to backend whenever they change
  useEffect(() => {
    const applyProxy = async () => {
      try {
        await invoke('cmd_apply_proxy_settings', {
          enabled: settings.proxyEnabled,
          proxyType: settings.proxyType,
          host: settings.proxyHost,
          port: settings.proxyPort,
          username: settings.proxyUsername,
          ["password"]: settings.proxyPassword,
        });
      } catch {
        // best-effort sync
      }
    };
    applyProxy();
  }, [
    settings.proxyEnabled, settings.proxyType, settings.proxyHost,
    settings.proxyPort, settings.proxyUsername, settings.proxyPassword,
  ]);

  const logoutHandler = useMemo(() => onLogout || (() => {}), [onLogout]);

  const {
    store, folders, activeFolderId, setActiveFolderId, isSyncing, isConnected,
    handleLogout, handleSyncFolders, handleCreateFolder, handleFolderDelete,
    handleFolderRename, handleFolderToggleVisibility, handleExportFolderInvite
  } = useTelegramConnection(logoutHandler);

  const activeFolderMeta = activeFolderId === null
    ? null
    : folders.find(f => f.id === activeFolderId) || null;
  const activeChannelReadOnly = activeFolderMeta?.role === 'member';
  const activeFolder = activeFolderMeta?.name || 'Personal Vault';
  const [showChannelInfo, setShowChannelInfo] = useState(false);
  const [channelFileCategory, setChannelFileCategory] = useState<FileCategory>('all');

  useEffect(() => {
    setShowChannelInfo(false);
    setChannelFileCategory('all');
  }, [activeFolderId]);

  const { handleManualUpload } = useFileUpload(activeFolderId, store);
  const { queueDownload, queueBulkDownload } = useFileDownload(store);

  const [previewFile, setPreviewFile] = useState<TelegramFile | null>(null);

  // ── Connection diagnostics state ──────────────────────────────────────
  const [checkingLatency, setCheckingLatency] = useState(false);
  const [latencyMs, setLatencyMs] = useState<number | null>(null);

  const { data: bandwidth } = useQuery({
    queryKey: ['bandwidth'],
    queryFn: () => invoke<BandwidthStats>('cmd_get_bandwidth'),
    refetchInterval: activeTab === 'settings' ? 5000 : false,
  });

  const handleCheckLatency = useCallback(async () => {
    setCheckingLatency(true);
    setLatencyMs(null);
    try {
      const ms = await invoke<number>('cmd_check_latency');
      setLatencyMs(ms);
      if (ms >= 0) {
        const emoji = ms < 100 ? '🟢' : ms < 250 ? '🟡' : '🔴';
        toast.success(`${emoji} Ping: ${ms}ms to Telegram DC`);
      } else {
        toast.error('Unable to reach Telegram servers');
      }
    } catch (e) {
      console.warn('Ping check failed:', e);
      toast.error('Unable to reach Telegram servers');
      setLatencyMs(-1);
    } finally {
      setCheckingLatency(false);
    }
  }, []);

  // Real files loader
  const { data: allFiles = [], isLoading } = useQuery({
    queryKey: ['files', activeFolderId],
    queryFn: () => invoke<any[]>('cmd_get_files', { folderId: activeFolderId }).then(res => res.map(f => ({
      ...f,
      sizeStr: formatBytes(f.size),
      type: f.icon_type || (f.name.endsWith('/') ? 'folder' : 'file')
    }))),
    enabled: !!store,
  });

  const [selectedIds, setSelectedIds] = useState<number[]>([]);
  const [fileRenames, setFileRenames] = useState<Map<number, string>>(new Map());
  const { handleDelete: handleDeleteOp, handleBulkDelete, handleBulkDownload, handleBulkMove } = useFileOperations(activeFolderId, selectedIds, setSelectedIds, allFiles, queueBulkDownload);

  // Folder action menu state (replaces swipe-to-reveal)
  const [folderActionMenu, setFolderActionMenu] = useState<TelegramFolder | null>(null);
  const [renameFolder, setRenameFolder] = useState<{ id: number; name: string } | null>(null);

  const handleFolderVisibilityToggle = useCallback(async (folder: TelegramFolder) => {
    const isPublic = folder.is_public || !!folder.username;
    if (isPublic) {
      // Make private
      try {
        await handleFolderToggleVisibility(folder.id, false);
      } catch { /* error already toasted */ }
    } else {
      // Make public — prompt for optional username
      const defaultUsername = folder.name.toLowerCase().replace(/[^a-z0-9_]/g, '').slice(0, 30);
      const username = prompt(`Make "${folder.name}" public. Enter a username (leave empty for auto-generated):`, defaultUsername)?.trim();
      if (username === undefined) return; // cancelled
      try {
        await handleFolderToggleVisibility(folder.id, true, username || undefined);
      } catch { /* error already toasted */ }
    }
  }, [handleFolderToggleVisibility]);

  const handleFolderShareInvite = useCallback(async (folder: TelegramFolder) => {
    try {
      const info = await handleExportFolderInvite(folder.id);
      try {
        await copyToClipboard(info.link);
        toast.success(`Invite link copied: ${info.link}`);
      } catch (e) {
        toast.error(`Failed to copy to clipboard: ${e}`);
      }
    } catch { /* backend error already toasted in hook */ }
  }, [handleExportFolderInvite]);

  const buildFolderActions = useCallback((folder: TelegramFolder): ActionItem[] => {
    if (folder.role === 'member') {
      return [
        {
          label: 'Leave Channel',
          icon: <Trash2 className="w-4 h-4" />,
          onClick: () => handleFolderDelete(folder.id, folder.name),
          destructive: true,
        },
      ];
    }

    const isPublic = folder.is_public || !!folder.username;
    return [
      {
        label: 'Rename',
        icon: <Pencil className="w-4 h-4" />,
        onClick: () => {
          setFolderActionMenu(null);
          setRenameFolder({ id: folder.id, name: folder.name });
        },
      },
      {
        label: isPublic ? 'Make Private' : 'Make Public',
        icon: isPublic ? <EyeOff className="w-4 h-4" /> : <Eye className="w-4 h-4" />,
        onClick: () => handleFolderVisibilityToggle(folder),
      },
      {
        label: 'Delete',
        icon: <Trash2 className="w-4 h-4" />,
        onClick: () => handleFolderDelete(folder.id, folder.name),
        destructive: true,
      },
    ];
  }, [handleFolderDelete, handleFolderVisibilityToggle]);

  const handleSelectAll = useCallback(() => {
    if (selectedIds.length === allFiles.length) {
      setSelectedIds([]);
    } else {
      setSelectedIds(allFiles.map(f => f.id));
    }
  }, [selectedIds.length, allFiles]);

  const handleClearSelection = useCallback(() => setSelectedIds([]), []);

  const handleToggleSelection = useCallback((id: number) => {
    setSelectedIds(prev => prev.includes(id) ? prev.filter(i => i !== id) : [...prev, id]);
  }, []);

  const handleDownload = useCallback((file: TelegramFile) => {
    queueDownload(file.id, file.name, activeFolderId);
  }, [queueDownload, activeFolderId]);

  const handleDeleteFile = useCallback((file: TelegramFile) => {
    if (activeChannelReadOnly) return;
    handleDeleteOp(file.id);
  }, [activeChannelReadOnly, handleDeleteOp]);

  const handlePreview = useCallback((file: TelegramFile) => {
    if (isImageFile(file.name)) {
      setPreviewFile(file);
    } else {
      toast.info('Mobile preview is currently available for images only. Download this file to open it.');
    }
  }, []);

  const handleRenameFile = useCallback((file: TelegramFile) => {
    if (activeChannelReadOnly) return;
    const currentName = fileRenames.get(file.id) || file.name;
    const newName = prompt(`Rename "${currentName}":`, currentName);
    if (!newName || !newName.trim() || newName.trim() === currentName) return;
    setFileRenames(prev => {
      const next = new Map(prev);
      next.set(file.id, newName.trim());
      return next;
    });
    toast.success(`Renamed to "${newName.trim()}"`);
  }, [activeChannelReadOnly, fileRenames]);

  // ── Copy Telegram native t.me link ────────────────────────────────────
  const handleCopyTelegramLink = useCallback((file: TelegramFile) => {
    const folder = folders.find(f => f.id === file.folder_id) || folders.find(f => f.id === activeFolderId);
    const username = folder?.username || (folder as any)?.chat?.username || (folder as any)?.channel?.username;
    if (!username) {
      toast.error('Only available for public channels');
      return;
    }
    const url = `https://t.me/${username}/${file.id}`;
    navigator.clipboard.writeText(url).then(() => {
      toast.success('Telegram link copied');
    }).catch(() => {
      toast.error('Failed to copy link');
    });
  }, [folders, activeFolderId]);

  const renamedFiles = useMemo(() => {
    if (fileRenames.size === 0) return allFiles;
    return allFiles.map(f =>
      fileRenames.has(f.id) ? { ...f, name: fileRenames.get(f.id)! } : f
    );
  }, [allFiles, fileRenames]);

  const displayFiles = useMemo(() => {
    if (activeFolderId === null || channelFileCategory === 'all') return renamedFiles;
    return renamedFiles.filter(file => getFileCategory(file.name) === channelFileCategory);
  }, [renamedFiles, activeFolderId, channelFileCategory]);

  return (
    <div className="absolute inset-0 flex flex-col bg-telegram-bg text-telegram-text overflow-hidden select-none font-sans">
      <header className="sticky top-0 z-40 flex items-center justify-between border-b border-telegram-border/60 bg-telegram-surface/95 px-5 pb-3 pt-[calc(0.75rem+env(safe-area-inset-top,24px))] backdrop-blur-md">
        <TeraRelayBrand size="sm" />
        <div className="flex items-center gap-2">
          <ThemeToggle />
          <button
            onClick={() => setIsSidebarOpen(true)}
            className="p-2 rounded-xl bg-telegram-hover/30 hover:bg-telegram-hover/60 border border-telegram-border/40 text-telegram-subtext transition-all duration-300"
          >
            <Menu className="w-5 h-5" />
          </button>
        </div>
      </header>

      {activeFolderMeta && activeFolderId !== null && (
        <ChannelInfoPanel
          open={showChannelInfo}
          channelId={activeFolderId}
          channelName={activeFolder}
          role={activeFolderMeta.role ?? 'owner'}
          files={renamedFiles}
          activeCategory={channelFileCategory}
          onSelectCategory={(category) => {
            setSelectedIds([]);
            setChannelFileCategory(category);
          }}
          onClose={() => setShowChannelInfo(false)}
          onInvite={activeChannelReadOnly ? undefined : () => handleFolderShareInvite(activeFolderMeta)}
        />
      )}

      {/* Main Viewport Container */}
      <main className="flex-1 overflow-y-auto px-4 py-3 space-y-4 pb-40 scroll-smooth">
        {activeTab === 'files' && (
          <div className="space-y-4">
            <div className="flex items-center justify-between rounded-2xl border border-telegram-border/40 bg-telegram-surface/80 p-3">
              <button
                type="button"
                onClick={() => {
                  if (activeFolderId !== null) setShowChannelInfo(true);
                }}
                className="flex min-w-0 items-center gap-2.5 rounded-xl px-1 py-1 text-left transition active:scale-[0.99]"
              >
                <Folder className="h-5 w-5 shrink-0 text-telegram-primary" />
                <div className="min-w-0">
                  <div className="max-w-[170px] truncate text-sm font-semibold">{activeFolder}</div>
                  <div className="mt-0.5 text-[10px] text-telegram-subtext">
                    {channelFileCategory === 'all'
                      ? `${renamedFiles.length} file${renamedFiles.length === 1 ? '' : 's'}`
                      : `${displayFiles.length} of ${renamedFiles.length} files`}
                  </div>
                </div>
              </button>
              <div className="flex items-center gap-1">
                {!activeChannelReadOnly && (
                  <button
                    onClick={handleManualUpload}
                    className="grid h-9 w-9 place-items-center rounded-xl text-telegram-primary transition active:scale-95 active:bg-telegram-primary/10"
                    title="Add files"
                    aria-label="Add files"
                  >
                    <UploadCloud className="h-4.5 w-4.5" />
                  </button>
                )}
                <button
                  onClick={handleSyncFolders}
                  disabled={isSyncing}
                  className="grid h-9 w-9 place-items-center rounded-xl text-telegram-subtext transition active:scale-95 active:bg-telegram-hover disabled:opacity-50"
                  title="Sync"
                  aria-label="Sync"
                >
                  <RefreshCw className={`h-4.5 w-4.5 ${isSyncing ? 'animate-spin' : ''}`} />
                </button>
              </div>
            </div>

            {/* Dynamic Real File List */}
            <TouchFileList
              files={displayFiles}
              isLoading={isLoading}
              onDownload={handleDownload}
              onDelete={handleDeleteFile}
              onPreview={handlePreview}
              onRename={handleRenameFile}
              onShare={undefined}
              onCopyTelegramLink={handleCopyTelegramLink}
              onBulkShare={undefined}
              selectedIds={selectedIds}
              onToggleSelection={handleToggleSelection}
              onSelectAll={handleSelectAll}
              onClearSelection={handleClearSelection}
              onBulkDelete={handleBulkDelete}
              onBulkDownload={handleBulkDownload}
              onBulkMove={handleBulkMove}
              folders={folders}
              activeFolderId={activeFolderId}
              readOnly={activeChannelReadOnly}
            />
          </div>
        )}

        {activeTab === 'downloads' && (
          <div className="flex flex-col items-center justify-center h-[60vh] space-y-3 text-center px-6">
            <div className="p-4 rounded-full bg-telegram-primary/10 text-telegram-primary border border-telegram-primary/20">
              <Download className="w-8 h-8 animate-bounce" />
            </div>
            <h3 className="text-base font-bold">Transfers Queue</h3>
            <p className="text-xs text-telegram-subtext max-w-xs leading-relaxed">
              Downloads and uploads are safely queued and managed in the background.
            </p>
          </div>
        )}

        {activeTab === 'settings' && (
          <div className="space-y-4">
            <div className="p-4 rounded-2xl bg-telegram-hover/20 border border-telegram-border/30 space-y-4">
              <h3 className="text-sm font-bold text-telegram-primary tracking-wide uppercase text-[10px]">{t('common.preferences')}</h3>
              <div className="flex items-center justify-between py-2">
                <div>
                  <p className="text-xs font-medium">{t('common.language')}</p>
                  <p className="text-[10px] text-telegram-subtext">{t('settings.select_app_language')}</p>
                </div>
                <div className="relative">
                  <select
                    value={settings.language}
                    onChange={e => updateSetting('language', e.target.value as any)}
                    className="appearance-none bg-telegram-bg border border-telegram-border rounded-lg pl-2.5 pr-7 py-1.5 text-xs text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                  >
                    {LANGUAGES.map(lang => (
                      <option key={lang.code} value={lang.code}>
                        {lang.nativeLabel}
                      </option>
                    ))}
                  </select>
                  <ChevronDown className="w-3.5 h-3.5 text-telegram-subtext absolute right-2 top-1/2 -translate-y-1/2 pointer-events-none" />
                </div>
              </div>
            </div>

            {/* Connection Diagnostics */}
            <div className="p-4 rounded-2xl bg-telegram-hover/20 border border-telegram-border/30 space-y-4">
              <h3 className="text-sm font-bold text-telegram-primary tracking-wide uppercase text-[10px] flex items-center gap-1.5">
                <Wifi className="w-3 h-3" />
                {t('settings.connection_diagnostics')}
              </h3>

              {/* Connection status indicator */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div className="flex items-center gap-2">
                  <Activity className="w-3.5 h-3.5 text-telegram-subtext" />
                  <p className="text-xs font-medium">{t('common.status')}</p>
                </div>
                <div className="flex items-center gap-1.5">
                  <span className={`w-2 h-2 rounded-full ${isConnected ? 'bg-green-500 animate-pulse' : 'bg-red-500'}`} />
                  <span className={`text-xs font-semibold ${isConnected ? 'text-green-400' : 'text-red-400'}`}>
                    {isConnected ? t('common.connected_telegram') : t('settings.offline')}
                  </span>
                </div>
              </div>

              {/* Ping test */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div>
                  <p className="text-xs font-medium">{t('common.ping')}</p>
                  <p className="text-[10px] text-telegram-subtext">
                    {latencyMs !== null
                      ? latencyMs >= 0
                        ? `${latencyMs}ms`
                        : t('settings.offline')
                      : t('settings.not_tested')}
                  </p>
                </div>
                <button
                  onClick={handleCheckLatency}
                  disabled={checkingLatency}
                  className="flex items-center gap-1.5 px-3 py-1.5 rounded-xl text-xs font-semibold bg-telegram-primary/15 text-telegram-primary hover:bg-telegram-primary/25 border border-telegram-primary/20 active:scale-95 transition-all duration-200 disabled:opacity-50"
                >
                  {checkingLatency ? (
                    <>
                      <div className="w-3 h-3 border-2 border-telegram-primary/30 border-t-telegram-primary rounded-full animate-spin" />
                      {t('settings.testing')}
                    </>
                  ) : (
                    <>
                      <Zap className="w-3 h-3" />
                      {t('settings.check_ping')}
                    </>
                  )}
                </button>
              </div>

              {/* Latency quality bar */}
              {latencyMs !== null && latencyMs >= 0 && (
                <div className="flex items-center gap-2 py-1">
                  <div className="flex-1 h-1.5 rounded-full bg-telegram-border/30 overflow-hidden">
                    <div
                      className={`h-full rounded-full transition-all duration-500 ${latencyMs < 100 ? 'bg-green-500' : latencyMs < 250 ? 'bg-yellow-500' : 'bg-red-500'}`}
                      style={{ width: `${Math.min(100, Math.max(5, (500 - latencyMs) / 5))}%` }}
                    />
                  </div>
                  <span className={`text-[10px] font-semibold ${latencyMs < 100 ? 'text-green-400' : latencyMs < 250 ? 'text-yellow-400' : 'text-red-400'}`}>
                    {latencyMs < 100 ? t('settings.excellent') : latencyMs < 250 ? t('settings.good') : t('settings.slow')}
                  </span>
                </div>
              )}

              {/* Bandwidth stats */}
              {bandwidth && (
                <div className="flex items-center justify-between py-2">
                  <div>
                    <p className="text-xs font-medium">{t('common.usage')}</p>
                    <p className="text-[10px] text-telegram-subtext">{t('settings.up_down_since_connected')}</p>
                  </div>
                  <div className="text-right">
                    <p className="text-[11px] font-mono font-semibold text-telegram-text">
                      <span className="text-emerald-400">↑ {formatBytes(bandwidth.up_bytes)}</span>
                      {' · '}
                      <span className="text-blue-400">↓ {formatBytes(bandwidth.down_bytes)}</span>
                    </p>
                  </div>
                </div>
              )}
            </div>

            {/* Proxy Configuration */}
            <div className="p-4 rounded-2xl bg-telegram-hover/20 border border-telegram-border/30 space-y-4">
              <h3 className="text-sm font-bold text-telegram-primary tracking-wide uppercase text-[10px] flex items-center gap-1.5">
                <Shield className="w-3 h-3" />
                {t('common.proxy')}
              </h3>

              {/* Enable Proxy Toggle */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div>
                  <p className="text-xs font-medium">{t('common.enable_proxy')}</p>
                  <p className="text-[10px] text-telegram-subtext">{t('settings.enable_proxy_desc')}</p>
                </div>
                <button
                  onClick={() => updateSetting('proxyEnabled', !settings.proxyEnabled)}
                  className={`relative w-11 h-6 rounded-full transition-colors duration-200 flex-shrink-0 ${settings.proxyEnabled ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                >
                  <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.proxyEnabled ? 'translate-x-5' : 'translate-x-0'}`} />
                </button>
              </div>

              {/* Proxy Type */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div>
                  <p className="text-xs font-medium">{t('common.proxy_type')}</p>
                  <p className="text-[10px] text-telegram-subtext">{t('settings.socks5_desc_mobile')}</p>
                </div>
                <div className="relative">
                  <select
                    value={settings.proxyType}
                    onChange={e => updateSetting('proxyType', e.target.value as 'socks5')}
                    className="appearance-none bg-telegram-bg border border-telegram-border rounded-lg pl-2.5 pr-7 py-1.5 text-xs text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                  >
                    <option value="socks5">SOCKS5</option>
                  </select>
                  <ChevronDown className="w-3.5 h-3.5 text-telegram-subtext absolute right-2 top-1/2 -translate-y-1/2 pointer-events-none" />
                </div>
              </div>

              {/* Host */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div>
                  <p className="text-xs font-medium">{t('common.host')}</p>
                  <p className="text-[10px] text-telegram-subtext">{t('settings.host_desc')}</p>
                </div>
                <input
                  type="text"
                  placeholder="127.0.0.1"
                  value={settings.proxyHost}
                  onChange={e => updateSetting('proxyHost', e.target.value)}
                  className="w-32 bg-telegram-bg border border-telegram-border rounded-lg px-2 py-1.5 text-xs text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                />
              </div>

              {/* Port */}
              <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                <div>
                  <p className="text-xs font-medium">{t('common.port')}</p>
                  <p className="text-[10px] text-telegram-subtext">{t('settings.port_desc')}</p>
                </div>
                <input
                  type="number"
                  min="1"
                  max="65535"
                  value={settings.proxyPort}
                  onChange={e => updateSetting('proxyPort', Math.max(1, Math.min(65535, parseInt(e.target.value) || 1080)))}
                  className="w-20 bg-telegram-bg border border-telegram-border rounded-lg px-2 py-1.5 text-xs text-telegram-text text-center focus:outline-none focus:border-telegram-primary/50 transition"
                />
              </div>

              {/* SOCKS5 auth fields */}
              {settings.proxyType === 'socks5' && (
                <>
                  <div className="flex items-center justify-between py-2 border-b border-telegram-border/20">
                    <div>
                      <p className="text-xs font-medium">{t('common.username')}</p>
                      <p className="text-[10px] text-telegram-subtext">{t('settings.optional')}</p>
                    </div>
                    <input
                      type="text"
                      placeholder={t('settings.optional')}
                      value={settings.proxyUsername}
                      onChange={e => updateSetting('proxyUsername', e.target.value)}
                      className="w-32 bg-telegram-bg border border-telegram-border rounded-lg px-2 py-1.5 text-xs text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                    />
                  </div>
                  <div className="flex items-center justify-between py-2">
                    <div>
                      <p className="text-xs font-medium">{t('common.password')}</p>
                      <p className="text-[10px] text-telegram-subtext">{t('settings.optional')}</p>
                    </div>
                    <input
                      type="password"
                      placeholder={t('settings.optional')}
                      value={settings.proxyPassword}
                      onChange={e => updateSetting('proxyPassword', e.target.value)}
                      className="w-32 bg-telegram-bg border border-telegram-border rounded-lg px-2 py-1.5 text-xs text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                    />
                  </div>
                </>
              )}

              {/* Info note */}
              <div className="p-2.5 rounded-lg bg-yellow-500/5 border border-yellow-500/10">
                <p className="text-[10px] text-yellow-400/70 leading-relaxed">
                  {t('settings.proxy_reconnect_note')}
                </p>
              </div>
            </div>

            <div className="p-4 rounded-2xl bg-telegram-hover/20 border border-telegram-border/30 space-y-4">
              <h3 className="text-sm font-bold text-telegram-primary tracking-wide uppercase text-[10px]">{t('common.about')}</h3>
              <div className="flex flex-col items-center py-3 space-y-4">
                <TeraRelayBrand size="lg" className="flex-col gap-2 text-center" />
                <p className="text-[11px] text-telegram-subtext">v{appVersion}</p>

                <div className="w-10 h-px bg-telegram-border" />

                <div className="text-center space-y-2.5">
                  <p className="text-xs font-semibold text-telegram-text">Yashwanth</p>
                  <button
                    onClick={(e) => { e.preventDefault(); openUrl('https://github.com/Yashwanth034'); }}
                    className="flex items-center justify-center gap-1.5 text-[11px] text-telegram-primary hover:text-telegram-primary/80 transition-colors cursor-pointer"
                  >
                    <svg className="w-3 h-3" viewBox="0 0 24 24" fill="currentColor">
                      <path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.756-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z"/>
                    </svg>
                    github.com/Yashwanth034
                  </button>
                </div>

                <p className="text-[10px] text-telegram-subtext/60 leading-relaxed text-center px-2">
                  {t('settings.tagline')}
                </p>
              </div>
            </div>

            <button onClick={handleLogout} className="w-full flex items-center justify-center gap-2 py-3 rounded-2xl bg-red-500/10 hover:bg-red-500/20 text-red-400 border border-red-500/20 font-semibold text-xs active:scale-98 transition-all duration-200">
              <LogOut className="w-4 h-4" />
              {t('common.logout')}
            </button>
          </div>
        )}
      </main>

      {/* Slide-out Sidebar Drawer Overlay */}
      {isSidebarOpen && (
        <div
          className="fixed inset-0 bg-black/60 z-[100] backdrop-blur-sm transition-opacity duration-300"
          onClick={() => setIsSidebarOpen(false)}
        />
      )}

      {/* Slide-out Sidebar Drawer Panel */}
      <div
        className={`fixed top-0 left-0 bottom-0 w-[280px] bg-telegram-surface border-r border-telegram-border/60 z-[110] shadow-2xl flex flex-col pt-[calc(1rem+env(safe-area-inset-top,24px))] pb-28 transition-transform duration-300 ease-out transform ${isSidebarOpen ? 'translate-x-0' : '-translate-x-full'
          }`}
        onClick={e => e.stopPropagation()}
      >
        <div className="p-4 flex items-center justify-between border-b border-telegram-border/30">
          <TeraRelayBrand size="sm" />
          <button
            onClick={() => setIsSidebarOpen(false)}
            className="p-1 rounded-lg bg-telegram-hover/30 hover:bg-telegram-hover/60 text-telegram-subtext text-xs"
          >
            ✕
          </button>
        </div>

        {/* Scrollable Folder List */}
        <nav className="flex-1 px-3 py-4 space-y-1.5 overflow-y-auto min-h-0">
          <button
            onClick={() => {
              setActiveFolderId(null);
              setIsSidebarOpen(false);
            }}
            className={`w-full flex items-center justify-between px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all duration-200 ${activeFolderId === null
                ? 'bg-telegram-primary/15 text-telegram-primary border border-telegram-primary/15'
                : 'text-telegram-subtext hover:bg-telegram-hover/40 hover:text-telegram-text border border-transparent'
              }`}
          >
            <span>Personal Vault</span>
          </button>

          {folders.map(folder => {
            const isPublic = folder.is_public || !!folder.username;
            return (
            <div key={folder.id} className="flex items-center gap-1">
              <button
                onClick={() => {
                  setActiveFolderId(folder.id);
                  setIsSidebarOpen(false);
                }}
                className={`flex-1 text-left px-3.5 py-2.5 rounded-xl text-xs font-semibold transition-all duration-200 ${
                  activeFolderId === folder.id
                    ? 'bg-telegram-primary/15 text-telegram-primary border border-telegram-primary/15'
                    : 'text-telegram-subtext hover:bg-telegram-hover/40 hover:text-telegram-text border border-transparent'
                }`}
              >
                <span className="flex items-center gap-1.5 max-w-[150px]">
                  <span className="truncate">{folder.name}</span>
                  {isPublic ? (
                    <Globe className="w-3 h-3 text-emerald-400 flex-shrink-0" />
                  ) : (
                    <Lock className="w-3 h-3 text-amber-400/60 flex-shrink-0" />
                  )}
                </span>
              </button>
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  setFolderActionMenu(folder);
                }}
                className="flex-shrink-0 p-2 rounded-xl hover:bg-telegram-hover/40 active:bg-telegram-hover/60 text-telegram-subtext/60 hover:text-telegram-subtext transition-all duration-200"
                aria-label="Folder actions"
              >
                <MoreVertical className="w-3.5 h-3.5" />
              </button>
            </div>
            );
          })}
        </nav>

        {/* Action Panel & Connection Status */}
        <div className="px-4 py-3 border-t border-telegram-border/30 space-y-3">
          <button
            onClick={async () => {
              const name = prompt("Enter folder name:");
              if (name && name.trim()) {
                await handleCreateFolder(name.trim());
              }
            }}
            className="w-full flex items-center justify-center gap-2 py-2.5 rounded-xl text-xs font-bold text-telegram-subtext hover:text-telegram-text border border-dashed border-telegram-border/60 hover:bg-telegram-hover/20 transition-all duration-200"
          >
            + Create Channel
          </button>
          <div className="flex items-center gap-2 text-telegram-subtext text-[10px] font-semibold uppercase tracking-wider">
            <span className={`w-1.5 h-1.5 rounded-full ${isConnected ? 'bg-green-500 animate-pulse' : 'bg-red-500'}`} />
            <span>{isConnected ? 'Connected' : 'Offline'}</span>
          </div>
        </div>
      </div>

      {/* Folder action popover (replaces swipe-to-reveal) */}
      {folderActionMenu && (
        <ActionPopover
          title={folderActionMenu.name}
          actions={buildFolderActions(folderActionMenu)}
          onClose={() => setFolderActionMenu(null)}
        />
      )}

      {/* Rename folder bottom sheet */}
      {renameFolder && (
        <RenameFolderSheet
          folderId={renameFolder.id}
          currentName={renameFolder.name}
          onRename={handleFolderRename}
          onClose={() => setRenameFolder(null)}
        />
      )}

      {/* Floating Bottom Nav Bar */}
      <BottomNavBar activeTab={activeTab} setActiveTab={setActiveTab} isAndroid={isAndroid} />

      {/* Image preview */}
      {previewFile && (
        <PreviewModal
          file={previewFile}
          activeFolderId={activeFolderId}
          onClose={() => setPreviewFile(null)}
        />
      )}

    </div>
  );
}
