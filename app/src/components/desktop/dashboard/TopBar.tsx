import {
    Globe,
    HardDrive,
    LayoutGrid,
    List,
    Moon,
    Search,
    Settings,
    Share2,
    Sun,
    X,
} from 'lucide-react';
import { useTheme } from '../../../context/ThemeContext';
import { useTranslation } from 'react-i18next';
import { useSettings } from '../../../context/SettingsContext';
import { invoke } from '@tauri-apps/api/core';
import { useEffect, useState } from 'react';
import {
    PremiumBadge,
    PremiumButton,
    PremiumIconButton,
} from '../../ui/PremiumPrimitives';

interface TopBarProps {
    currentFolderName: string;
    selectedIds: number[];
    onShowMoveModal: () => void;
    onBulkDownload: () => void;
    onBulkDelete: () => void;
    onBulkShare: () => void;
    onDownloadFolder: () => void;
    onClearSelection: () => void;
    viewMode: 'grid' | 'list';
    setViewMode: (mode: 'grid' | 'list') => void;
    searchTerm: string;
    onSearchChange: (term: string) => void;
    onSettingsClick: () => void;
    onRemoteUploadClick: () => void;
    readOnly?: boolean;
    channelMode?: boolean;
    channelFileCount?: number;
    channelCategoryLabel?: string;
    onChannelInfo?: () => void;
}

