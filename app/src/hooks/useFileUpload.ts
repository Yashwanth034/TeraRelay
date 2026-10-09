import { useEffect, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { useQueryClient } from '@tanstack/react-query';
import { toast } from 'sonner';
import { QueueItem, TelegramFile, TransferEngine } from '../types';
import { isAndroidPlatform, isIOSPlatform, showFileDialogFallback, pickWithFallback } from '../utils';
import { useSettings } from '../context/SettingsContext';
import { useFastTransferAuth } from '../context/FastTransferAuthContext';
import { useTransferMethod } from '../context/TransferMethodContext';
import { useUploadSuggestion, type UploadPreflightResult } from '../context/UploadSuggestionContext';
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

interface DriveStagedPayload {
    pending_id: string;
    path: string;
    folder_id: number;
    file_name: string;
}

interface DrivePendingCancelledPayload {
    pending_id: string;
}

interface DriveSnapshotPayload {
    pending: Array<{
        id: string;
        display_name: string;
        staging_path: string;
        backing_channel_id: number;
    }>;
}

interface UploadFileResult {
    message: string;
    logical_file_id?: string | null;
}

export interface VersionUploadTarget {
    stackId?: string;
    baseFileId?: string;
    makePrimary?: boolean;
}

export function useFileUpload(
    activeFolderId: number | null,
    store: Store | null,
    knownFiles: TelegramFile[] = [],
) {
    const queryClient = useQueryClient();
    const { settings } = useSettings();
    // App.tsx sets this runtime flag only after Vite confirms an explicit
    // dev-only Feature A QA build. Undefined is always treated as production.
    const qaFeatureATest =
        (globalThis as typeof globalThis & { __TERARELAY_QA_FEATURE_A__?: boolean })
            .__TERARELAY_QA_FEATURE_A__ === true;
    const { queue: uploadQueue, setQueue: setUploadQueue, queueRef, initialized, durable, persist } =
        useRecoverableTransferQueue('upload', store, normalizeUploadQueue);
    const cancelledRef = useRef<Set<string>>(new Set());
    const pausedRef = useRef<Set<string>>(new Set());
    const activeIdsRef = useRef<Set<string>>(new Set());
    const { ensureFastTransferReady } = useFastTransferAuth();
    const { chooseTransferMethod } = useTransferMethod();
    const { reviewUploadSuggestion } = useUploadSuggestion();

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

    // Linux TeraRelay Drive writes land in a bounded staging area first. The
    // mounted filesystem emits a durable pending ID on close; feed that exact
    // record into the normal recoverable upload queue so app restarts, pause,
    // cancel, SHA-256 duplicate detection and transfer progress keep working.
    useEffect(() => {
        if (!initialized || isAndroidPlatform || isIOSPlatform) return;

        let disposed = false;
        let unlistenStaged: UnlistenFn | undefined;
        let unlistenCancelled: UnlistenFn | undefined;

        const enqueueDriveUpload = (payload: DriveStagedPayload) => {
            if (disposed) return;
            setUploadQueue(queue => {
                if (queue.some(item => item.drivePendingId === payload.pending_id)) {
                    return queue;
                }
                const item: QueueItem = {
                    id: `drive-${payload.pending_id}`,
                    path: payload.path,
                    folderId: payload.folder_id,
                    // Drive writes must never interrupt Nemo with a transfer
                    // chooser. Boost reuses the existing Grammers session and
                    // needs no second login.
                    transferEngine: 'boost',
                    status: 'pending',
                    drivePendingId: payload.pending_id,
                    driveFileName: payload.file_name,
                };
                return [...queue, item];
            });
        };

        void listen<DriveStagedPayload>('drive-upload-staged', event => {
            enqueueDriveUpload(event.payload);
        }).then(unlisten => {
            if (disposed) unlisten();
            else unlistenStaged = unlisten;
        });

        void listen<DrivePendingCancelledPayload>('drive-upload-cancelled', event => {
            const pendingId = event.payload.pending_id;
            const active = queueRef.current.find(item => item.drivePendingId === pendingId);
            if (active && activeIdsRef.current.has(active.id)) {
                cancelledRef.current.add(active.id);
                void invoke('cmd_cancel_transfer', { transferId: active.id }).catch(() => {});
            }
            setUploadQueue(queue => queue.filter(item => item.drivePendingId !== pendingId));
        }).then(unlisten => {
            if (disposed) unlisten();
            else unlistenCancelled = unlisten;
        });

        // The isolated QA harness intentionally keeps the original whole-file
        // staging path because it has no real Telegram session. Production
        // mounted-drive writes are finalized by Rust in bounded chunks and must
        // never be re-enqueued as a second whole-file frontend upload.
        const qaDriveUploadFallback = qaFeatureATest;
        if (qaDriveUploadFallback) {
            void invoke<DriveSnapshotPayload>('cmd_drive_get_snapshot')
                .then(snapshot => {
                    for (const pending of snapshot.pending) {
                        enqueueDriveUpload({
                            pending_id: pending.id,
                            path: pending.staging_path,
                            folder_id: pending.backing_channel_id,
                            file_name: pending.display_name,
                        });
                    }
                })
                .catch(error => {
                    console.warn('[Drive] Could not recover QA mounted-drive writes:', error);
                });
        }

        return () => {
            disposed = true;
            unlistenStaged?.();
            unlistenCancelled?.();
        };
    }, [initialized, setUploadQueue]);

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
        let workingItem = item;
        try {
            await persist();
            const queued = queueRef.current.find(i => i.id === item.id);
            if (queued?.status !== 'pending') return;
            workingItem = queued;
            started = true;

            // TeraRelay-channel uploads get one full-file SHA-256 before any bytes
            // are sent. The source identity travels with that hash so Rust can
            // reject a changed file instead of trusting a stale fingerprint.
            if (
                !workingItem.url
                && workingItem.folderId !== null
                && !isAndroidPlatform
                && !isIOSPlatform
                && (!workingItem.sourceSha256 || !workingItem.sourceIdentity)
            ) {
                const preflight = await invoke<UploadPreflightResult>('cmd_preflight_upload_candidate', {
                    path: workingItem.path,
                    folderId: workingItem.folderId,
                });

                if (preflight.exact_duplicate) {
                    if (workingItem.drivePendingId) {
                        // A mounted-drive copy of bytes we already own should
                        // create another normal filesystem entry without
                        // uploading those bytes again. entry_id is independent
                        // from logical_file_id, so this is a real copy/alias in
                        // the virtual tree rather than accidentally moving the
                        // existing path.
                        await invoke('cmd_drive_finalize_upload', {
                            pendingId: workingItem.drivePendingId,
                            logicalFileId: preflight.exact_duplicate.logical_file_id,
                        });
                        void invoke('cmd_drive_sync_metadata').catch(error => {
                            console.warn('[Drive] Metadata sync will retry later:', error);
                        });
                        const next = queueRef.current.filter(i => i.id !== workingItem.id);
                        setUploadQueue(next);
                        await persist(next);
                        toast.success(`${workingItem.driveFileName || preflight.file_name} added to TeraRelay Drive`);
                        return;
                    }

                    const decision = await reviewUploadSuggestion({
                        kind: 'duplicate',
                        incomingName: preflight.file_name,
                        incomingSize: preflight.size,
                        candidate: preflight.exact_duplicate,
                    });
                    if (decision !== 'upload_anyway') {
                        const next = queueRef.current.filter(i => i.id !== workingItem.id);
                        setUploadQueue(next);
                        await persist(next);
                        toast.info('Duplicate upload skipped');
                        return;
                    }
                } else if (!workingItem.versionStackId && !workingItem.versionBaseFileId && !workingItem.drivePendingId) {
                    const versionMatch = preflight.similar_files
                        .map(candidate => ({
                            candidate,
                            file: knownFiles.find(file =>
                                file.logical_file_id === candidate.logical_file_id
                            ),
                        }))
                        .find(match => !!match.file);

                    if (versionMatch?.file) {
                        const decision = await reviewUploadSuggestion({
                            kind: 'version',
                            incomingName: preflight.file_name,
                            incomingSize: preflight.size,
                            candidate: versionMatch.candidate,
                            stackName: versionMatch.file.stack_name,
                        });

                        if (decision === 'cancel') {
                            const next = queueRef.current.filter(i => i.id !== workingItem.id);
                            setUploadQueue(next);
                            await persist(next);
                            return;
                        }

                        if (decision === 'add_version') {
                            if (versionMatch.file.stack_id) {
                                workingItem = {
                                    ...workingItem,
                                    versionStackId: versionMatch.file.stack_id,
                                    versionBaseFileId: undefined,
                                };
                            } else {
                                workingItem = {
                                    ...workingItem,
                                    versionBaseFileId: versionMatch.candidate.logical_file_id,
                                    versionStackId: undefined,
                                };
                            }
                        }
                    }
                }

                workingItem = {
                    ...workingItem,
                    sourceSha256: preflight.source_sha256,
                    sourceIdentity: preflight.source_identity,
                };
                const next = queueRef.current.map(current =>
                    current.id === workingItem.id ? workingItem : current
                );
                setUploadQueue(next);
                // The fingerprint and any selected post-upload stack action are
                // recovery metadata. Commit them before transport starts.
                await persist(next);
            }

            const initialStatus = workingItem.url ? 'downloading' : 'uploading';
            setUploadQueue(q => q.map(i => i.id === workingItem.id ? {
                ...i,
                status: initialStatus,
                error: undefined,
                progress: 0,
                speedBytesPerSec: undefined,
                uploadPhase: workingItem.url ? undefined : 'uploading',
            } : i));

            if (workingItem.url) {
                await invoke('cmd_upload_from_url', {
                    url: workingItem.url,
                    folderId: workingItem.folderId,
                    transferId: workingItem.id,
                });
            } else {
                const transferEngine = workingItem.transferEngine;
                // New Boost transfers bypass TDLib entirely. Legacy queue items
                // intentionally have no engine field: keep their historic
                // readiness behavior and let Rust choose the old platform path.
                if (transferEngine !== 'boost' && !qaFeatureATest) {
                    const ready = await ensureFastTransferReady();
                    if (!ready) throw new Error('TDLIB_SETUP_CANCELLED');
                }
                if (cancelledRef.current.has(workingItem.id) || pausedRef.current.has(workingItem.id)) {
                    throw new Error('Transfer cancelled');
                }

                const uploadResult = await invoke<UploadFileResult>('cmd_upload_file', {
                    path: workingItem.path,
                    folderId: workingItem.folderId,
                    transferId: workingItem.id,
                    transferEngine,
                    sourceSha256: workingItem.sourceSha256,
                    sourceIdentity: workingItem.sourceIdentity,
                });
                completed = true;

                if (workingItem.drivePendingId) {
                    if (!uploadResult.logical_file_id) {
                        throw new Error('Upload completed, but TeraRelay Drive did not receive a stable logical file ID');
                    }
                    await invoke('cmd_drive_finalize_upload', {
                        pendingId: workingItem.drivePendingId,
                        logicalFileId: uploadResult.logical_file_id,
                    });
                    // Local filesystem state is already durable. Cross-device
                    // metadata publication is retried by the background Drive
                    // sync loop if Telegram is temporarily unavailable.
                    void invoke('cmd_drive_sync_metadata').catch(error => {
                        console.warn('[Drive] Metadata sync will retry later:', error);
                    });
                }

                // Version grouping is deliberately a post-upload metadata step.
                // If it fails, the uploaded file remains intact and visible as a
                // normal file; retrying the queue must never upload duplicate bytes.
                if (workingItem.versionStackId || workingItem.versionBaseFileId) {
                    if (workingItem.folderId === null || !uploadResult.logical_file_id) {
                        toast.error('Upload completed, but this file could not be attached as a version. It remains safely available as a normal file.');
                    } else {
                        try {
                            if (workingItem.versionStackId) {
                                await invoke('cmd_add_file_to_stack', {
                                    folderId: workingItem.folderId,
                                    stackId: workingItem.versionStackId,
                                    fileId: uploadResult.logical_file_id,
                                    makePrimary: workingItem.versionMakePrimary === true,
                                });
                            } else if (workingItem.versionBaseFileId) {
                                await invoke('cmd_create_file_stack', {
                                    folderId: workingItem.folderId,
                                    baseFileId: workingItem.versionBaseFileId,
                                    versionFileId: uploadResult.logical_file_id,
                                    primaryFileId: workingItem.versionMakePrimary === true
                                        ? uploadResult.logical_file_id
                                        : workingItem.versionBaseFileId,
                                });
                            }
                            toast.success('New version added');
                        } catch (stackError) {
                            toast.error(`Upload completed, but version grouping failed: ${stackError}. The file remains separate and safe.`);
                        }
                    }
                }
            }

            completed = true;
            if (cancelledRef.current.has(workingItem.id)) {
                cancelledRef.current.delete(workingItem.id);
                await persist();
                const current = queueRef.current.find(i => i.id === workingItem.id);
                if (!current || current.status === 'cancelled') await cleanupTempZip(workingItem);
            } else {
                pausedRef.current.delete(workingItem.id);
                setUploadQueue(q => q.map(i => i.id === workingItem.id
                    ? { ...i, status: 'success', progress: 100 }
                    : i));
                // Keep the source until removing recoverable work has really committed.
                await persist();
                await cleanupTempZip(workingItem);
                window.setTimeout(() => {
                    setUploadQueue(q => q.filter(i => !(i.id === workingItem.id && i.status === 'success')));
                }, 5000);
                queryClient.invalidateQueries({ queryKey: ['files', workingItem.folderId] });
            }
        } catch (e) {
            if (!started) {
                toast.error(`Could not save upload queue: ${e}`);
            } else if (pausedRef.current.has(workingItem.id)) {
                pausedRef.current.delete(workingItem.id);
                setUploadQueue(q => q.map(i => i.id === workingItem.id ? {
                    ...i, status: 'paused', speedBytesPerSec: 0,
                } : i));
            } else if (cancelledRef.current.has(workingItem.id)) {
                cancelledRef.current.delete(workingItem.id);
                try {
                    await persist();
                    const current = queueRef.current.find(i => i.id === workingItem.id);
                    if (!current || current.status === 'cancelled') await cleanupTempZip(workingItem);
                } catch (saveError) {
                    setUploadQueue(q => q.map(i => i.id === workingItem.id ? {
                        ...i, status: 'error', error: `Could not save cancellation: ${saveError}`, speedBytesPerSec: 0,
                    } : i));
                }
            } else {
                const errMsg = completed ? `Upload completed, but queue save failed: ${e}` : String(e);
                if (!completed && errMsg.includes('TDLIB_SETUP_CANCELLED')) {
                    setUploadQueue(q => q.map(i => i.id === workingItem.id ? {
                        ...i, status: 'paused',
                        error: 'Transfer setup was cancelled. Resume when you are ready to finish setup.',
                        speedBytesPerSec: 0,
                    } : i));
                } else if (!completed && errMsg.includes('Transfer cancelled')) {
                    setUploadQueue(q => q.map(i => i.id === workingItem.id
                        ? { ...i, status: 'cancelled', speedBytesPerSec: 0 }
                        : i));
                } else {
                    setUploadQueue(q => q.map(i => i.id === workingItem.id ? {
                        ...i, status: 'error', error: errMsg, speedBytesPerSec: 0,
                    } : i));
                    if (workingItem.url && (errMsg.includes('FILE_TOO_BIG') || errMsg.includes('too large') || errMsg.includes('2 GB') || errMsg.includes('2GB'))) {
                        toast.error('Upload failed: URL uploads are limited to 2 GB.');
                    } else {
                        const displayPath = workingItem.url || workingItem.path;
                        toast.error(`Upload failed for ${displayPath.split('/').pop()}: ${errMsg}`);
                    }
                }
                // Failed and auth-cancelled uploads retain folder ZIPs for Retry/restart.
            }
        } finally {
            activeIdsRef.current.delete(workingItem.id);
            setUploadQueue(q => [...q]);
        }
    };

    /** Queues a set of file paths for upload using one explicitly chosen engine. */
    const queueFiles = async (paths: string[], selectedEngine?: TransferEngine) => {
        if (!paths || paths.length === 0) return;
        const transferEngine = selectedEngine ?? await chooseTransferMethod('upload');
        if (!transferEngine) return;
        const newItems: QueueItem[] = paths.map((path: string) => ({
            id: Math.random().toString(36).substr(2, 9),
            path,
            folderId: activeFolderId,
            transferEngine,
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
            await queueFiles(paths);
        }
    };

    const handleManualUpload = performManualUpload;

    /**
     * Upload exactly one file as a new version. The normal resilient upload
     * queue/TDLib path is reused; only the post-upload metadata action differs.
     */
    const handleVersionUpload = async (target: VersionUploadTarget) => {
        if (activeFolderId === null) {
            toast.info('File Versions are available inside TeraRelay channels.');
            return;
        }
        if ((!target.stackId && !target.baseFileId) || (target.stackId && target.baseFileId)) {
            toast.error('Invalid version target');
            return;
        }

        const selected = await pickWithFallback(
            async () => {
                const value = await open({ multiple: false, directory: false, title: 'Choose New Version' });
                if (!value) return null;
                return Array.isArray(value) ? value[0] || null : value;
            },
            () => handleVersionUpload(target),
            {
                errorTitle: 'File picker failed',
                onBrowserPicker: async () => {
                    const paths = await showFileDialogFallback({ directory: false, multiple: false });
                    return paths[0] || null;
                },
            },
        );
        if (!selected) return;

        const item: QueueItem = {
            id: Math.random().toString(36).substr(2, 9),
            path: selected,
            folderId: activeFolderId,
            status: 'pending',
            versionStackId: target.stackId,
            versionBaseFileId: target.baseFileId,
            versionMakePrimary: target.makePrimary === true,
        };
        setUploadQueue(prev => [...prev, item]);
        toast.info('New version queued for upload');
    };

    /** Queue files dropped from the OS file manager (drag-and-drop upload) */
    const handleDropUpload = (paths: string[]) => {
        if (!paths || paths.length === 0) return;
        void queueFiles(paths);
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
                        await queueFiles(fallbackPaths);
                    }
                    return null; // Already handled via queueFiles — signal that the main flow should stop
                },
            },
        );
        if (!folderPath) return;

        const transferEngine = await chooseTransferMethod('upload');
        if (!transferEngine) return;

        const folderName = folderPath.split('/').pop() || folderPath.split('\\').pop() || 'folder';

        toast.info(`Zipping "${folderName}"...`);
        try {
            const zipPath = await invoke<string>('cmd_zip_folder', { folderPath });
            const item: QueueItem = {
                id: Math.random().toString(36).substr(2, 9),
                path: zipPath,
                folderId: activeFolderId,
                transferEngine,
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

    const dismissItem = (id: string) => {
        const item = queueRef.current.find(i => i.id === id);
        if (!item || !['success', 'error', 'cancelled'].includes(item.status)) return;
        const next = queueRef.current.filter(i => i.id !== id);
        setUploadQueue(next);
        void persist(next)
            .then(() => cleanupTempZip(item))
            .catch(error => {
                setUploadQueue(current => current.some(i => i.id === id) ? current : [...current, item]);
                toast.error(`Could not dismiss upload: ${error}`);
            });
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
        handleVersionUpload,
        handleDropUpload,
        handleUrlUpload,
        cancelAll,
        cancelItem,
        pauseItem,
        resumeItem,
        retryItem,
        dismissItem,
    };
}
