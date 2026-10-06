import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { save, open } from '@tauri-apps/plugin-dialog';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { DownloadItem, TelegramFile } from '../types';
import { isAndroidPlatform, isIOSPlatform, showFileDialogFallback, pickWithFallback, sanitizeFilename } from '../utils';
import { useSettings } from '../context/SettingsContext';
import { useFastTransferAuth } from '../context/FastTransferAuthContext';
import type { Store } from '@tauri-apps/plugin-store';
import { normalizeDownloadQueue, useRecoverableTransferQueue } from '../transferQueue';

interface ProgressPayload {
    id: string;
    percent: number;
    uploaded_bytes: number;
    total_bytes: number;
    speed_bytes_per_sec: number;
}

export function useFileDownload(store: Store | null) {
    const { queue: downloadQueue, setQueue: setDownloadQueue, queueRef, initialized, durable, persist } =
        useRecoverableTransferQueue('download', store, normalizeDownloadQueue);
    const cancelledRef = useRef<Set<string>>(new Set());
    const pausedRef = useRef<Set<string>>(new Set());
    const activeIdsRef = useRef<Set<string>>(new Set());
    const { settings } = useSettings();
    const { ensureFastTransferReady } = useFastTransferAuth();

    // Listen for progress events from Rust
    useEffect(() => {
        let unlisten: UnlistenFn | undefined;
        listen<ProgressPayload>('download-progress', (event) => {
            setDownloadQueue(q => q.map(i =>
                i.id === event.payload.id ? {
                    ...i,
                    progress: event.payload.percent,
                    downloadedBytes: event.payload.uploaded_bytes,
                    totalBytes: event.payload.total_bytes,
                    speedBytesPerSec: event.payload.speed_bytes_per_sec,
                } : i
            ));
        }).then(fn => { unlisten = fn; });
        return () => { unlisten?.(); };
    }, []);

    useEffect(() => {
        if (!initialized || !durable) return;
        const maxConcurrent = settings.maxConcurrentDownloads || 1;
        const available = maxConcurrent - activeIdsRef.current.size;
        if (available <= 0) return;
        const pendingItems = downloadQueue
            .filter(i => i.status === 'pending' && !activeIdsRef.current.has(i.id))
            .slice(0, available);
        for (const item of pendingItems) {
            void processItem(item);
        }
    }, [downloadQueue, settings.maxConcurrentDownloads, initialized, durable]);

    const processItem = async (item: DownloadItem) => {
        if (activeIdsRef.current.has(item.id)) return;
        activeIdsRef.current.add(item.id);
        let started = false;
        let completed = false;
        let savePath: string | null = item.savePath || null;
        try {
            await persist();
            if (queueRef.current.find(i => i.id === item.id)?.status !== 'pending') return;
            started = true;
            setDownloadQueue(q => q.map(i => i.id === item.id ? {
                ...i, status: 'downloading', error: undefined, progress: 0, speedBytesPerSec: undefined,
            } : i));
            if (!savePath) {
                savePath = await pickWithFallback(
                    () => save({ defaultPath: item.filename }),
                    () => {
                        setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'pending' as const, error: undefined } : i));
                    },
                    { errorTitle: 'Save dialog failed' },
                );
                if (!savePath) {
                    setDownloadQueue(q => q.filter(i => i.id !== item.id));
                    await persist();
                    return;
                }
            }

            if (savePath !== item.savePath) {
                const destination = savePath;
                setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, savePath: destination } : i));
                // Destination must survive reopening before TDLib receives any work.
                await persist();
            }
            const ready = await ensureFastTransferReady();
            if (!ready) throw new Error('TDLIB_SETUP_CANCELLED');
            if (cancelledRef.current.has(item.id) || pausedRef.current.has(item.id)) {
                throw new Error('Transfer cancelled');
            }

            await invoke('cmd_download_file', {
                req: {
                    message_id: item.messageId,
                    save_path: savePath,
                    folder_id: item.folderId,
                    transfer_id: item.id
                }
            });
            completed = true;
            if (cancelledRef.current.has(item.id)) {
                cancelledRef.current.delete(item.id);
                await persist();
            } else {
                pausedRef.current.delete(item.id);
                setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'success', progress: 100 } : i));
                await persist();
                window.setTimeout(() => {
                    setDownloadQueue(q => q.filter(i => !(i.id === item.id && i.status === 'success')));
                }, 5000);
                toast.success(`Downloaded: ${item.filename}`);
            }
        } catch (e) {
            if (!started) {
                toast.error(`Could not save download queue: ${e}`);
            } else if (pausedRef.current.has(item.id)) {
                pausedRef.current.delete(item.id);
                setDownloadQueue(q => q.map(i => i.id === item.id ? {
                    ...i, status: 'paused', speedBytesPerSec: 0,
                } : i));
            } else if (cancelledRef.current.has(item.id)) {
                cancelledRef.current.delete(item.id);
            } else {
                const errMsg = completed ? `Download completed, but queue save failed: ${e}` : String(e);
                if (!completed && errMsg.includes('TDLIB_SETUP_CANCELLED')) {
                    setDownloadQueue(q => q.map(i => i.id === item.id ? {
                        ...i, status: 'paused',
                        error: 'Transfer setup was cancelled. Resume when you are ready to finish setup.',
                        speedBytesPerSec: 0,
                    } : i));
                } else if (!completed && errMsg.includes('Transfer cancelled')) {
                    setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'cancelled', speedBytesPerSec: 0 } : i));
                } else {
                    setDownloadQueue(q => {
                        const failed = { ...item, savePath: savePath || undefined, status: 'error' as const, error: errMsg, speedBytesPerSec: 0 };
                        return q.some(i => i.id === item.id)
                            ? q.map(i => i.id === item.id ? { ...i, ...failed } : i)
                            : [...q, failed];
                    });
                    toast.error(`Download failed: ${item.filename}`);
                }
            }
        } finally {
            // Every path (including save-dialog cancellation) releases exactly once.
            activeIdsRef.current.delete(item.id);
            setDownloadQueue(q => [...q]);
        }
    };

    const queueDownload = (messageId: number, filename: string, folderId: number | null) => {
        const newItem: DownloadItem = {
            id: Math.random().toString(36).substr(2, 9),
            messageId,
            filename: sanitizeFilename(filename),
            folderId,
            status: 'pending'
        };
        setDownloadQueue(prev => [...prev, newItem]);
    };

    const queueBulkDownload = async (files: TelegramFile[], folderId: number | null) => {
        // Mobile platforms do not support a directory picker here.
        // Resolve each destination up front, sequentially, so queue workers never
        // open overlapping native save dialogs.
        if (isAndroidPlatform || isIOSPlatform) {
            const newItems: DownloadItem[] = [];
            for (const file of files) {
                const filename = sanitizeFilename(file.name);
                const savePath = await save({ defaultPath: filename });
                if (!savePath) continue;
                newItems.push({
                    id: Math.random().toString(36).substr(2, 9),
                    messageId: file.id,
                    filename,
                    folderId,
                    savePath,
                    status: 'pending' as const,
                });
            }
            if (newItems.length > 0) {
                setDownloadQueue(prev => [...prev, ...newItems]);
                toast.info(`Queued ${newItems.length} file${newItems.length !== 1 ? 's' : ''} for download`);
            }
            return;
        }

        const enqueueFiles = (dir: string) => {
            const separator = dir.includes('\\') ? '\\' : '/';
            const newItems: DownloadItem[] = files.map(file => {
                const sanitizedName = sanitizeFilename(file.name);
                return {
                    id: Math.random().toString(36).substr(2, 9),
                    messageId: file.id,
                    filename: sanitizedName,
                    folderId,
                    status: 'pending' as const,
                    savePath: dir.endsWith(separator) ? `${dir}${sanitizedName}` : `${dir}${separator}${sanitizedName}`
                };
            });
            setDownloadQueue(prev => [...prev, ...newItems]);
            toast.info(`Queued ${files.length} files for download`);
        };

        const dirPath = await pickWithFallback(
            () => open({ directory: true, multiple: false, title: "Select Download Destination" }),
            () => queueBulkDownload(files, folderId),
            {
                errorTitle: 'Folder picker failed',
                onBrowserPicker: async () => {
                    const paths = await showFileDialogFallback({ directory: true, multiple: false });
                    if (paths.length === 0) return null;
                    const sep = paths[0].includes('\\') ? '\\' : '/';
                    return paths[0].substring(0, paths[0].lastIndexOf(sep));
                },
            },
        );
        if (!dirPath) return;

        enqueueFiles(dirPath);
    };

    const clearFinished = () => {
        setDownloadQueue(q => q.filter(i => i.status !== 'success'));
    };

    const finishCancellation = async (items: DownloadItem[]) => {
        try {
            await persist();
        } catch (e) {
            setDownloadQueue(q => {
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
            toast.error(`Could not save download cancellation: ${e}`);
        }
    };

    const cancelAll = () => {
        const items = queueRef.current.filter(i =>
            i.status === 'pending' || i.status === 'downloading' || i.status === 'pausing' || i.status === 'paused');
        for (const item of items) {
            if (activeIdsRef.current.has(item.id)) {
                pausedRef.current.delete(item.id);
                cancelledRef.current.add(item.id);
                invoke('cmd_cancel_transfer', { transferId: item.id }).catch(() => {});
            }
        }
        setDownloadQueue(q => q
            .filter(i => i.status !== 'pending')
            .map(i => (i.status === 'downloading' || i.status === 'pausing' || i.status === 'paused')
                ? { ...i, status: 'cancelled' as const, speedBytesPerSec: 0 }
                : i));
        void finishCancellation(items);
        toast.info('All downloads cancelled');
    };

    const cancelItem = (id: string) => {
        const item = queueRef.current.find(i => i.id === id);
        if (!item || !['pending', 'downloading', 'pausing', 'paused'].includes(item.status)) return;
        if (activeIdsRef.current.has(id)) {
            pausedRef.current.delete(id);
            cancelledRef.current.add(id);
            invoke('cmd_cancel_transfer', { transferId: id }).catch(() => {});
        }
        setDownloadQueue(q => item.status === 'pending'
            ? q.filter(i => i.id !== id)
            : q.map(i => i.id === id ? { ...i, status: 'cancelled' as const, speedBytesPerSec: 0 } : i));
        void finishCancellation([item]);
    };

    const pauseItem = (id: string) => {
        setDownloadQueue(q => {
            const item = q.find(i => i.id === id);
            if (item?.status !== 'downloading') return q;
            pausedRef.current.add(id);
            invoke('cmd_cancel_transfer', { transferId: id }).catch(() => {});
            return q.map(i => i.id === id
                ? { ...i, status: 'pausing' as const, speedBytesPerSec: 0 }
                : i);
        });
    };

    const resumeItem = (id: string) => {
        setDownloadQueue(q => q.map(i =>
            i.id === id && i.status === 'paused'
                ? { ...i, status: 'pending' as const, error: undefined, speedBytesPerSec: 0 }
                : i
        ));
    };

    const retryItem = (id: string) => {
        setDownloadQueue(q => q.map(i =>
            i.id === id && (i.status === 'error' || i.status === 'cancelled')
                ? { ...i, status: 'pending' as const, error: undefined, progress: undefined, downloadedBytes: undefined, totalBytes: undefined, speedBytesPerSec: undefined }
                : i
        ));
    };

    return {
        downloadQueue,
        queueDownload,
        queueBulkDownload,
        clearFinished,
        cancelAll,
        cancelItem,
        pauseItem,
        resumeItem,
        retryItem,
    };
}
