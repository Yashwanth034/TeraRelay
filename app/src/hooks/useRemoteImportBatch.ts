import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Store } from '@tauri-apps/plugin-store';
import type { FileStackView, TelegramFile } from '../types';
import {
    planRemoteImportFinalization,
    type RemoteImportJob,
} from '../remoteImport';
import {
    analyzeBatchEntriesSequentially,
    batchConfigurationLocked,
    createRemoteImportBatch,
    groupSelectedMembers,
    loadPersistedRemoteImportBatches,
    normalizeRecoveredRemoteImportBatches,
    persistRemoteImportBatches,
    planCrossSourceStackFinalization,
    planEntryFinalization,
    selectCrossSourceStackSeed,
    missingCrossSourceStackMembers,
    runSequentialBatchEntries,
    setStackGroupPrimary,
    skipRemoteImportBatchEntry,
    ungroupMembers,
    validateBatchReadyForImport,
    validateStackGroups,
    type RemoteImportBatch,
    type RemoteImportBatchEntry,
    type RemoteImportMemberRef,
} from '../remoteImportBatch';

interface RemoteImportController {
    jobs: RemoteImportJob[];
    prepareSource: (source: string, options?: { folderId?: number }) => Promise<RemoteImportJob>;
    analyzeSource: (source: string, options?: { defaultSelection?: boolean }) => Promise<RemoteImportJob>;
    reanalyzeSource: (jobId: string, options?: { defaultSelection?: boolean }) => Promise<RemoteImportJob>;
    startImport: (
        jobId: string,
        selectedIds?: string[],
        options?: { deferAutomaticFinalization?: boolean },
    ) => Promise<RemoteImportJob>;
    retryJob: (jobId: string) => Promise<RemoteImportJob>;
    cancelJob: (jobId: string) => Promise<boolean>;
    getRemoteImportJob: (jobId: string) => RemoteImportJob | undefined;
    finalizeRemoteImportJob: (
        jobId: string,
        options?: { allowStack?: boolean },
    ) => Promise<RemoteImportJob | undefined>;
    resetRemoteImportUploadOutcomes: (jobId: string, queueIds: string[]) => Promise<void>;
}

type QueueAction = (queueId: string) => void;
type BatchesUpdater = RemoteImportBatch[] | ((current: RemoteImportBatch[]) => RemoteImportBatch[]);

function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

function sleep(ms: number): Promise<void> {
    return new Promise(resolve => setTimeout(resolve, ms));
}

function failedQueueIds(job: RemoteImportJob): string[] {
    const outcomes = job.uploadOutcomes ?? {};
    return job.uploadQueueIds.filter(queueId => {
        const outcome = outcomes[queueId];
        return outcome?.status === 'error' || outcome?.status === 'cancelled';
    });
}

