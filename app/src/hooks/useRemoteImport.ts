import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { Store } from '@tauri-apps/plugin-store';
import { useQueryClient } from '@tanstack/react-query';
import {
    buildRemoteImportSelections,
    defaultRemoteImportSelectionIds,
    loadPersistedRemoteImportJobs,
    normalizeBackendImportPhase,
    normalizeRecoveredRemoteImportJobs,
    persistRemoteImportJobs,
    planRemoteImportFinalization,
    subscribeRemoteImportUploadOutcomes,
    type PreparedImportInput,
    type RemoteImportAcquireResult,
    type RemoteImportAnalysis,
    type RemoteImportEngineStatus,
    type RemoteImportJob,
    type RemoteImportProgress,
} from '../remoteImport';
export { REMOTE_IMPORT_STORE_KEY } from '../remoteImport';

type EnqueuePreparedImports = (inputs: PreparedImportInput[], options?: { isCancelled: () => boolean }) => Promise<string[]>;

type JobsUpdater = RemoteImportJob[] | ((current: RemoteImportJob[]) => RemoteImportJob[]);

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

function remoteImportJobError(message: string, jobId: string): Error & { remoteImportJobId: string } {
    return Object.assign(new Error(message), { remoteImportJobId: jobId });
}

function newJobId(): string {
    const uuid = globalThis.crypto?.randomUUID?.();
    if (uuid) return `import-${uuid}`;
    return `import-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 11)}`;
}

function preparedInputs(job: RemoteImportJob): PreparedImportInput[] {
    return job.outputs.map(output => ({
        path: output.staging_path,
        folderId: job.folderId,
        uploadName: output.display_name,
        remoteImportJobId: job.id,
        remoteImportSelectionId: output.selection_id,
        remoteImportLabel: output.label,
        remoteImportHeight: output.height,
    }));
}

