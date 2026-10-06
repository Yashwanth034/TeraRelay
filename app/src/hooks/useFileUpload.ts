import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { useQueryClient } from '@tanstack/react-query';
import { toast } from 'sonner';
import { QueueItem } from '../types';
import { isAndroidPlatform, isIOSPlatform, showFileDialogFallback, pickWithFallback } from '../utils';
import { useSettings } from '../context/SettingsContext';
import { useFastTransferAuth } from '../context/FastTransferAuthContext';
import type { Store } from '@tauri-apps/plugin-store';
import { normalizeUploadQueue, useRecoverableTransferQueue } from '../transferQueue';

interface ProgressPayload {
    id: string;
    percent: number;
    uploaded_bytes: number;
    total_bytes: number;
    speed_bytes_per_sec: number;
}

interface RemoteProgressPayload {
    id: string;
    phase: 'downloading' | 'uploading';
    percent: number;
    speed: number;
    uploaded_bytes: number;
    total_bytes: number;
}

interface UploadPhasePayload {
    id: string;
    phase: 'preparing' | 'uploading';
}

export function useFileUpload(activeFolderId: number | null, store: Store | null) {
    const queryClient = useQueryClient();
    const { settings } = useSettings();
    const { queue: uploadQueue, setQueue: setUploadQueue, queueRef, initialized, durable, persist } =
        useRecoverableTransferQueue('upload', store, normalizeUploadQueue);
    const cancelledRef = useRef<Set<string>>(new Set());
    const pausedRef = useRef<Set<string>>(new Set());
    const activeIdsRef = useRef<Set<string>>(new Set());
    const { ensureFastTransferReady } = useFastTransferAuth();

    // Listen for progress events from Rust
    useEffect(() => {
        let unlistenProgress: UnlistenFn | undefined;
        let unlistenRemote: UnlistenFn | undefined;
        let unlistenPhase: UnlistenFn | undefined;

        listen<ProgressPayload>('upload-progress', (event) => {
            setUploadQueue(q => q.map(i =>
                i.id === event.payload.id ? {
                    ...i,
                    progress: event.payload.percent,
                    uploadedBytes: event.payload.uploaded_bytes,
                    totalBytes: event.payload.total_bytes,
                    // Rust already reports a rolling real-byte transfer rate.
                    // Do not average it again in the UI.
                    speedBytesPerSec: event.payload.speed_bytes_per_sec,
                } : i
            ));
        }).then(fn => { unlistenProgress = fn; });

        listen<RemoteProgressPayload>('remote-upload-progress', (event) => {
            setUploadQueue(q => q.map(i =>
                i.id === event.payload.id && activeIdsRef.current.has(i.id) &&
                (i.status === 'uploading' || i.status === 'downloading') ? {
                    ...i,
                    status: event.payload.phase,
                    progress: event.payload.percent,
                    speedBytesPerSec: event.payload.speed,
                    uploadedBytes: event.payload.uploaded_bytes,
                    totalBytes: event.payload.total_bytes,
                } : i
            ));
        }).then(fn => { unlistenRemote = fn; });

        listen<UploadPhasePayload>('upload-phase', (event) => {
            setUploadQueue(q => q.map(i =>
                i.id === event.payload.id ? {
                    ...i,
                    uploadPhase: event.payload.phase,
                    ...(event.payload.phase === 'preparing' ? { speedBytesPerSec: undefined } : {}),
                } : i
            ));
        }).then(fn => { unlistenPhase = fn; });

        return () => {
            unlistenProgress?.();
            unlistenRemote?.();
            unlistenPhase?.();
        };
    }, []);

    // Only durable pending work can reserve the configured worker slots.
    useEffect(() => {
        if (!initialized || !durable) return;
        const maxConcurrent = settings.maxConcurrentUploads || 1;
        const available = maxConcurrent - activeIdsRef.current.size;
        if (available <= 0) return;
        const pendingItems = uploadQueue
            .filter(i => i.status === 'pending' && !activeIdsRef.current.has(i.id))
            .slice(0, available);
        for (const item of pendingItems) {
            void processItem(item);
        }
    }, [uploadQueue, settings.maxConcurrentUploads, initialized, durable]);

    // Manage Android Foreground Service for persistent uploads
    useEffect(() => {
        if (!isAndroidPlatform) return;

        const hasActiveUploads = uploadQueue.some(i => i.status === 'uploading' || i.status === 'pending');
        if (hasActiveUploads) {
            invoke('cmd_start_foreground_service').catch(() => {});
        } else if (initialized) {
            invoke('cmd_stop_foreground_service').catch(() => {});
        }
    }, [uploadQueue, initialized]);

    /** Clean up temp zip file if the item was created from a folder */
    const cleanupTempZip = async (item: QueueItem) => {
        if (item.tempZipPath) {
            try {
                await invoke('cmd_delete_temp_zip', { path: item.tempZipPath });
            } catch {
                // Best-effort cleanup
            }
        }
    };

    const processItem = async (item: QueueItem) => {
        if (activeIdsRef.current.has(item.id)) return;
        // Reserve synchronously, before any save/auth await can trigger another render.
        activeIdsRef.current.add(item.id);
        let started = false;
        let completed = false;
        try {
            await persist();
            const queued = queueRef.current.find(i => i.id === item.id);
            if (queued?.status !== 'pending') return;
            started = true;
            const initialStatus = item.url ? 'downloading' : 'uploading';
            setUploadQueue(q => q.map(i => i.id === item.id ? {
                ...i,
                status: initialStatus,
                error: undefined,
                progress: 0,
                speedBytesPerSec: undefined,
                uploadPhase: item.url ? undefined : 'uploading',
            } : i));
            if (item.url) {
                await invoke('cmd_upload_from_url', { url: item.url, folderId: item.folderId, transferId: item.id });
            } else {
                const ready = await ensureFastTransferReady();
                if (!ready) throw new Error('TDLIB_SETUP_CANCELLED');
                if (cancelledRef.current.has(item.id) || pausedRef.current.has(item.id)) {
                    throw new Error('Transfer cancelled');
                }
                await invoke('cmd_upload_file', { path: item.path, folderId: item.folderId, transferId: item.id });
            }
            completed = true;
            if (cancelledRef.current.has(item.id)) {
                cancelledRef.current.delete(item.id);
                await persist();
                const current = queueRef.current.find(i => i.id === item.id);
                if (!current || current.status === 'cancelled') await cleanupTempZip(item);
            } else {
                pausedRef.current.delete(item.id);
                setUploadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'success', progress: 100 } : i));
                // Keep the source until removing recoverable work has really committed.
                await persist();
                await cleanupTempZip(item);
                window.setTimeout(() => {
                    setUploadQueue(q => q.filter(i => !(i.id === item.id && i.status === 'success')));
                }, 5000);
                queryClient.invalidateQueries({ queryKey: ['files', item.folderId] });
            }
        } catch (e) {
            if (!started) {
                toast.error(`Could not save upload queue: ${e}`);
            } else if (pausedRef.current.has(item.id)) {
                pausedRef.current.delete(item.id);
                setUploadQueue(q => q.map(i => i.id === item.id ? {
                    ...i, status: 'paused', speedBytesPerSec: 0,
                } : i));
            } else if (cancelledRef.current.has(item.id)) {
                cancelledRef.current.delete(item.id);
                try {
                    await persist();
                    const current = queueRef.current.find(i => i.id === item.id);
                    if (!current || current.status === 'cancelled') await cleanupTempZip(item);
                } catch (saveError) {
                    setUploadQueue(q => q.map(i => i.id === item.id ? {
                        ...i, status: 'error', error: `Could not save cancellation: ${saveError}`, speedBytesPerSec: 0,
                    } : i));
                }
            } else {
                const errMsg = completed ? `Upload completed, but queue save failed: ${e}` : String(e);
                if (!completed && errMsg.includes('TDLIB_SETUP_CANCELLED')) {
                    setUploadQueue(q => q.map(i => i.id === item.id ? {
                        ...i, status: 'paused',
                        error: 'Transfer setup was cancelled. Resume when you are ready to finish setup.',
                        speedBytesPerSec: 0,
                    } : i));
                } else if (!completed && errMsg.includes('Transfer cancelled')) {
                    setUploadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'cancelled', speedBytesPerSec: 0 } : i));
                } else {
                    setUploadQueue(q => q.map(i => i.id === item.id ? {
                        ...i, status: 'error', error: errMsg, speedBytesPerSec: 0,
                    } : i));
                    if (item.url && (errMsg.includes('FILE_TOO_BIG') || errMsg.includes('too large') || errMsg.includes('2 GB') || errMsg.includes('2GB'))) {
                        toast.error('Upload failed: URL uploads are limited to 2 GB.');
                    } else {
                        const displayPath = item.url || item.path;
                        toast.error(`Upload failed for ${displayPath.split('/').pop()}: ${errMsg}`);
                    }
                }
                // Failed and auth-cancelled uploads retain folder ZIPs for Retry/restart.
            }
        } finally {
            activeIdsRef.current.delete(item.id);
            setUploadQueue(q => [...q]);
        }
    };

    /** Queues a set of file paths for upload */
    const queueFiles = (paths: string[]) => {
        if (!paths || paths.length === 0) return;
        const newItems: QueueItem[] = paths.map((path: string) => ({
            id: Math.random().toString(36).substr(2, 9),
            path,
            folderId: activeFolderId,
            status: 'pending' as const,
        }));
        setUploadQueue(prev => [...prev, ...newItems]);
        toast.info(`Queued ${paths.length} file${paths.length !== 1 ? 's' : ''} for upload`);
    };

    const performManualUpload = async () => {
        const paths = await pickWithFallback(
            async () => {
                const selected = await open({ multiple: true, directory: false });
                if (!selected) return null;
                return Array.isArray(selected) ? selected : [selected];
            },
            () => performManualUpload(),
            {
                errorTitle: 'File picker failed',
                onBrowserPicker: async () => {
                    const fallbackPaths = await showFileDialogFallback({ directory: false, multiple: true });
                    return fallbackPaths.length > 0 ? fallbackPaths : null;
                },
            },
        );
        if (paths && paths.length > 0) {
            queueFiles(paths);
        }
    };

    const handleManualUpload = performManualUpload;

    /** Queue files dropped from the OS file manager (drag-and-drop upload) */
    const handleDropUpload = (paths: string[]) => {
        if (!paths || paths.length === 0) return;
        queueFiles(paths);
    };

    const performFolderUpload = async () => {
        if (isIOSPlatform) {
            toast.info('iOS does not provide a folder picker here. Select the folder files instead.');
            await performManualUpload();
            return;
        }

        const folderPath = await pickWithFallback(
            async () => {
                const selected = await open({ multiple: false, directory: true, title: 'Select Folder to Upload' });
                if (!selected) return null;
                const fp = Array.isArray(selected) ? selected[0] : selected;
                return fp || null;
            },
            () => performFolderUpload(),
            {
                errorTitle: 'Folder picker failed',
                onBrowserPicker: async () => {
                    const fallbackPaths = await showFileDialogFallback({ directory: true, multiple: true });
                    if (fallbackPaths.length > 0) {
                        // HTML folder picker returns individual file paths, not a folder path.
                        // We can't zip without a folder path, so files upload individually.
                        toast.info('Folder zipping unavailable with browser picker — uploading files individually.');
                        queueFiles(fallbackPaths);
                    }
                    return null; // Already handled via queueFiles — signal that the main flow should stop
                },
            },
        );
        if (!folderPath) return;

        const folderName = folderPath.split('/').pop() || folderPath.split('\\').pop() || 'folder';

        toast.info(`Zipping "${folderName}"...`);
        try {
            const zipPath = await invoke<string>('cmd_zip_folder', { folderPath });
            const item: QueueItem = {
                id: Math.random().toString(36).substr(2, 9),
                path: zipPath,
                folderId: activeFolderId,
                status: 'pending',
                tempZipPath: zipPath,
            };
            setUploadQueue(prev => [...prev, item]);
            toast.success(`Queued "${folderName}.zip" for upload`);
        } catch (e) {
            console.error('[Upload] Zip error:', e);
            toast.error(`Failed to zip folder: ${e}`);
        }
    };

    const handleFolderUpload = performFolderUpload;

    const finishCancellation = async (items: QueueItem[]) => {
        try {
            await persist();
            for (const item of items) {
                // The running worker cleans its source after the backend unwinds.
                // A manual Retry can supersede this cancellation while its save waits.
                const current = queueRef.current.find(i => i.id === item.id);
                if (!activeIdsRef.current.has(item.id) && (!current || current.status === 'cancelled')) {
                    await cleanupTempZip(item);
                }
            }
        } catch (e) {
            // A failed removal must keep its source and an actionable entry in memory.
            setUploadQueue(q => {
                const ids = new Set(items.map(item => item.id));
                const kept = q.map(item => ids.has(item.id) ? {
                    ...item, status: 'error' as const, error: `Could not save cancellation: ${e}`,
                } : item);
                for (const item of items) {
                    if (!kept.some(i => i.id === item.id)) {
                        kept.push({ ...item, status: 'error', error: `Could not save cancellation: ${e}` });
                    }
                }
                return kept;
            });
            toast.error(`Could not save upload cancellation: ${e}`);
        }
    };

    const cancelAll = () => {
        const items = queueRef.current.filter(i =>
            i.status === 'pending' || i.status === 'uploading' || i.status === 'downloading' ||
            i.status === 'pausing' || i.status === 'paused');
        for (const item of items) {
            if (activeIdsRef.current.has(item.id)) {
                pausedRef.current.delete(item.id);
                cancelledRef.current.add(item.id);
                invoke('cmd_cancel_transfer', { transferId: item.id }).catch(() => {});
            }
        }
        setUploadQueue(q => q
            .filter(i => i.status !== 'pending')
            .map(i => (i.status === 'uploading' || i.status === 'downloading' || i.status === 'pausing' || i.status === 'paused')
                ? { ...i, status: 'cancelled' as const, speedBytesPerSec: 0 }
                : i));
        void finishCancellation(items);
        toast.info('All uploads cancelled');
    };

    const cancelItem = (id: string) => {
        const item = queueRef.current.find(i => i.id === id);
        if (!item || !['pending', 'uploading', 'downloading', 'pausing', 'paused'].includes(item.status)) return;
        if (activeIdsRef.current.has(id)) {
            pausedRef.current.delete(id);
            cancelledRef.current.add(id);
            invoke('cmd_cancel_transfer', { transferId: id }).catch(() => {});
        }
        setUploadQueue(q => item.status === 'pending'
            ? q.filter(i => i.id !== id)
            : q.map(i => i.id === id ? { ...i, status: 'cancelled' as const, speedBytesPerSec: 0 } : i));
        void finishCancellation([item]);
    };

    const pauseItem = (id: string) => {
        setUploadQueue(q => {
            const item = q.find(i => i.id === id);
            if (item?.status !== 'uploading' && item?.status !== 'downloading') return q;
            pausedRef.current.add(id);
            invoke('cmd_cancel_transfer', { transferId: id }).catch(() => {});
            return q.map(i => i.id === id
                ? { ...i, status: 'pausing' as const, speedBytesPerSec: 0 }
                : i);
        });
    };

    const resumeItem = (id: string) => {
        setUploadQueue(q => q.map(i =>
            i.id === id && i.status === 'paused'
                ? { ...i, status: 'pending' as const, error: undefined, speedBytesPerSec: 0 }
                : i
        ));
    };

    const retryItem = (id: string) => {
        setUploadQueue(q => q.map(i =>
            i.id === id && (i.status === 'error' || i.status === 'cancelled')
                ? {
                    ...i,
                    status: 'pending' as const,
                    error: undefined,
                    progress: undefined,
                    uploadedBytes: undefined,
                    totalBytes: undefined,
                    speedBytesPerSec: undefined,
                    uploadPhase: undefined,
                }
                : i
        ));
    };

    const handleUrlUpload = (url: string, folderId: number | null) => {
        if (!url || !url.trim()) return;
        let filename: string;
        try {
            filename = new URL(url).pathname.split('/').pop() || 'remote_file';
        } catch {
            filename = url.split('/').pop() || 'remote_file';
        }
        const item: QueueItem = {
            id: Math.random().toString(36).substr(2, 9),
            path: filename,
            url: url.trim(),
            folderId: folderId,
            status: 'pending' as const,
        };
        setUploadQueue(prev => [...prev, item]);
        toast.info(`Queued remote upload from URL`);
    };

    return {
        uploadQueue,
        setUploadQueue,
        handleManualUpload,
        handleFolderUpload,
        handleDropUpload,
        handleUrlUpload,
        cancelAll,
        cancelItem,
        pauseItem,
        resumeItem,
        retryItem,
    };
}