export function TopBar({
    currentFolderName,
    selectedIds,
    onShowMoveModal,
    onBulkDownload,
    onBulkDelete,
    onBulkShare,
    onDownloadFolder,
    onClearSelection,
    viewMode,
    setViewMode,
    searchTerm,
    onSearchChange,
    onSettingsClick,
    onRemoteUploadClick,
    readOnly = false,
    channelMode = false,
    channelFileCount,
    channelCategoryLabel,
    onChannelInfo,
}: TopBarProps) {
    const { theme, toggleTheme } = useTheme();
    const { t } = useTranslation();
    const { settings } = useSettings();
    const [proxyStatus, setProxyStatus] = useState<{ reachable: boolean; latency_ms: number } | null>(null);

    useEffect(() => {
        if (!settings.proxyEnabled || !settings.proxyLiveStateEnabled) {
            setProxyStatus(null);
            return;
        }

        const checkProxy = async () => {
            try {
                const status = await invoke<{ reachable: boolean; latency_ms: number }>('cmd_get_proxy_status');
                setProxyStatus(status);
            } catch {
                setProxyStatus({ reachable: false, latency_ms: -1 });
            }
        };

        checkProxy();
        const interval = setInterval(checkProxy, 5000);
        return () => clearInterval(interval);
    }, [settings.proxyEnabled, settings.proxyLiveStateEnabled]);

    const proxyTone = !proxyStatus
        ? 'warning'
        : proxyStatus.reachable
            ? 'success'
            : 'danger';

    const proxyLabel = !proxyStatus
        ? 'Checking…'
        : proxyStatus.reachable
            ? `${proxyStatus.latency_ms}ms`
            : 'Offline';

    return (
        <header
            className={'tr-topbar ' + (channelMode ? 'h-14' : 'h-16') + ' border-b flex items-center gap-4 px-4 sticky top-0 z-10'}
            onClick={e => e.stopPropagation()}
        >
            <button
                type="button"
                className={'tr-topbar__identity min-w-0 ' + (channelMode ? 'tr-topbar__identity--interactive' : '')}
                onClick={channelMode ? onChannelInfo : undefined}
                disabled={!channelMode || !onChannelInfo}
                title={channelMode ? 'Channel info' : currentFolderName}
            >
                <div className="min-w-0 text-left">
                    <div className="tr-topbar__title" title={currentFolderName}>
                        {currentFolderName}
                    </div>
                    <div className="tr-topbar__eyebrow mt-0.5">
                        {channelMode
                            ? channelCategoryLabel
                                ? `${channelCategoryLabel} · ${channelFileCount ?? 0}`
                                : `${channelFileCount ?? 0} item${channelFileCount === 1 ? '' : 's'}`
                            : 'Workspace'}
                    </div>
                </div>
            </button>

            {selectedIds.length > 0 ? (
                <div className="tr-selection-strip">
                    <PremiumBadge tone="primary">
                        {t('files.items_selected', { count: selectedIds.length })}
                    </PremiumBadge>
                    <PremiumIconButton
                        label={t('files.clear_selection')}
                        onClick={onClearSelection}
                    >
                        <X />
                    </PremiumIconButton>
                    {!readOnly && (
                        <PremiumButton variant="secondary" size="sm" onClick={onShowMoveModal}>
                            {t('files.move_to')}
                        </PremiumButton>
                    )}
                    <PremiumButton variant="secondary" size="sm" onClick={onBulkDownload}>
                        {t('files.download_selected')}
                    </PremiumButton>
                    {!readOnly && (
                        <>
                            <PremiumButton
                                variant="secondary"
                                size="sm"
                                icon={<Share2 />}
                                onClick={onBulkShare}
                            >
                                {t('files.share')}
                            </PremiumButton>
                            <PremiumButton variant="danger" size="sm" onClick={onBulkDelete}>
                                {t('files.delete')}
                            </PremiumButton>
                        </>
                    )}
                </div>
            ) : (
                <div className="tr-topbar__search">
                    <Search />
                    <input
                        type="text"
                        placeholder={t('common.search_placeholder')}
                        value={searchTerm}
                        onChange={(e) => onSearchChange(e.target.value)}
                        aria-label={t('common.search_placeholder')}
                    />
                    {searchTerm && (
                        <button
                            type="button"
                            className="tr-topbar__search-clear"
                            onClick={() => onSearchChange('')}
                            aria-label={t('files.clear_selection')}
                            title={t('files.clear_selection')}
                        >
                            <X className="w-3.5 h-3.5" />
                        </button>
                    )}
                </div>
            )}

            <div className="flex-1 flex items-center justify-end gap-2 min-w-0">
                {settings.proxyEnabled && settings.proxyLiveStateEnabled && (
                    <PremiumBadge
                        tone={proxyTone}
                        dot
                        title={
                            !proxyStatus
                                ? 'Proxy status: checking…'
                                : proxyStatus.reachable
                                    ? `Proxy active: ${proxyStatus.latency_ms}ms latency`
                                    : 'Proxy status: unreachable'
                        }
                    >
                        {t('common.proxy')}: {proxyLabel}
                    </PremiumBadge>
                )}

                <div className="tr-topbar__actions">
                    <PremiumIconButton
                        label={t('files.download_folder')}
                        onClick={onDownloadFolder}
                    >
                        <HardDrive />
                    </PremiumIconButton>

                    {!readOnly && (
                        <PremiumIconButton
                            label={t('files.remote_upload')}
                            onClick={onRemoteUploadClick}
                        >
                            <Globe />
                        </PremiumIconButton>
                    )}

                    {!channelMode && (
                        <PremiumIconButton
                            label={t('files.toggle_layout')}
                            active
                            onClick={() => setViewMode(viewMode === 'grid' ? 'list' : 'grid')}
                        >
                            {viewMode === 'grid' ? <List /> : <LayoutGrid />}
                        </PremiumIconButton>
                    )}

                    <PremiumIconButton
                        label={t('common.settings')}
                        onClick={onSettingsClick}
                    >
                        <Settings />
                    </PremiumIconButton>

                    <PremiumIconButton
                        label={theme === 'dark' ? t('common.switch_light') : t('common.switch_dark')}
                        onClick={toggleTheme}
                    >
                        {theme === 'dark' ? <Sun /> : <Moon />}
                    </PremiumIconButton>
                </div>
            </div>
        </header>
    );
}