export function useRemoteImport(
    activeFolderId: number | null,
    store: Store | null,
    enqueuePreparedImports: EnqueuePreparedImports,
    cancelUploadItem?: (queueId: string) => void,
) {
    const [jobs, setJobsState] = useState<RemoteImportJob[]>([]);
    const [initialized, setInitialized] = useState(false);
    const jobsRef = useRef<RemoteImportJob[]>([]);
    const finalizingRef = useRef<Set<string>>(new Set());
    const cancelledRef = useRef<Set<string>>(new Set());
    const activeAcquisitionsRef = useRef<Set<string>>(new Set());
    const activeEnqueuesRef = useRef<Set<string>>(new Set());
    const jobMutationTailRef = useRef<Promise<unknown>>(Promise.resolve());
    const queryClient = useQueryClient();

    const persistSnapshot = useCallback((snapshot: RemoteImportJob[]) => {
        return persistRemoteImportJobs(store, snapshot);
    }, [store]);

    const setJobs = useCallback((updater: JobsUpdater): Promise<RemoteImportJob[]> => {
        const task = jobMutationTailRef.current.catch(() => undefined).then(async () => {
            const next = typeof updater === 'function' ? updater(jobsRef.current) : updater;
            const persisted = await persistSnapshot(next);
            // persistRemoteImportJobs merges upload outcomes/stack progress that may
            // have landed while this mutation was waiting. Adopt that merged copy
            // so the orchestrator never waits on a stale in-memory snapshot.
            jobsRef.current = persisted;
            setJobsState(persisted);
            return persisted;
        });
        jobMutationTailRef.current = task;
        return task;
    }, [persistSnapshot]);

    const updateJob = useCallback((jobId: string, update: (job: RemoteImportJob) => RemoteImportJob) => {
        return setJobs(current => current.map(job => {
            if (job.id !== jobId) return job;
            const next = update(job);
            return cancelledRef.current.has(jobId) && next.phase !== 'cancelled'
                ? { ...job, phase: 'cancelled' as const, error: undefined, progress: undefined } : next;
        }));
    }, [setJobs]);

    useEffect(() => {
        let disposed = false;
        if (!store) {
            jobsRef.current = [];
            setJobsState([]);
            setInitialized(true);
            return () => { disposed = true; };
        }
        setInitialized(false);
        void loadPersistedRemoteImportJobs(store)
            .then(raw => {
                if (disposed) return;
                const recovered = normalizeRecoveredRemoteImportJobs(raw);
                jobsRef.current = recovered;
                setJobsState(recovered);
                setInitialized(true);
                return persistSnapshot(recovered);
            })
            .catch(error => {
                if (disposed) return;
                console.error('[RemoteImport] Could not restore jobs:', error);
                setInitialized(true);
            });
        return () => { disposed = true; };
    }, [store, persistSnapshot]);

    useEffect(() => {
        let disposed = false;
        let unlisten: UnlistenFn | undefined;
        void listen<RemoteImportProgress>('remote-import-progress', event => {
            if (disposed) return;
            const payload = event.payload;
            const current = jobsRef.current.find(job => job.id === payload.job_id);
            if (!current || cancelledRef.current.has(current.id) || !['analyzing', 'downloading', 'processing', 'ready-to-upload'].includes(current.phase)) return;
            const normalizedPhase = normalizeBackendImportPhase(payload.phase);
            // Progress ticks stay in memory; phase transitions are already made
            // durable before starting each backend operation, avoiding disk writes
            // every 500ms while a large download runs.
            const next = jobsRef.current.map(job => job.id === payload.job_id ? {
                ...job,
                ...(normalizedPhase ? { phase: normalizedPhase } : {}),
                progress: payload,
                updatedAt: Date.now(),
            } : job);
            jobsRef.current = next;
            setJobsState(next);
        }).then(value => {
            if (disposed) value();
            else unlisten = value;
        });
        return () => {
            disposed = true;
            unlisten?.();
        };
    }, []);

    useEffect(() => subscribeRemoteImportUploadOutcomes((jobId, outcome) => {
        const next = jobsRef.current.map(job => job.id === jobId ? {
            ...job,
            uploadOutcomes: {
                ...(job.uploadOutcomes ?? {}),
                [outcome.queueId]: outcome,
            },
            updatedAt: Date.now(),
        } : job);
        jobsRef.current = next;
        setJobsState(next);
    }), []);

    const getJob = useCallback((jobId: string) => jobsRef.current.find(job => job.id === jobId), []);

    const finalizeCompletedJob = useCallback(async (
        job: RemoteImportJob,
        options?: { allowStack?: boolean },
    ) => {
        const plan = planRemoteImportFinalization(job);
        if (!plan.ready || finalizingRef.current.has(job.id)) return;
        finalizingRef.current.add(job.id);
        try {
            await updateJob(job.id, current => ({
                ...current,
                phase: 'finalizing',
                error: undefined,
                updatedAt: Date.now(),
            }));

            let current = jobsRef.current.find(candidate => candidate.id === job.id) ?? job;
            let stackId = current.stackId;
            const stackedFileIds = new Set(current.stackedFileIds ?? []);

            if (options?.allowStack !== false && plan.shouldStack) {
                if (!stackId) {
                    const [base, version] = plan.available;
                    stackId = await invoke<string>('cmd_create_file_stack', {
                        folderId: job.folderId,
                        baseFileId: base.logicalFileId,
                        versionFileId: version.logicalFileId,
                        primaryFileId: plan.primary?.logicalFileId ?? base.logicalFileId,
                    });
                    stackedFileIds.add(base.logicalFileId);
                    stackedFileIds.add(version.logicalFileId);
                    await updateJob(job.id, value => ({
                        ...value,
                        stackId,
                        stackedFileIds: [...stackedFileIds],
                        updatedAt: Date.now(),
                    }));
                }

                for (const member of plan.available) {
                    if (!stackedFileIds.has(member.logicalFileId)) {
                        await invoke('cmd_add_file_to_stack', {
                            folderId: job.folderId,
                            stackId,
                            fileId: member.logicalFileId,
                            makePrimary: false,
                        });
                        stackedFileIds.add(member.logicalFileId);
                        await updateJob(job.id, value => ({
                            ...value,
                            stackId,
                            stackedFileIds: [...stackedFileIds],
                            updatedAt: Date.now(),
                        }));
                    }
                    await invoke('cmd_update_file_stack_label', {
                        folderId: job.folderId,
                        stackId,
                        fileId: member.logicalFileId,
                        label: member.label ?? null,
                    });
                }

                if (plan.primary) {
                    await invoke('cmd_set_file_stack_primary', {
                        folderId: job.folderId,
                        stackId,
                        fileId: plan.primary.logicalFileId,
                    });
                }
            }

            const terminalError = plan.terminalPhase === 'partial-error'
                ? `${plan.failures.length} selected import${plan.failures.length === 1 ? '' : 's'} did not finish. Successful files remain safe and can be completed by retrying the failed uploads.`
                : plan.terminalPhase === 'error'
                    ? 'Remote Import did not produce a successful upload. Retry the failed upload(s).'
                    : undefined;
            await updateJob(job.id, value => ({
                ...value,
                phase: plan.terminalPhase,
                stackId,
                stackedFileIds: stackId ? [...stackedFileIds] : value.stackedFileIds,
                error: terminalError,
                progress: undefined,
                updatedAt: Date.now(),
            }));

            await queryClient.invalidateQueries({ queryKey: ['files', job.folderId] });
            if (plan.terminalPhase === 'success') {
                await invoke('cmd_remote_import_cleanup', { jobId: job.id });
            }
            current = jobsRef.current.find(candidate => candidate.id === job.id) ?? current;
        } catch (error) {
            const message = `Uploads are safe, but Remote Import finalization failed: ${errorMessage(error)}`;
            await updateJob(job.id, current => ({
                ...current,
                phase: plan.available.length > 0 ? 'partial-error' : 'error',
                error: message,
                updatedAt: Date.now(),
            })).catch(() => {});
        } finally {
            finalizingRef.current.delete(job.id);
        }
    }, [queryClient, updateJob]);

    useEffect(() => {
        if (!initialized) return;
        for (const job of jobs) {
            if (job.phase !== 'uploading' && job.phase !== 'finalizing' && job.phase !== 'partial-error') continue;
            if (job.deferAutomaticFinalization) continue;
            const plan = planRemoteImportFinalization(job);
            if (plan.ready) void finalizeCompletedJob(job);
        }
    }, [jobs, initialized, finalizeCompletedJob]);

    const prepareSource = useCallback(async (
        sourceInput: string,
        options?: { folderId?: number },
    ): Promise<RemoteImportJob> => {
        if (!initialized) throw new Error('Remote Import is still restoring saved jobs. Try again in a moment.');
        const source = sourceInput.trim();
        if (!source) throw new Error('Paste a link to import');
        const folderId = options?.folderId ?? activeFolderId;
        if (folderId === null || folderId === undefined) {
            throw new Error('Open a writable TeraRelay channel first');
        }
        const now = Date.now();
        const initial: RemoteImportJob = {
            id: newJobId(),
            source,
            folderId,
            phase: 'analyzing',
            selectedIds: [],
            outputs: [],
            uploadQueueIds: [],
            createdAt: now,
            updatedAt: now,
        };
        await setJobs(current => [...current, initial]);
        return getJob(initial.id) ?? initial;
    }, [activeFolderId, getJob, initialized, setJobs]);

    const analyzeSource = useCallback(async (
        sourceInput: string,
        options?: { defaultSelection?: boolean },
    ): Promise<RemoteImportJob> => {
        if (!initialized) throw new Error('Remote Import is still restoring saved jobs. Try again in a moment.');
        const source = sourceInput.trim();
        if (!source) throw new Error('Paste a link to import');
        if (activeFolderId === null) throw new Error('Open a writable TeraRelay channel first');

        const now = Date.now();
        const initial: RemoteImportJob = {
            id: newJobId(),
            source,
            folderId: activeFolderId,
            phase: 'analyzing',
            selectedIds: [],
            outputs: [],
            uploadQueueIds: [],
            createdAt: now,
            updatedAt: now,
        };
        await setJobs(current => [...current, initial]);

        try {
            const analysis = await invoke<RemoteImportAnalysis>('cmd_remote_import_analyze', { source });
            const ready: RemoteImportJob = {
                ...initial,
                phase: 'waiting-for-selection',
                analysis,
                selectedIds: options?.defaultSelection !== false ? defaultRemoteImportSelectionIds(analysis) : [],
                error: undefined,
                updatedAt: Date.now(),
            };
            await updateJob(initial.id, () => ready);
            return ready;
        } catch (error) {
            const message = errorMessage(error);
            const failed: RemoteImportJob = {
                ...initial,
                phase: 'error',
                error: message,
                updatedAt: Date.now(),
            };
            await updateJob(initial.id, () => failed);
            throw remoteImportJobError(message, initial.id);
        }
    }, [activeFolderId, initialized, setJobs, updateJob]);

    const reanalyzeSource = useCallback(async (
        jobId: string,
        options?: { defaultSelection?: boolean },
    ): Promise<RemoteImportJob> => {
        const job = getJob(jobId);
        if (!job) throw new Error('Remote Import job no longer exists');
        await updateJob(jobId, current => ({
            ...current,
            phase: 'analyzing',
            dismissedAt: undefined,
            error: undefined,
            updatedAt: Date.now(),
        }));
        try {
            const analysis = await invoke<RemoteImportAnalysis>('cmd_remote_import_analyze', { source: job.source });
            const ready: RemoteImportJob = {
                ...job,
                phase: 'waiting-for-selection',
                dismissedAt: undefined,
                analysis,
                selectedIds: options?.defaultSelection !== false ? defaultRemoteImportSelectionIds(analysis) : [],
                error: undefined,
                updatedAt: Date.now(),
            };
            await updateJob(jobId, () => ready);
            return ready;
        } catch (error) {
            const message = errorMessage(error);
            await updateJob(jobId, current => ({
                ...current,
                phase: 'error',
                error: message,
                updatedAt: Date.now(),
            }));
            throw remoteImportJobError(message, jobId);
        }
    }, [getJob, updateJob]);

    const enqueueOutputs = useCallback(async (job: RemoteImportJob): Promise<RemoteImportJob> => {
        const isCancelled = () => cancelledRef.current.has(job.id) || getJob(job.id)?.phase === 'cancelled';
        if (isCancelled()) return getJob(job.id) ?? job;
        if (!job.outputs.length) throw new Error('Remote Import has no acquired files to upload');
        activeEnqueuesRef.current.add(job.id);
        try {
            if (job.dismissedAt !== undefined) {
                await updateJob(job.id, current => ({ ...current, dismissedAt: undefined }));
            }
            const queueIds = await enqueuePreparedImports(preparedInputs(job), { isCancelled });
            if (isCancelled()) {
                await updateJob(job.id, current => ({ ...current, phase: 'cancelled', uploadQueueIds: queueIds }));
                for (const id of queueIds) cancelUploadItem?.(id);
                return getJob(job.id) ?? job;
            }
            if (!queueIds.length) throw new Error('Upload setup was cancelled. Retry when you are ready.');
            await updateJob(job.id, current => ({
                ...current, phase: 'uploading', uploadQueueIds: queueIds,
                error: undefined, progress: undefined, updatedAt: Date.now(),
            }));
            if (isCancelled()) for (const id of queueIds) cancelUploadItem?.(id);
            return getJob(job.id) ?? job;
        } finally {
            activeEnqueuesRef.current.delete(job.id);
            const current = getJob(job.id);
            if (isCancelled() && !current?.uploadQueueIds.length && !activeAcquisitionsRef.current.has(job.id)) {
                await invoke('cmd_remote_import_cleanup', { jobId: job.id }).catch(() => false);
            }
        }
    }, [cancelUploadItem, enqueuePreparedImports, getJob, updateJob]);

    const startImport = useCallback(async (
        jobId: string,
        selectedIds?: string[],
        options?: { deferAutomaticFinalization?: boolean },
    ): Promise<RemoteImportJob> => {
        const job = getJob(jobId);
        if (!job) throw new Error('Remote Import job no longer exists');
        if (!job.analysis) throw new Error('Analyze this source before importing it');
        const selectionIds = selectedIds ?? job.selectedIds;
        if (!selectionIds.length && job.analysis.source_kind !== 'direct_file') {
            throw new Error('Choose at least one item to import');
        }

        const downloading: RemoteImportJob = {
            ...job,
            phase: 'downloading',
            dismissedAt: undefined,
            selectedIds: selectionIds,
            uploadQueueIds: [],
            error: undefined,
            progress: undefined,
            deferAutomaticFinalization: options?.deferAutomaticFinalization === true,
            updatedAt: Date.now(),
        };
        await updateJob(job.id, () => downloading);

        activeAcquisitionsRef.current.add(job.id);
        try {
            if (cancelledRef.current.has(job.id)) return getJob(job.id) ?? job;
            const selections = buildRemoteImportSelections(job.analysis, selectionIds);
            const acquired = await invoke<RemoteImportAcquireResult>('cmd_remote_import_acquire', {
                req: {
                    job_id: job.id,
                    source: job.source,
                    selections,
                },
            });
            const current = getJob(job.id);
            if (current?.phase === 'cancelled') return current;
            if (!acquired.outputs.length) throw new Error('Remote Import produced no files');
            const ready: RemoteImportJob = {
                ...downloading,
                phase: 'ready-to-upload',
                outputs: acquired.outputs,
                updatedAt: Date.now(),
            };
            // Persist exact staging paths before they enter the upload queue. If
            // choosing a transfer engine is cancelled, Retry can reuse these
            // files without reacquiring the remote source.
            await updateJob(job.id, () => ready);
            return await enqueueOutputs(ready);
        } catch (error) {
            const current = getJob(job.id);
            if (current?.phase === 'cancelled') return current;
            const message = errorMessage(error);
            await updateJob(job.id, current => ({
                ...current,
                phase: 'error',
                error: message,
                updatedAt: Date.now(),
            }));
            throw new Error(message);
        } finally {
            activeAcquisitionsRef.current.delete(job.id);
            // Acquisition has settled; a cancelled worker can no longer recreate staging.
            const current = getJob(job.id);
            if (current?.phase === 'cancelled' && current.uploadQueueIds.length === 0) {
                await invoke('cmd_remote_import_cleanup', { jobId: job.id }).catch(() => false);
            }
        }
    }, [enqueueOutputs, getJob, updateJob]);

    const retryJob = useCallback(async (jobId: string): Promise<RemoteImportJob> => {
        const job = getJob(jobId);
        if (!job) throw new Error('Remote Import job no longer exists');
        cancelledRef.current.delete(jobId);
        if (job.outputs.length > 0 && job.uploadQueueIds.length === 0) {
            return enqueueOutputs(job);
        }
        if (job.uploadQueueIds.length > 0) {
            throw new Error('This import is already in the upload queue. Retry the failed upload there.');
        }
        return startImport(job.id, job.selectedIds, {
            deferAutomaticFinalization: job.deferAutomaticFinalization,
        });
    }, [enqueueOutputs, getJob, startImport]);

    const cancelJob = useCallback(async (jobId: string) => {
        const job = getJob(jobId);
        if (!job) return false;
        if (job.uploadQueueIds.length && !cancelUploadItem) {
            throw new Error('Use the upload queue to cancel this import');
        }
        // Record intent before any native acknowledgement or store await.
        cancelledRef.current.add(jobId);
        await updateJob(job.id, current => ({
            ...current, phase: 'cancelled', error: undefined, progress: undefined, updatedAt: Date.now(),
        }));
        for (const id of getJob(jobId)?.uploadQueueIds ?? []) cancelUploadItem?.(id);
        await invoke<boolean>('cmd_remote_import_cancel', { jobId: job.id }).catch(() => false);
        if (!activeAcquisitionsRef.current.has(jobId) && !activeEnqueuesRef.current.has(jobId) && !job.uploadQueueIds.length) {
            await invoke('cmd_remote_import_cleanup', { jobId: job.id }).catch(() => false);
        }
        return true;
    }, [cancelUploadItem, getJob, updateJob]);

    const dismissJob = useCallback(async (jobId: string, options?: { retainJournal?: boolean }) => {
        const job = getJob(jobId);
        if (!job) return;
        if (!['success', 'partial-error', 'error', 'cancelled'].includes(job.phase)) {
            throw new Error('Remote Import is still active');
        }
        if (options?.retainJournal) {
            // Batch stack finalization and retries still need this job's outcomes.
            // Persist visibility separately; do not delete its journal or staging.
            await updateJob(jobId, current => {
                if (!['success', 'partial-error', 'error', 'cancelled'].includes(current.phase)) {
                    throw new Error('Remote Import is still active');
                }
                const now = Date.now();
                return { ...current, dismissedAt: now, updatedAt: now };
            });
            return;
        }
        if (job.uploadQueueIds.length === 0) {
            await invoke('cmd_remote_import_cleanup', { jobId: job.id }).catch(() => false);
        }
        await setJobs(current => current.filter(candidate => candidate.id !== jobId));
    }, [getJob, setJobs, updateJob]);

    const resetRemoteImportUploadOutcomes = useCallback(async (jobId: string, queueIds: string[]) => {
        cancelledRef.current.delete(jobId);
        const reset = new Set(queueIds);
        await updateJob(jobId, current => {
            const outcomes = { ...(current.uploadOutcomes ?? {}) };
            for (const queueId of reset) delete outcomes[queueId];
            return {
                ...current,
                phase: 'uploading',
                dismissedAt: undefined,
                uploadOutcomes: outcomes,
                error: undefined,
                deferAutomaticFinalization: true,
                updatedAt: Date.now(),
            };
        });
    }, [updateJob]);

    const getRemoteImportJob = useCallback((jobId: string) => getJob(jobId), [getJob]);

    const finalizeRemoteImportJob = useCallback(async (
        jobId: string,
        options?: { allowStack?: boolean },
    ): Promise<RemoteImportJob | undefined> => {
        const job = getJob(jobId);
        if (!job) return undefined;
        await finalizeCompletedJob(job, options);
        return getJob(jobId);
    }, [finalizeCompletedJob, getJob]);

    const getEngineStatus = useCallback(() => invoke<RemoteImportEngineStatus[]>('cmd_remote_import_engine_status'), []);

    return {
        jobs,
        initialized,
        prepareSource,
        analyzeSource,
        reanalyzeSource,
        startImport,
        getRemoteImportJob,
        finalizeRemoteImportJob,
        resetRemoteImportUploadOutcomes,
        retryJob,
        cancelJob,
        dismissJob,
        getEngineStatus,
    };
}