export function useRemoteImportBatch(
    activeFolderId: number | null,
    store: Store | null,
    remoteImport: RemoteImportController,
    cancelUploadItem?: QueueAction,
    retryUploadItem?: QueueAction,
) {
    const [batches, setBatchesState] = useState<RemoteImportBatch[]>([]);
    const [initialized, setInitialized] = useState(false);
    const batchesRef = useRef<RemoteImportBatch[]>([]);
    const cancelledRef = useRef<Set<string>>(new Set());
    const runningRef = useRef<Set<string>>(new Set());
    const mutationTailRef = useRef<Promise<unknown>>(Promise.resolve());

    const setBatches = useCallback((updater: BatchesUpdater): Promise<RemoteImportBatch[]> => {
        const task = mutationTailRef.current.catch(() => undefined).then(async () => {
            const next = typeof updater === 'function' ? updater(batchesRef.current) : updater;
            // Persist first: no network/import step may observe a batch transition
            // that failed to become durable. Serializing the calculation also
            // prevents rapid checkbox clicks from overwriting one another.
            await persistRemoteImportBatches(store, next);
            batchesRef.current = next;
            setBatchesState(next);
            return next;
        });
        mutationTailRef.current = task;
        return task;
    }, [store]);

    const persistOne = useCallback(async (batch: RemoteImportBatch) => {
        await setBatches(current => {
            const exists = current.some(candidate => candidate.id === batch.id);
            return exists
                ? current.map(candidate => {
                    if (candidate.id !== batch.id) return candidate;
                    if (candidate.phase === 'cancelled' || cancelledRef.current.has(batch.id)) return candidate;
                    return {
                        ...batch,
                        stackGroups: batch.phase === 'analyzing' ? candidate.stackGroups : batch.stackGroups,
                        entries: batch.entries.map(entry => {
                            const latest = candidate.entries.find(value => value.id === entry.id);
                            if (latest?.phase === 'skipped' || latest?.phase === 'cancelled') return latest;
                            return batch.phase === 'analyzing' && latest
                                ? { ...entry, selectedIds: latest.selectedIds } : entry;
                        }),
                    };
                })
                : [...current, batch];
        });
    }, [setBatches]);

    const updateBatch = useCallback(async (
        batchId: string,
        update: (batch: RemoteImportBatch) => RemoteImportBatch,
    ) => {
        let updated: RemoteImportBatch | undefined;
        await setBatches(current => current.map(batch => {
            if (batch.id !== batchId) return batch;
            const next = update(batch);
            updated = (cancelledRef.current.has(batchId) || batch.phase === 'cancelled') && next.phase !== 'cancelled' ? batch : next;
            return updated;
        }));
        if (!updated) throw new Error('Remote Import batch no longer exists');
        return updated;
    }, [setBatches]);

    useEffect(() => {
        let disposed = false;
        if (!store) {
            batchesRef.current = [];
            setBatchesState([]);
            setInitialized(true);
            return () => { disposed = true; };
        }
        setInitialized(false);
        void loadPersistedRemoteImportBatches(store)
            .then(raw => {
                if (disposed) return;
                const recovered = normalizeRecoveredRemoteImportBatches(raw);
                batchesRef.current = recovered;
                setBatchesState(recovered);
                setInitialized(true);
                return persistRemoteImportBatches(store, recovered);
            })
            .catch(error => {
                if (disposed) return;
                console.error('[RemoteImportBatch] Could not restore batches:', error);
                setInitialized(true);
            });
        return () => { disposed = true; };
    }, [store]);

    const getBatch = useCallback(
        (batchId: string) => batchesRef.current.find(batch => batch.id === batchId),
        [],
    );

    const jobsById = useCallback(() => {
        const result = new Map<string, RemoteImportJob>();
        for (const batch of batchesRef.current) {
            for (const entry of batch.entries) {
                if (!entry.jobId) continue;
                const job = remoteImport.getRemoteImportJob(entry.jobId);
                if (job) result.set(job.id, job);
            }
        }
        return result;
    }, [remoteImport.getRemoteImportJob]);

    const analyzeBatch = useCallback(async (rawInput: string): Promise<RemoteImportBatch> => {
        if (!initialized) throw new Error('Remote Import is still restoring saved batches. Try again in a moment.');
        if (activeFolderId === null) throw new Error('Open a writable TeraRelay channel first');
        const initial = createRemoteImportBatch(activeFolderId, rawInput);
        await persistOne(initial);
        return analyzeBatchEntriesSequentially(
            initial,
            async (entry, rememberJobId) => {
                const prepared = await remoteImport.prepareSource(entry.source, { folderId: initial.folderId });
                await rememberJobId(prepared.id);
                return remoteImport.reanalyzeSource(prepared.id, { defaultSelection: false });
            },
            persistOne,
            () => getBatch(initial.id),
        );
    }, [activeFolderId, getBatch, initialized, persistOne, remoteImport]);

    const resumeBatchAnalysis = useCallback(async (batchId: string): Promise<RemoteImportBatch> => {
        const batch = getBatch(batchId);
        if (!batch) throw new Error('Remote Import batch no longer exists');
        if (batchConfigurationLocked(batch)) {
            throw new Error('This batch has already started; source analysis is locked');
        }
        return analyzeBatchEntriesSequentially(
            batch,
            async (entry, rememberJobId) => {
                const prepared = await remoteImport.prepareSource(entry.source, { folderId: batch.folderId });
                await rememberJobId(prepared.id);
                return remoteImport.reanalyzeSource(prepared.id, { defaultSelection: false });
            },
            persistOne,
            () => getBatch(batchId),
        );
    }, [getBatch, persistOne, remoteImport]);

    const setEntrySelections = useCallback(async (
        batchId: string,
        entryId: string,
        selectedIds: string[],
    ) => {
        const unique = [...new Set(selectedIds)];
        await updateBatch(batchId, batch => {
            if (batchConfigurationLocked(batch)) {
                throw new Error('This batch has already started; source selections are locked');
            }
            const entries = batch.entries.map(entry => entry.id === entryId
                ? { ...entry, selectedIds: unique, phase: 'waiting-for-selection' as const, error: undefined }
                : entry);
            const selectedByEntry = new Map(entries.map(entry => [entry.id, new Set(entry.selectedIds)]));
            const stackGroups = batch.stackGroups.filter(group => group.members.every(member =>
                selectedByEntry.get(member.entryId)?.has(member.selectionId)
            ));
            return { ...batch, entries, stackGroups, updatedAt: Date.now() };
        });
    }, [updateBatch]);

    const toggleEntrySelection = useCallback(async (
        batchId: string,
        entryId: string,
        selectionId: string,
    ) => {
        await updateBatch(batchId, batch => {
            if (batchConfigurationLocked(batch)) {
                throw new Error('This batch has already started; source selections are locked');
            }
            const entries = batch.entries.map(entry => {
                if (entry.id !== entryId) return entry;
                const selectedIds = entry.selectedIds.includes(selectionId)
                    ? entry.selectedIds.filter(value => value !== selectionId)
                    : [...entry.selectedIds, selectionId];
                return { ...entry, selectedIds, phase: 'waiting-for-selection' as const, error: undefined };
            });
            const selectedByEntry = new Map(entries.map(entry => [entry.id, new Set(entry.selectedIds)]));
            const stackGroups = batch.stackGroups.filter(group => group.members.every(member =>
                selectedByEntry.get(member.entryId)?.has(member.selectionId)
            ));
            return { ...batch, entries, stackGroups, updatedAt: Date.now() };
        });
    }, [updateBatch]);

    const skipEntry = useCallback(async (batchId: string, entryId: string) => {
        await updateBatch(batchId, batch => skipRemoteImportBatchEntry(batch, entryId));
    }, [updateBatch]);

    const retryEntryAnalysis = useCallback(async (batchId: string, entryId: string) => {
        const batch = getBatch(batchId);
        const entry = batch?.entries.find(candidate => candidate.id === entryId);
        if (!batch || !entry) throw new Error('Remote Import source no longer exists');
        await updateBatch(batchId, current => ({
            ...current,
            entries: current.entries.map(candidate => candidate.id === entryId
                ? { ...candidate, phase: 'analyzing' as const, error: undefined }
                : candidate),
            updatedAt: Date.now(),
        }));
        try {
            let job: RemoteImportJob;
            if (entry.jobId) {
                job = await remoteImport.reanalyzeSource(entry.jobId, { defaultSelection: false });
            } else {
                const prepared = await remoteImport.prepareSource(entry.source, { folderId: batch.folderId });
                await updateBatch(batchId, current => ({
                    ...current,
                    entries: current.entries.map(candidate => candidate.id === entryId
                        ? { ...candidate, jobId: prepared.id, phase: 'analyzing' as const, error: undefined }
                        : candidate),
                    updatedAt: Date.now(),
                }));
                job = await remoteImport.reanalyzeSource(prepared.id, { defaultSelection: false });
            }
            await updateBatch(batchId, current => ({
                ...current,
                phase: current.entries.some(candidate => candidate.id !== entryId && candidate.phase === 'error')
                    ? 'paused'
                    : 'waiting-for-selection',
                entries: current.entries.map(candidate => candidate.id === entryId ? {
                    ...candidate,
                    jobId: job.id,
                    selectedIds: [],
                    phase: 'waiting-for-selection' as const,
                    error: undefined,
                } : candidate),
                updatedAt: Date.now(),
            }));
            const latest = getBatch(batchId);
            if (latest?.entries.some(candidate => !candidate.jobId && candidate.phase === 'pending')) {
                await resumeBatchAnalysis(batchId);
            }
        } catch (error) {
            await updateBatch(batchId, current => ({
                ...current,
                phase: 'paused',
                entries: current.entries.map(candidate => candidate.id === entryId
                    ? { ...candidate, phase: 'error' as const, error: errorMessage(error) }
                    : candidate),
                updatedAt: Date.now(),
            }));
            throw error;
        }
    }, [getBatch, remoteImport, resumeBatchAnalysis, updateBatch]);

    const groupMembers = useCallback(async (
        batchId: string,
        memberRefs: RemoteImportMemberRef[],
        primaryRef?: RemoteImportMemberRef,
    ) => {
        await updateBatch(batchId, batch => groupSelectedMembers(batch, memberRefs, {
            primary: primaryRef,
        }));
    }, [updateBatch]);

    const setGroupPrimary = useCallback(async (
        batchId: string,
        groupId: string,
        primaryRef: RemoteImportMemberRef,
    ) => {
        await updateBatch(batchId, batch => setStackGroupPrimary(batch, groupId, primaryRef));
    }, [updateBatch]);

    const ungroup = useCallback(async (batchId: string, groupId: string) => {
        await updateBatch(batchId, batch => ungroupMembers(batch, groupId));
    }, [updateBatch]);

    const waitForTerminalUploadState = useCallback(async (
        batchId: string,
        jobId: string,
    ): Promise<RemoteImportJob> => {
        while (true) {
            if (cancelledRef.current.has(batchId)) throw new Error('Remote Import batch cancelled');
            const job = remoteImport.getRemoteImportJob(jobId);
            if (!job) throw new Error('Remote Import job no longer exists');
            const plan = planRemoteImportFinalization(job);
            if (plan.ready) return job;
            if (job.phase === 'error' && job.uploadQueueIds.length === 0) return job;
            await sleep(250);
        }
    }, [remoteImport]);

    const recoverExistingCrossSourceStackId = useCallback(async (
        batch: RemoteImportBatch,
        memberFileIds: string[],
    ): Promise<{ stackId: string; stackedFileIds: string[] } | null> => {
        if (memberFileIds.length < 2) return null;
        const wanted = new Set(memberFileIds);
        const files = await invoke<TelegramFile[]>('cmd_get_files', { folderId: batch.folderId });
        const candidateIds = [...new Set(files
            .filter(file => file.logical_file_id && wanted.has(file.logical_file_id) && file.stack_id)
            .map(file => file.stack_id as string))];

        for (const stackId of candidateIds) {
            const view = await invoke<FileStackView>('cmd_get_file_stack', {
                folderId: batch.folderId,
                stackId,
            }).catch(() => null);
            if (!view) continue;
            const existingIds = new Set(view.members
                .map(member => member.file.logical_file_id)
                .filter((value): value is string => typeof value === 'string' && value.length > 0));
            if (memberFileIds.every(fileId => existingIds.has(fileId))) {
                return {
                    stackId: view.stack_id,
                    stackedFileIds: [...existingIds].filter(fileId => wanted.has(fileId)),
                };
            }
        }
        return null;
    }, []);

    const finalizeCrossSourceGroup = useCallback(async (batchId: string, groupId: string) => {
        let batch = getBatch(batchId);
        if (!batch) throw new Error('Remote Import batch no longer exists');
        let group = batch.stackGroups.find(candidate => candidate.id === groupId);
        if (!group) return false;
        let plan = planCrossSourceStackFinalization(batch, group, jobsById());
        if (!plan.ready || !plan.primary) return false;

        let stackId = group.stackId;
        const stacked = new Set(group.stackedFileIds ?? []);
        if (!stackId) {
            const seed = selectCrossSourceStackSeed(plan);
            if (seed.length !== 2) return false;
            const [primary, second] = seed;
            const seedFileIds = seed.map(member => member.logicalFileId);

            let recovered = await recoverExistingCrossSourceStackId(batch, seedFileIds);
            if (!recovered) {
                try {
                    stackId = await invoke<string>('cmd_create_file_stack', {
                        folderId: batch.folderId,
                        baseFileId: primary.logicalFileId,
                        versionFileId: second.logicalFileId,
                        primaryFileId: primary.logicalFileId,
                    });
                    recovered = {
                        stackId,
                        stackedFileIds: seedFileIds,
                    };
                } catch (createError) {
                    // A crash can happen after Telegram accepted the stack manifest but
                    // before the batch store persisted its stackId. Re-read the normal
                    // file/stack views before deciding creation actually failed.
                    recovered = await recoverExistingCrossSourceStackId(batch, seedFileIds);
                    if (!recovered) throw createError;
                }
            }

            stackId = recovered.stackId;
            for (const fileId of recovered.stackedFileIds) stacked.add(fileId);
            await updateBatch(batchId, current => ({
                ...current,
                stackGroups: current.stackGroups.map(candidate => candidate.id === groupId ? {
                    ...candidate,
                    stackId,
                    stackedFileIds: [...stacked],
                } : candidate),
                updatedAt: Date.now(),
            }));
        }

        batch = getBatch(batchId)!;
        group = batch.stackGroups.find(candidate => candidate.id === groupId)!;
        plan = planCrossSourceStackFinalization(batch, group, jobsById());
        if (!plan.ready || !plan.primary) return false;
        const pendingMembers = missingCrossSourceStackMembers(group, plan);
        for (const member of plan.members) {
            if (pendingMembers.some(pending => pending.logicalFileId === member.logicalFileId)) {
                await invoke('cmd_add_file_to_stack', {
                    folderId: batch.folderId,
                    stackId,
                    fileId: member.logicalFileId,
                    makePrimary: false,
                });
                stacked.add(member.logicalFileId);
                await updateBatch(batchId, current => ({
                    ...current,
                    stackGroups: current.stackGroups.map(candidate => candidate.id === groupId ? {
                        ...candidate,
                        stackId,
                        stackedFileIds: [...stacked],
                    } : candidate),
                    updatedAt: Date.now(),
                }));
            }
            await invoke('cmd_update_file_stack_label', {
                folderId: batch.folderId,
                stackId,
                fileId: member.logicalFileId,
                label: member.label ?? null,
            });
        }
        await invoke('cmd_set_file_stack_primary', {
            folderId: batch.folderId,
            stackId,
            fileId: plan.primary.logicalFileId,
        });
        return true;
    }, [getBatch, jobsById, recoverExistingCrossSourceStackId, updateBatch]);

    const finalizeReadyGroups = useCallback(async (batchId: string): Promise<number> => {
        const batch = getBatch(batchId);
        if (!batch) return 0;
        let unresolved = 0;
        for (const group of batch.stackGroups) {
            const current = getBatch(batchId);
            if (!current) return unresolved;
            const latestGroup = current.stackGroups.find(candidate => candidate.id === group.id);
            if (!latestGroup) continue;
            const plan = planCrossSourceStackFinalization(current, latestGroup, jobsById());
            if (!plan.ready) {
                unresolved += 1;
                continue;
            }
            const finalized = await finalizeCrossSourceGroup(batchId, group.id);
            if (!finalized) unresolved += 1;
        }
        return unresolved;
    }, [finalizeCrossSourceGroup, getBatch, jobsById]);

    const runEntry = useCallback(async (
        batchId: string,
        entry: RemoteImportBatchEntry,
    ): Promise<RemoteImportBatchEntry> => {
        if (cancelledRef.current.has(batchId) || getBatch(batchId)?.phase === 'cancelled') return { ...entry, phase: 'cancelled' };
        if (!entry.jobId) return { ...entry, phase: 'error', error: 'Analyze this source before importing it' };
        let job = remoteImport.getRemoteImportJob(entry.jobId);
        if (!job?.analysis) return { ...entry, phase: 'error', error: 'Source analysis is unavailable' };

        try {
            if (job.phase === 'success') {
                return { ...entry, phase: 'success', error: undefined };
            }
            if (job.outputs.length > 0 && job.uploadQueueIds.length === 0) {
                job = await remoteImport.retryJob(job.id);
            } else if (job.uploadQueueIds.length === 0) {
                job = await remoteImport.startImport(job.id, entry.selectedIds, {
                    deferAutomaticFinalization: true,
                });
            }

            job = await waitForTerminalUploadState(batchId, job.id);
            if (cancelledRef.current.has(batchId)) return { ...entry, phase: 'cancelled' };
            const outcomePlan = planRemoteImportFinalization(job);
            if (!outcomePlan.ready) {
                return { ...entry, phase: 'error', error: job.error ?? 'Remote Import did not reach a durable upload outcome' };
            }

            const batch = getBatch(batchId);
            if (!batch) throw new Error('Remote Import batch no longer exists');
            const currentEntry = batch.entries.find(candidate => candidate.id === entry.id) ?? entry;
            const entryPlan = planEntryFinalization(batch, currentEntry, job);
            const finalized = await remoteImport.finalizeRemoteImportJob(job.id, {
                allowStack: entryPlan.automaticSameSourceStack,
            });
            if (!finalized || finalized.phase === 'error' || finalized.phase === 'partial-error') {
                return {
                    ...currentEntry,
                    phase: 'error',
                    error: finalized?.error ?? 'Remote Import finalization needs attention',
                };
            }
            return { ...currentEntry, phase: 'success', error: undefined };
        } catch (error) {
            if (cancelledRef.current.has(batchId)) return { ...entry, phase: 'cancelled' };
            return { ...entry, phase: 'error', error: errorMessage(error) };
        }
    }, [getBatch, remoteImport, waitForTerminalUploadState]);

    const startBatch = useCallback(async (batchId: string) => {
        if (runningRef.current.has(batchId)) return;
        const batch = getBatch(batchId);
        if (!batch) throw new Error('Remote Import batch no longer exists');
        if (batch.folderId !== activeFolderId) {
            throw new Error('Open the same destination channel used when this batch was created');
        }
        const currentJobs = jobsById();
        const errors = [
            ...validateBatchReadyForImport(batch, currentJobs),
            ...validateStackGroups(batch, currentJobs),
        ];
        if (errors.length) throw new Error(errors[0]);

        runningRef.current.add(batchId);
        cancelledRef.current.delete(batchId);
        try {
            const completed = await runSequentialBatchEntries(
                batch,
                entry => runEntry(batchId, entry),
                persistOne,
                () => getBatch(batchId),
            );
            if (completed.phase !== 'cancelled') {
                try {
                    const unresolvedGroups = await finalizeReadyGroups(batchId);
                    if (completed.phase === 'finalizing') {
                        if (unresolvedGroups > 0) {
                            throw new Error(`${unresolvedGroups} Version Stack group${unresolvedGroups === 1 ? '' : 's'} still need finalization`);
                        }
                        await updateBatch(batchId, current => ({
                            ...current,
                            phase: 'success',
                            error: undefined,
                            updatedAt: Date.now(),
                        }));
                    }
                } catch (error) {
                    await updateBatch(batchId, current => ({
                        ...current,
                        phase: completed.phase === 'finalizing' ? 'partial-error' : current.phase,
                        error: `Uploads are safe, but Version Stack finalization failed: ${errorMessage(error)}`,
                        updatedAt: Date.now(),
                    }));
                }
            }
        } finally {
            runningRef.current.delete(batchId);
        }
    }, [activeFolderId, finalizeReadyGroups, getBatch, jobsById, persistOne, runEntry, updateBatch]);

    const retryBatchEntry = useCallback(async (batchId: string, entryId: string) => {
        const batch = getBatch(batchId);
        const entry = batch?.entries.find(candidate => candidate.id === entryId);
        if (!batch || !entry) throw new Error('Remote Import source no longer exists');
        if (!entry.jobId) {
            await retryEntryAnalysis(batchId, entryId);
            return;
        }
        const job = remoteImport.getRemoteImportJob(entry.jobId);
        if (!job?.analysis) {
            await retryEntryAnalysis(batchId, entryId);
            return;
        }
        const failed = failedQueueIds(job);
        if (failed.length > 0) {
            if (!retryUploadItem) throw new Error('Retry the failed upload from the upload queue');
            await remoteImport.resetRemoteImportUploadOutcomes(job.id, failed);
            for (const queueId of failed) retryUploadItem(queueId);
        }
        await updateBatch(batchId, current => ({
            ...current,
            phase: 'paused',
            entries: current.entries.map(candidate => candidate.id === entryId
                ? { ...candidate, phase: 'waiting-for-selection' as const, error: undefined }
                : candidate),
            error: undefined,
            updatedAt: Date.now(),
        }));
        await startBatch(batchId);
    }, [getBatch, remoteImport, retryEntryAnalysis, retryUploadItem, startBatch, updateBatch]);

    const cancelBatch = useCallback(async (batchId: string) => {
        const batch = getBatch(batchId);
        if (!batch) return;
        cancelledRef.current.add(batchId);
        const active = batch.activeEntryIndex === null
            ? undefined
            : batch.entries.find(entry => entry.ordinal === batch.activeEntryIndex);
        await updateBatch(batchId, current => ({
            ...current,
            phase: 'cancelled',
            activeEntryIndex: null,
            entries: current.entries.map(entry => entry.id === active?.id
                ? { ...entry, phase: 'cancelled' as const, error: undefined }
                : entry),
            error: undefined,
            updatedAt: Date.now(),
        }));
        if (active?.jobId) {
            const job = remoteImport.getRemoteImportJob(active.jobId);
            if (job?.uploadQueueIds.length) {
                for (const queueId of job.uploadQueueIds) cancelUploadItem?.(queueId);
            } else {
                await remoteImport.cancelJob(active.jobId).catch(() => false);
            }
        }
    }, [cancelUploadItem, getBatch, remoteImport, updateBatch]);

    return {
        batches,
        initialized,
        analyzeBatch,
        resumeBatchAnalysis,
        setEntrySelections,
        toggleEntrySelection,
        skipEntry,
        retryEntryAnalysis,
        groupMembers,
        setGroupPrimary,
        ungroup,
        startBatch,
        retryBatchEntry,
        cancelBatch,
    };
}
