import { useState, useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { save, open } from '@tauri-apps/plugin-dialog';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { DownloadItem, TelegramFile } from '../types';
import { isAndroidPlatform, isIOSPlatform, showFileDialogFallback, pickWithFallback, sanitizeFilename } from '../utils';
import { useSettings } from '../context/SettingsContext';
import type { Store } from '@tauri-apps/plugin-store';

interface ProgressPayload {
    id: string;
    percent: number;
    uploaded_bytes: number;
    total_bytes: number;
    speed_bytes_per_sec: number;
}

interface FastTransferStatus {
    supported: boolean;
    runtime_installed: boolean;
    ready: boolean;
    auth_state: string;
    backend: string;
    detail: string;
}

export function useFileDownload(store: Store | null) {
    const [downloadQueue, setDownloadQueue] = useState<DownloadItem[]>([]);
    const [initialized, setInitialized] = useState(false);
    const cancelledRef = useRef<Set<string>>(new Set());
    const activeCountRef = useRef(0);
    const { settings } = useSettings();
    const fastReadyCheckRef = useRef<Promise<boolean> | null>(null);
    const persistedQueueRef = useRef('');

    const ensureFastTransferReady = async (): Promise<boolean> => {
        if (isAndroidPlatform) return true;
        if (fastReadyCheckRef.current) return fastReadyCheckRef.current;

        const check = (async () => {
            const current = await invoke<FastTransferStatus>('cmd_fast_transfer_status');
            if (!current.supported || current.ready) return true;

            const prepared = await invoke<FastTransferStatus>('cmd_fast_transfer_prepare_saved', {
                install: !current.runtime_installed,
            });
            if (prepared.ready) return true;

            throw new Error('TeraRelay could not prepare the transfer session. Restart TeraRelay or sign in again.');
        })();

        fastReadyCheckRef.current = check;
        try {
            return await check;
        } finally {
            if (fastReadyCheckRef.current === check) {
                fastReadyCheckRef.current = null;
            }
        }
    };

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

    // Load saved queue on mount
    useEffect(() => {
        if (!store || initialized) return;
        store.get<DownloadItem[]>('downloadQueue').then((saved) => {
            if (saved && saved.length > 0) {
                const pending = saved.filter(i => i.status === 'pending');
                if (pending.length > 0) {
                    setDownloadQueue(pending);
                    toast.info(`Restored ${pending.length} pending downloads`);
                }
            }
            setInitialized(true);
        });
    }, [store, initialized]);

    // Persist recoverable work. Active downloads are stored as pending so
    // an app restart can safely resume them from TDLib's partial cache.
    useEffect(() => {
        if (!store || !initialized) return;
        const recoverable = downloadQueue
            .filter(i => i.status === 'pending' || i.status === 'downloading')
            .map(i => ({
                id: i.id,
                messageId: i.messageId,
                filename: i.filename,
                folderId: i.folderId,
                savePath: i.savePath,
                status: 'pending' as const,
            }));
        const serialized = JSON.stringify(recoverable);
        if (serialized === persistedQueueRef.current) return;
        persistedQueueRef.current = serialized;
        store.set('downloadQueue', recoverable).then(() => store.save());
    }, [store, downloadQueue, initialized]);

    // Process up to maxConcurrentDownloads in parallel
    useEffect(() => {
        const maxConcurrent = settings.maxConcurrentDownloads || 1;
        const available = maxConcurrent - activeCountRef.current;
        if (available <= 0) return;
        const pendingItems = downloadQueue.filter(i => i.status === 'pending').slice(0, available);
        for (const item of pendingItems) {
            processItem(item);
        }
    }, [downloadQueue, settings.maxConcurrentDownloads]);

    const processItem = async (item: DownloadItem) => {
        activeCountRef.current++;
        setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'downloading', progress: 0 } : i));

        try {
            let savePath: string | null = item.savePath || null;
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
                    activeCountRef.current--;
                    return;
                }
            }

            if (savePath && savePath !== item.savePath) {
                setDownloadQueue(q => q.map(i =>
                    i.id === item.id ? { ...i, savePath } : i
                ));
            }

            const ready = await ensureFastTransferReady();
            if (!ready) {
                throw new Error('TDLIB_SETUP_CANCELLED');
            }

            await invoke('cmd_download_file', {
                req: {
                    message_id: item.messageId,
                    save_path: savePath,
                    folder_id: item.folderId,
                    transfer_id: item.id
                }
            });

            if (cancelledRef.current.has(item.id)) {
                cancelledRef.current.delete(item.id);
            } else {
                setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'success', progress: 100 } : i));
                window.setTimeout(() => {
                    setDownloadQueue(q => q.filter(i => !(i.id === item.id && i.status === 'success')));
                }, 5000);
                toast.success(`Downloaded: ${item.filename}`);
            }
        } catch (e) {
            if (!cancelledRef.current.has(item.id)) {
                const errMsg = String(e);
                if (errMsg.includes('Transfer cancelled') || errMsg.includes('TDLIB_SETUP_CANCELLED')) {
                    setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'cancelled' } : i));
                } else {
                    setDownloadQueue(q => q.map(i => i.id === item.id ? { ...i, status: 'error', error: errMsg } : i));
                    toast.error(`Download failed: ${item.filename}`);
                }
            } else {
                cancelledRef.current.delete(item.id);
            }
        } finally {
            activeCountRef.current--;
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

    const cancelAll = () => {
        setDownloadQueue(q => {
            const downloading = q.find(i => i.status === 'downloading');
            if (downloading) {
                cancelledRef.current.add(downloading.id);
                invoke('cmd_cancel_transfer', { transferId: downloading.id }).catch(() => {});
            }
            return q
                .filter(i => i.status !== 'pending')
                .map(i => i.status === 'downloading' ? { ...i, status: 'cancelled' as const } : i);
        });
        toast.info('All downloads cancelled');
    };

    const cancelItem = (id: string) => {
        setDownloadQueue(q => {
            const item = q.find(i => i.id === id);
            if (item?.status === 'downloading') {
                cancelledRef.current.add(id);
                invoke('cmd_cancel_transfer', { transferId: id }).catch(() => {});
                return q.map(i => i.id === id ? { ...i, status: 'cancelled' as const } : i);
            }
            if (item?.status === 'pending') {
                return q.filter(i => i.id !== id);
            }
            return q;
        });
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
        retryItem,
    };
}
