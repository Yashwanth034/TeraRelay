import {
    Globe,
    Copy,
    ClipboardList,
    HardDrive,
    LayoutGrid,
    List,
    Moon,
    Search,
    RefreshCw,
    Settings,
    Share2,
    Sun,
    X,
} from 'lucide-react';
import { useTheme } from '../../../context/ThemeContext';
import { useTranslation } from 'react-i18next';
import { useSettings } from '../../../context/SettingsContext';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-shell';
import { writeText } from '@tauri-apps/plugin-clipboard-manager';
import { toast } from 'sonner';
import { useEffect, useRef, useState } from 'react';
import { validateNativeDriveMountStatus, type NativeDriveMountStatus } from '../../../driveMountStatus';
import { validateDriveSyncResult, validateDriveSnapshot } from '../../../deviceContinuity';
import { publishVerifiedDriveSync } from '../../../driveSyncSignal';

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
    onSelectAll?: () => void;
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
    onSelectAll,
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
    const [driveStatus, setDriveStatus] = useState<NativeDriveMountStatus | null>(null);
    const [driveStatusError, setDriveStatusError] = useState(false);
    const [driveStatusRetryBusy, setDriveStatusRetryBusy] = useState(false);
    const driveStatusRetryInFlightRef = useRef(false);
    const [driveMountBusy, setDriveMountBusy] = useState(false);
    const [driveSyncBusy, setDriveSyncBusy] = useState(false);
    const [driveOpenBusy, setDriveOpenBusy] = useState(false);
    const [driveCopyBusy, setDriveCopyBusy] = useState(false);
    const [driveDiagnosticsBusy, setDriveDiagnosticsBusy] = useState(false);
    const driveDiagnosticsInFlightRef = useRef(false);
    const driveCopyInFlightRef = useRef(false);
    const driveOpenInFlightRef = useRef(false);
    const drivePathActionInFlightRef = useRef(false);
    const driveSyncInFlightRef = useRef(false);
    const driveMountInFlightRef = useRef(false);
    const driveMountLifecycleRef = useRef(0);
    const driveStatusEpochRef = useRef(0);

    useEffect(() => {
        ++driveMountLifecycleRef.current;
        let disposed = false;
        let inFlight = false;
        const refreshDrive = async () => {
            if (disposed || inFlight || driveMountInFlightRef.current || driveStatusRetryInFlightRef.current || drivePathActionInFlightRef.current || driveSyncInFlightRef.current) return;
            inFlight = true;
            const epoch = ++driveStatusEpochRef.current;
            try {
                const status = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
                if (!disposed && epoch === driveStatusEpochRef.current && !driveMountInFlightRef.current) {
                    setDriveStatus(status);
                    setDriveStatusError(false);
                }
            } catch {
                if (!disposed && epoch === driveStatusEpochRef.current && !driveMountInFlightRef.current) {
                    setDriveStatusError(true);
                }
            } finally {
                inFlight = false;
            }
        };
        void refreshDrive();
        const timer = window.setInterval(() => { void refreshDrive(); }, 15_000);
        window.addEventListener('focus', refreshDrive);
        return () => {
            disposed = true;
            ++driveMountLifecycleRef.current;
            ++driveStatusEpochRef.current;
            window.clearInterval(timer);
            window.removeEventListener('focus', refreshDrive);
        };
    }, []);

    const mountedDrivePath = driveStatus?.mounted ? driveStatus.mount_point : null;
    // macOS File Provider is surfaced in Finder without a traditional mount
    // path. Native cmd_drive_mount intentionally reports mount_point=null.
    const pathlessMacosDrive = Boolean(driveStatus?.supported && driveStatus.mounted
        && driveStatus.provider_kind === 'macos-file-provider' && driveStatus.mount_point === null);
    const canSyncDrive = Boolean(!driveStatusError && (mountedDrivePath || pathlessMacosDrive));
    // Retry only by explicit user request; never race a native recovery worker.
    const canRetryDrive = Boolean(driveStatus?.supported && !driveStatus?.mounted
        && !driveStatus?.native_recovery && !driveStatusError);
    const canRecheckDrive = driveStatusError || Boolean(driveStatus?.supported
        && driveStatus?.native_recovery && !driveStatus?.mounted);
    const driveTone = driveStatusError ? 'danger' : driveStatus?.mounted ? 'success' : 'warning';
    const driveLabel = driveStatusError
        ? 'Status unavailable'
        : !driveStatus ? 'Checking…'
            : !driveStatus.supported ? 'Unsupported'
                : driveStatus.mounted ? 'Mounted'
                    : driveStatus.native_recovery ? 'Recovering…' : 'Not mounted';

    const recheckNativeDriveStatus = async () => {
        if (!canRecheckDrive || driveStatusRetryInFlightRef.current || drivePathActionInFlightRef.current) return;
        driveStatusRetryInFlightRef.current = true;
        const epoch = ++driveStatusEpochRef.current;
        const lifecycle = driveMountLifecycleRef.current;
        setDriveStatusRetryBusy(true);
        try {
            const status = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) return;
            setDriveStatus(status);
            setDriveStatusError(false);
        } catch (error) {
            if (lifecycle === driveMountLifecycleRef.current && epoch === driveStatusEpochRef.current) {
                setDriveStatusError(true);
                toast.error('TeraRelay Drive status is still unavailable: ' + String(error));
            }
        } finally {
            if (lifecycle === driveMountLifecycleRef.current) {
                driveStatusRetryInFlightRef.current = false;
                setDriveStatusRetryBusy(false);
            }
        }
    };

    const retryMountedDrive = async () => {
        if (!canRetryDrive || driveMountInFlightRef.current || drivePathActionInFlightRef.current) return;
        driveMountInFlightRef.current = true;
        // A previous periodic status response must not undo a newer manual
        // mount result or present a stale polling error.
        ++driveStatusEpochRef.current;
        const lifecycle = driveMountLifecycleRef.current;
        setDriveMountBusy(true);
        let verifiedBeforeMount = false;
        try {
            // The badge may be up to 15 seconds old. Native recovery could
            // have started, or another window may have mounted Drive already.
            const liveStatus = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current) return;
            verifiedBeforeMount = true;
            setDriveStatus(liveStatus);
            setDriveStatusError(false);
            if (liveStatus.mounted) {
                toast.success(liveStatus.mount_point
                    ? 'TeraRelay Drive is already mounted and ready to open.'
                    : 'TeraRelay Drive File Provider is connected; browse it in Finder.');
                return;
            }
            if (!liveStatus.supported) {
                toast.warning('Native Drive mounting is not supported on this device.');
                return;
            }
            if (liveStatus.native_recovery) {
                toast.warning('TeraRelay Drive recovery is already in progress.');
                return;
            }
            await invoke('cmd_drive_mount');
            // The native mount can outlive this dashboard. Never let its late
            // response trigger a status probe or toast for a closed dashboard.
            if (lifecycle !== driveMountLifecycleRef.current) return;
            const status = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current) return;
            setDriveStatus(status);
            setDriveStatusError(false);
            if (status.mounted) {
                toast.success(status.mount_point
                    ? 'TeraRelay Drive is mounted and ready to open.'
                    : 'TeraRelay Drive File Provider is connected; browse it in Finder.');
            } else {
                toast.warning('Drive mount requested. TeraRelay will continue checking its status.');
            }
        } catch (error) {
            if (lifecycle === driveMountLifecycleRef.current) {
                if (!verifiedBeforeMount) setDriveStatusError(true);
                toast.error((verifiedBeforeMount
                    ? 'Could not mount TeraRelay Drive: '
                    : 'Could not check native Drive status: ') + String(error));
            }
        } finally {
            if (lifecycle === driveMountLifecycleRef.current) {
                driveMountInFlightRef.current = false;
                setDriveMountBusy(false);
            }
        }
    };

    const syncMountedDrive = async () => {
        if (!canSyncDrive || driveSyncInFlightRef.current || drivePathActionInFlightRef.current) return;
        driveSyncInFlightRef.current = true;
        const lifecycle = driveMountLifecycleRef.current;
        const epoch = ++driveStatusEpochRef.current;
        let verifiedNativeStatus = false;
        setDriveSyncBusy(true);
        try {
            // The cached badge can be stale after native unmount/remount.
            // Never initiate a user-requested Telegram metadata sync based on
            // an outdated mounted Drive path.
            const liveStatus = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) return;
            verifiedNativeStatus = true;
            setDriveStatus(liveStatus);
            setDriveStatusError(false);
            const sameProvider = mountedDrivePath
                ? liveStatus.mount_point === mountedDrivePath
                : pathlessMacosDrive && liveStatus.provider_kind === 'macos-file-provider'
                    && liveStatus.mount_point === null;
            if (!liveStatus.supported || !liveStatus.mounted || !sameProvider) {
                toast.warning('Drive mount changed or is unavailable. Check its updated status before syncing.');
                return;
            }
            const rawResult = await invoke<unknown>('cmd_drive_sync_metadata');
            if (lifecycle !== driveMountLifecycleRef.current) return;
            // A resolved native command is not proof of a successful cloud sync.
            // Reuse the strict startup validator before presenting any success.
            const result = validateDriveSyncResult(rawResult);
            // Successful explicit Sync must dismiss an earlier upload warning
            // even when the upload queue is empty. Never broadcast QA/dirty.
            publishVerifiedDriveSync(result);
            if (result.dirty) {
                toast.warning('Drive still has local metadata changes waiting to sync.');
            } else {
                switch (result.source) {
                    case 'current':
                        toast.success('TeraRelay Drive is already up to date.');
                        break;
                    case 'remote':
                        toast.success('TeraRelay Drive metadata updated from Telegram.');
                        break;
                    case 'remote-merged':
                        toast.success('TeraRelay Drive metadata merged with updates from Telegram.');
                        break;
                    case 'merged-published':
                        toast.success('Local Drive metadata changes synced to Telegram.');
                        break;
                    case 'qa-local':
                        toast.warning('Drive metadata checked locally (QA mode); no Telegram sync was performed.');
                        break;
                    default:
                        toast.warning('Drive sync completed, but its outcome was not recognized.');
                }
            }
        } catch (error) {
            if (lifecycle === driveMountLifecycleRef.current) {
                if (!verifiedNativeStatus) setDriveStatusError(true);
                toast.error((verifiedNativeStatus
                    ? 'Could not refresh TeraRelay Drive metadata: '
                    : 'Could not verify TeraRelay Drive mount before syncing: ') + String(error));
            }
        } finally {
            if (lifecycle === driveMountLifecycleRef.current) {
                driveSyncInFlightRef.current = false;
                setDriveSyncBusy(false);
            }
        }
    };

    const copyDriveDiagnostics = async () => {
        if (driveDiagnosticsInFlightRef.current || driveMountInFlightRef.current
            || drivePathActionInFlightRef.current || driveSyncInFlightRef.current
            || driveStatusRetryInFlightRef.current) return;
        driveDiagnosticsInFlightRef.current = true;
        // Serialize explicit diagnostics with polling, open/copy, sync, mount,
        // and status rechecks, so no operation invalidates this user request.
        drivePathActionInFlightRef.current = true;
        const lifecycle = driveMountLifecycleRef.current;
        const epoch = ++driveStatusEpochRef.current;
        setDriveDiagnosticsBusy(true);
        let verified = false;
        let report = 'TeraRelay Drive diagnostics\nstatus: unavailable';
        try {
            // Explicit copy only. Never include the mount path, usernames,
            // account data, passphrases or backend exception strings.
            const status = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) {
                drivePathActionInFlightRef.current = false;
                driveDiagnosticsInFlightRef.current = false;
                if (lifecycle === driveMountLifecycleRef.current) setDriveDiagnosticsBusy(false);
                return;
            }
            setDriveStatus(status);
            setDriveStatusError(false);
            verified = true;
            const allowedProviderKinds = ['fuse', 'windows-cloud-files', 'macos-file-provider', 'unsupported'];
            const provider = status.provider_kind && allowedProviderKinds.includes(status.provider_kind)
                ? status.provider_kind : 'unrecognized';
            report = ['TeraRelay Drive diagnostics', 'status: verified',
                `provider: ${provider}`, `supported: ${status.supported}`,
                `mounted: ${status.mounted}`, `native_recovery: ${status.native_recovery}`,
            ].join('\n');
        } catch {
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) {
                drivePathActionInFlightRef.current = false;
                driveDiagnosticsInFlightRef.current = false;
                if (lifecycle === driveMountLifecycleRef.current) setDriveDiagnosticsBusy(false);
                return;
            }
            setDriveStatusError(true);
        }
        if (verified) {
            try {
                // Snapshot contents can contain private file metadata. Only
                // validated aggregate counts/revision/dirty state are copied.
                const snapshot = validateDriveSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
                if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) {
                    drivePathActionInFlightRef.current = false;
                    driveDiagnosticsInFlightRef.current = false;
                    if (lifecycle === driveMountLifecycleRef.current) setDriveDiagnosticsBusy(false);
                    return;
                }
                report += `\nlocal_revision: ${snapshot.revision}`
                    + `\nlocal_metadata_dirty: ${snapshot.dirty}`
                    + `\npending_writes: ${snapshot.pending.length}`;
            } catch {
                if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) {
                    drivePathActionInFlightRef.current = false;
                    driveDiagnosticsInFlightRef.current = false;
                    if (lifecycle === driveMountLifecycleRef.current) setDriveDiagnosticsBusy(false);
                    return;
                }
                report += '\nlocal_metadata: unavailable';
            }
        }
        try {
            await writeText(report);
            if (lifecycle === driveMountLifecycleRef.current && epoch === driveStatusEpochRef.current) {
                if (verified) toast.success('Drive diagnostics copied without personal folder paths.');
                else toast.warning('Drive status was unavailable; copied a safe diagnostic report. Try Recheck.');
            }
        } catch {
            if (lifecycle === driveMountLifecycleRef.current && epoch === driveStatusEpochRef.current) {
                toast.error('Could not copy Drive diagnostics. Check clipboard access and retry.');
            }
        } finally {
            drivePathActionInFlightRef.current = false;
            driveDiagnosticsInFlightRef.current = false;
            if (lifecycle === driveMountLifecycleRef.current) setDriveDiagnosticsBusy(false);
        }
    };

    const copyMountedDrivePath = async () => {
        if (!mountedDrivePath || driveStatusError || drivePathActionInFlightRef.current || driveSyncInFlightRef.current) return;
        drivePathActionInFlightRef.current = true;
        driveCopyInFlightRef.current = true;
        const lifecycle = driveMountLifecycleRef.current;
        const epoch = ++driveStatusEpochRef.current;
        setDriveCopyBusy(true);
        let verifiedLiveStatus = false;
        try {
            const liveStatus = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) return;
            verifiedLiveStatus = true;
            setDriveStatus(liveStatus);
            setDriveStatusError(false);
            if (!liveStatus.supported || !liveStatus.mounted || !liveStatus.mount_point
                || liveStatus.mount_point !== mountedDrivePath) {
                toast.warning('Drive mount changed or is no longer available. Check its updated status before copying.');
                return;
            }
            await writeText(liveStatus.mount_point);
            if (lifecycle === driveMountLifecycleRef.current) {
                toast.success('Mounted TeraRelay Drive path copied to clipboard.');
            }
        } catch (error) {
            if (lifecycle === driveMountLifecycleRef.current) {
                if (!verifiedLiveStatus) setDriveStatusError(true);
                toast.error('Could not verify or copy the mounted Drive location: ' + String(error));
            }
        } finally {
            if (lifecycle === driveMountLifecycleRef.current) {
                driveCopyInFlightRef.current = false;
                drivePathActionInFlightRef.current = false;
                setDriveCopyBusy(false);
            }
        }
    };

    const openMountedDrive = async () => {
        if (!mountedDrivePath || driveStatusError || drivePathActionInFlightRef.current || driveSyncInFlightRef.current) return;
        drivePathActionInFlightRef.current = true;
        driveOpenInFlightRef.current = true;
        // Prevent an earlier polling response from overriding a more recent
        // native status check made right before opening the filesystem.
        const epoch = ++driveStatusEpochRef.current;
        const lifecycle = driveMountLifecycleRef.current;
        setDriveOpenBusy(true);
        try {
            // The cached badge can lag native unmount/recovery by 15 seconds.
            // Never ask the OS to open its old path without checking ownership.
            const liveStatus = validateNativeDriveMountStatus(await invoke<unknown>('cmd_drive_status'));
            if (lifecycle !== driveMountLifecycleRef.current || epoch !== driveStatusEpochRef.current) return;
            setDriveStatus(liveStatus);
            setDriveStatusError(false);
            if (!liveStatus.supported || !liveStatus.mounted || !liveStatus.mount_point
                || liveStatus.mount_point !== mountedDrivePath) {
                toast.warning('Drive mount changed or is no longer available. Check its updated status and try again.');
                return;
            }
            try {
                await open(liveStatus.mount_point);
            } catch (error) {
                if (lifecycle !== driveMountLifecycleRef.current) return;
                try {
                    await writeText(liveStatus.mount_point);
                    if (lifecycle === driveMountLifecycleRef.current) {
                        toast.warning('Could not open the Drive folder. Its location was copied to your clipboard.');
                    }
                } catch {
                    if (lifecycle === driveMountLifecycleRef.current) {
                        console.warn('[Drive] Could not open mounted Drive folder:', error);
                        toast.error('Could not open the Drive folder. Try opening it from your file manager.');
                    }
                }
            }
        } catch (error) {
            if (lifecycle === driveMountLifecycleRef.current) {
                setDriveStatusError(true);
                toast.error('Could not verify the mounted Drive folder: ' + String(error));
            }
        } finally {
            if (lifecycle === driveMountLifecycleRef.current) {
                driveOpenInFlightRef.current = false;
                drivePathActionInFlightRef.current = false;
                setDriveOpenBusy(false);
            }
        }
    };

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

    const identityContent = (
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
    );

    return (
        <header
            className={'tr-topbar ' + (channelMode ? 'h-14' : 'h-16') + ' border-b flex items-center gap-4 px-4 sticky top-0 z-10'}
            onClick={e => e.stopPropagation()}
        >
            {channelMode && onChannelInfo ? (
                <button
                    type="button"
                    className="tr-topbar__identity tr-topbar__identity--interactive min-w-0"
                    onClick={onChannelInfo}
                    aria-label={`Channel info for ${currentFolderName}`}
                    title="Channel info"
                >
                    {identityContent}
                </button>
            ) : (
                <div className="tr-topbar__identity min-w-0" title={currentFolderName}>
                    {identityContent}
                </div>
            )}

            {selectedIds.length > 0 ? (
                <div className="tr-selection-strip">
                    <PremiumBadge tone="primary">
                        {t('files.items_selected', { count: selectedIds.length })}
                    </PremiumBadge>
                    {onSelectAll && (
                        <PremiumButton variant="secondary" size="sm" onClick={onSelectAll}>
                            {t('files.select_all')}
                        </PremiumButton>
                    )}
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
                {mountedDrivePath && !driveStatusError ? (
                    <button
                        type="button"
                        onClick={() => { void openMountedDrive(); }}
                        disabled={driveOpenBusy || driveCopyBusy || driveSyncBusy || driveDiagnosticsBusy}
                        aria-label="Open mounted TeraRelay Drive folder"
                        title={`Open TeraRelay Drive at ${mountedDrivePath}`}
                        className="rounded-md focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                    >
                        <PremiumBadge tone={driveTone} dot title={`Drive location: ${mountedDrivePath}`}>
                            Drive: {driveLabel}
                        </PremiumBadge>
                    </button>
                ) : canRecheckDrive ? (
                    <button
                        type="button"
                        disabled={driveStatusRetryBusy || driveDiagnosticsBusy}
                        onClick={() => { void recheckNativeDriveStatus(); }}
                        aria-label="Recheck TeraRelay Drive status"
                        title="Check native Drive provider status again"
                        className="rounded-md focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                    >
                        <PremiumBadge tone={driveStatusError ? 'danger' : 'warning'} dot title="Recheck native Drive status">
                            Drive: {driveStatusRetryBusy ? 'Checking…' : driveStatusError ? 'Status unavailable · Retry' : 'Recovering… · Recheck'}
                        </PremiumBadge>
                    </button>
                ) : canRetryDrive ? (
                    <button
                        type="button"
                        disabled={driveMountBusy || driveDiagnosticsBusy}
                        onClick={() => { void retryMountedDrive(); }}
                        aria-label="Mount TeraRelay Drive"
                        title="Retry mounting TeraRelay Drive"
                        className="rounded-md focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                    >
                        <PremiumBadge tone={driveTone} dot title="Retry native Drive mount">
                            Drive: {driveMountBusy ? 'Mounting…' : 'Not mounted · Retry'}
                        </PremiumBadge>
                    </button>
                ) : (
                    <PremiumBadge tone={driveTone} dot title="Native Drive provider status">
                        Drive: {driveLabel}
                    </PremiumBadge>
                )}
                {mountedDrivePath && !driveStatusError && (
                    <button
                        type="button"
                        aria-label="Copy mounted TeraRelay Drive path"
                        title="Copy verified Drive folder location"
                        disabled={driveCopyBusy || driveOpenBusy || driveSyncBusy || driveDiagnosticsBusy}
                        onClick={() => { void copyMountedDrivePath(); }}
                        className="rounded-md p-1.5 text-telegram-subtext hover:text-telegram-text focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                    >
                        <Copy className="w-4 h-4" aria-hidden="true" />
                    </button>
                )}
                <button
                    type="button"
                    aria-label="Copy safe TeraRelay Drive diagnostics"
                    title="Copy current native Drive health without personal folder paths"
                    disabled={driveDiagnosticsBusy || driveMountBusy || driveSyncBusy || driveOpenBusy || driveCopyBusy || driveStatusRetryBusy}
                    onClick={() => { void copyDriveDiagnostics(); }}
                    className="rounded-md p-1.5 text-telegram-subtext hover:text-telegram-text focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                >
                    <ClipboardList className={`w-4 h-4 ${driveDiagnosticsBusy ? 'animate-pulse' : ''}`} aria-hidden="true" />
                </button>
                {canSyncDrive && (
                    <button
                        type="button"
                        aria-label="Sync TeraRelay Drive metadata"
                        title="Refresh cloud Drive metadata now"
                        disabled={driveSyncBusy || driveCopyBusy || driveOpenBusy || driveDiagnosticsBusy}
                        onClick={() => { void syncMountedDrive(); }}
                        className="rounded-md p-1.5 text-telegram-subtext hover:text-telegram-text focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-60"
                    >
                        <RefreshCw className={`w-4 h-4 ${driveSyncBusy ? 'animate-spin' : ''}`} aria-hidden="true" />
                    </button>
                )}
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

                    {channelMode && !readOnly && (
                        <PremiumIconButton
                            label="Upload from Link"
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
