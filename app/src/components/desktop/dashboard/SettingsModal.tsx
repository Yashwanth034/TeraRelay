import { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { X, RotateCcw, Download, Upload, Trash2, HardDrive, Globe, Key, Copy, Check, Search, RefreshCw, FolderArchive, Shield, Zap, Activity, Gauge, Wifi, ChevronDown, Link, Sparkles, Info, Clipboard, Monitor, Loader2, Languages, Play, Palette, Plus, Tag } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { type as operatingSystemType } from '@tauri-apps/plugin-os';
import { open } from '@tauri-apps/plugin-shell';
import { toast } from 'sonner';
import { useSettings } from '../../../context/SettingsContext';
import { useAppUpdate } from '../../../context/AppUpdateContext';
import { useConfirm } from '../../../context/ConfirmContext';
import { useTranslation } from 'react-i18next';
import { LANGUAGES } from '../../../i18n/languages';
import { ShareInfo, CacheEntry, DetailedCacheInfo } from '../../../types';
import { version as appVersion } from '../../../../package.json';
import { useTheme } from '../../../context/ThemeContext';
import { CustomTheme, ThemeColorPalette, generateThemeId } from '../../../theme/themeEngine';
import { getDefaultPalette } from '../../../theme/presets';
import { TeraRelayBrand } from '../../shared/TeraRelayBrand';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';
import { validateDriveCryptoStatus, validateDriveSyncResult, type DriveCryptoStatusLike } from '../../../deviceContinuity';
import { normalizeDriveFinderSnapshot, filterDriveFinderEntries, type DriveFinderEntry, type DriveFinderKind } from '../../../driveLocalFinder';
import { summarizeDriveStorage } from '../../../driveStorageInsights';
import { sortDriveFinderEntries, type DriveFinderSortOrder } from '../../../driveFinderSort';
import { filterDriveFinderBySize, type DriveFinderSizeFilter } from '../../../driveFinderSizeFilter';
import { filterDriveFinderWithinFolder, parentDriveFinderFolder } from '../../../driveFinderFolderScope';
import { formatDriveFinderCsv } from '../../../driveFinderCsv';
import { DriveBulkOrganizer } from './DriveBulkOrganizer';
import { DriveBatchRestore } from './DriveBatchRestore';
import { DriveBatchTrash } from './DriveBatchTrash';
import { DriveFolderOperations } from './DriveFolderOperations';
import { DriveFolderUndo } from './DriveFolderUndo';
import { DriveFolderClone } from './DriveFolderClone';
import { DriveFolderBlueprint } from './DriveFolderBlueprint';
import { DriveFileCopy } from './DriveFileCopy';
import { DriveFolderCompleteCopy } from './DriveFolderCompleteCopy';
import { DriveFolderMirror } from './DriveFolderMirror';
import { DriveFolderTreeTrash } from './DriveFolderTreeTrash';
import { DriveTreeReceipt } from './DriveTreeReceipt';
import { DriveBulkUndo } from './DriveBulkUndo';
import { DriveHealthDoctor } from './DriveHealthDoctor';
import { DriveTransferCenter } from './DriveTransferCenter';
import { DrivePendingRescue } from './DrivePendingRescue';
import { DriveBulkExport } from './DriveBulkExport';
import { DriveIntegrityLedger } from './DriveIntegrityLedger';
import { DriveNamespaceAudit } from './DriveNamespaceAudit';
import { DriveTimeCapsule } from './DriveTimeCapsule';
import { DrivePortableManifest } from './DrivePortableManifest';
import { DriveFolderParity } from './DriveFolderParity';
import { DriveBackupWatchlist } from './DriveBackupWatchlist';
import { DriveBackupTimeline } from './DriveBackupTimeline';
import { DriveBackupBatchRepair } from './DriveBackupBatchRepair';
import { DriveDuplicateExplorer } from './DriveDuplicateExplorer';
import { DriveCollections } from './DriveCollections';
import { DriveChangeReviewPanel } from './DriveChangeReview';
import { DriveSafeMode } from './DriveSafeMode';
import { DriveSmartViews } from './DriveSmartViews';
import { filterDriveTrashEntries, filterDriveTrashAvailability, filterDriveTrashByAge, normalizeDriveTrashEntries, sortDriveTrashEntries, formatDriveTrashBytes, formatDriveTrashInventory, summarizeSelectedDriveTrashSizes, type DriveTrashFilterKind, type DriveTrashSortOrder, type DriveTrashAvailability, type DriveTrashAgeRange } from '../../../driveTrashBrowse';

interface SettingsModalProps {
    isOpen: boolean;
    onClose: () => void;
}

interface ApiSettings {
    enabled: boolean;
    port: number;
    key_set: boolean;
    running: boolean;
}

type DriveCryptoStatus = DriveCryptoStatusLike;

interface DriveSyncOutcome {
    revision: number;
    dirty: boolean;
    source: string;
}
type DriveRestoreSyncState = 'synced' | 'pending' | 'qa-local' | 'unverified' | 'offline';

interface DrivePendingRecoveryResult {
    recovered: number;
    remaining: number;
    failures: number;
}

interface DriveTrashItem {
    kind: 'file' | 'folder' | 'symlink';
    id: string;
    name: string;
    deleted_at: number;
    original_directory_id: string | null;
    size: number | null;
    restorable: boolean;
}

type SettingsTab = 'general' | 'themes' | 'proxy' | 'vpn' | 'sharing' | 'about';

const SHOW_ADVANCED_SETTINGS = false;

export function SettingsModal({ isOpen, onClose }: SettingsModalProps) {
    useEscapeToClose(isOpen, onClose);
    const dialogRef = useRef<HTMLDivElement>(null);
    const closeButtonRef = useRef<HTMLButtonElement>(null);

    useEffect(() => {
        if (!isOpen) return;
        const returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
        const frame = requestAnimationFrame(() => closeButtonRef.current?.focus({ preventScroll: true }));
        return () => {
            cancelAnimationFrame(frame);
            if (returnFocus?.isConnected) returnFocus.focus({ preventScroll: true });
        };
    }, [isOpen]);

    const trapDialogTab = (event: React.KeyboardEvent<HTMLDivElement>) => {
        if (event.key !== 'Tab') return;
        const root = dialogRef.current;
        if (!root) return;
        const focusable = Array.from(root.querySelectorAll<HTMLElement>(
            'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
        )).filter(element => element.getClientRects().length > 0);
        if (!focusable.length) {
            event.preventDefault();
            root.focus();
            return;
        }
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const active = document.activeElement;
        if (event.shiftKey && (active === first || !root.contains(active))) {
            event.preventDefault();
            last.focus();
        } else if (!event.shiftKey && (active === last || !root.contains(active))) {
            event.preventDefault();
            first.focus();
        }
    };
    const { settings, updateSetting, resetSettings } = useSettings();
    const { status: updateStatus, release: updateRelease, error: updateError, checkForUpdates } = useAppUpdate();
    const { confirm } = useConfirm();
    const { t } = useTranslation();
    const [clearing, setClearing] = useState(false);
    const [linuxAutostartSupported, setLinuxAutostartSupported] = useState(false);
    const [linuxAutostartEnabled, setLinuxAutostartEnabled] = useState<boolean | null>(null);
    const [linuxAutostartBusy, setLinuxAutostartBusy] = useState(false);
    const [linuxAutostartError, setLinuxAutostartError] = useState<string | null>(null);

    useEffect(() => {
        if (!isOpen) return;
        try {
            if (operatingSystemType() !== 'linux') return;
        } catch {
            return;
        }
        setLinuxAutostartSupported(true);
        let mounted = true;
        void invoke<boolean>('cmd_desktop_autostart_status')
            .then(enabled => {
                if (mounted) {
                    setLinuxAutostartEnabled(enabled);
                    setLinuxAutostartError(null);
                }
            })
            .catch(error => {
                if (mounted) setLinuxAutostartError(String(error));
            });
        return () => { mounted = false; };
    }, [isOpen]);

    const toggleLinuxAutostart = async () => {
        if (linuxAutostartEnabled === null || linuxAutostartBusy) return;
        setLinuxAutostartBusy(true);
        try {
            const enabled = await invoke<boolean>('cmd_desktop_autostart_set', { enabled: !linuxAutostartEnabled });
            setLinuxAutostartEnabled(enabled);
            setLinuxAutostartError(null);
            toast.success(enabled ? 'TeraRelay Drive will start after Linux sign-in.' : 'TeraRelay Drive login startup disabled.');
        } catch (error) {
            setLinuxAutostartError(String(error));
            toast.error('Could not change TeraRelay startup preference.');
        } finally {
            setLinuxAutostartBusy(false);
        }
    };

    // Transcode cache state
    const [transcodeCache, setTranscodeCache] = useState<DetailedCacheInfo | null>(null);
    const [cacheLoading, setCacheLoading] = useState(false);
    const [clearingVariant, setClearingVariant] = useState<string | null>(null); // file_key:quality being cleared
    const [activeTab, setActiveTab] = useState<SettingsTab>('general');
    const [latencyMs, setLatencyMs] = useState<number | null>(null);
    const [vpnDetected, setVpnDetected] = useState<boolean | null>(null);
    const [proxyStatus, setProxyStatus] = useState<{ reachable: boolean; latency_ms: number } | null>(null);
    const [isTestingProxy, setIsTestingProxy] = useState(false);

    const [driveCryptoStatus, setDriveCryptoStatus] = useState<DriveCryptoStatus | null>(null);
    const [driveCryptoBusy, setDriveCryptoBusy] = useState(false);
    const [drivePassphrase, setDrivePassphrase] = useState('');
    const [driveCurrentPassphrase, setDriveCurrentPassphrase] = useState('');
    const [driveNewPassphrase, setDriveNewPassphrase] = useState('');
    const [driveConfirmPassphrase, setDriveConfirmPassphrase] = useState('');
    const [driveFinderOpen, setDriveFinderOpen] = useState(false);
    const [driveFinderEntries, setDriveFinderEntries] = useState<DriveFinderEntry[]>([]);
    const [driveFinderSearch, setDriveFinderSearch] = useState('');
    const [driveFinderKind, setDriveFinderKind] = useState<DriveFinderKind>('all');
    const [driveFinderSort, setDriveFinderSort] = useState<DriveFinderSortOrder>('path');
    const [driveFinderSizeFilter, setDriveFinderSizeFilter] = useState<DriveFinderSizeFilter>('all');
    const [driveFinderFolderScope, setDriveFinderFolderScope] = useState('');
    const [driveFinderVisibleLimit, setDriveFinderVisibleLimit] = useState(50);
    const [driveFinderBusy, setDriveFinderBusy] = useState(false);
    const [driveFinderCopyBusy, setDriveFinderCopyBusy] = useState(false);
    const [driveFinderCsvCopyBusy, setDriveFinderCsvCopyBusy] = useState(false);
    const [driveFinderError, setDriveFinderError] = useState<string | null>(null);
    const [driveFinderSkipped, setDriveFinderSkipped] = useState(0);
    const [driveFinderTruncated, setDriveFinderTruncated] = useState(0);
    const driveFinderMatches = useMemo(() => sortDriveFinderEntries(filterDriveFinderBySize(filterDriveFinderEntries(
        filterDriveFinderWithinFolder(driveFinderEntries, driveFinderFolderScope), driveFinderSearch, driveFinderKind), driveFinderSizeFilter), driveFinderSort),
        [driveFinderEntries, driveFinderFolderScope, driveFinderSearch, driveFinderKind, driveFinderSizeFilter, driveFinderSort]);
    const driveFinderShown = driveFinderMatches.slice(0, driveFinderVisibleLimit);
    // Storage totals cover the whole verified local index, not the user's
    // text filter or the first 50 visible rows. They are logical bytes only.
    const driveFinderInsights = useMemo(() => summarizeDriveStorage(driveFinderEntries), [driveFinderEntries]);
    const [driveTrashOpen, setDriveTrashOpen] = useState(false);
    const [driveTrash, setDriveTrash] = useState<DriveTrashItem[]>([]);
    const [driveTrashBusy, setDriveTrashBusy] = useState(false);
    const [driveTrashError, setDriveTrashError] = useState<string | null>(null);
    const [driveTrashWarning, setDriveTrashWarning] = useState<string | null>(null);
    const [driveTrashSearch, setDriveTrashSearch] = useState('');
    const [driveTrashFilter, setDriveTrashFilter] = useState<DriveTrashFilterKind>('all');
    const [driveTrashAvailability, setDriveTrashAvailability] = useState<DriveTrashAvailability>('all');
    const [driveTrashAge, setDriveTrashAge] = useState<DriveTrashAgeRange>('all');
    const [driveTrashClock, setDriveTrashClock] = useState(() => Date.now());
    const [driveTrashSort, setDriveTrashSort] = useState<DriveTrashSortOrder>('newest');
    const [driveTrashVisibleLimit, setDriveTrashVisibleLimit] = useState(50);
    const [selectedDriveTrashKeys, setSelectedDriveTrashKeys] = useState<string[]>([]);
    const [driveTrashBatchProgress, setDriveTrashBatchProgress] = useState<{ completed: number; total: number } | null>(null);
    const [driveTrashBatchError, setDriveTrashBatchError] = useState<string | null>(null);
    const [driveTrashBatchStopRequested, setDriveTrashBatchStopRequested] = useState(false);
    const [driveTrashInventoryCopyBusy, setDriveTrashInventoryCopyBusy] = useState(false);
    const hasActiveDriveTrashFilters = driveTrashSearch.trim() !== ''
        || driveTrashFilter !== 'all'
        || driveTrashAvailability !== 'all'
        || driveTrashAge !== 'all'
        || driveTrashSort !== 'newest';
    const resetDriveTrashFilters = () => {
        setDriveTrashSearch('');
        setDriveTrashFilter('all');
        setDriveTrashAvailability('all');
        setDriveTrashAge('all');
        setDriveTrashSort('newest');
        setDriveTrashVisibleLimit(50);
    };
    const visibleDriveTrash = useMemo(() => sortDriveTrashEntries(
        filterDriveTrashByAge(
            filterDriveTrashAvailability(
                filterDriveTrashEntries(driveTrash, driveTrashSearch, driveTrashFilter), driveTrashAvailability,
            ), driveTrashAge, driveTrashClock,
        ), driveTrashSort,
    ), [driveTrash, driveTrashSearch, driveTrashFilter, driveTrashAvailability, driveTrashAge, driveTrashClock, driveTrashSort]);
    // Large deleted-item lists should not mount thousands of DOM rows at once.
    // Apply display paging only after the full local search/type/sort pass.
    const shownDriveTrash = visibleDriveTrash.slice(0, driveTrashVisibleLimit);
    const selectedRestorableDriveTrash = driveTrash.filter(item => item.restorable
        && selectedDriveTrashKeys.includes(item.kind + ':' + item.id));
    const selectedDriveTrashSizes = summarizeSelectedDriveTrashSizes(selectedRestorableDriveTrash);
    const shownRestorableDriveTrash = shownDriveTrash.filter(item => item.restorable);
    const selectShownDriveTrash = () => {
        setSelectedDriveTrashKeys(current => [...new Set([
            ...current, ...shownRestorableDriveTrash.map(item => item.kind + ':' + item.id),
        ])].slice(0, 100));
    };
    const copyFilteredDriveTrashInventory = async () => {
        if (!driveSettingsOpenRef.current || !driveTrashOpen || driveTrashBusy || driveTrashError
            || driveTrashInventoryCopyInFlightRef.current || visibleDriveTrash.length === 0) return;
        driveTrashInventoryCopyInFlightRef.current = true;
        setDriveTrashInventoryCopyBusy(true);
        const session = driveTrashSessionRef.current;
        const inventory = formatDriveTrashInventory(visibleDriveTrash);
        try {
            await navigator.clipboard.writeText(inventory.text);
            if (driveSettingsOpenRef.current && session === driveTrashSessionRef.current) {
                toast.success(`Copied ${inventory.exported} matching deleted Drive items${inventory.omitted > 0 ? `; ${inventory.omitted} more were omitted.` : '.'}`);
            }
        } catch {
            if (driveSettingsOpenRef.current && session === driveTrashSessionRef.current) {
                toast.error('Could not copy deleted Drive list. Check clipboard access and retry.');
            }
        } finally {
            driveTrashInventoryCopyInFlightRef.current = false;
            if (driveSettingsOpenRef.current && session === driveTrashSessionRef.current) {
                setDriveTrashInventoryCopyBusy(false);
            }
        }
    };
    // Age windows are rolling, not snapshots of the moment the dialog opened.
    // Tick only while a time filter is visible; never poll Telegram for this.
    useEffect(() => {
        if (!isOpen || !driveTrashOpen || driveTrashAge === 'all') return;
        setDriveTrashClock(Date.now());
        const timer = window.setInterval(() => setDriveTrashClock(Date.now()), 60_000);
        return () => window.clearInterval(timer);
    }, [isOpen, driveTrashOpen, driveTrashAge]);
    const [restoringDriveItem, setRestoringDriveItem] = useState<string | null>(null);
    const [driveRestoreSyncProblem, setDriveRestoreSyncProblem] = useState<Exclude<DriveRestoreSyncState, 'synced'> | null>(null);
    const [driveRestoreSyncBusy, setDriveRestoreSyncBusy] = useState(false);
    const [drivePendingRecoveryBusy, setDrivePendingRecoveryBusy] = useState(false);
    const [drivePendingRecoveryResult, setDrivePendingRecoveryResult] = useState<DrivePendingRecoveryResult | null>(null);
    const [drivePendingRecoveryError, setDrivePendingRecoveryError] = useState<string | null>(null);
    // A delayed list reply cannot overwrite a newer refresh or another
    // Settings session. Guard mutations synchronously before React rerenders.
    const driveFinderRequestRef = useRef(0);
    const driveFinderFetchInFlightRef = useRef(false);
    const driveFinderCopyInFlightRef = useRef(false);
    const driveFinderCsvCopyInFlightRef = useRef(false);
    const driveFinderCsvCopyGenerationRef = useRef(0);
    const driveTrashRequestRef = useRef(0);
    const driveTrashSessionRef = useRef(0);
    const driveTrashInventoryCopyInFlightRef = useRef(false);
    const driveCryptoSessionRef = useRef(0);
    const driveCryptoStatusEpochRef = useRef(0);
    const driveCryptoActionInFlightRef = useRef(false);
    const driveTrashRestoreInFlightRef = useRef(false);
    const driveTrashBatchStopRequestedRef = useRef(false);
    const driveRestoreSyncInFlightRef = useRef(false);
    const drivePendingRecoveryInFlightRef = useRef(false);
    const drivePendingRecoverySessionRef = useRef(0);
    const driveSettingsOpenRef = useRef(isOpen);
    driveSettingsOpenRef.current = isOpen;
    useEffect(() => {
        if (isOpen) {
            if (driveRestoreSyncInFlightRef.current) setDriveRestoreSyncBusy(true);
            if (drivePendingRecoveryInFlightRef.current) setDrivePendingRecoveryBusy(true);
            return;
        }
        drivePendingRecoverySessionRef.current += 1;
        setDrivePendingRecoveryResult(null);
        setDrivePendingRecoveryError(null);
        setDrivePendingRecoveryBusy(false);
        driveFinderRequestRef.current += 1;
        driveFinderFetchInFlightRef.current = false;
        driveFinderCopyInFlightRef.current = false;
        driveFinderCsvCopyInFlightRef.current = false;
        driveFinderCsvCopyGenerationRef.current++;
        setDriveFinderCopyBusy(false);
        setDriveFinderCsvCopyBusy(false);
        setDriveFinderOpen(false);
        setDriveFinderEntries([]);
        setDriveFinderSearch('');
        setDriveFinderKind('all');
        setDriveFinderSort('path');
        setDriveFinderSizeFilter('all');
        setDriveFinderFolderScope('');
        setDriveFinderVisibleLimit(50);
        setDriveFinderBusy(false);
        setDriveFinderError(null);
        setDriveFinderSkipped(0);
        setDriveFinderTruncated(0);
        driveTrashRequestRef.current += 1;
        driveTrashSessionRef.current += 1;
        setDriveTrashVisibleLimit(50);
        setSelectedDriveTrashKeys([]);
        setDriveTrashBatchProgress(null);
        setDriveTrashBatchError(null);
        driveTrashBatchStopRequestedRef.current = true;
        setDriveTrashBatchStopRequested(false);
        setDriveTrashInventoryCopyBusy(false);
        setDriveTrashBusy(false);
        setDriveRestoreSyncBusy(false);
    }, [isOpen]);

    // Update check state
    // Reconnect state
    const [reconnecting, setReconnecting] = useState(false);

    // Diagnostics state
    const [diagLoading, setDiagLoading] = useState(false);

    // Sharing settings state
    const [shares, setShares] = useState<ShareInfo[]>([]);
    const [refreshing, setRefreshing] = useState(false);
    const [copiedId, setCopiedId] = useState<string | null>(null);
    const [globalDomain, setGlobalDomain] = useState('');

    const fetchShares = useCallback(async () => {
        setRefreshing(true);
        try {
            const list = await invoke<ShareInfo[]>('cmd_list_shares');
            setShares(list);
        } catch (e) {
            toast.error(t('settings.load_shares_failed', { error: e }));
        } finally {
            setRefreshing(false);
        }
    }, [t]);

    useEffect(() => {
        if (isOpen && activeTab === 'sharing') {
            fetchShares();
        }
    }, [isOpen, activeTab, fetchShares]);

    const handleRevokeShare = async (id: string) => {
        const ok = await confirm({
            title: t('settings.revoke_link_title'),
            message: t('settings.revoke_link_desc'),
            confirmText: t('settings.revoke'),
            variant: 'danger',
        });
        if (!ok) return;

        try {
            await invoke('cmd_revoke_share', { id });
            toast.success(t('settings.link_revoked'));
            fetchShares();
        } catch (e) {
            toast.error(t('settings.link_revoke_failed', { error: e }));
        }
    };

    const handleCopyShare = (id: string) => {
        const share = shares.find(s => s.id === id);
        if (!share) return;
        
        let link = share.link;
        if (globalDomain.trim()) {
            link = `http://${globalDomain.trim()}/d/${share.id}`;
        }
        
        navigator.clipboard.writeText(link);
        setCopiedId(share.id);
        setTimeout(() => setCopiedId(null), 2000);
    };

    // API settings state
    const [apiSettings, setApiSettings] = useState<ApiSettings>({ enabled: false, port: 8550, ["key_set"]: false, running: false });
    const [apiPort, setApiPort] = useState('8550');
    const [apiLoading, setApiLoading] = useState(false);
    const [generatedAccessValue, setGeneratedAccessValue] = useState<string | null>(null);
    const [keyCopied, setKeyCopied] = useState(false);

    const fetchDriveCryptoStatus = useCallback(async () => {
        const session = driveCryptoSessionRef.current;
        const request = ++driveCryptoStatusEpochRef.current;
        try {
            const status = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_status'));
            if (driveSettingsOpenRef.current && session === driveCryptoSessionRef.current
                && request === driveCryptoStatusEpochRef.current) {
                setDriveCryptoStatus(status);
            }
        } catch {
            if (driveSettingsOpenRef.current && session === driveCryptoSessionRef.current
                && request === driveCryptoStatusEpochRef.current) {
                setDriveCryptoStatus(null);
            }
        }
    }, []);

    const clearDrivePassphrases = useCallback(() => {
        setDrivePassphrase('');
        setDriveCurrentPassphrase('');
        setDriveNewPassphrase('');
        setDriveConfirmPassphrase('');
    }, []);

    // Treat every close/reopen as a fresh crypto UI session. Clear sensitive
    // passphrases on close rather than leaving them in a hidden React dialog.
    // Native requests already issued are not cancelled; their late UI results
    // must never leak into a different Settings session.
    useEffect(() => {
        if (isOpen) {
            if (driveCryptoActionInFlightRef.current) setDriveCryptoBusy(true);
            return;
        }
        driveCryptoSessionRef.current += 1;
        clearDrivePassphrases();
        setDriveCryptoBusy(false);
    }, [isOpen, clearDrivePassphrases]);

    const isCurrentDriveCryptoSession = (session: number) =>
        driveSettingsOpenRef.current && session === driveCryptoSessionRef.current;

    const fetchDriveTrash = useCallback(async () => {
        const request = ++driveTrashRequestRef.current;
        setDriveTrashBusy(true);
        setDriveTrashError(null);
        try {
            const response = await invoke<unknown>('cmd_drive_list_trash');
            if (!driveSettingsOpenRef.current || request !== driveTrashRequestRef.current) return;
            const { entries, skipped } = normalizeDriveTrashEntries(response);
            if (skipped > 0 && entries.length === 0) {
                throw new TypeError('Native Drive returned no valid recently deleted items');
            }
            setDriveTrash(entries);
            const available = new Set(entries.filter(item => item.restorable)
                .map(item => item.kind + ':' + item.id));
            setSelectedDriveTrashKeys(current => current.filter(key => available.has(key)));
            setDriveTrashWarning(skipped > 0
                ? `Skipped ${skipped} malformed or duplicate deleted Drive ${skipped === 1 ? 'item' : 'items'}. The remaining entries are available; refresh to retry.`
                : null);
            setDriveTrashError(null);
        } catch (error) {
            if (driveSettingsOpenRef.current && request === driveTrashRequestRef.current) {
                setDriveTrashError('Could not refresh recently deleted items. The previous list may be out of date.');
            }
        } finally {
            if (driveSettingsOpenRef.current && request === driveTrashRequestRef.current) {
                setDriveTrashBusy(false);
            }
        }
    }, []);

    // The panel stays expanded across Settings sessions. Reopen must fetch
    // a fresh list, but expanding once should never issue duplicate requests.
    useEffect(() => {
        if (isOpen && driveTrashOpen) void fetchDriveTrash();
    }, [isOpen, driveTrashOpen, fetchDriveTrash]);

    const recoverPendingDriveWrites = async () => {
        if (drivePendingRecoveryInFlightRef.current) return;
        drivePendingRecoveryInFlightRef.current = true;
        const session = drivePendingRecoverySessionRef.current;
        const isCurrentDialog = () => driveSettingsOpenRef.current
            && session === drivePendingRecoverySessionRef.current;
        setDrivePendingRecoveryBusy(true);
        setDrivePendingRecoveryResult(null);
        setDrivePendingRecoveryError(null);
        try {
            // This native command finalizes closed staged writes. It is
            // intentionally user-initiated here, never an automatic UI poll.
            const value = await invoke<unknown>('cmd_drive_recover_pending');
            if (!value || typeof value !== 'object' || Array.isArray(value)) {
                throw new TypeError('Native Drive recovery returned an invalid result');
            }
            const row = value as Record<string, unknown>;
            const counters = [row.recovered, row.remaining, row.failures];
            if (counters.some(count => typeof count !== 'number'
                || !Number.isSafeInteger(count) || count < 0 || count > 0xffffffff)) {
                throw new TypeError('Native Drive recovery returned invalid item counts');
            }
            if (!isCurrentDialog()) return;
            const result: DrivePendingRecoveryResult = {
                recovered: row.recovered as number,
                remaining: row.remaining as number,
                failures: row.failures as number,
            };
            setDrivePendingRecoveryResult(result);
            if (result.remaining > 0 || result.failures > 0) {
                toast.warning('Drive write recovery: ' + result.recovered + ' completed, '
                    + result.remaining + ' still pending, ' + result.failures + ' failed.');
            } else {
                toast.success(result.recovered > 0
                    ? 'Recovered ' + result.recovered + ' pending Drive '
                        + (result.recovered === 1 ? 'write.' : 'writes.')
                    : 'No closed pending Drive writes need recovery.');
            }
        } catch (error) {
            if (isCurrentDialog()) {
                setDrivePendingRecoveryError('Could not retry pending Drive writes: ' + String(error));
                toast.error('Drive write recovery did not complete. You can retry.');
            }
        } finally {
            // Do not release native ownership on close. The old command must
            // actually settle before a reopened dialog can retry it.
            drivePendingRecoveryInFlightRef.current = false;
            if (driveSettingsOpenRef.current) setDrivePendingRecoveryBusy(false);
        }
    };

    const classifyDriveRestoreSync = (result: unknown): DriveRestoreSyncState => {
        // Native IPC's compile-time types do not guarantee a real revision,
        // publication status or source. Never claim Telegram sync from a
        // corrupt response after a successful local restore.
        let verified: ReturnType<typeof validateDriveSyncResult>;
        try {
            verified = validateDriveSyncResult(result);
        } catch {
            return 'unverified';
        }
        if (verified.source === 'qa-local') return 'qa-local';
        if (verified.dirty) return 'pending';
        return 'synced';
    };

    const retryDriveRestoreMetadataSync = async () => {
        if (!driveRestoreSyncProblem || driveRestoreSyncInFlightRef.current
            || driveTrashRestoreInFlightRef.current) return;
        driveRestoreSyncInFlightRef.current = true;
        const session = driveTrashSessionRef.current;
        const isCurrentDialog = () => driveSettingsOpenRef.current && session === driveTrashSessionRef.current;
        setDriveRestoreSyncBusy(true);
        try {
            const result = await invoke<DriveSyncOutcome>('cmd_drive_sync_metadata');
            if (!isCurrentDialog()) return;
            const outcome = classifyDriveRestoreSync(result);
            setDriveRestoreSyncProblem(outcome === 'synced' ? null : outcome);
            if (outcome === 'synced') {
                toast.success('Restored Drive metadata is now synchronized.');
            } else {
                toast.warning(outcome === 'qa-local'
                    ? 'Drive is in QA mode; metadata is still local.'
                    : outcome === 'pending'
                        ? 'Drive metadata still has pending changes. You can retry synchronization.'
                        : 'Drive synchronization completed, but remote publication could not be verified.');
            }
        } catch {
            if (isCurrentDialog()) {
                setDriveRestoreSyncProblem('offline');
                toast.warning('Drive metadata is still local. Retry when Telegram is connected.');
            }
        } finally {
            driveRestoreSyncInFlightRef.current = false;
            // A closed/reopened Settings session must never receive the old
            // notification, but should no longer appear busy after native work ends.
            if (driveSettingsOpenRef.current) setDriveRestoreSyncBusy(false);
        }
    };

    const restoreDriveTrashItem = async (entry: DriveTrashItem) => {
        if (driveTrashRestoreInFlightRef.current || driveRestoreSyncInFlightRef.current) return;
        driveTrashRestoreInFlightRef.current = true;
        const session = driveTrashSessionRef.current;
        const isCurrentDialog = () => driveSettingsOpenRef.current && session === driveTrashSessionRef.current;
        setRestoringDriveItem(entry.kind + ':' + entry.id);
        try {
            const restoredName = await invoke<string>('cmd_drive_restore_trash_item', {
                kind: entry.kind,
                id: entry.id,
            });
            // Native restore has already succeeded. Immediately remove the exact
            // restored row so the user cannot retry it while a slow list refresh
            // is pending. Invalidate any pre-restore list reply first: otherwise
            // an older response could reintroduce this deleted item.
            if (isCurrentDialog()) {
                ++driveTrashRequestRef.current;
                setDriveTrash(current => current.filter(item =>
                    item.kind !== entry.kind || item.id !== entry.id));
                setDriveTrashBusy(false);
            }
            // Cloud metadata sync is essential; don't hold it hostage to an
            // optional, potentially slow, Recently Deleted list refresh.
            let syncOutcome: DriveRestoreSyncState = 'offline';
            try {
                const result = await invoke<DriveSyncOutcome>('cmd_drive_sync_metadata');
                syncOutcome = classifyDriveRestoreSync(result);
            } catch {
                // Native restore already succeeded; only metadata sync failed.
            }
            if (driveSettingsOpenRef.current) void fetchDriveTrash();
            if (isCurrentDialog()) {
                setDriveRestoreSyncProblem(syncOutcome === 'synced' ? null : syncOutcome);
                switch (syncOutcome) {
                    case 'synced':
                        toast.success('Restored ' + restoredName + '; Drive metadata is synchronized.');
                        break;
                    case 'qa-local':
                        toast.warning('Restored ' + restoredName + ' locally. QA mode does not sync to Telegram.');
                        break;
                    case 'pending':
                        toast.warning('Restored ' + restoredName + ' locally. Drive metadata still has pending changes.');
                        break;
                    case 'unverified':
                        toast.warning('Restored ' + restoredName + ' locally, but remote sync status could not be verified.');
                        break;
                    default:
                        toast.warning('Restored ' + restoredName + ' locally. Remote Drive sync is unavailable; retry when Telegram is connected.');
                }
            }
        } catch (error) {
            if (isCurrentDialog()) toast.error('Could not restore ' + entry.name + ': ' + String(error));
        } finally {
            driveTrashRestoreInFlightRef.current = false;
            setRestoringDriveItem(null);
        }
    };

    const restoreSelectedDriveTrash = async () => {
        if (driveTrashRestoreInFlightRef.current || driveRestoreSyncInFlightRef.current
            || driveTrashBusy || driveTrashError) return;
        // Snapshot verified, restorable identities at the moment of the click.
        // A bounded sequential batch avoids overlapping native restore writes.
        const entries = selectedRestorableDriveTrash.slice(0, 100);
        if (entries.length === 0) return;
        driveTrashRestoreInFlightRef.current = true;
        const session = driveTrashSessionRef.current;
        const isCurrentDialog = () => driveSettingsOpenRef.current
            && session === driveTrashSessionRef.current;
        setRestoringDriveItem('bulk');
        setDriveTrashBatchProgress({ completed: 0, total: entries.length });
        setDriveTrashBatchError(null);
        driveTrashBatchStopRequestedRef.current = false;
        setDriveTrashBatchStopRequested(false);
        let restored = 0;
        let failed: string | null = null;
        try {
            for (const entry of entries) {
                // Closing Settings stops the batch after any already-issued
                // native request settles, without abandoning its metadata sync.
                if (!isCurrentDialog() || driveTrashBatchStopRequestedRef.current) break;
                try {
                    await invoke('cmd_drive_restore_trash_item', { kind: entry.kind, id: entry.id });
                } catch (error) {
                    failed = `Could not restore ${entry.name}: ${String(error)}. Remaining items were not attempted.`;
                    if (isCurrentDialog()) setDriveTrashBatchError(failed);
                    break;
                }
                restored += 1;
                if (isCurrentDialog()) {
                    ++driveTrashRequestRef.current;
                    setDriveTrash(current => current.filter(item =>
                        item.kind !== entry.kind || item.id !== entry.id));
                    setSelectedDriveTrashKeys(current => current.filter(key => key !== entry.kind + ':' + entry.id));
                    setDriveTrashBatchProgress({ completed: restored, total: entries.length });
                }
            }
            if (restored === 0) {
                if (failed && isCurrentDialog()) toast.error(failed);
                else if (driveTrashBatchStopRequestedRef.current && isCurrentDialog()) {
                    toast.warning('Bulk Drive restore stopped. No items were restored.');
                }
                return;
            }
            // Sync once after all completed native restores, including a
            // partial batch or one closed halfway through. A failed sync must
            // never make restored local files appear to have been published.
            let syncOutcome: DriveRestoreSyncState = 'offline';
            try {
                const result = await invoke<DriveSyncOutcome>('cmd_drive_sync_metadata');
                syncOutcome = classifyDriveRestoreSync(result);
            } catch {
                // The local restore still succeeded; users can retry sync.
            }
            if (isCurrentDialog()) void fetchDriveTrash();
            if (isCurrentDialog()) {
                setDriveRestoreSyncProblem(syncOutcome === 'synced' ? null : syncOutcome);
                const summary = `Restored ${restored} of ${entries.length} selected Drive items locally.`;
                if (failed) {
                    toast.warning(summary + ' Some items remain selected; check the batch error.');
                } else if (driveTrashBatchStopRequestedRef.current && restored < entries.length) {
                    toast.warning(summary + (syncOutcome === 'synced'
                        ? ' Stopped as requested; completed items are synchronized.'
                        : ' Stopped as requested; completed items still need verified remote sync.'));
                } else if (syncOutcome === 'synced') {
                    toast.success(`Restored ${restored} Drive items; metadata synchronized.`);
                } else {
                    toast.warning(summary + ' Remote metadata sync is not confirmed; use Retry sync.');
                }
            }
        } finally {
            driveTrashRestoreInFlightRef.current = false;
            setRestoringDriveItem(null);
        }
    };

    const copyLocalDriveInventoryCsv = async () => {
        if (!driveSettingsOpenRef.current || !driveFinderOpen || driveFinderBusy || driveFinderError
            || driveFinderCsvCopyInFlightRef.current || driveFinderMatches.length === 0) return;
        driveFinderCsvCopyInFlightRef.current = true;
        setDriveFinderCsvCopyBusy(true);
        const generation = driveFinderCsvCopyGenerationRef.current;
        const exported = formatDriveFinderCsv(driveFinderMatches);
        try {
            await navigator.clipboard.writeText(exported.csv);
            if (driveSettingsOpenRef.current && generation === driveFinderCsvCopyGenerationRef.current) {
                toast.success(`Copied ${exported.exported} local Drive entries as CSV${exported.omitted ? `; ${exported.omitted} additional matches omitted.` : '.'}`);
            }
        } catch {
            if (driveSettingsOpenRef.current && generation === driveFinderCsvCopyGenerationRef.current) {
                toast.error('Could not copy Drive CSV inventory. Check clipboard permissions and retry.');
            }
        } finally {
            if (generation === driveFinderCsvCopyGenerationRef.current) {
                driveFinderCsvCopyInFlightRef.current = false;
                if (driveSettingsOpenRef.current) setDriveFinderCsvCopyBusy(false);
            }
        }
    };

    const copyLocalDriveRelativePath = async (relativePath: string) => {
        if (!driveSettingsOpenRef.current || !driveFinderOpen || driveFinderCopyInFlightRef.current) return;
        driveFinderCopyInFlightRef.current = true;
        setDriveFinderCopyBusy(true);
        const request = driveFinderRequestRef.current;
        try {
            await navigator.clipboard.writeText(relativePath);
            if (driveSettingsOpenRef.current && request === driveFinderRequestRef.current) {
                toast.success('Drive-relative path copied. This is not an absolute OS mount path.');
            }
        } catch {
            if (driveSettingsOpenRef.current && request === driveFinderRequestRef.current) {
                toast.error('Could not copy Drive-relative path. Check clipboard access and retry.');
            }
        } finally {
            if (request === driveFinderRequestRef.current) {
                driveFinderCopyInFlightRef.current = false;
                if (driveSettingsOpenRef.current) setDriveFinderCopyBusy(false);
            }
        }
    };

    const refreshLocalDriveFinder = async () => {
        if (!driveSettingsOpenRef.current || driveFinderFetchInFlightRef.current) return;
        driveFinderFetchInFlightRef.current = true;
        const request = ++driveFinderRequestRef.current;
        setDriveFinderBusy(true);
        setDriveFinderError(null);
        try {
            // This reads the local metadata database only; there is no
            // Telegram fetch, filesystem traversal, mount or mutation.
            const snapshot = normalizeDriveFinderSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
            if (!driveSettingsOpenRef.current || request !== driveFinderRequestRef.current) return;
            setDriveFinderEntries(snapshot.entries);
            setDriveFinderSkipped(snapshot.skipped);
            setDriveFinderTruncated(snapshot.truncated);
            setDriveFinderVisibleLimit(50);
        } catch {
            if (!driveSettingsOpenRef.current || request !== driveFinderRequestRef.current) return;
            setDriveFinderError('Could not read and verify local Drive metadata. Retry the search.');
            setDriveFinderEntries([]);
        } finally {
            if (request === driveFinderRequestRef.current) {
                driveFinderFetchInFlightRef.current = false;
                if (driveSettingsOpenRef.current) setDriveFinderBusy(false);
            }
        }
    };

    const fetchApiSettings = useCallback(async () => {
        try {
            const result = await invoke<ApiSettings>('cmd_get_api_settings');
            setApiSettings(result);
            setApiPort(result.port.toString());
        } catch {
            // API settings not available
        }
    }, []);

    // Load API settings when modal opens
    useEffect(() => {
        if (isOpen && SHOW_ADVANCED_SETTINGS) {
            fetchApiSettings();
            setGeneratedAccessValue(null);
            setKeyCopied(false);
        }
    }, [isOpen, fetchApiSettings]);

    useEffect(() => {
        if (!isOpen || activeTab !== 'general') return;
        void fetchDriveCryptoStatus();
        clearDrivePassphrases();
    }, [isOpen, activeTab, fetchDriveCryptoStatus, clearDrivePassphrases]);

    // Fetch transcode cache info
    const fetchTranscodeCache = useCallback(async () => {
        setCacheLoading(true);
        try {
            const info = await invoke<DetailedCacheInfo>('cmd_get_detailed_transcode_cache');
            setTranscodeCache(info);
        } catch {
            setTranscodeCache(null);
        } finally {
            setCacheLoading(false);
        }
    }, []);

    // Load transcode cache when on general tab
    useEffect(() => {
        if (isOpen && activeTab === 'general' && SHOW_ADVANCED_SETTINGS) {
            fetchTranscodeCache();
        }
    }, [isOpen, activeTab, fetchTranscodeCache]);

    // Poll API status while modal is open and API is enabled
    useEffect(() => {
        if (!isOpen || !apiSettings.enabled) return;
        const interval = setInterval(fetchApiSettings, 3000);
        return () => clearInterval(interval);
    }, [isOpen, apiSettings.enabled, fetchApiSettings]);

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

    // Sync VPN optimizer settings to backend whenever they change
    useEffect(() => {
        const applyVpn = async () => {
            try {
                await invoke('cmd_apply_vpn_settings', {
                    enabled: settings.vpnMode,
                    timeoutMultiplier: settings.timeoutMultiplier,
                    retryAttempts: settings.retryAttempts,
                    retryBaseBackoffMs: Math.round(settings.retryBaseBackoffSec * 1000),
                    retryMaxBackoffMs: Math.round(settings.retryMaxBackoffSec * 1000),
                    adaptivePolling: settings.adaptivePolling,
                    pollingMinSec: settings.pollingMinSec,
                    pollingMaxSec: settings.pollingMaxSec,
                    preferredDc: settings.preferredDC,
                    dcFallbackAttempts: settings.dcFallbackAttempts,
                    floodWaitRespect: settings.floodWaitRespect,
                    peerCacheSize: settings.peerCacheSize,
                    bandwidthLimitUpKbs: settings.bandwidthLimitUpKBs,
                    bandwidthLimitDownKbs: settings.bandwidthLimitDownKBs,
                    chunkSizeKb: settings.chunkSizeKb,
                    keepAliveIntervalSec: settings.keepAliveIntervalSec,
                    autoDetectVpn: settings.autoDetectVpn,
                    archiveMaxBytes: settings.archiveMaxBytes * 1024 * 1024,
                });
            } catch {
                // best-effort sync
            }
        };
        applyVpn();
    }, [
        settings.vpnMode, settings.timeoutMultiplier, settings.retryAttempts,
        settings.retryBaseBackoffSec, settings.retryMaxBackoffSec, settings.adaptivePolling,
        settings.pollingMinSec, settings.pollingMaxSec, settings.preferredDC,
        settings.dcFallbackAttempts, settings.floodWaitRespect, settings.peerCacheSize,
        settings.bandwidthLimitUpKBs, settings.bandwidthLimitDownKBs, settings.chunkSizeKb,
        settings.keepAliveIntervalSec, settings.autoDetectVpn, settings.archiveMaxBytes,
    ]);

    // Poll latency when VPN tab is active
    useEffect(() => {
        if (!isOpen || activeTab !== 'vpn') return;
        const check = async () => {
            try {
                const ms = await invoke<number>('cmd_check_latency');
                setLatencyMs(ms);
            } catch { setLatencyMs(null); }
        };
        check();
        const interval = setInterval(check, 5000);
        return () => clearInterval(interval);
    }, [isOpen, activeTab]);

    // Poll proxy status when Proxy tab is active
    useEffect(() => {
        if (!isOpen || activeTab !== 'proxy') return;
        const checkProxy = async () => {
            if (!settings.proxyEnabled || !settings.proxyLiveStateEnabled) {
                setProxyStatus(null);
                return;
            }
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
    }, [isOpen, activeTab, settings.proxyEnabled, settings.proxyLiveStateEnabled]);

    // Detect VPN interfaces when VPN tab opens
    useEffect(() => {
        if (!isOpen || activeTab !== 'vpn') return;
        const detect = async () => {
            try {
                const found = await invoke<boolean>('cmd_detect_vpn');
                setVpnDetected(found);
            } catch { setVpnDetected(null); }
        };
        detect();
    }, [isOpen, activeTab]);

    const handleApiToggle = async () => {
        setApiLoading(true);
        try {
            const port = parseInt(apiPort, 10);
            if (isNaN(port) || port < 1024 || port > 65535) {
                toast.error(t('settings.port_range_error'));
                setApiLoading(false);
                return;
            }
            const result = await invoke<ApiSettings>('cmd_update_api_settings', {
                enabled: !apiSettings.enabled,
                port,
            });
            setApiSettings(result);
            toast.success(result.enabled ? t('settings.api_server_started') : t('settings.api_server_stopped'));
        } catch (e) {
            toast.error(t('settings.api_update_failed', { error: e }));
        } finally {
            setApiLoading(false);
        }
    };

    const handlePortApply = async () => {
        const port = parseInt(apiPort, 10);
        if (isNaN(port) || port < 1024 || port > 65535) {
            toast.error(t('settings.port_range_error'));
            return;
        }
        if (port === apiSettings.port) return;
        setApiLoading(true);
        try {
            const result = await invoke<ApiSettings>('cmd_update_api_settings', {
                enabled: apiSettings.enabled,
                port,
            });
            setApiSettings(result);
            toast.success(t('settings.api_port_updated', { port }));
        } catch (e) {
            toast.error(t('settings.api_port_update_failed', { error: e }));
        } finally {
            setApiLoading(false);
        }
    };

    const handleGenerateKey = async () => {
        const ok = await confirm({
            title: t('settings.generate_api_key_title'),
            message: apiSettings.key_set
                ? t('settings.regenerate_api_key_desc')
                : t('settings.generate_api_key_desc'),
            confirmText: apiSettings.key_set ? t('settings.regenerate') : t('settings.generate'),
            variant: apiSettings.key_set ? 'danger' : 'info',
        });
        if (!ok) return;
        try {
            const freshAccessValue = await invoke<string>('cmd_regenerate_api_key');
            setGeneratedAccessValue(freshAccessValue);
            setKeyCopied(false);
            setApiSettings(prev => ({ ...prev, ["key_set"]: true }));
            toast.success(t('settings.api_key_generated'));
        } catch (e) {
            toast.error(t('settings.api_key_generate_failed', { error: e }));
        }
    };

    const handleCopyKey = async () => {
        if (!generatedAccessValue) return;
        try {
            await navigator.clipboard.writeText(generatedAccessValue);
            setKeyCopied(true);
            setTimeout(() => setKeyCopied(false), 2000);
        } catch {
            toast.error(t('settings.copy_clipboard_failed'));
        }
    };

    const handleDriveCryptoSetup = async () => {
        if (driveCryptoActionInFlightRef.current) return;
        if (drivePassphrase.length < 12) {
            toast.error('Drive passphrase must contain at least 12 characters');
            return;
        }
        if (drivePassphrase !== driveConfirmPassphrase) {
            toast.error('Drive passphrases do not match');
            return;
        }

        driveCryptoActionInFlightRef.current = true;
        ++driveCryptoStatusEpochRef.current;
        const cryptoSession = driveCryptoSessionRef.current;
        setDriveCryptoBusy(true);
        try {
            const ok = await confirm({
                title: 'Enable TeraRelay Drive encryption?',
                message: 'New and edited Drive file contents will be encrypted before upload. Keep this passphrase safe: another computer needs it to unlock the same Drive.',
                confirmText: 'Enable encryption',
                variant: 'info',
            });
            if (!ok || !isCurrentDriveCryptoSession(cryptoSession)) return;

            // Reconcile the Telegram-synced Drive manifest before generating
            // a master key, avoiding a competing key on another computer.
            await invoke('cmd_drive_sync_metadata');
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            const current = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_status'));
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            if (current.configured) {
                ++driveCryptoStatusEpochRef.current;
                setDriveCryptoStatus(current);
                clearDrivePassphrases();
                toast.info('Drive encryption is already configured. Unlock it with the existing passphrase.');
                return;
            }

            const status = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_setup', {
                passphrase: drivePassphrase,
            }));
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            if (!status?.configured || !status.unlocked) {
                throw new Error('Drive encryption setup returned an unconfigured or locked state');
            }
            ++driveCryptoStatusEpochRef.current;
            setDriveCryptoStatus(status);
            clearDrivePassphrases();
            toast.success('TeraRelay Drive encryption enabled and synced');
        } catch (error) {
            if (isCurrentDriveCryptoSession(cryptoSession)) {
                void fetchDriveCryptoStatus();
                toast.error('Could not finish Drive encryption setup: ' + String(error));
            }
        } finally {
            // Even an abandoned confirmation or a late native response must
            // release ownership before another crypto action can be attempted.
            driveCryptoActionInFlightRef.current = false;
            if (driveSettingsOpenRef.current) setDriveCryptoBusy(false);
        }
    };

    const handleDriveCryptoUnlock = async () => {
        if (driveCryptoActionInFlightRef.current) return;
        if (!drivePassphrase) {
            toast.error('Enter your Drive passphrase');
            return;
        }
        driveCryptoActionInFlightRef.current = true;
        ++driveCryptoStatusEpochRef.current;
        const cryptoSession = driveCryptoSessionRef.current;
        setDriveCryptoBusy(true);
        try {
            const status = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_unlock', {
                passphrase: drivePassphrase,
            }));
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            if (!status?.configured || !status.unlocked) {
                throw new Error('Drive remained locked after the unlock request');
            }
            ++driveCryptoStatusEpochRef.current;
            setDriveCryptoStatus(status);
            clearDrivePassphrases();
            toast.success('TeraRelay Drive unlocked');
        } catch (error) {
            if (isCurrentDriveCryptoSession(cryptoSession)) {
                toast.error('Could not unlock Drive: ' + String(error));
            }
        } finally {
            driveCryptoActionInFlightRef.current = false;
            if (driveSettingsOpenRef.current) setDriveCryptoBusy(false);
        }
    };

    const handleDriveCryptoChangePassphrase = async () => {
        if (driveCryptoActionInFlightRef.current) return;
        if (!driveCurrentPassphrase) {
            toast.error('Enter your current Drive passphrase');
            return;
        }
        if (driveNewPassphrase.length < 12) {
            toast.error('New Drive passphrase must contain at least 12 characters');
            return;
        }
        if (driveNewPassphrase !== driveConfirmPassphrase) {
            toast.error('New Drive passphrases do not match');
            return;
        }
        driveCryptoActionInFlightRef.current = true;
        ++driveCryptoStatusEpochRef.current;
        const cryptoSession = driveCryptoSessionRef.current;
        setDriveCryptoBusy(true);
        try {
            const status = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_change_passphrase', {
                currentPassphrase: driveCurrentPassphrase,
                newPassphrase: driveNewPassphrase,
            }));
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            if (!status?.configured || !status.unlocked) {
                throw new Error('Drive passphrase change did not return an unlocked encrypted Drive');
            }
            ++driveCryptoStatusEpochRef.current;
            setDriveCryptoStatus(status);
            clearDrivePassphrases();
            toast.success('Drive passphrase changed and synced');
        } catch (error) {
            if (isCurrentDriveCryptoSession(cryptoSession)) {
                void fetchDriveCryptoStatus();
                toast.error('Could not finish Drive passphrase change: ' + String(error));
            }
        } finally {
            driveCryptoActionInFlightRef.current = false;
            if (driveSettingsOpenRef.current) setDriveCryptoBusy(false);
        }
    };

    const handleDriveCryptoLock = async () => {
        if (driveCryptoActionInFlightRef.current) return;
        driveCryptoActionInFlightRef.current = true;
        ++driveCryptoStatusEpochRef.current;
        const cryptoSession = driveCryptoSessionRef.current;
        setDriveCryptoBusy(true);
        try {
            const status = validateDriveCryptoStatus(await invoke<unknown>('cmd_drive_crypto_lock'));
            if (!isCurrentDriveCryptoSession(cryptoSession)) return;
            if (!status?.configured || status.unlocked !== false) {
                throw new Error('Drive remained unlocked after the lock request');
            }
            ++driveCryptoStatusEpochRef.current;
            setDriveCryptoStatus(status);
            clearDrivePassphrases();
            toast.success('TeraRelay Drive locked');
        } catch (error) {
            if (isCurrentDriveCryptoSession(cryptoSession)) {
                toast.error('Could not lock Drive: ' + String(error));
            }
        } finally {
            driveCryptoActionInFlightRef.current = false;
            if (driveSettingsOpenRef.current) setDriveCryptoBusy(false);
        }
    };

    return (
        <AnimatePresence>
            {isOpen && (
                <motion.div
                    initial={{ opacity: 0 }}
                    animate={{ opacity: 1 }}
                    exit={{ opacity: 0 }}
                    className="tr-modal-backdrop fixed inset-0 z-[100] flex items-center justify-center p-4"
                    onClick={onClose}
                >
                    <motion.div
                        layout
                        initial={{ opacity: 0, scale: 0.95, y: 10 }}
                        animate={{ opacity: 1, scale: 1, y: 0 }}
                        exit={{ opacity: 0, scale: 0.95, y: 10 }}
                        transition={{ type: 'spring', damping: 25, stiffness: 220 }}
                        ref={dialogRef}
                        tabIndex={-1}
                        onKeyDown={trapDialogTab}
                        className="tr-modal tr-settings-modal w-[700px] max-w-[calc(100vw-2rem)] overflow-hidden flex flex-col"
                        role="dialog"
                        aria-modal="true"
                        aria-labelledby="tr-settings-dialog-title"
                        onClick={e => e.stopPropagation()}
                    >
                        {/* Header */}
                        <div className="tr-modal-header px-5 py-4 flex justify-between items-center">
                            <h2 id="tr-settings-dialog-title" className="text-telegram-text font-semibold text-base">{t('settings.title')}</h2>
                            <button
                                ref={closeButtonRef}
                                type="button"
                                onClick={onClose}
                                aria-label={t('common.close')}
                                className="tr-modal-close"
                            >
                                <X className="w-4 h-4" />
                            </button>
                        </div>

                        {/* Tab Bar */}
                        <div className="tr-settings-tabs px-4 pt-3 pb-0 grid grid-cols-5 gap-1">
                            {([['general', Globe], ['themes', Palette], ['proxy', Shield], ['sharing', Link], ['about', Info]] as const).map(([key, Icon]) => (
                                <button
                                    key={key}
                                    onClick={() => setActiveTab(key as SettingsTab)}
                                    className={`tr-settings-tab flex min-w-0 items-center justify-center gap-1.5 px-2 py-2 text-xs font-medium ${
                                        activeTab === key
                                            ? 'tr-settings-tab--active text-telegram-primary'
                                            : 'text-telegram-subtext'
                                    }`}
                                >
                                    <Icon className="w-3.5 h-3.5" />
                                    {t(`settings.tab_${key}`)}
                                </button>
                            ))}
                        </div>

                        {/* Body */}
                        <motion.div layout className="tr-settings-body px-5 py-4 max-h-[72vh] overflow-y-auto overflow-x-hidden relative">
                            <AnimatePresence mode="popLayout" initial={false}>

                                {activeTab === 'general' && (
                                    <motion.div
                                        key="general"
                                        initial={{ opacity: 0, x: -20 }}
                                        animate={{ opacity: 1, x: 0 }}
                                        exit={{ opacity: 0, x: 20 }}
                                        transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
                                        className="space-y-6 w-full"
                                    >

                            {linuxAutostartSupported && (
                                <section className="space-y-3">
                                    <h3 className="tr-setting-section-title flex items-center gap-2">
                                        <HardDrive className="w-3.5 h-3.5" />
                                        Drive background startup
                                    </h3>
                                    <div className="tr-setting-row flex items-center justify-between gap-4 p-3">
                                        <div className="min-w-0">
                                            <p className="text-sm text-telegram-text font-medium">Start TeraRelay Drive when I sign in</p>
                                            <p className="text-xs text-telegram-subtext">
                                                Starts hidden in the tray after Linux login. Closing the window keeps Drive mounted;
                                                Quit TeraRelay unmounts it. Open TeraRelay normally to return.
                                            </p>
                                            {linuxAutostartError && (
                                                <p className="mt-1 text-xs text-red-400" role="alert">
                                                    {linuxAutostartError}
                                                </p>
                                            )}
                                        </div>
                                        <button
                                            type="button"
                                            onClick={() => { void toggleLinuxAutostart(); }}
                                            disabled={linuxAutostartEnabled === null || linuxAutostartBusy}
                                            aria-label="Start TeraRelay Drive when I sign in"
                                            aria-pressed={linuxAutostartEnabled === true}
                                            className={`relative w-11 h-6 shrink-0 rounded-full transition-colors duration-200 disabled:opacity-50 ${linuxAutostartEnabled ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                        >
                                            <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${linuxAutostartEnabled ? 'translate-x-5' : 'translate-x-0'}`} />
                                        </button>
                                    </div>
                                </section>
                            )}

                            {/* Transfers Section */}
                            <section className="space-y-3">
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <Upload className="w-3.5 h-3.5" />
                                    {t('settings.transfers')}
                                </h3>

                                {/* Max Concurrent Uploads */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Upload className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.concurrent_uploads')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.max_uploads_desc')}</p>
                                        </div>
                                    </div>
                                    <div className="flex items-center gap-2">
                                        <button
                                            onClick={() => updateSetting('maxConcurrentUploads', Math.max(1, settings.maxConcurrentUploads - 1))}
                                            aria-label={`${t('settings.concurrent_uploads')} -`}
                                            className="w-7 h-7 flex items-center justify-center rounded-md bg-telegram-bg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-border transition text-sm font-medium"
                                        >
                                            -
                                        </button>
                                        <span className="text-sm text-telegram-text font-medium w-5 text-center">
                                            {settings.maxConcurrentUploads}
                                        </span>
                                        <button
                                            onClick={() => updateSetting('maxConcurrentUploads', Math.min(10, settings.maxConcurrentUploads + 1))}
                                            aria-label={`${t('settings.concurrent_uploads')} +`}
                                            className="w-7 h-7 flex items-center justify-center rounded-md bg-telegram-bg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-border transition text-sm font-medium"
                                        >
                                            +
                                        </button>
                                    </div>
                                </div>

                                {/* Max Concurrent Downloads */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Download className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.concurrent_downloads')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.max_downloads_desc')}</p>
                                        </div>
                                    </div>
                                    <div className="flex items-center gap-2">
                                        <button
                                            onClick={() => updateSetting('maxConcurrentDownloads', Math.max(1, settings.maxConcurrentDownloads - 1))}
                                            aria-label={`${t('settings.concurrent_downloads')} -`}
                                            className="w-7 h-7 flex items-center justify-center rounded-md bg-telegram-bg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-border transition text-sm font-medium"
                                        >
                                            -
                                        </button>
                                        <span className="text-sm text-telegram-text font-medium w-5 text-center">
                                            {settings.maxConcurrentDownloads}
                                        </span>
                                        <button
                                            onClick={() => updateSetting('maxConcurrentDownloads', Math.min(10, settings.maxConcurrentDownloads + 1))}
                                            aria-label={`${t('settings.concurrent_downloads')} +`}
                                            className="w-7 h-7 flex items-center justify-center rounded-md bg-telegram-bg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-border transition text-sm font-medium"
                                        >
                                            +
                                        </button>
                                    </div>
                                </div>

                            {SHOW_ADVANCED_SETTINGS && (<>
                                {/* Zip Folders */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <FolderArchive className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.zip_before_upload')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.zip_folders_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => updateSetting('zipFolders', !settings.zipFolders)}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.zipFolders ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.zipFolders ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                            </>)}

                                {/* Hide Folder Groups */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Tag className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('common.hide_groups')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('common.hide_groups_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => updateSetting('hideGroups', !settings.hideGroups)}
                                        aria-label={t('common.hide_groups')}
                                        aria-pressed={settings.hideGroups}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.hideGroups ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.hideGroups ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                            {SHOW_ADVANCED_SETTINGS && (<>
                                {/* Performance Mode */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Zap className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.performance_mode')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.performance_mode_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => updateSetting('performanceMode', !settings.performanceMode)}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.performanceMode ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.performanceMode ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                            </>)}

                            {SHOW_ADVANCED_SETTINGS && (<>
                                {/* Linux Rendering Fix */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Monitor className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.linux_rendering_fix')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.linux_rendering_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => {
                                            updateSetting('linuxRenderingFix', !settings.linuxRenderingFix);
                                            toast.info(t('settings.restart_app_toast'), { duration: 5000 });
                                        }}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.linuxRenderingFix ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.linuxRenderingFix ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>
                            </>)}
                            </section>

                            {/* Language & Region Section */}
                            <section className="space-y-3">
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <Languages className="w-3.5 h-3.5" />
                                    {t('settings.language_region')}
                                </h3>

                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Globe className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.app_language')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.choose_language')}</p>
                                        </div>
                                    </div>
                                    <div className="relative">
                                        <select
                                            aria-label={t('settings.app_language')}
                                            value={settings.language}
                                            onChange={e => updateSetting('language', e.target.value as any)}
                                            className="tr-modal-input appearance-none pl-3 pr-8 py-1.5 text-sm text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                                        >
                                            {LANGUAGES.map(lang => (
                                                <option key={lang.code} value={lang.code}>
                                                    {lang.nativeLabel}
                                                </option>
                                            ))}
                                        </select>
                                        <ChevronDown className="w-4 h-4 text-telegram-subtext absolute right-2.5 top-1/2 -translate-y-1/2 pointer-events-none" />
                                    </div>
                                </div>
                            </section>

                            {SHOW_ADVANCED_SETTINGS && (<>
                            {/* REST API Section */}
                            <section className="space-y-3">
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <Globe className="w-3.5 h-3.5" />
                                    {t('settings.rest_api')}
                                </h3>

                                {/* Enable Toggle */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <div className={`w-2 h-2 rounded-full ${apiSettings.running ? 'bg-green-400 shadow-[0_0_6px_rgba(74,222,128,0.5)]' : 'bg-gray-500'}`} />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.enable_api_server')}</p>
                                            <p className="text-xs text-telegram-subtext">
                                                {apiSettings.running ? t('settings.api_running', { port: apiSettings.port }) : t('settings.api_stopped')}
                                            </p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={handleApiToggle}
                                        disabled={apiLoading}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${apiSettings.enabled ? 'bg-telegram-primary' : 'bg-telegram-border'} disabled:opacity-50`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${apiSettings.enabled ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                                {/* Port */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.port')}</p>
                                        <p className="text-xs text-telegram-subtext">1024 - 65535</p>
                                    </div>
                                    <div className="flex items-center gap-2">
                                        <input
                                            type="number"
                                            min="1024"
                                            max="65535"
                                            value={apiPort}
                                            onChange={e => setApiPort(e.target.value)}
                                            onBlur={handlePortApply}
                                            onKeyDown={e => { if (e.key === 'Enter') handlePortApply(); }}
                                            className="w-20 bg-telegram-bg border border-telegram-border rounded-md px-2 py-1 text-sm text-telegram-text text-center focus:outline-none focus:border-telegram-primary/50 transition"
                                        />
                                    </div>
                                </div>

                                {/* API Key */}
                                <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2.5">
                                    <div className="flex items-center justify-between">
                                        <div className="flex items-center gap-2">
                                            <Key className="w-4 h-4 text-telegram-subtext" />
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.api_key')}</p>
                                                <p className="text-xs text-telegram-subtext">
                                                    {apiSettings.key_set ? t('settings.api_key_configured') : t('settings.api_key_unset')}
                                                </p>
                                            </div>
                                        </div>
                                        <button
                                            onClick={handleGenerateKey}
                                            className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary/10 text-telegram-primary hover:bg-telegram-primary/20 transition"
                                        >
                                            <RefreshCw className="w-3 h-3" />
                                            {apiSettings.key_set ? t('settings.regenerate') : t('settings.generate')}
                                        </button>
                                    </div>

                                    {/* One-time key reveal */}
                                    {generatedAccessValue && (
                                        <div className="mt-2 p-2.5 bg-telegram-bg rounded-lg border border-yellow-500/20">
                                            <p className="text-[10px] text-yellow-400/80 uppercase tracking-wider font-semibold mb-1.5">
                                                {t('settings.api_copy_alert')}
                                            </p>
                                            <div className="flex items-center gap-2">
                                                <code className="flex-1 text-xs text-telegram-text font-mono bg-telegram-hover rounded px-2 py-1.5 overflow-x-auto select-all">
                                                    {generatedAccessValue}
                                                </code>
                                                <button
                                                    onClick={handleCopyKey}
                                                    className="p-1.5 rounded-md hover:bg-telegram-hover text-telegram-subtext hover:text-telegram-text transition flex-shrink-0"
                                                    title="Copy to clipboard"
                                                >
                                                    {keyCopied ? <Check className="w-4 h-4 text-green-400" /> : <Copy className="w-4 h-4" />}
                                                </button>
                                            </div>
                                        </div>
                                    )}
                                </div>
                            </section>

                            </>)}

                            {/* Storage Section */}
                            <section className="space-y-3">
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <HardDrive className="w-3.5 h-3.5" />
                                    {t('settings.storage')}
                                </h3>

                                <div className="tr-setting-row p-3 space-y-3">
                                    <div className="flex items-start justify-between gap-4">
                                        <div className="flex items-start gap-2.5 min-w-0">
                                            <Shield className="w-4 h-4 text-telegram-subtext mt-0.5 flex-shrink-0" />
                                            <div className="min-w-0">
                                                <p className="text-sm text-telegram-text font-medium">TeraRelay Drive encryption</p>
                                                <p className="text-xs text-telegram-subtext mt-0.5 leading-relaxed">
                                                    Files written after encryption is enabled are protected before Telegram upload. The passphrase is never stored.
                                                </p>
                                            </div>
                                        </div>
                                        <div className="text-xs flex-shrink-0 pt-0.5">
                                            {driveCryptoStatus === null ? (
                                                <span className="text-telegram-subtext">Unavailable</span>
                                            ) : driveCryptoStatus.configured ? (
                                                <span className={driveCryptoStatus.unlocked ? 'text-emerald-400' : 'text-amber-400'}>
                                                    {driveCryptoStatus.unlocked ? 'Unlocked' : 'Locked'}
                                                </span>
                                            ) : (
                                                <span className="text-telegram-subtext">Not enabled</span>
                                            )}
                                        </div>
                                    </div>

                                    {driveCryptoStatus && !driveCryptoStatus.configured && (
                                        <div className="space-y-2.5 pl-6">
                                            <div className="grid grid-cols-1 md:grid-cols-2 gap-2">
                                                <input
                                                    type="password"
                                                    value={drivePassphrase}
                                                    onChange={e => setDrivePassphrase(e.target.value)}
                                                    placeholder="Passphrase (12+ characters)"
                                                    autoComplete="new-password"
                                                    className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                                />
                                                <input
                                                    type="password"
                                                    value={driveConfirmPassphrase}
                                                    onChange={e => setDriveConfirmPassphrase(e.target.value)}
                                                    placeholder="Confirm passphrase"
                                                    autoComplete="new-password"
                                                    onKeyDown={e => { if (e.key === 'Enter') void handleDriveCryptoSetup(); }}
                                                    className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                                />
                                            </div>
                                            <div className="flex items-center justify-between gap-3">
                                                <p className="text-[11px] text-telegram-subtext">
                                                    Existing unencrypted Drive files stay readable; new and edited files use encryption.
                                                </p>
                                                <button
                                                    onClick={() => void handleDriveCryptoSetup()}
                                                    disabled={driveCryptoBusy}
                                                    className="px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary text-white hover:opacity-90 transition disabled:opacity-50 flex items-center gap-1.5 flex-shrink-0"
                                                >
                                                    {driveCryptoBusy && <Loader2 className="w-3 h-3 animate-spin" />}
                                                    Enable
                                                </button>
                                            </div>
                                        </div>
                                    )}

                                    {driveCryptoStatus?.configured && !driveCryptoStatus.unlocked && (
                                        <div className="flex items-center gap-2 pl-6">
                                            <input
                                                type="password"
                                                value={drivePassphrase}
                                                onChange={e => setDrivePassphrase(e.target.value)}
                                                placeholder="Drive passphrase"
                                                autoComplete="current-password"
                                                onKeyDown={e => { if (e.key === 'Enter') void handleDriveCryptoUnlock(); }}
                                                className="flex-1 bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                            />
                                            <button
                                                onClick={() => void handleDriveCryptoUnlock()}
                                                disabled={driveCryptoBusy}
                                                className="px-3 py-2 rounded-lg text-xs font-medium bg-telegram-primary text-white hover:opacity-90 transition disabled:opacity-50 flex items-center gap-1.5"
                                            >
                                                {driveCryptoBusy && <Loader2 className="w-3 h-3 animate-spin" />}
                                                Unlock
                                            </button>
                                        </div>
                                    )}

                                    {driveCryptoStatus?.configured && driveCryptoStatus.unlocked && (
                                        <div className="space-y-2.5 pl-6">
                                            <div className="grid grid-cols-1 md:grid-cols-3 gap-2">
                                                <input
                                                    type="password"
                                                    value={driveCurrentPassphrase}
                                                    onChange={e => setDriveCurrentPassphrase(e.target.value)}
                                                    placeholder="Current passphrase"
                                                    autoComplete="current-password"
                                                    className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                                />
                                                <input
                                                    type="password"
                                                    value={driveNewPassphrase}
                                                    onChange={e => setDriveNewPassphrase(e.target.value)}
                                                    placeholder="New passphrase"
                                                    autoComplete="new-password"
                                                    className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                                />
                                                <input
                                                    type="password"
                                                    value={driveConfirmPassphrase}
                                                    onChange={e => setDriveConfirmPassphrase(e.target.value)}
                                                    placeholder="Confirm new passphrase"
                                                    autoComplete="new-password"
                                                    onKeyDown={e => { if (e.key === 'Enter') void handleDriveCryptoChangePassphrase(); }}
                                                    className="bg-telegram-bg border border-telegram-border rounded-lg px-3 py-2 text-xs text-telegram-text placeholder:text-telegram-subtext/70 focus:outline-none focus:border-telegram-primary/50"
                                                />
                                            </div>
                                            <div className="flex items-center justify-end gap-2">
                                                <button
                                                    onClick={() => void handleDriveCryptoLock()}
                                                    disabled={driveCryptoBusy}
                                                    className="px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-hover text-telegram-text hover:bg-telegram-border/60 transition disabled:opacity-50"
                                                >
                                                    Lock now
                                                </button>
                                                <button
                                                    onClick={() => void handleDriveCryptoChangePassphrase()}
                                                    disabled={driveCryptoBusy}
                                                    className="px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary/10 text-telegram-primary hover:bg-telegram-primary/20 transition disabled:opacity-50 flex items-center gap-1.5"
                                                >
                                                    {driveCryptoBusy && <Loader2 className="w-3 h-3 animate-spin" />}
                                                    Change passphrase
                                                </button>
                                            </div>
                                        </div>
                                    )}
                                </div>

                                <div className="tr-setting-row p-3 space-y-2">
                                    <div className="flex flex-wrap items-center justify-between gap-3">
                                        <div className="flex items-center gap-2.5 min-w-0">
                                            <RefreshCw className="w-4 h-4 text-telegram-subtext flex-shrink-0" />
                                            <div className="min-w-0">
                                                <p className="text-sm text-telegram-text font-medium">TeraRelay Drive · Unfinished writes</p>
                                                <p className="text-xs text-telegram-subtext">
                                                    Manually retry completed staging writes that did not finish recovering at startup.
                                                </p>
                                            </div>
                                        </div>
                                        <button
                                            type="button"
                                            aria-label="Retry pending TeraRelay Drive writes"
                                            disabled={drivePendingRecoveryBusy}
                                            onClick={() => { void recoverPendingDriveWrites(); }}
                                            className="inline-flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-xs font-medium bg-telegram-hover text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50"
                                        >
                                            {drivePendingRecoveryBusy && <Loader2 className="w-3 h-3 animate-spin" aria-hidden="true" />}
                                            {drivePendingRecoveryBusy ? 'Recovering…' : 'Retry unfinished writes'}
                                        </button>
                                    </div>
                                    {drivePendingRecoveryResult && (
                                        <p className="rounded-md bg-telegram-hover/40 px-3 py-2 text-xs text-telegram-text" role="status">
                                            Recovery checked: {drivePendingRecoveryResult.recovered} completed,
                                            {' '}{drivePendingRecoveryResult.remaining} still pending,
                                            {' '}{drivePendingRecoveryResult.failures} failed.
                                            {' '}This does not independently verify Telegram metadata synchronization.
                                        </p>
                                    )}
                                    {drivePendingRecoveryError && (
                                        <p className="rounded-md border border-red-500/30 p-2 text-xs text-telegram-text" role="alert">
                                            {drivePendingRecoveryError}
                                        </p>
                                    )}
                                </div>

                                <DriveBulkOrganizer active={isOpen && activeTab === 'general'} />
                                <DriveBatchRestore active={isOpen && activeTab === 'general'} />
                                <DriveBatchTrash active={isOpen && activeTab === 'general'} />
                                <DriveFolderOperations active={isOpen && activeTab === 'general'} />
                                <DriveFolderUndo active={isOpen && activeTab === 'general'} />
                                <DriveFolderClone active={isOpen && activeTab === 'general'} />
                                <DriveFolderBlueprint active={isOpen && activeTab === 'general'} />
                                <DriveFileCopy active={isOpen && activeTab === 'general'} />
                                <DriveFolderCompleteCopy active={isOpen && activeTab === 'general'} />
                                <DriveFolderMirror active={isOpen && activeTab === 'general'} />
                                <DriveFolderTreeTrash active={isOpen && activeTab === 'general'} />
                                <DriveTreeReceipt active={isOpen && activeTab === 'general'} />
                                <DriveBulkUndo active={isOpen && activeTab === 'general'} />
                                <DriveDuplicateExplorer active={isOpen && activeTab === 'general'} />
                                <DriveCollections active={isOpen && activeTab === 'general'} />
                                <DriveSmartViews active={isOpen && activeTab === 'general'} />
                                <DriveChangeReviewPanel active={isOpen && activeTab === 'general'} />
                                <DriveHealthDoctor active={isOpen && activeTab === 'general'} />
                                <DriveTransferCenter active={isOpen && activeTab === 'general'} />
                                <DrivePendingRescue active={isOpen && activeTab === 'general'} />
                                <DriveBulkExport active={isOpen && activeTab === 'general'} />
                                <DriveIntegrityLedger active={isOpen && activeTab === 'general'} />
                                <DriveNamespaceAudit active={isOpen && activeTab === 'general'} />
                                <DriveTimeCapsule active={isOpen && activeTab === 'general'} />
                                <DrivePortableManifest active={isOpen && activeTab === 'general'} />
                                <DriveFolderParity active={isOpen && activeTab === 'general'} />
                                <DriveBackupWatchlist active={isOpen && activeTab === 'general'} />
                                <DriveBackupTimeline active={isOpen && activeTab === 'general'} />
                                <DriveBackupBatchRepair active={isOpen && activeTab === 'general'} />
                                <DriveSafeMode active={isOpen && activeTab === 'general'} />

                                <div className="tr-setting-row p-3 space-y-3">
                                    <div className="flex flex-wrap items-center justify-between gap-3">
                                        <div className="flex items-center gap-2.5 min-w-0">
                                            <Search className="w-4 h-4 text-telegram-subtext flex-shrink-0" />
                                            <div className="min-w-0">
                                                <p className="text-sm text-telegram-text font-medium">TeraRelay Drive · Find files</p>
                                                <p className="text-xs text-telegram-subtext">Search local Drive metadata, including unfinished writes, without downloading file contents.</p>
                                            </div>
                                        </div>
                                        <button type="button" aria-label="Open local Drive file finder"
                                            aria-expanded={driveFinderOpen}
                                            onClick={() => {
                                                if (driveFinderOpen) {
                                                    ++driveFinderRequestRef.current;
                                                    driveFinderFetchInFlightRef.current = false;
                                                    driveFinderCopyInFlightRef.current = false;
                                                    driveFinderCsvCopyInFlightRef.current = false;
                                                    driveFinderCsvCopyGenerationRef.current++;
                                                    setDriveFinderBusy(false);
                                                    setDriveFinderCopyBusy(false);
                                                    setDriveFinderCsvCopyBusy(false);
                                                    setDriveFinderOpen(false);
                                                } else {
                                                    setDriveFinderEntries([]);
                                                    setDriveFinderOpen(true);
                                                    void refreshLocalDriveFinder();
                                                }
                                            }}
                                            className="rounded-lg bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60"
                                        >{driveFinderOpen ? 'Hide finder' : 'Find files'}</button>
                                    </div>
                                    {driveFinderOpen && (
                                        <div className="space-y-2 pl-6">
                                            <nav aria-label="Current local Drive folder" className="flex flex-wrap items-center gap-2 text-xs">
                                                <span className="min-w-0 break-all font-medium text-telegram-text">
                                                    {driveFinderFolderScope || '/ (All Drive)'}
                                                </span>
                                                {driveFinderFolderScope && (
                                                    <>
                                                        <button type="button" aria-label="Go up one Drive folder"
                                                            onClick={() => {
                                                                setDriveFinderFolderScope(parentDriveFinderFolder(driveFinderFolderScope));
                                                                setDriveFinderVisibleLimit(50);
                                                            }}
                                                            className="rounded-md bg-telegram-hover px-2 py-1 text-telegram-primary hover:underline">
                                                            Up
                                                        </button>
                                                        <button type="button" aria-label="Show all Drive folders"
                                                            onClick={() => { setDriveFinderFolderScope(''); setDriveFinderVisibleLimit(50); }}
                                                            className="rounded-md bg-telegram-hover px-2 py-1 text-telegram-primary hover:underline">
                                                            All Drive
                                                        </button>
                                                    </>
                                                )}
                                            </nav>
                                            <div className="flex flex-wrap items-center gap-2">
                                                <input type="search" aria-label="Search local Drive filenames or folder paths"
                                                    placeholder="Find by filename or folder…" value={driveFinderSearch}
                                                    onChange={event => { setDriveFinderSearch(event.target.value); setDriveFinderVisibleLimit(50); }}
                                                    className="min-w-[160px] flex-1 rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text" />
                                                <select aria-label="Filter local Drive finder by type" value={driveFinderKind}
                                                    onChange={event => { setDriveFinderKind(event.target.value as DriveFinderKind); setDriveFinderVisibleLimit(50); }}
                                                    className="rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text">
                                                    <option value="all">All types</option>
                                                    <option value="file">Files</option>
                                                    <option value="folder">Folders</option>
                                                    <option value="pending">Unfinished writes</option>
                                                </select>
                                                <select aria-label="Sort local Drive search results" value={driveFinderSort}
                                                    onChange={event => {
                                                        setDriveFinderSort(event.target.value as DriveFinderSortOrder);
                                                        setDriveFinderVisibleLimit(50);
                                                    }}
                                                    className="rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text">
                                                    <option value="path">Path A-Z</option>
                                                    <option value="largest">Largest first</option>
                                                    <option value="smallest">Smallest first</option>
                                                </select>
                                                <select aria-label="Filter local Drive finder by file size" value={driveFinderSizeFilter}
                                                    onChange={event => {
                                                        setDriveFinderSizeFilter(event.target.value as DriveFinderSizeFilter);
                                                        setDriveFinderVisibleLimit(50);
                                                    }}
                                                    className="rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text">
                                                    <option value="all">All sizes</option>
                                                    <option value="10m">10 MiB or larger</option>
                                                    <option value="100m">100 MiB or larger</option>
                                                    <option value="1g">1 GiB or larger</option>
                                                    <option value="empty">Empty files (0 bytes)</option>
                                                </select>
                                                <button type="button" aria-label="Refresh local Drive file finder" disabled={driveFinderBusy}
                                                    onClick={() => { void refreshLocalDriveFinder(); }}
                                                    className="rounded-md bg-telegram-hover px-2 py-1.5 text-xs text-telegram-text disabled:opacity-50"
                                                >{driveFinderBusy ? 'Reading…' : 'Refresh'}</button>
                                                <button type="button" aria-label="Copy filtered local Drive inventory as CSV"
                                                    title="Copy up to 1,000 matching Drive paths, types and known sizes as CSV (no IDs or file contents)"
                                                    disabled={driveFinderBusy || driveFinderError !== null || driveFinderMatches.length === 0 || driveFinderCsvCopyBusy}
                                                    onClick={() => { void copyLocalDriveInventoryCsv(); }}
                                                    className="rounded-md bg-telegram-primary/15 px-2 py-1.5 text-xs font-semibold text-telegram-primary disabled:opacity-50"
                                                >{driveFinderCsvCopyBusy ? 'Copying CSV…' : 'Copy CSV'}</button>
                                            </div>
                                            {driveFinderError && <p role="alert" className="rounded-md border border-red-500/30 p-2 text-xs text-red-400">{driveFinderError}</p>}
                                            <p role="status" className="text-xs text-telegram-subtext">
                                                {driveFinderShown.length} of {driveFinderMatches.length} matches ({driveFinderEntries.length} locally indexed)
                                                {driveFinderSkipped > 0 ? ` · ${driveFinderSkipped} unverified entries skipped` : ''}
                                                {driveFinderTruncated > 0 ? ` · ${driveFinderTruncated} entries beyond the 25,000-item safety cap not indexed` : ''}
                                            </p>
                                            {driveFinderEntries.length > 0 && (
                                                <section aria-label="Local Drive storage insights" className="space-y-2 rounded-md border border-telegram-border p-2.5">
                                                    <p className="text-xs font-semibold text-telegram-text">Storage overview · all locally indexed entries</p>
                                                    <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-telegram-subtext">
                                                        <span>{driveFinderInsights.files} files</span>
                                                        <span>{driveFinderInsights.folders} folders</span>
                                                        <span>{driveFinderInsights.pendingWrites} unfinished writes</span>
                                                    </div>
                                                    <div className="grid grid-cols-1 gap-1 text-xs text-telegram-text sm:grid-cols-2">
                                                        <p>Known logical listed file bytes: <strong>{formatDriveTrashBytes(driveFinderInsights.knownFileBytes)}</strong></p>
                                                        <p>Known unfinished write bytes: <strong>{formatDriveTrashBytes(driveFinderInsights.knownPendingBytes)}</strong></p>
                                                    </div>
                                                    {(driveFinderInsights.unknownFileSizes > 0 || driveFinderInsights.unknownPendingSizes > 0) && (
                                                        <p className="text-xs text-amber-400">
                                                            Unknown sizes: {driveFinderInsights.unknownFileSizes} files, {driveFinderInsights.unknownPendingSizes} unfinished writes (excluded from totals).
                                                        </p>
                                                    )}
                                                    {driveFinderInsights.largestFolders.length > 0 && (
                                                        <div className="space-y-1">
                                                            <p className="text-xs font-semibold text-telegram-text">Largest folder trees by known descendant file sizes</p>
                                                            <ol aria-label="Largest Drive folder trees" className="space-y-1">
                                                                {driveFinderInsights.largestFolders.map(folder => (
                                                                    <li key={folder.path} className="flex flex-wrap justify-between gap-x-2 text-xs text-telegram-subtext">
                                                                        <span className="min-w-0 break-all">{folder.path}</span>
                                                                        <span>{formatDriveTrashBytes(folder.knownBytes)} · {folder.knownFiles} files
                                                                            {folder.unknownFiles > 0 ? ` · ${folder.unknownFiles} sizes unknown` : ''}
                                                                        </span>
                                                                    </li>
                                                                ))}
                                                            </ol>
                                                        </div>
                                                    )}
                                                    <p className="text-[11px] text-telegram-subtext">
                                                        Logical listed file bytes are not disk usage, physical storage or cloud quota. Hardlinks may appear more than once;
                                                        nested folder totals overlap. Data is from the local indexed subset only.
                                                    </p>
                                                </section>
                                            )}
                                            <ul aria-label="Local Drive search results" className="max-h-72 space-y-1.5 overflow-y-auto">
                                                {driveFinderShown.map(entry => (
                                                    <li key={entry.kind + ':' + entry.id} className="rounded-md bg-telegram-hover/40 px-2 py-1.5">
                                                        <p className="truncate text-xs font-medium text-telegram-text" title={entry.name}>{entry.name}</p>
                                                        <div className="flex flex-wrap items-center justify-between gap-2">
                                                            <p className="min-w-0 break-all text-[11px] text-telegram-subtext">{entry.path}</p>
                                                            <div className="flex shrink-0 flex-wrap items-center gap-2">
                                                                {entry.kind === 'folder' && (
                                                                    <button type="button" aria-label={`Browse this Drive folder: ${entry.name}`}
                                                                        onClick={() => {
                                                                            setDriveFinderFolderScope(entry.path);
                                                                            setDriveFinderSearch('');
                                                                            setDriveFinderKind('all');
                                                                            setDriveFinderSizeFilter('all');
                                                                            setDriveFinderVisibleLimit(50);
                                                                        }}
                                                                        className="text-[11px] font-semibold text-telegram-primary hover:underline">
                                                                        Browse
                                                                    </button>
                                                                )}
                                                                <button type="button" aria-label={`Copy Drive-relative path for ${entry.name}`}
                                                                    disabled={driveFinderCopyBusy}
                                                                    onClick={() => { void copyLocalDriveRelativePath(entry.path); }}
                                                                    className="text-[11px] font-medium text-telegram-primary hover:underline disabled:opacity-50">
                                                                    Copy path
                                                                </button>
                                                            </div>
                                                        </div>
                                                        <p className="text-[11px] text-telegram-subtext">{entry.kind === 'pending' ? 'Unfinished write' : entry.kind === 'folder' ? 'Folder' : 'File'}
                                                            {entry.kind !== 'folder' ? ` · ${formatDriveTrashBytes(entry.size)}` : ''}
                                                        </p>
                                                    </li>
                                                ))}
                                            </ul>
                                            {driveFinderShown.length < driveFinderMatches.length && (
                                                <button type="button" aria-label="Show more local Drive search results"
                                                    onClick={() => setDriveFinderVisibleLimit(current => current + 50)}
                                                    className="text-xs font-semibold text-telegram-primary hover:underline">
                                                    Show next {Math.min(50, driveFinderMatches.length - driveFinderShown.length)} results
                                                </button>
                                            )}
                                            {driveFinderEntries.length === 0 && !driveFinderBusy && !driveFinderError &&
                                                <p className="text-xs text-telegram-subtext">No active local Drive entries found.</p>}
                                        </div>
                                    )}
                                </div>

                                <div className="tr-setting-row p-3 space-y-2">
                                    <div className="flex items-center justify-between gap-3">
                                        <div className="flex items-center gap-2.5 min-w-0">
                                            <RotateCcw className="w-4 h-4 text-telegram-subtext flex-shrink-0" />
                                            <div className="min-w-0">
                                                <p className="text-sm text-telegram-text font-medium">TeraRelay Drive · Recently deleted</p>
                                                <p className="text-xs text-telegram-subtext">
                                                    Restore files removed from the Drive without deleting their Telegram content.
                                                </p>
                                            </div>
                                        </div>
                                        <button
                                            onClick={() => setDriveTrashOpen(value => !value)}
                                            className="px-3 py-1.5 rounded-lg text-xs bg-telegram-hover text-telegram-text hover:bg-telegram-border/60 transition flex-shrink-0"
                                            aria-expanded={driveTrashOpen}
                                        >
                                            {driveTrashOpen ? 'Hide' : 'View'}
                                        </button>
                                    </div>
                                    {driveTrashOpen && (
                                        <div className="pl-6 space-y-2">
                                            {driveRestoreSyncProblem && (
                                                <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-amber-500/30 p-2 text-xs text-telegram-text" role="status">
                                                    <span>
                                                        {driveRestoreSyncProblem === 'qa-local'
                                                            ? 'Restored Drive metadata is local (QA mode); Telegram has not been updated.'
                                                            : driveRestoreSyncProblem === 'pending'
                                                                ? 'Restored Drive metadata still has pending changes.'
                                                                : driveRestoreSyncProblem === 'offline'
                                                                    ? 'Restored locally; Telegram metadata sync is unavailable.'
                                                                    : 'Restored locally; remote metadata sync is not verified.'}
                                                    </span>
                                                    <button
                                                        type="button"
                                                        aria-label="Retry Drive metadata sync after restore"
                                                        disabled={driveRestoreSyncBusy || restoringDriveItem !== null}
                                                        onClick={() => { void retryDriveRestoreMetadataSync(); }}
                                                        className="font-semibold text-telegram-primary hover:underline disabled:opacity-50"
                                                    >
                                                        {driveRestoreSyncBusy ? 'Syncing…' : 'Retry sync'}
                                                    </button>
                                                </div>
                                            )}
                                            <div className="flex justify-end">
                                                <button
                                                    onClick={() => void fetchDriveTrash()}
                                                    disabled={driveTrashBusy || restoringDriveItem !== null || driveRestoreSyncBusy}
                                                    className="inline-flex items-center gap-1 text-xs text-telegram-subtext hover:text-telegram-text disabled:opacity-50"
                                                >
                                                    <RefreshCw className={`w-3 h-3 ${driveTrashBusy ? 'animate-spin' : ''}`} />
                                                    Refresh
                                                </button>
                                            </div>
                                            <div className="flex flex-wrap items-center gap-2">
                                                <input
                                                    type="search"
                                                    aria-label="Search recently deleted Drive items"
                                                    placeholder="Find deleted items…"
                                                    value={driveTrashSearch}
                                                    onChange={event => {
                                                        setDriveTrashSearch(event.target.value);
                                                        setDriveTrashVisibleLimit(50);
                                                    }}
                                                    className="min-w-0 flex-1 rounded-md bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text border border-telegram-border"
                                                />
                                                <select
                                                    aria-label="Filter recently deleted Drive items by type"
                                                    value={driveTrashFilter}
                                                    onChange={event => {
                                                        setDriveTrashFilter(event.target.value as DriveTrashFilterKind);
                                                        setDriveTrashVisibleLimit(50);
                                                    }}
                                                    className="rounded-md bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text border border-telegram-border"
                                                >
                                                    <option value="all">All types</option>
                                                    <option value="file">Files</option>
                                                    <option value="folder">Folders</option>
                                                    <option value="symlink">Symlinks</option>
                                                </select>
                                                <select
                                                    aria-label="Filter recently deleted Drive items by availability"
                                                    value={driveTrashAvailability}
                                                    onChange={event => {
                                                        setDriveTrashAvailability(event.target.value as DriveTrashAvailability);
                                                        setDriveTrashVisibleLimit(50);
                                                    }}
                                                    className="rounded-md bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text border border-telegram-border"
                                                >
                                                    <option value="all">Any availability</option>
                                                    <option value="restorable">Can restore</option>
                                                    <option value="unavailable">Unavailable</option>
                                                </select>
                                                <select
                                                    aria-label="Filter recently deleted Drive items by age"
                                                    value={driveTrashAge}
                                                    onChange={event => {
                                                        setDriveTrashAge(event.target.value as DriveTrashAgeRange);
                                                        setDriveTrashVisibleLimit(50);
                                                    }}
                                                    className="rounded-md bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text border border-telegram-border"
                                                >
                                                    <option value="all">Any time</option>
                                                    <option value="24h">Last 24 hours</option>
                                                    <option value="7d">Last 7 days</option>
                                                    <option value="30d">Last 30 days</option>
                                                </select>
                                                <select
                                                    aria-label="Sort recently deleted Drive items"
                                                    value={driveTrashSort}
                                                    onChange={event => {
                                                        setDriveTrashSort(event.target.value as DriveTrashSortOrder);
                                                        setDriveTrashVisibleLimit(50);
                                                    }}
                                                    className="rounded-md bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text border border-telegram-border"
                                                >
                                                    <option value="newest">Newest first</option>
                                                    <option value="oldest">Oldest first</option>
                                                    <option value="name">Name A–Z</option>
                                                    <option value="largest">Largest files first</option>
                                                    <option value="smallest">Smallest files first</option>
                                                </select>
                                            </div>
                                            <div className="flex flex-wrap items-center justify-between gap-2">
                                                <p className="text-[11px] text-telegram-subtext" role="status">
                                                    Showing {shownDriveTrash.length} of {visibleDriveTrash.length} matching deleted items ({driveTrash.length} total)
                                                </p>
                                                <button
                                                    type="button"
                                                    aria-label="Copy filtered recently deleted Drive inventory"
                                                    title="Copy up to 500 matching deleted items without internal IDs or paths"
                                                    onClick={() => { void copyFilteredDriveTrashInventory(); }}
                                                    disabled={visibleDriveTrash.length === 0 || driveTrashBusy || driveTrashError !== null || driveTrashInventoryCopyBusy}
                                                    className="text-xs font-medium text-telegram-primary hover:underline focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary disabled:opacity-50"
                                                >
                                                    {driveTrashInventoryCopyBusy ? 'Copying…' : 'Copy list'}
                                                </button>
                                                {hasActiveDriveTrashFilters && (
                                                    <button
                                                        type="button"
                                                        aria-label="Reset recently deleted Drive filters"
                                                        onClick={resetDriveTrashFilters}
                                                        className="text-xs font-medium text-telegram-primary hover:underline focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-telegram-primary"
                                                    >
                                                        Reset filters
                                                    </button>
                                                )}
                                            </div>
                                            {driveTrash.length > 0 && (
                                                <div className="flex flex-wrap items-center gap-2 rounded-md border border-telegram-border p-2" aria-label="Bulk restore recently deleted Drive items">
                                                    <button
                                                        type="button"
                                                        aria-label="Select shown restorable Drive items"
                                                        onClick={selectShownDriveTrash}
                                                        disabled={shownRestorableDriveTrash.length === 0 || restoringDriveItem !== null || driveRestoreSyncBusy}
                                                        className="text-xs text-telegram-primary hover:underline disabled:opacity-50"
                                                    >
                                                        Select shown
                                                    </button>
                                                    <button
                                                        type="button"
                                                        aria-label="Clear selected deleted Drive items"
                                                        onClick={() => setSelectedDriveTrashKeys([])}
                                                        disabled={selectedDriveTrashKeys.length === 0 || restoringDriveItem !== null}
                                                        className="text-xs text-telegram-subtext hover:underline disabled:opacity-50"
                                                    >
                                                        Clear selection
                                                    </button>
                                                    <span className="text-xs text-telegram-subtext" role="status">
                                                        {selectedRestorableDriveTrash.length} selected (up to 100)
                                                        {selectedDriveTrashSizes.knownFiles > 0
                                                            ? ` · ${formatDriveTrashBytes(selectedDriveTrashSizes.knownBytes)} in known file sizes`
                                                            : ''}
                                                        {selectedDriveTrashSizes.unknownFiles > 0
                                                            ? ` · ${selectedDriveTrashSizes.unknownFiles} file size${selectedDriveTrashSizes.unknownFiles === 1 ? '' : 's'} unknown`
                                                            : ''}
                                                    </span>
                                                    <button
                                                        type="button"
                                                        aria-label="Restore selected deleted Drive items"
                                                        onClick={() => void restoreSelectedDriveTrash()}
                                                        disabled={selectedRestorableDriveTrash.length === 0 || restoringDriveItem !== null
                                                            || driveRestoreSyncBusy || driveTrashBusy || driveTrashError !== null}
                                                        className="ml-auto rounded-md bg-telegram-primary px-2 py-1.5 text-xs font-semibold text-white disabled:opacity-50"
                                                    >
                                                        {restoringDriveItem === 'bulk' ? 'Restoring selected…' : `Restore selected (${selectedRestorableDriveTrash.length})`}
                                                    </button>
                                                    {restoringDriveItem === 'bulk' && (
                                                        <button
                                                            type="button"
                                                            aria-label="Stop bulk Drive restore after current item"
                                                            disabled={driveTrashBatchStopRequested}
                                                            onClick={() => {
                                                                driveTrashBatchStopRequestedRef.current = true;
                                                                setDriveTrashBatchStopRequested(true);
                                                            }}
                                                            className="rounded-md border border-amber-500/40 px-2 py-1.5 text-xs text-telegram-text disabled:opacity-50"
                                                        >
                                                            {driveTrashBatchStopRequested ? 'Stopping…' : 'Stop after current item'}
                                                        </button>
                                                    )}
                                                </div>
                                            )}
                                            {driveTrashBatchProgress && (
                                                <p className="text-xs text-telegram-subtext" role="status">
                                                    Bulk restore progress: {driveTrashBatchProgress.completed} of {driveTrashBatchProgress.total} completed
                                                </p>
                                            )}
                                            {driveTrashBatchError && (
                                                <p className="rounded-md border border-red-500/30 p-2 text-xs text-red-400" role="alert">
                                                    {driveTrashBatchError}
                                                </p>
                                            )}
                                            {driveTrashWarning && (
                                                <p className="rounded-md border border-amber-500/30 p-2 text-xs text-telegram-text" role="alert">
                                                    {driveTrashWarning}
                                                </p>
                                            )}
                                            {driveTrashError && (
                                                <div className="flex flex-wrap items-center gap-2 rounded-md border border-red-500/30 p-2 text-xs text-red-400" role="alert">
                                                    <span>{driveTrashError}</span>
                                                    <button type="button" onClick={() => void fetchDriveTrash()} className="font-semibold underline" disabled={driveTrashBusy}>
                                                        Retry
                                                    </button>
                                                </div>
                                            )}
                                            {!driveTrashBusy && !driveTrashError && driveTrash.length === 0 && (
                                                <p className="text-xs text-telegram-subtext py-2">No recently deleted Drive items.</p>
                                            )}
                                            {!driveTrashBusy && !driveTrashError && driveTrash.length > 0 && visibleDriveTrash.length === 0 && (
                                                <p className="text-xs text-telegram-subtext py-2">No deleted items match the current search, type, availability, or time filters.</p>
                                            )}
                                            <div className="space-y-1 max-h-48 overflow-y-auto">
                                                {shownDriveTrash.map(entry => (
                                                    <div key={entry.kind + ':' + entry.id} className="flex items-center justify-between gap-3 rounded-lg px-2 py-2 bg-telegram-hover/40">
                                                        {entry.restorable && (
                                                            <input
                                                                type="checkbox"
                                                                aria-label={`Select ${entry.kind} ${entry.name} for bulk restore`}
                                                                checked={selectedDriveTrashKeys.includes(entry.kind + ':' + entry.id)}
                                                                disabled={restoringDriveItem !== null || driveRestoreSyncBusy}
                                                                onChange={event => {
                                                                    const key = entry.kind + ':' + entry.id;
                                                                    setSelectedDriveTrashKeys(current => event.target.checked
                                                                        ? current.includes(key) ? current : [...current, key].slice(0, 100)
                                                                        : current.filter(item => item !== key));
                                                                }}
                                                                className="h-4 w-4 flex-shrink-0 accent-telegram-primary"
                                                            />
                                                        )}
                                                        <div className="min-w-0">
                                                            <p className="text-xs text-telegram-text truncate" title={entry.name}>{entry.name}</p>
                                                            <p className="text-[11px] text-telegram-subtext">
                                                                {entry.kind === 'folder' ? 'Folder' : entry.kind === 'symlink' ? 'Symlink' : 'File'} · {new Date(entry.deleted_at).toLocaleString()}
                                                                {entry.kind === 'file' ? ` · ${formatDriveTrashBytes(entry.size)}` : ''}
                                                            </p>
                                                        </div>
                                                        {entry.restorable ? (
                                                            <button
                                                                onClick={() => void restoreDriveTrashItem(entry)}
                                                                aria-label={restoringDriveItem === entry.kind + ':' + entry.id
                                                                    ? `Restoring ${entry.kind} ${entry.name}`
                                                                    : `Restore ${entry.kind} ${entry.name}`}
                                                                disabled={restoringDriveItem !== null || driveRestoreSyncBusy}
                                                                className="text-xs text-telegram-primary hover:opacity-80 disabled:opacity-50 flex-shrink-0"
                                                            >
                                                                {restoringDriveItem === entry.kind + ':' + entry.id ? 'Restoring…' : 'Restore'}
                                                            </button>
                                                        ) : (
                                                            <span className="text-xs text-telegram-subtext flex-shrink-0" title="The original cloud file is no longer available">
                                                                File unavailable
                                                            </span>
                                                        )}
                                                    </div>
                                                ))}
                                            </div>
                                            {shownDriveTrash.length < visibleDriveTrash.length && (
                                                <button
                                                    type="button"
                                                    aria-label="Show more recently deleted Drive items"
                                                    onClick={() => setDriveTrashVisibleLimit(limit => Math.min(limit + 50, visibleDriveTrash.length))}
                                                    className="w-full rounded-md px-3 py-2 text-xs font-medium text-telegram-primary bg-telegram-hover/50 hover:bg-telegram-hover"
                                                >
                                                    Show next {Math.min(50, visibleDriveTrash.length - shownDriveTrash.length)} deleted items
                                                </button>
                                            )}
                                        </div>
                                    )}
                                </div>

                            {SHOW_ADVANCED_SETTINGS && (<>
                                {/* Transcode Cache Size */}
                                <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                    <div className="flex items-center justify-between">
                                        <div className="flex items-center gap-2">
                                            <HardDrive className="w-4 h-4 text-telegram-subtext" />
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.transcode_cache_limit')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.transcode_cache_desc')}</p>
                                            </div>
                                        </div>
                                        <span className="text-sm text-telegram-primary font-mono font-medium">{settings.transcodeCacheMaxGb} GB</span>
                                    </div>
                                    <input type="range" min="1" max="50" step="1" value={settings.transcodeCacheMaxGb}
                                        onChange={e => {
                                            const gb = parseInt(e.target.value);
                                            updateSetting('transcodeCacheMaxGb', gb);
                                            invoke('cmd_set_transcode_cache_limit', { maxGb: gb }).catch(() => {});
                                        }}
                                        className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                </div>

                            </>)}

                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <Trash2 className="w-4 h-4 text-telegram-subtext" />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.clear_local_cache')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.clear_local_cache_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        disabled={clearing}
                                        onClick={async () => {
                                            const ok = await confirm({
                                                title: t('settings.clear_cache_title'),
                                                message: t('settings.clear_cache_desc'),
                                                confirmText: t('settings.clear'),
                                                variant: 'danger',
                                            });
                                            if (!ok) return;
                                            setClearing(true);
                                            try {
                                                await invoke('cmd_clean_cache');
                                                toast.success(t('settings.cache_cleared'));
                                            } catch {
                                                toast.error(t('settings.cache_clear_failed'));
                                            } finally {
                                                setClearing(false);
                                            }
                                        }}
                                        className="px-3 py-1.5 rounded-lg text-xs font-medium bg-red-500/10 text-red-400 hover:bg-red-500/20 transition disabled:opacity-50 disabled:cursor-not-allowed"
                                    >
                                        {clearing ? t('settings.clearing') : t('settings.clear')}
                                    </button>
                                </div>

                            {SHOW_ADVANCED_SETTINGS && (<>
                                {/* Transcode Cache */}
                                <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-3">
                                    <div className="flex items-center justify-between">
                                        <div className="flex items-center gap-2">
                                            <HardDrive className="w-4 h-4 text-telegram-subtext" />
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.transcode_cache')}</p>
                                                <p className="text-xs text-telegram-subtext">
                                                    {transcodeCache
                                                        ? `${(transcodeCache.total_bytes / 1048576).toFixed(1)} MB / ${(transcodeCache.max_bytes / 1073741824).toFixed(1)} GB`
                                                        : t('common.loading')}
                                                </p>
                                            </div>
                                        </div>
                                        <div className="flex items-center gap-1.5">
                                            <button
                                                onClick={fetchTranscodeCache}
                                                disabled={cacheLoading}
                                                className="p-1.5 rounded-md hover:bg-telegram-hover text-telegram-subtext hover:text-telegram-text transition"
                                                title={t('settings.refresh_links')}
                                            >
                                                <RefreshCw className={`w-3 h-3 ${cacheLoading ? 'animate-spin' : ''}`} />
                                            </button>
                                            <button
                                                disabled={!transcodeCache || transcodeCache.entries.length === 0}
                                                onClick={async () => {
                                                    const ok = await confirm({
                                                        title: t('settings.clear_transcode_title'),
                                                        message: t('settings.clear_transcode_message'),
                                                        confirmText: t('settings.clear_all'),
                                                        variant: 'danger',
                                                    });
                                                    if (!ok) return;
                                                    setClearingVariant('__all__');
                                                    try {
                                                        const msg = await invoke<string>('cmd_clear_transcode_cache', {});
                                                        toast.success(msg);
                                                        fetchTranscodeCache();
                                                    } catch (e) {
                                                        toast.error(t('settings.failed_prefix', { error: e }));
                                                    } finally {
                                                        setClearingVariant(null);
                                                    }
                                                }}
                                                className="px-2.5 py-1 rounded-md text-[10px] font-medium bg-red-500/10 text-red-400 hover:bg-red-500/20 transition disabled:opacity-50 disabled:cursor-not-allowed"
                                            >
                                                {clearingVariant === '__all__' ? t('settings.clearing') : t('settings.clear_all')}
                                            </button>
                                        </div>
                                    </div>

                                    {/* Cache entries list */}
                                    {transcodeCache && transcodeCache.entries.length > 0 ? (
                                        <div className="space-y-1.5 max-h-[200px] overflow-y-auto pr-1 custom-scrollbar">
                                            {/* Group HLS variants by file_key (exclude originals, which are cleared via per-file Clear or Clear All) */}
                                            {(() => {
                                                const grouped: Record<string, CacheEntry[]> = {};
                                                for (const e of transcodeCache.entries) {
                                                    // Skip original entries — they're cleared via per-file or Clear All only
                                                    if (e.quality === 'original') continue;
                                                    if (!grouped[e.file_key]) grouped[e.file_key] = [];
                                                    grouped[e.file_key].push(e);
                                                }
                                                return Object.entries(grouped).map(([fileKey, entries]) => (
                                                    <div key={fileKey} className="p-2 rounded bg-telegram-bg/50 border border-telegram-border/30">
                                                        <div className="flex items-center justify-between mb-1">
                                                            <span className="text-[10px] font-mono text-telegram-subtext truncate max-w-[180px]" title={fileKey}>
                                                                {fileKey}
                                                            </span>
                                                            <button
                                                                disabled={clearingVariant !== null}
                                                                onClick={async () => {
                                                                    setClearingVariant(fileKey);
                                                                    try {
                                                                        const msg = await invoke<string>('cmd_clear_transcode_cache', { fileKey });
                                                                        toast.success(msg);
                                                                        fetchTranscodeCache();
                                                                    } catch (e) {
                                                                        toast.error(t('settings.failed_prefix', { error: e }));
                                                                    } finally {
                                                                        setClearingVariant(null);
                                                                    }
                                                                }}
                                                                className="text-[9px] text-red-400/60 hover:text-red-400 transition px-1 py-0.5 rounded hover:bg-red-500/10 disabled:opacity-30"
                                                                title={t('settings.clear_variants_for', { key: fileKey })}
                                                            >
                                                                {clearingVariant === fileKey ? '...' : t('settings.clear')}
                                                            </button>
                                                        </div>
                                                        <div className="flex flex-wrap gap-1">
                                                            {entries.map(e => (
                                                                <button
                                                                    key={`${e.file_key}:${e.quality}`}
                                                                    disabled={clearingVariant !== null}
                                                                    onClick={async () => {
                                                                        const variantKey = `${e.file_key}:${e.quality}`;
                                                                        setClearingVariant(variantKey);
                                                                        try {
                                                                            const msg = await invoke<string>('cmd_clear_transcode_cache', { fileKey: e.file_key, quality: e.quality });
                                                                            toast.success(msg);
                                                                            fetchTranscodeCache();
                                                                        } catch (err) {
                                                                            toast.error(t('settings.failed_prefix', { error: err }));
                                                                        } finally {
                                                                            setClearingVariant(null);
                                                                        }
                                                                    }}
                                                                    className={`flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] font-medium transition ${
                                                                        e.playlist_exists
                                                                            ? 'bg-emerald-500/10 text-emerald-400 hover:bg-red-500/10 hover:text-red-400 border border-emerald-500/20'
                                                                            : 'bg-amber-500/10 text-amber-400/60 border border-amber-500/20'
                                                                    } disabled:opacity-30`}
                                                                    title={`${e.quality} — ${(e.size_bytes / 1048576).toFixed(2)} MB${e.playlist_exists ? ' (ready)' : ' (partial)'}`}
                                                                >
                                     {e.quality === 'original' ? t('settings.original') : e.quality}
                                                                    <span className="text-[8px] opacity-60">{e.playlist_exists ? '✓' : '~'}</span>
                                                                </button>
                                                            ))}
                                                        </div>
                                                    </div>
                                                ));
                                            })()}
                                        </div>
                                    ) : transcodeCache && transcodeCache.entries.length === 0 ? (
                                        <p className="text-[11px] text-telegram-subtext/50 text-center py-2">{t('settings.no_transcoded_cached')}</p>
                                    ) : (
                                        <div className="flex items-center justify-center py-2">
                                            <RefreshCw className="w-3 h-3 text-telegram-subtext animate-spin" />
                                        </div>
                                    )}
                                </div>
                            </>)}
                            </section>

                                    </motion.div>
                                )}

                                {activeTab === 'proxy' && (
                                    <motion.section
                                        key="proxy"
                                        initial={{ opacity: 0, x: -20 }}
                                        animate={{ opacity: 1, x: 0 }}
                                        exit={{ opacity: 0, x: 20 }}
                                        transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
                                        className="space-y-3 w-full"
                                    >
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <Shield className="w-3.5 h-3.5" />
                                    {t('settings.proxy_config')}
                                </h3>

                                {/* Enable Proxy */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <div className={`w-2.5 h-2.5 rounded-full ${
                                            !settings.proxyEnabled || !settings.proxyLiveStateEnabled
                                                ? 'bg-gray-500' 
                                                : !proxyStatus 
                                                    ? 'bg-amber-400 animate-pulse shadow-[0_0_6px_rgba(251,191,36,0.5)]' 
                                                    : proxyStatus.reachable 
                                                        ? 'bg-green-400 shadow-[0_0_6px_rgba(74,222,128,0.5)]' 
                                                        : 'bg-red-500 shadow-[0_0_6px_rgba(239,68,68,0.5)]'
                                        }`} />
                                        <div>
                                            <div className="flex items-center gap-2">
                                                <p className="text-sm text-telegram-text font-medium">{t('common.enable_proxy')}</p>
                                                {settings.proxyEnabled && (
                                                    <span className="text-[10px] px-1.5 py-0.5 rounded bg-white/5 text-telegram-subtext font-mono">
                                                        {!settings.proxyLiveStateEnabled
                                                            ? t('settings.proxy_status_off') || 'Off'
                                                            : !proxyStatus 
                                                                ? t('settings.proxy_status_checking') || 'Checking…' 
                                                                : proxyStatus.reachable 
                                                                    ? `${t('settings.proxy_status_connected') || 'Connected'} (${proxyStatus.latency_ms}ms)` 
                                                                    : t('settings.proxy_status_unreachable') || 'Unreachable'}
                                                    </span>
                                                )}
                                            </div>
                                            <p className="text-xs text-telegram-subtext">{t('settings.enable_proxy_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => updateSetting('proxyEnabled', !settings.proxyEnabled)}
                                        aria-label={t('common.enable_proxy')}
                                        aria-pressed={settings.proxyEnabled}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.proxyEnabled ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.proxyEnabled ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                                {/* Live Connection Monitoring */}
                                {settings.proxyEnabled && (
                                    <div className="tr-setting-row flex items-center justify-between p-3">
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.live_state') || 'Live Connection Monitoring'}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.live_state_desc') || 'Periodically check connectivity and display latency'}</p>
                                        </div>
                                        <button
                                            onClick={() => updateSetting('proxyLiveStateEnabled', !settings.proxyLiveStateEnabled)}
                                            aria-label={t('settings.live_state')}
                                            aria-pressed={settings.proxyLiveStateEnabled}
                                            className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.proxyLiveStateEnabled ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                        >
                                            <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.proxyLiveStateEnabled ? 'translate-x-5' : 'translate-x-0'}`} />
                                        </button>
                                    </div>
                                )}

                                {/* Proxy Type */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.proxy_type')}</p>
                                        <p className="text-xs text-telegram-subtext">
                                            {settings.proxyType === 'socks5' 
                                                ? t('settings.socks5_desc') 
                                                : t('settings.http_bridge_desc') || 'HTTP/HTTPS proxy tunneling via local SOCKS5 bridge.'}
                                        </p>
                                    </div>
                                    <div className="relative">
                                        <select
                                            aria-label={t('common.proxy_type')}
                                            value={settings.proxyType}
                                            onChange={e => updateSetting('proxyType', e.target.value as 'socks5' | 'http' | 'https')}
                                            className="tr-modal-input appearance-none pl-3 pr-8 py-1.5 text-sm text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                                        >
                                            <option value="socks5">SOCKS5</option>
                                            <option value="http">HTTP</option>
                                            <option value="https">HTTPS</option>
                                        </select>
                                        <ChevronDown className="w-4 h-4 text-telegram-subtext absolute right-2.5 top-1/2 -translate-y-1/2 pointer-events-none" />
                                    </div>
                                </div>

                                {/* Host */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.host')}</p>
                                        <p className="text-xs text-telegram-subtext">{t('settings.host_desc')}</p>
                                    </div>
                                    <input
                                        type="text"
                                        placeholder="e.g. 127.0.0.1"
                                        aria-label={t('common.host')}
                                        value={settings.proxyHost}
                                        onChange={e => updateSetting('proxyHost', e.target.value)}
                                        className="w-40 bg-telegram-bg border border-telegram-border rounded-md px-2 py-1 text-sm text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                                    />
                                </div>

                                {/* Port */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.port')}</p>
                                        <p className="text-xs text-telegram-subtext">{t('settings.port_desc')}</p>
                                    </div>
                                    <input
                                        type="number"
                                        min="1"
                                        max="65535"
                                        aria-label={t('common.port')}
                                        value={settings.proxyPort}
                                        onChange={e => updateSetting('proxyPort', Math.max(1, Math.min(65535, parseInt(e.target.value) || 1080)))}
                                        className="w-20 bg-telegram-bg border border-telegram-border rounded-md px-2 py-1 text-sm text-telegram-text text-center focus:outline-none focus:border-telegram-primary/50 transition"
                                    />
                                </div>

                                {/* SOCKS5/HTTP auth fields */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.username')}</p>
                                        <p className="text-xs text-telegram-subtext">{t('settings.optional')}</p>
                                    </div>
                                    <input
                                        type="text"
                                        placeholder={t('settings.optional')}
                                        aria-label={t('common.username')}
                                        value={settings.proxyUsername}
                                        onChange={e => updateSetting('proxyUsername', e.target.value)}
                                        className="w-40 bg-telegram-bg border border-telegram-border rounded-md px-2 py-1 text-sm text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                                    />
                                </div>
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div>
                                        <p className="text-sm text-telegram-text font-medium">{t('common.password')}</p>
                                        <p className="text-xs text-telegram-subtext">{t('settings.optional')}</p>
                                    </div>
                                    <input
                                        type="password"
                                        placeholder={t('settings.optional')}
                                        aria-label={t('common.password')}
                                        value={settings.proxyPassword}
                                        onChange={e => updateSetting('proxyPassword', e.target.value)}
                                        className="w-40 bg-telegram-bg border border-telegram-border rounded-md px-2 py-1 text-sm text-telegram-text text-right focus:outline-none focus:border-telegram-primary/50 transition placeholder:text-telegram-subtext/40"
                                    />
                                </div>

                                {/* Info note */}
                                <div className="p-3 rounded-lg bg-yellow-500/5 border border-yellow-500/10 space-y-3">
                                    <p className="text-[11px] text-yellow-400/70 leading-relaxed">
                                        {t('settings.proxy_reconnect_note')}
                                    </p>
                                    <div className="flex items-center gap-2">
                                        <button
                                            onClick={async () => {
                                                setReconnecting(true);
                                                try {
                                                    const ok = await invoke<boolean>('cmd_reconnect_with_network_settings');
                                                    if (ok) {
                                                        toast.success(t('settings.reconnect_success_toast'));
                                                    } else {
                                                        toast.error(t('settings.reconnect_failed_toast'));
                                                    }
                                                } catch (e) {
                                                    toast.error(t('settings.reconnect_failed_err_toast', { error: e }));
                                                } finally {
                                                    setReconnecting(false);
                                                }
                                            }}
                                            disabled={reconnecting}
                                            className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary/10 text-telegram-primary hover:bg-telegram-primary/20 transition disabled:opacity-50 disabled:cursor-not-allowed"
                                        >
                                            {reconnecting ? (
                                                <>
                                                    <Loader2 className="w-3 h-3 animate-spin" />
                                                    {t('settings.reconnecting')}
                                                </>
                                            ) : (
                                                <>
                                                    <RefreshCw className="w-3 h-3" />
                                                    {t('settings.reconnect_now')}
                                                </>
                                            )}
                                        </button>
                                        <button
                                            onClick={async () => {
                                                setIsTestingProxy(true);
                                                try {
                                                    const success = await invoke<boolean>('cmd_test_proxy_traffic');
                                                    if (success) {
                                                        toast.success(t('settings.proxy_test_success') || 'Proxy connection working!');
                                                    } else {
                                                        toast.error(t('settings.proxy_test_failed') || 'Proxy traffic test failed.');
                                                    }
                                                } catch (e) {
                                                    toast.error(`Error testing proxy: ${e}`);
                                                } finally {
                                                    setIsTestingProxy(false);
                                                }
                                            }}
                                            disabled={isTestingProxy || reconnecting || !settings.proxyEnabled}
                                            className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-white/5 text-telegram-text hover:bg-white/10 transition disabled:opacity-30 disabled:cursor-not-allowed"
                                        >
                                            {isTestingProxy ? (
                                                <>
                                                    <Loader2 className="w-3 h-3 animate-spin" />
                                                    {t('settings.proxy_testing') || 'Testing…'}
                                                </>
                                            ) : (
                                                <>
                                                    <Play className="w-3.5 h-3.5" />
                                                    {t('settings.test_connection') || 'Test Connection'}
                                                </>
                                            )}
                                        </button>
                                    </div>
                                </div>
                            </motion.section>
                        )}

                        {activeTab === 'vpn' && (
                                    <motion.section
                                        key="vpn"
                                        initial={{ opacity: 0, x: -20 }}
                                        animate={{ opacity: 1, x: 0 }}
                                        exit={{ opacity: 0, x: 20 }}
                                        transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
                                        className="space-y-3 w-full"
                                    >
                                <h3 className="tr-setting-section-title flex items-center gap-2">
                                    <Zap className="w-3.5 h-3.5" />
                                    {t('settings.vpn_optimizer')}
                                    {latencyMs !== null && (
                                        <span className={`ml-auto text-[10px] font-mono px-1.5 py-0.5 rounded-full ${
                                            latencyMs < 0 ? 'bg-red-500/10 text-red-400' :
                                            latencyMs < 100 ? 'bg-green-500/10 text-green-400' :
                                            latencyMs < 300 ? 'bg-yellow-500/10 text-yellow-400' :
                                            'bg-red-500/10 text-red-400'
                                        }`}>
                                            <Activity className="w-3 h-3 inline mr-0.5" />
                                            {latencyMs < 0 ? 'Offline' : `${latencyMs}ms`}
                                        </span>
                                    )}
                                </h3>

                                {/* Master Toggle */}
                                <div className="tr-setting-row flex items-center justify-between p-3">
                                    <div className="flex items-center gap-2">
                                        <div className={`w-2 h-2 rounded-full ${settings.vpnMode ? 'bg-emerald-400 shadow-[0_0_6px_rgba(52,211,153,0.5)]' : 'bg-gray-500'}`} />
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.vpn_mode')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.vpn_mode_desc')}</p>
                                        </div>
                                    </div>
                                    <button
                                        onClick={() => updateSetting('vpnMode', !settings.vpnMode)}
                                        className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.vpnMode ? 'bg-emerald-500' : 'bg-telegram-border'}`}
                                    >
                                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.vpnMode ? 'translate-x-5' : 'translate-x-0'}`} />
                                    </button>
                                </div>

                                {settings.vpnMode && (<>
                                    {/* Timeout Multiplier */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.timeout_multiplier')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.timeout_multiplier_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">{settings.timeoutMultiplier}×</span>
                                        </div>
                                        <input type="range" min="1" max="5" step="1" value={settings.timeoutMultiplier}
                                            onChange={e => updateSetting('timeoutMultiplier', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Retry Attempts */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.retry_attempts')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.retry_attempts_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">{settings.retryAttempts}</span>
                                        </div>
                                        <input type="range" min="0" max="5" step="1" value={settings.retryAttempts}
                                            onChange={e => updateSetting('retryAttempts', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Backoff Settings */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <p className="text-sm text-telegram-text font-medium">{t('settings.retry_backoff')}</p>
                                        <div className="flex items-center justify-between">
                                            <p className="text-xs text-telegram-subtext">{t('settings.base_delay')}</p>
                                            <span className="text-xs text-telegram-primary font-mono">{settings.retryBaseBackoffSec}s</span>
                                        </div>
                                        <input type="range" min="0.5" max="5" step="0.5" value={settings.retryBaseBackoffSec}
                                            onChange={e => updateSetting('retryBaseBackoffSec', parseFloat(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                        <div className="flex items-center justify-between">
                                            <p className="text-xs text-telegram-subtext">{t('settings.max_delay')}</p>
                                            <span className="text-xs text-telegram-primary font-mono">{settings.retryMaxBackoffSec}s</span>
                                        </div>
                                        <input type="range" min="8" max="60" step="2" value={settings.retryMaxBackoffSec}
                                            onChange={e => updateSetting('retryMaxBackoffSec', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Adaptive Polling */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.adaptive_polling')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.adaptive_polling_desc')}</p>
                                            </div>
                                            <button
                                                onClick={() => updateSetting('adaptivePolling', !settings.adaptivePolling)}
                                                className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.adaptivePolling ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                            >
                                                <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.adaptivePolling ? 'translate-x-5' : 'translate-x-0'}`} />
                                            </button>
                                        </div>
                                        {settings.adaptivePolling && (<>
                                            <div className="flex items-center justify-between">
                                                <p className="text-xs text-telegram-subtext">{t('settings.min_interval')}</p>
                                                <span className="text-xs text-telegram-primary font-mono">{settings.pollingMinSec}s</span>
                                            </div>
                                            <input type="range" min="10" max="30" step="5" value={settings.pollingMinSec}
                                                onChange={e => updateSetting('pollingMinSec', parseInt(e.target.value))}
                                                className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                            <div className="flex items-center justify-between">
                                                <p className="text-xs text-telegram-subtext">{t('settings.max_interval')}</p>
                                                <span className="text-xs text-telegram-primary font-mono">{settings.pollingMaxSec}s</span>
                                            </div>
                                            <input type="range" min="45" max="120" step="15" value={settings.pollingMaxSec}
                                                onChange={e => updateSetting('pollingMaxSec', parseInt(e.target.value))}
                                                className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                        </>)}
                                    </div>

                                    {/* Preferred DC */}
                                    <div className="tr-setting-row flex items-center justify-between p-3">
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.preferred_dc')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.preferred_dc_desc')}</p>
                                        </div>
                                        <div className="relative">
                                            <select
                                                value={settings.preferredDC}
                                                onChange={e => updateSetting('preferredDC', e.target.value as typeof settings.preferredDC)}
                                                className="tr-modal-input appearance-none pl-3 pr-8 py-1.5 text-sm text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                                            >
                                                <option value="auto">{t('settings.auto')}</option>
                                                <option value="dc1">DC 1</option>
                                                <option value="dc2">DC 2</option>
                                                <option value="dc3">DC 3</option>
                                                <option value="dc4">DC 4</option>
                                                <option value="dc5">DC 5</option>
                                            </select>
                                            <ChevronDown className="w-4 h-4 text-telegram-subtext absolute right-2.5 top-1/2 -translate-y-1/2 pointer-events-none" />
                                        </div>
                                    </div>

                                    {/* DC Fallback Attempts */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.dc_fallback_attempts')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.dc_fallback_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">{settings.dcFallbackAttempts}</span>
                                        </div>
                                        <input type="range" min="1" max="4" step="1" value={settings.dcFallbackAttempts}
                                            onChange={e => updateSetting('dcFallbackAttempts', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Flood Wait */}
                                    <div className="tr-setting-row flex items-center justify-between p-3">
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.respect_flood')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.respect_flood_desc')}</p>
                                        </div>
                                        <button
                                            onClick={() => updateSetting('floodWaitRespect', !settings.floodWaitRespect)}
                                            className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.floodWaitRespect ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                        >
                                            <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.floodWaitRespect ? 'translate-x-5' : 'translate-x-0'}`} />
                                        </button>
                                    </div>

                                    {/* Peer Cache Size */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.peer_cache_size')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.peer_cache_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">{settings.peerCacheSize}</span>
                                        </div>
                                        <input type="range" min="100" max="2000" step="100" value={settings.peerCacheSize}
                                            onChange={e => updateSetting('peerCacheSize', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Bandwidth Throttle */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <p className="text-sm text-telegram-text font-medium flex items-center gap-1.5">
                                            <Gauge className="w-3.5 h-3.5 text-telegram-subtext" />
                                            {t('settings.bandwidth_throttle')}
                                        </p>
                                        <div className="flex items-center justify-between">
                                            <p className="text-xs text-telegram-subtext">{t('settings.upload_limit')}</p>
                                            <span className="text-xs text-telegram-primary font-mono">
                                                {settings.bandwidthLimitUpKBs === 0 ? t('settings.unlimited') : `${settings.bandwidthLimitUpKBs} KB/s`}
                                            </span>
                                        </div>
                                        <input type="range" min="0" max="5120" step="128" value={settings.bandwidthLimitUpKBs}
                                            onChange={e => updateSetting('bandwidthLimitUpKBs', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                        <div className="flex items-center justify-between">
                                            <p className="text-xs text-telegram-subtext">{t('settings.download_limit')}</p>
                                            <span className="text-xs text-telegram-primary font-mono">
                                                {settings.bandwidthLimitDownKBs === 0 ? t('settings.unlimited') : `${settings.bandwidthLimitDownKBs} KB/s`}
                                            </span>
                                        </div>
                                        <input type="range" min="0" max="5120" step="128" value={settings.bandwidthLimitDownKBs}
                                            onChange={e => updateSetting('bandwidthLimitDownKBs', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Chunk Size */}
                                    <div className="tr-setting-row flex items-center justify-between p-3">
                                        <div>
                                            <p className="text-sm text-telegram-text font-medium">{t('settings.transfer_chunk_size')}</p>
                                            <p className="text-xs text-telegram-subtext">{t('settings.chunk_size_desc')}</p>
                                        </div>
                                        <div className="relative">
                                            <select
                                                value={settings.chunkSizeKb}
                                                onChange={e => updateSetting('chunkSizeKb', parseInt(e.target.value))}
                                                className="tr-modal-input appearance-none pl-3 pr-8 py-1.5 text-sm text-telegram-text focus:outline-none focus:border-telegram-primary/50 transition cursor-pointer"
                                            >
                                                <option value={128}>128 KB</option>
                                                <option value={256}>256 KB</option>
                                                <option value={512}>512 KB</option>
                                            </select>
                                            <ChevronDown className="w-4 h-4 text-telegram-subtext absolute right-2.5 top-1/2 -translate-y-1/2 pointer-events-none" />
                                        </div>
                                    </div>

                                    {/* Keep-Alive */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.keep_alive')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.keep_alive_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">
                                                {settings.keepAliveIntervalSec === 0 ? t('settings.off') : `${settings.keepAliveIntervalSec}s`}
                                            </span>
                                        </div>
                                        <input type="range" min="0" max="120" step="15" value={settings.keepAliveIntervalSec}
                                            onChange={e => updateSetting('keepAliveIntervalSec', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Archive Size Limit */}
                                    <div className="p-3 rounded-lg bg-telegram-hover/50 space-y-2">
                                        <div className="flex items-center justify-between">
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.bulk_archive_limit')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.bulk_archive_desc')}</p>
                                            </div>
                                            <span className="text-sm text-telegram-primary font-mono font-medium">
                                                {settings.archiveMaxBytes === 0 ? t('settings.unlimited') : `${settings.archiveMaxBytes} MiB`}
                                            </span>
                                        </div>
                                        <input type="range" min="0" max="2048" step="64" value={settings.archiveMaxBytes}
                                            onChange={e => updateSetting('archiveMaxBytes', parseInt(e.target.value))}
                                            className="w-full h-1.5 rounded-full appearance-none bg-telegram-border accent-telegram-primary cursor-pointer" />
                                    </div>

                                    {/* Auto-Detect VPN */}
                                    <div className="tr-setting-row flex items-center justify-between p-3">
                                        <div className="flex items-center gap-2">
                                            <Wifi className="w-4 h-4 text-telegram-subtext" />
                                            <div>
                                                <p className="text-sm text-telegram-text font-medium">{t('settings.auto_detect_vpn')}</p>
                                                <p className="text-xs text-telegram-subtext">
                                                    {vpnDetected === true ? t('settings.vpn_detected') : vpnDetected === false ? t('settings.no_vpn_detected') : t('settings.checking')}
                                                </p>
                                            </div>
                                        </div>
                                        <button
                                            onClick={() => updateSetting('autoDetectVpn', !settings.autoDetectVpn)}
                                            className={`relative w-11 h-6 rounded-full transition-colors duration-200 ${settings.autoDetectVpn ? 'bg-telegram-primary' : 'bg-telegram-border'}`}
                                        >
                                            <span className={`absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white shadow transition-transform duration-200 ${settings.autoDetectVpn ? 'translate-x-5' : 'translate-x-0'}`} />
                                        </button>
                                    </div>
                                </>)}
                                    </motion.section>
                                )}

                                {activeTab === 'sharing' && (
                                    <motion.section
                                        key="sharing"
                                        initial={{ opacity: 0, x: -20 }}
                                        animate={{ opacity: 1, x: 0 }}
                                        exit={{ opacity: 0, x: 20 }}
                                        transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
                                        className="space-y-4 w-full"
                                    >
                                        <div className="flex items-center justify-between">
                                            <h3 className="tr-setting-section-title flex items-center gap-2">
                                                <Link className="w-3.5 h-3.5 text-telegram-primary" />
                                                {t('settings.shared_links', { count: shares.length })}
                                            </h3>
                                            <button 
                                                onClick={fetchShares} 
                                                className="text-telegram-subtext hover:text-telegram-text p-1 rounded hover:bg-telegram-hover transition"
                                                title={t('settings.refresh_links')}
                                            >
                                                <RefreshCw className={`w-3.5 h-3.5 ${refreshing ? 'animate-spin' : ''}`} />
                                            </button>
                                        </div>

                                        {SHOW_ADVANCED_SETTINGS && (<>
                                        <div className="bg-telegram-hover/30 border border-telegram-border/50 rounded-lg p-3 space-y-2">
                                            <div className="text-[11px] font-semibold text-telegram-text flex items-center gap-1">🌐 {t('settings.ip_override')}</div>
                                            <input
                                                type="text"
                                                placeholder="e.g. 100.115.22.45 or my-pc:14201"
                                                value={globalDomain}
                                                onChange={(e) => setGlobalDomain(e.target.value)}
                                                className="w-full bg-telegram-surface border border-telegram-border rounded-md px-2.5 py-1.5 text-xs text-telegram-text focus:outline-none focus:border-telegram-primary/50 placeholder:text-telegram-subtext/40"
                                            />
                                            <p className="text-[10px] text-telegram-subtext">
                                                {t('settings.ip_override_desc')}
                                            </p>
                                        </div>

                                        </>)}

                                        {shares.length === 0 ? (
                                            <div className="py-8 text-center space-y-2">
                                                <Link className="w-8 h-8 text-telegram-subtext/40 mx-auto" />
                                                <p className="text-sm font-medium text-telegram-text">{t('settings.no_active_links')}</p>
                                                <p className="text-xs text-telegram-subtext">{t('settings.no_active_links_desc')}</p>
                                            </div>
                                        ) : (
                                            <div className="space-y-2 max-h-[300px] overflow-y-auto pr-1 custom-scrollbar">
                                                {shares.map((share) => {
                                                    const isExpired = share.expires_at ? (share.expires_at < Math.floor(Date.now() / 1000)) : false;
                                                    return (
                                                        <div key={share.id} className="p-3 rounded-lg bg-telegram-hover/40 border border-telegram-border/50 flex flex-col gap-2 relative">
                                                              <div className="flex justify-between items-start gap-4">
                                                                <div className="min-w-0 flex-1">
                                                                    <div className="text-xs font-semibold text-telegram-text truncate" title={share.file_name}>
                                                                        {share.file_name}
                                                                    </div>
                                                                    <div className="flex gap-2 items-center mt-1 flex-wrap text-[10px]">
                                                                        <span className="text-telegram-subtext">
                                                                            {new Date(share.created_at * 1000).toLocaleDateString()}
                                                                        </span>
                                                                        <span className="w-1 h-1 rounded-full bg-telegram-border" />
                                                                        {share.has_password ? (
                                                                            <span className="text-emerald-400 bg-emerald-500/10 px-1.5 py-0.5 rounded flex items-center gap-0.5 font-medium">
                                                                                <Key className="w-2.5 h-2.5" /> {t('settings.protected')}
                                                                            </span>
                                                                        ) : (
                                                                            <span className="text-blue-400 bg-blue-500/10 px-1.5 py-0.5 rounded font-medium">{t('settings.public')}</span>
                                                                        )}
                                                                        <span className="w-1 h-1 rounded-full bg-telegram-border" />
                                                                        {share.expires_at ? (
                                                                            isExpired ? (
                                                                                <span className="text-red-400 bg-red-500/10 px-1.5 py-0.5 rounded font-medium">{t('settings.expired')}</span>
                                                                            ) : (
                                                                                <span className="text-amber-400 bg-amber-500/10 px-1.5 py-0.5 rounded font-medium">
                                                                                    {t('settings.expires_at', { date: new Date(share.expires_at * 1000).toLocaleDateString() })}
                                                                                </span>
                                                                            )
                                                                        ) : (
                                                                            <span className="text-teal-400 bg-teal-500/10 px-1.5 py-0.5 rounded font-medium">{t('settings.never_expires')}</span>
                                                                        )}
                                                                    </div>
                                                                </div>
                                                                
                                                                <div className="flex gap-1">
                                                                    <button
                                                                        onClick={() => handleCopyShare(share.id)}
                                                                        className={`p-1.5 rounded bg-telegram-surface border border-telegram-border text-telegram-text hover:bg-telegram-hover transition ${
                                                                            copiedId === share.id ? 'text-emerald-400 border-emerald-500/30 bg-emerald-500/5' : ''
                                                                        }`}
                                                                        title={t('settings.copy_share_link')}
                                                                    >
                                                                        {copiedId === share.id ? <Check className="w-3.5 h-3.5" /> : <Copy className="w-3.5 h-3.5" />}
                                                                    </button>
                                                                    <button
                                                                        onClick={() => handleRevokeShare(share.id)}
                                                                        className="p-1.5 rounded bg-telegram-surface border border-telegram-border text-red-400 hover:bg-red-500/10 hover:border-red-500/30 transition"
                                                                        title={t('settings.revoke_link')}
                                                                    >
                                                                        <Trash2 className="w-3.5 h-3.5" />
                                                                    </button>
                                                                </div>
                                                            </div>
                                                        </div>
                                                    );
                                                })}
                                            </div>
                                        )}
                                    </motion.section>
                                )}
                                {activeTab === 'themes' && (
                                    <ThemesTab />
                                )}
                                {activeTab === 'about' && (
                                    <motion.section
                                        key="about"
                                        initial={{ opacity: 0, x: -20 }}
                                        animate={{ opacity: 1, x: 0 }}
                                        exit={{ opacity: 0, x: 20 }}
                                        transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
                                        className="space-y-4 w-full"
                                    >
                                        <div className="flex flex-col items-center py-6 space-y-5">
                                            {/* Logo */}
                                            <TeraRelayBrand size="lg" showName={false} />
                                            
                                            {/* App Name & Version */}
                                            <div className="text-center">
                                                <h3 className="text-base font-bold text-telegram-text">TeraRelay</h3>
                                                <p className="text-xs text-telegram-subtext mt-0.5">v{appVersion}</p>
                                            </div>

                                            <div className="flex flex-col items-center gap-2">
                                                <button
                                                    onClick={() => void checkForUpdates(true)}
                                                    disabled={updateStatus === 'checking' || updateStatus === 'downloading' || updateStatus === 'installing'}
                                                    className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary/10 border border-telegram-primary/20 text-telegram-primary hover:bg-telegram-primary/15 transition disabled:opacity-50"
                                                >
                                                    <RefreshCw className={`w-3.5 h-3.5 ${updateStatus === 'checking' ? 'animate-spin' : ''}`} />
                                                    {updateStatus === 'checking' ? t('settings.checking_for_updates') : t('settings.check_for_updates')}
                                                </button>
                                                {updateStatus === 'up-to-date' && (
                                                    <p className="text-[10px] text-emerald-400">{t('settings.up_to_date')}</p>
                                                )}
                                                {updateRelease && (
                                                    <p className="text-[10px] text-telegram-primary">{t('settings.update_available_version', { version: updateRelease.version })}</p>
                                                )}
                                                {updateStatus === 'error' && updateError && (
                                                    <p className="max-w-[300px] text-center text-[10px] text-red-400">{updateError}</p>
                                                )}
                                            </div>

                                            {/* Divider */}
                                            <div className="w-12 h-px bg-telegram-border" />

                                            {/* Diagnostics */}
                                            <button
                                                onClick={async () => {
                                                    setDiagLoading(true);
                                                    try {
                                                        const info = await invoke<string>('cmd_get_system_diagnostics');
                                                        await navigator.clipboard.writeText(info);
                                                        toast.success(t('settings.diagnostics_copied'));
                                                    } catch (e) {
                                                        toast.error(t('settings.diagnostics_copy_failed', { error: e }));
                                                    } finally {
                                                        setDiagLoading(false);
                                                    }
                                                }}
                                                disabled={diagLoading}
                                                className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium bg-telegram-hover border border-telegram-border text-telegram-subtext hover:text-telegram-text hover:bg-telegram-border/30 transition disabled:opacity-50"
                                            >
                                                {diagLoading ? (
                                                    <Loader2 className="w-3 h-3 animate-spin" />
                                                ) : (
                                                    <Clipboard className="w-3 h-3" />
                                                )}
                                                {t('settings.copy_diagnostics')}
                                            </button>

                                            {/* Creator Info */}
                                            <div className="text-center space-y-3">
                                                <div>
                                                    <p className="text-sm font-semibold text-telegram-text">Yashwanth</p>
                                                </div>
                                                <button
                                                    onClick={(e) => { e.preventDefault(); open('https://github.com/Yashwanth034'); }}
                                                    className="flex items-center justify-center gap-1.5 text-xs text-telegram-primary hover:text-telegram-primary/80 transition-colors cursor-pointer"
                                                >
                                                    <svg className="w-3.5 h-3.5" viewBox="0 0 24 24" fill="currentColor">
                                                        <path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.756-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z"/>
                                                    </svg>
                                                    github.com/Yashwanth034
                                                </button>
                                            </div>

                                            {/* Tagline */}
                                            <p className="text-[11px] text-telegram-subtext/60 leading-relaxed max-w-[280px] text-center">
                                                {t('settings.tagline')}
                                            </p>
                                        </div>
                                    </motion.section>
                                )}
                            </AnimatePresence>
                        </motion.div>

                        {/* Footer */}
                        <div className="px-5 py-3 border-t border-telegram-border flex items-center justify-between">
                            <button
                                onClick={resetSettings}
                                className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs text-telegram-subtext hover:text-red-400 hover:bg-red-500/10 transition font-medium"
                            >
                                <RotateCcw className="w-3.5 h-3.5" />
                                {t('settings.reset_defaults')}
                            </button>
                            <button
                                onClick={onClose}
                                className="px-4 py-1.5 rounded-lg text-xs font-medium bg-telegram-primary text-white hover:bg-telegram-primary/90 transition"
                            >
                                {t('settings.done')}
                            </button>
                        </div>
                    </motion.div>
                </motion.div>
            )}
        </AnimatePresence>
    );
}

// ── Themes Tab ──────────────────────────────────────────────────────
// Inline component (follows the pattern of the other tabs in this file).

const PALETTE_KEYS: { key: keyof ThemeColorPalette; labelKey: string }[] = [
    { key: 'bg', labelKey: 'settings.color_bg' },
    { key: 'surface', labelKey: 'settings.color_surface' },
    { key: 'primary', labelKey: 'settings.color_primary' },
    { key: 'secondary', labelKey: 'settings.color_secondary' },
    { key: 'text', labelKey: 'settings.color_text' },
    { key: 'subtext', labelKey: 'settings.color_subtext' },
];

function ThemesTab() {
    const { t } = useTranslation();
    const {
        customThemes,
        activeCustomThemeId,
        setActiveCustomTheme,
        addCustomTheme,
        deleteCustomTheme,
        updateCustomTheme,
    } = useTheme();
    const { confirm } = useConfirm();

    const [editingId, setEditingId] = useState<string | null>(null);

    const builtinThemes = customThemes.filter(t => t.isBuiltin);
    const userThemes = customThemes.filter(t => !t.isBuiltin);
    const editingTheme = editingId ? customThemes.find(t => t.id === editingId) : null;

    const handleCreateTheme = () => {
        const id = generateThemeId();
        const newTheme: CustomTheme = {
            id,
            name: 'My Theme',
            isDark: true,
            palette: getDefaultPalette(true),
        };
        addCustomTheme(newTheme);
        setEditingId(id);
        setActiveCustomTheme(id);
    };

    const handleSelectTheme = (theme: CustomTheme) => {
        if (activeCustomThemeId === theme.id) {
            // Deselect → reset to default
            setActiveCustomTheme(null);
            setEditingId(null);
        } else {
            setActiveCustomTheme(theme.id);
            if (!theme.isBuiltin) {
                setEditingId(theme.id);
            } else {
                setEditingId(null);
            }
        }
    };

    const handleDeleteTheme = async (id: string) => {
        const ok = await confirm({
            title: t('settings.delete_theme'),
            message: t('settings.delete_theme_confirm'),
            confirmText: t('common.delete'),
            variant: 'danger',
        });
        if (!ok) return;
        deleteCustomTheme(id);
        if (editingId === id) setEditingId(null);
    };

    const handlePaletteChange = (key: keyof ThemeColorPalette, value: string) => {
        if (!editingTheme || editingTheme.isBuiltin) return;
        const newPalette = { ...editingTheme.palette, [key]: value };
        updateCustomTheme(editingTheme.id, { palette: newPalette });
    };

    const handleBaseToggle = (isDark: boolean) => {
        if (!editingTheme || editingTheme.isBuiltin) return;
        updateCustomTheme(editingTheme.id, { isDark });
    };

    const handleNameChange = (name: string) => {
        if (!editingTheme || editingTheme.isBuiltin) return;
        updateCustomTheme(editingTheme.id, { name });
    };

    return (
        <motion.section
            key="themes"
            initial={{ opacity: 0, x: -20 }}
            animate={{ opacity: 1, x: 0 }}
            exit={{ opacity: 0, x: 20 }}
            transition={{ type: 'spring', damping: 25, stiffness: 220, opacity: { duration: 0.15 } }}
            className="space-y-5 w-full"
        >
            {/* Presets */}
            <div className="space-y-2">
                <h3 className="tr-setting-section-title flex items-center gap-2">
                    <Palette className="w-3.5 h-3.5" />
                    {t('settings.presets')}
                </h3>
                <div className="grid grid-cols-4 gap-2">
                    {builtinThemes.map(theme => (
                        <button
                            key={theme.id}
                            onClick={() => handleSelectTheme(theme)}
                            className={`relative rounded-lg p-0.5 transition-all duration-200 ${
                                activeCustomThemeId === theme.id
                                    ? 'ring-2 ring-telegram-primary ring-offset-1 ring-offset-telegram-surface'
                                    : 'hover:ring-1 hover:ring-telegram-subtext/30'
                            }`}
                            title={theme.name}
                        >
                            {/* Color preview swatch */}
                            <div className="rounded-md overflow-hidden h-10 flex">
                                <div className="flex-1" style={{ background: theme.palette.bg }} />
                                <div className="flex-1" style={{ background: theme.palette.surface }} />
                                <div className="flex-1" style={{ background: theme.palette.primary }} />
                            </div>
                            <p className="text-[10px] text-telegram-subtext mt-1 truncate text-center">
                                {theme.name}
                            </p>
                            {activeCustomThemeId === theme.id && (
                                <div className="absolute -top-1 -right-1 w-4 h-4 bg-telegram-primary rounded-full flex items-center justify-center">
                                    <Check className="w-2.5 h-2.5 text-white" />
                                </div>
                            )}
                        </button>
                    ))}
                </div>
            </div>

            {/* Custom Themes */}
            <div className="space-y-2">
                <h3 className="tr-setting-section-title flex items-center gap-2">
                    <Sparkles className="w-3.5 h-3.5" />
                    {t('settings.custom_themes')}
                </h3>

                {userThemes.length > 0 && (
                    <div className="grid grid-cols-4 gap-2">
                        {userThemes.map(theme => (
                            <button
                                key={theme.id}
                                onClick={() => handleSelectTheme(theme)}
                                className={`relative rounded-lg p-0.5 transition-all duration-200 ${
                                    activeCustomThemeId === theme.id
                                        ? 'ring-2 ring-telegram-primary ring-offset-1 ring-offset-telegram-surface'
                                        : 'hover:ring-1 hover:ring-telegram-subtext/30'
                                }`}
                                title={theme.name}
                            >
                                <div className="rounded-md overflow-hidden h-10 flex">
                                    <div className="flex-1" style={{ background: theme.palette.bg }} />
                                    <div className="flex-1" style={{ background: theme.palette.surface }} />
                                    <div className="flex-1" style={{ background: theme.palette.primary }} />
                                </div>
                                <p className="text-[10px] text-telegram-subtext mt-1 truncate text-center">
                                    {theme.name}
                                </p>
                                {activeCustomThemeId === theme.id && (
                                    <div className="absolute -top-1 -right-1 w-4 h-4 bg-telegram-primary rounded-full flex items-center justify-center">
                                        <Check className="w-2.5 h-2.5 text-white" />
                                    </div>
                                )}
                            </button>
                        ))}
                    </div>
                )}

                <button
                    onClick={handleCreateTheme}
                    className="w-full flex items-center justify-center gap-2 px-3 py-2.5 rounded-lg border border-dashed border-telegram-border text-telegram-subtext hover:text-telegram-primary hover:border-telegram-primary/50 transition-colors text-xs"
                >
                    <Plus className="w-3.5 h-3.5" />
                    {t('settings.create_theme')}
                </button>
            </div>

            {/* Editor (shown when a custom theme is selected) */}
            {editingTheme && !editingTheme.isBuiltin && (
                <div className="space-y-3 p-3 rounded-lg bg-telegram-hover/30 border border-telegram-border/50">
                    <h3 className="text-xs font-semibold text-telegram-subtext uppercase tracking-wider">
                        {t('settings.edit_theme')}
                    </h3>

                    {/* Theme Name */}
                    <div className="flex items-center gap-2">
                        <label className="text-xs text-telegram-subtext w-16 shrink-0">{t('settings.theme_name')}</label>
                        <input
                            type="text"
                            value={editingTheme.name}
                            onChange={e => handleNameChange(e.target.value)}
                            className="flex-1 px-2 py-1.5 rounded-md text-xs bg-telegram-surface border border-telegram-border text-telegram-text focus:border-telegram-primary outline-none transition"
                            maxLength={32}
                        />
                    </div>

                    {/* Base Mode Toggle */}
                    <div className="flex items-center gap-2">
                        <label className="text-xs text-telegram-subtext w-16 shrink-0">{t('settings.base_mode')}</label>
                        <div className="flex gap-1">
                            <button
                                onClick={() => handleBaseToggle(true)}
                                className={`px-3 py-1 rounded-md text-xs font-medium transition ${
                                    editingTheme.isDark
                                        ? 'bg-telegram-primary text-white'
                                        : 'bg-telegram-hover text-telegram-subtext hover:text-telegram-text'
                                }`}
                            >
                                Dark
                            </button>
                            <button
                                onClick={() => handleBaseToggle(false)}
                                className={`px-3 py-1 rounded-md text-xs font-medium transition ${
                                    !editingTheme.isDark
                                        ? 'bg-telegram-primary text-white'
                                        : 'bg-telegram-hover text-telegram-subtext hover:text-telegram-text'
                                }`}
                            >
                                Light
                            </button>
                        </div>
                    </div>

                    {/* Color Pickers */}
                    <div className="space-y-2">
                        {PALETTE_KEYS.map(({ key, labelKey }) => (
                            <div key={key} className="flex items-center gap-2">
                                <label className="text-xs text-telegram-subtext w-16 shrink-0">{t(labelKey)}</label>
                                <div className="flex items-center gap-1.5 flex-1">
                                    <input
                                        type="color"
                                        value={editingTheme.palette[key].startsWith('rgba') ? '#888888' : editingTheme.palette[key]}
                                        onChange={e => handlePaletteChange(key, e.target.value)}
                                        className="w-7 h-7 rounded-md border border-telegram-border cursor-pointer p-0.5 bg-transparent"
                                    />
                                    <input
                                        type="text"
                                        value={editingTheme.palette[key]}
                                        onChange={e => handlePaletteChange(key, e.target.value)}
                                        className="flex-1 px-2 py-1 rounded-md text-xs bg-telegram-surface border border-telegram-border text-telegram-text focus:border-telegram-primary outline-none transition font-mono"
                                        maxLength={30}
                                    />
                                </div>
                            </div>
                        ))}
                    </div>

                    {/* Delete Button */}
                    <button
                        onClick={() => handleDeleteTheme(editingTheme.id)}
                        className="flex items-center gap-1.5 px-3 py-1.5 rounded-lg text-xs font-medium text-red-400 hover:bg-red-500/10 transition"
                    >
                        <Trash2 className="w-3.5 h-3.5" />
                        {t('settings.delete_theme')}
                    </button>
                </div>
            )}

            {/* Reset to Default */}
            {activeCustomThemeId && (
                <button
                    onClick={() => {
                        setActiveCustomTheme(null);
                        setEditingId(null);
                    }}
                    className="w-full flex items-center justify-center gap-2 px-3 py-2 rounded-lg text-xs font-medium text-telegram-subtext hover:text-telegram-text bg-telegram-hover/50 hover:bg-telegram-hover transition"
                >
                    <RotateCcw className="w-3.5 h-3.5" />
                    {t('settings.reset_default')}
                </button>
            )}
        </motion.section>
    );
}
