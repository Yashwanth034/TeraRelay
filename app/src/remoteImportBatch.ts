import type { Store } from '@tauri-apps/plugin-store';
import type { RemoteImportAnalysis, RemoteImportJob } from './remoteImport';

export const REMOTE_IMPORT_BATCH_STORE_KEY = 'remoteImportBatchesV1';
export const MAX_REMOTE_IMPORT_BATCH_SOURCES = 50;
export const MAX_REMOTE_IMPORT_STACK_MEMBERS = 128;

export type RemoteImportBatchPhase =
    | 'draft'
    | 'analyzing'
    | 'waiting-for-selection'
    | 'importing'
    | 'finalizing'
    | 'paused'
    | 'success'
    | 'partial-error'
    | 'cancelled';

export type RemoteImportBatchEntryPhase =
    | 'pending'
    | 'analyzing'
    | 'waiting-for-selection'
    | 'queued'
    | 'importing'
    | 'success'
    | 'skipped'
    | 'error'
    | 'cancelled';

export interface RemoteImportMemberRef {
    entryId: string;
    selectionId: string;
}

export interface RemoteImportStackGroup {
    id: string;
    name?: string;
    members: RemoteImportMemberRef[];
    primary?: RemoteImportMemberRef;
    suggestedPrimary?: RemoteImportMemberRef;
    primaryConfirmed: boolean;
    stackId?: string;
    stackedFileIds?: string[];
}

export interface RemoteImportBatchEntry {
    id: string;
    ordinal: number;
    source: string;
    jobId?: string;
    selectedIds: string[];
    phase: RemoteImportBatchEntryPhase;
    error?: string;
}

export interface RemoteImportBatch {
    schema: 1;
    id: string;
    folderId: number;
    phase: RemoteImportBatchPhase;
    activeEntryIndex: number | null;
    entries: RemoteImportBatchEntry[];
    stackGroups: RemoteImportStackGroup[];
    /** Durable marker: once set, source selections and manual grouping are immutable. */
    startedAt?: number;
    error?: string;
    createdAt: number;
    updatedAt: number;
}

function makeId(prefix: string): string {
    const uuid = globalThis.crypto?.randomUUID?.();
    if (uuid) return `${prefix}-${uuid}`;
    return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 11)}`;
}

export function parseRemoteImportSources(input: string): string[] {
    const sources = input
        .split(/\r?\n/)
        .map(value => value.trim())
        .filter(Boolean);
    if (sources.length === 0) throw new Error('Paste at least one link to import');
    if (sources.length > MAX_REMOTE_IMPORT_BATCH_SOURCES) {
        throw new Error(`Remote Import supports up to ${MAX_REMOTE_IMPORT_BATCH_SOURCES} links in one batch`);
    }
    return sources;
}

export function createRemoteImportBatch(folderId: number, rawInput: string): RemoteImportBatch {
    if (!Number.isFinite(folderId)) throw new Error('Open a writable TeraRelay channel first');
    const sources = parseRemoteImportSources(rawInput);
    const now = Date.now();
    return {
        schema: 1,
        id: makeId('batch'),
        folderId,
        phase: 'draft',
        activeEntryIndex: null,
        entries: sources.map((source, ordinal) => ({
            id: makeId('entry'),
            ordinal,
            source,
            selectedIds: [],
            phase: 'pending',
        })),
        stackGroups: [],
        createdAt: now,
        updatedAt: now,
    };
}

export function multiLinkDefaultSelectionIds(_analysis: RemoteImportAnalysis): string[] {
    return [];
}

export function validateBatchReadyForImport(
    batch: RemoteImportBatch,
    jobsById: Map<string, RemoteImportJob>,
): string[] {
    const errors: string[] = [];
    for (const entry of batch.entries) {
        if (entry.phase === 'skipped') continue;
        if (!entry.jobId) {
            errors.push(`Source ${entry.ordinal + 1} has not been analyzed`);
            continue;
        }
        const job = jobsById.get(entry.jobId);
        if (!job?.analysis) {
            errors.push(`Source ${entry.ordinal + 1} analysis is unavailable`);
            continue;
        }
        if (job.folderId !== batch.folderId) {
            errors.push(`Source ${entry.ordinal + 1} is bound to a different destination channel`);
            continue;
        }
        if (entry.selectedIds.length === 0) {
            errors.push(`Source ${entry.ordinal + 1} needs at least one selection or must be skipped`);
        }
    }
    return errors;
}

function isMemberRef(value: unknown): value is RemoteImportMemberRef {
    if (!value || typeof value !== 'object') return false;
    const member = value as Partial<RemoteImportMemberRef>;
    return typeof member.entryId === 'string'
        && member.entryId.length > 0
        && typeof member.selectionId === 'string'
        && member.selectionId.length > 0;
}

function isStackGroup(value: unknown): value is RemoteImportStackGroup {
    if (!value || typeof value !== 'object') return false;
    const group = value as Partial<RemoteImportStackGroup>;
    return typeof group.id === 'string'
        && group.id.length > 0
        && Array.isArray(group.members)
        && group.members.every(isMemberRef)
        && typeof group.primaryConfirmed === 'boolean'
        && (group.primary === undefined || isMemberRef(group.primary))
        && (group.suggestedPrimary === undefined || isMemberRef(group.suggestedPrimary))
        && (group.stackId === undefined || typeof group.stackId === 'string')
        && (group.stackedFileIds === undefined
            || (Array.isArray(group.stackedFileIds) && group.stackedFileIds.every(fileId => typeof fileId === 'string')));
}

const BATCH_PHASES = new Set<RemoteImportBatchPhase>([
    'draft', 'analyzing', 'waiting-for-selection', 'importing', 'finalizing',
    'paused', 'success', 'partial-error', 'cancelled',
]);
const ENTRY_PHASES = new Set<RemoteImportBatchEntryPhase>([
    'pending', 'analyzing', 'waiting-for-selection', 'queued', 'importing',
    'success', 'skipped', 'error', 'cancelled',
]);

function isEntry(value: unknown): value is RemoteImportBatchEntry {
    if (!value || typeof value !== 'object') return false;
    const entry = value as Partial<RemoteImportBatchEntry>;
    return typeof entry.id === 'string'
        && entry.id.length > 0
        && Number.isInteger(entry.ordinal)
        && (entry.ordinal ?? -1) >= 0
        && typeof entry.source === 'string'
        && entry.source.length > 0
        && (entry.jobId === undefined || typeof entry.jobId === 'string')
        && Array.isArray(entry.selectedIds)
        && entry.selectedIds.every(selectionId => typeof selectionId === 'string')
        && typeof entry.phase === 'string'
        && ENTRY_PHASES.has(entry.phase as RemoteImportBatchEntryPhase)
        && (entry.error === undefined || typeof entry.error === 'string');
}

function isBatch(value: unknown): value is RemoteImportBatch {
    if (!value || typeof value !== 'object') return false;
    const batch = value as Partial<RemoteImportBatch>;
    return batch.schema === 1
        && typeof batch.id === 'string'
        && batch.id.length > 0
        && Number.isFinite(batch.folderId)
        && typeof batch.phase === 'string'
        && BATCH_PHASES.has(batch.phase as RemoteImportBatchPhase)
        && (batch.activeEntryIndex === null || Number.isInteger(batch.activeEntryIndex))
        && Array.isArray(batch.entries)
        && batch.entries.every(isEntry)
        && Array.isArray(batch.stackGroups)
        && batch.stackGroups.every(isStackGroup)
        && (batch.startedAt === undefined || Number.isFinite(batch.startedAt))
        && (batch.error === undefined || typeof batch.error === 'string')
        && typeof batch.createdAt === 'number'
        && Number.isFinite(batch.createdAt)
        && typeof batch.updatedAt === 'number'
        && Number.isFinite(batch.updatedAt);
}

export function normalizeRecoveredRemoteImportBatches(value: unknown): RemoteImportBatch[] {
    if (!Array.isArray(value)) return [];
    const now = Date.now();
    return value.filter(isBatch).map(batch => {
        const interrupted = batch.phase === 'draft'
            || batch.phase === 'analyzing'
            || batch.phase === 'importing'
            || batch.phase === 'finalizing';
        return {
            ...batch,
            phase: interrupted ? 'paused' : batch.phase,
            activeEntryIndex: interrupted ? null : batch.activeEntryIndex,
            entries: batch.entries.map(entry => {
                if (entry.phase !== 'analyzing' && entry.phase !== 'importing') return { ...entry };
                return {
                    ...entry,
                    phase: 'error',
                    error: 'This source was interrupted. Retry it to continue from durable Remote Import state.',
                };
            }),
            stackGroups: batch.stackGroups.map(group => ({
                ...group,
                members: group.members.map(member => ({ ...member })),
                primary: group.primary ? { ...group.primary } : undefined,
                suggestedPrimary: group.suggestedPrimary ? { ...group.suggestedPrimary } : undefined,
                stackedFileIds: group.stackedFileIds ? [...group.stackedFileIds] : undefined,
            })),
            updatedAt: interrupted ? now : batch.updatedAt,
        };
    });
}



function replaceBatchEntry(
    batch: RemoteImportBatch,
    entryId: string,
    replacement: RemoteImportBatchEntry,
): RemoteImportBatch {
    return {
        ...batch,
        entries: batch.entries.map(entry => entry.id === entryId ? replacement : entry),
        updatedAt: Date.now(),
    };
}

export async function analyzeBatchEntriesSequentially(
    initialBatch: RemoteImportBatch,
    analyze: (
        entry: RemoteImportBatchEntry,
        rememberJobId: (jobId: string) => Promise<void>,
    ) => Promise<{ id: string }>,
    persist: (batch: RemoteImportBatch) => Promise<void>,
): Promise<RemoteImportBatch> {
    let batch: RemoteImportBatch = { ...initialBatch, phase: 'analyzing', updatedAt: Date.now() };
    await persist(batch);
    for (const original of batch.entries) {
        if (original.phase === 'skipped' || original.phase === 'success' || original.jobId) continue;
        const current = batch.entries.find(entry => entry.id === original.id)!;
        batch = replaceBatchEntry(batch, current.id, { ...current, phase: 'analyzing', error: undefined });
        batch = { ...batch, activeEntryIndex: current.ordinal, updatedAt: Date.now() };
        await persist(batch);

        const rememberJobId = async (jobId: string) => {
            const latest = batch.entries.find(entry => entry.id === current.id) ?? current;
            batch = replaceBatchEntry(batch, current.id, {
                ...latest,
                jobId,
                phase: 'analyzing',
                error: undefined,
            });
            batch = { ...batch, activeEntryIndex: current.ordinal, updatedAt: Date.now() };
            await persist(batch);
        };

        try {
            const job = await analyze(current, rememberJobId);
            const latest = batch.entries.find(entry => entry.id === current.id) ?? current;
            if (latest.jobId !== job.id) await rememberJobId(job.id);
            const remembered = batch.entries.find(entry => entry.id === current.id) ?? latest;
            const analyzed: RemoteImportBatchEntry = {
                ...remembered,
                jobId: job.id,
                selectedIds: [],
                phase: 'waiting-for-selection',
                error: undefined,
            };
            batch = replaceBatchEntry(batch, current.id, analyzed);
        } catch (error) {
            const latest = batch.entries.find(entry => entry.id === current.id) ?? current;
            const failedJobId = error && typeof error === 'object' && 'remoteImportJobId' in error
                ? String((error as { remoteImportJobId: unknown }).remoteImportJobId)
                : latest.jobId;
            batch = replaceBatchEntry(batch, current.id, {
                ...latest,
                jobId: failedJobId,
                phase: 'error',
                error: error instanceof Error ? error.message : String(error),
            });
        }
        await persist(batch);
    }
    const hasErrors = batch.entries.some(entry => entry.phase === 'error');
    batch = {
        ...batch,
        phase: hasErrors ? 'paused' : 'waiting-for-selection',
        activeEntryIndex: null,
        updatedAt: Date.now(),
    };
    await persist(batch);
    return batch;
}

export async function runSequentialBatchEntries(
    initialBatch: RemoteImportBatch,
    runEntry: (entry: RemoteImportBatchEntry) => Promise<RemoteImportBatchEntry>,
    persist: (batch: RemoteImportBatch) => Promise<void>,
): Promise<RemoteImportBatch> {
    const startedAt = initialBatch.startedAt ?? Date.now();
    let batch: RemoteImportBatch = {
        ...initialBatch,
        phase: 'importing',
        startedAt,
        updatedAt: Date.now(),
    };
    await persist(batch);
    for (const original of batch.entries) {
        if (original.phase === 'success' || original.phase === 'skipped') continue;
        if (original.phase === 'cancelled') {
            batch = { ...batch, phase: 'cancelled', activeEntryIndex: null, updatedAt: Date.now() };
            await persist(batch);
            return batch;
        }
        const current = batch.entries.find(entry => entry.id === original.id)!;
        batch = replaceBatchEntry(batch, current.id, { ...current, phase: 'importing', error: undefined });
        batch = { ...batch, activeEntryIndex: current.ordinal, updatedAt: Date.now() };
        await persist(batch);
        const result = await runEntry(batch.entries.find(entry => entry.id === current.id)!);
        batch = replaceBatchEntry(batch, current.id, result);
        await persist(batch);
        if (result.phase === 'error' || result.phase === 'cancelled') {
            batch = {
                ...batch,
                phase: result.phase === 'cancelled' ? 'cancelled' : 'paused',
                activeEntryIndex: null,
                error: result.error,
                updatedAt: Date.now(),
            };
            await persist(batch);
            return batch;
        }
    }
    batch = { ...batch, phase: 'finalizing', activeEntryIndex: null, error: undefined, updatedAt: Date.now() };
    await persist(batch);
    return batch;
}

export interface ResolvedBatchGroupMember {
    ref: RemoteImportMemberRef;
    logicalFileId: string;
    label?: string | null;
    height?: number | null;
}

export interface CrossSourceStackPlan {
    ready: boolean;
    members: ResolvedBatchGroupMember[];
    primary: ResolvedBatchGroupMember | null;
    missing: RemoteImportMemberRef[];
}

function sameMember(left: RemoteImportMemberRef, right: RemoteImportMemberRef): boolean {
    return left.entryId === right.entryId && left.selectionId === right.selectionId;
}

function selectedMemberExists(batch: RemoteImportBatch, member: RemoteImportMemberRef): boolean {
    const entry = batch.entries.find(candidate => candidate.id === member.entryId);
    return !!entry && entry.selectedIds.includes(member.selectionId);
}

export function batchConfigurationLocked(batch: RemoteImportBatch): boolean {
    return typeof batch.startedAt === 'number' && Number.isFinite(batch.startedAt);
}

export function skipRemoteImportBatchEntry(
    batch: RemoteImportBatch,
    entryId: string,
): RemoteImportBatch {
    const locked = batchConfigurationLocked(batch);
    return {
        ...batch,
        entries: batch.entries.map(entry => entry.id === entryId ? {
            ...entry,
            selectedIds: locked ? [...entry.selectedIds] : [],
            phase: 'skipped' as const,
            error: undefined,
        } : entry),
        stackGroups: locked
            ? batch.stackGroups
            : batch.stackGroups.filter(group => !group.members.some(member => member.entryId === entryId)),
        updatedAt: Date.now(),
    };
}

function assertBatchConfigurationEditable(batch: RemoteImportBatch): void {
    if (batchConfigurationLocked(batch)) {
        throw new Error('This batch has already started; its selections and Version Stack configuration are locked');
    }
}

export function sourceUsesManualStack(batch: RemoteImportBatch, entryId: string): boolean {
    return batch.stackGroups.some(group => group.members.some(member => member.entryId === entryId));
}

export function groupSelectedMembers(
    batch: RemoteImportBatch,
    members: RemoteImportMemberRef[],
    options: { name?: string; primary?: RemoteImportMemberRef } = {},
): RemoteImportBatch {
    assertBatchConfigurationEditable(batch);
    const unique: RemoteImportMemberRef[] = [];
    for (const member of members) {
        if (!unique.some(candidate => sameMember(candidate, member))) unique.push({ ...member });
    }
    if (unique.length < 2) throw new Error('Choose at least two selected outputs to group as versions');
    if (unique.length > MAX_REMOTE_IMPORT_STACK_MEMBERS) {
        throw new Error(`A Version Stack can contain at most ${MAX_REMOTE_IMPORT_STACK_MEMBERS} files`);
    }
    if (new Set(unique.map(member => member.entryId)).size < 2) {
        throw new Error('Cross-link Version Stack groups must include outputs from different links');
    }
    for (const member of unique) {
        if (!selectedMemberExists(batch, member)) {
            throw new Error('A Version Stack member is missing or is not currently selected');
        }
        const alreadyGrouped = batch.stackGroups.some(group => group.members.some(candidate => sameMember(candidate, member)));
        if (alreadyGrouped) throw new Error('A selected output already belongs to another Version Stack group');
    }
    if (options.primary && !unique.some(member => sameMember(member, options.primary!))) {
        throw new Error('Primary version must be one of the selected group members');
    }
    const group: RemoteImportStackGroup = {
        id: makeId('group'),
        name: options.name?.trim() || undefined,
        members: unique,
        primary: options.primary ? { ...options.primary } : undefined,
        primaryConfirmed: !!options.primary,
        stackedFileIds: [],
    };
    return {
        ...batch,
        stackGroups: [...batch.stackGroups, group],
        updatedAt: Date.now(),
    };
}

export function setStackGroupPrimary(
    batch: RemoteImportBatch,
    groupId: string,
    primary: RemoteImportMemberRef,
): RemoteImportBatch {
    assertBatchConfigurationEditable(batch);
    const group = batch.stackGroups.find(candidate => candidate.id === groupId);
    if (!group) throw new Error('Version Stack group no longer exists');
    if (!group.members.some(member => sameMember(member, primary))) {
        throw new Error('Primary version must belong to this Version Stack group');
    }
    return {
        ...batch,
        stackGroups: batch.stackGroups.map(candidate => candidate.id === groupId ? {
            ...candidate,
            primary: { ...primary },
            primaryConfirmed: true,
        } : candidate),
        updatedAt: Date.now(),
    };
}

export function ungroupMembers(batch: RemoteImportBatch, groupId: string): RemoteImportBatch {
    assertBatchConfigurationEditable(batch);
    return {
        ...batch,
        stackGroups: batch.stackGroups.filter(group => group.id !== groupId),
        updatedAt: Date.now(),
    };
}

export function validateStackGroups(
    batch: RemoteImportBatch,
    jobsById: Map<string, RemoteImportJob>,
): string[] {
    const errors: string[] = [];
    const claimed = new Set<string>();
    for (const group of batch.stackGroups) {
        if (group.members.length < 2 || new Set(group.members.map(member => member.entryId)).size < 2) {
            errors.push(`Version group ${group.name ?? group.id} must include at least two different links`);
        }
        for (const member of group.members) {
            const key = `${member.entryId}\0${member.selectionId}`;
            if (claimed.has(key)) errors.push('A selected output belongs to more than one Version Stack group');
            claimed.add(key);
            const entry = batch.entries.find(candidate => candidate.id === member.entryId);
            if (!entry || !entry.selectedIds.includes(member.selectionId)) {
                errors.push('A Version Stack member is no longer selected');
                continue;
            }
            if (entry.jobId) {
                const job = jobsById.get(entry.jobId);
                const validIds = new Set([
                    ...(job?.analysis?.qualities.map(value => value.format_id) ?? []),
                    ...(job?.analysis?.items.map(value => value.id) ?? []),
                ]);
                if (job?.analysis && !validIds.has(member.selectionId)) {
                    errors.push('A Version Stack member no longer exists in its source analysis');
                }
            }
        }
        if (!group.primaryConfirmed || !group.primary) {
            errors.push(`Version group ${group.name ?? group.id} needs a primary version`);
        } else if (!group.members.some(member => sameMember(member, group.primary!))) {
            errors.push('A Version Stack primary is not a member of its group');
        }
    }
    return errors;
}

export function planEntryFinalization(
    batch: RemoteImportBatch,
    entry: RemoteImportBatchEntry,
    job: RemoteImportJob,
): { automaticSameSourceStack: boolean; standaloneSelectionIds: string[] } {
    const manual = sourceUsesManualStack(batch, entry.id);
    const automaticSameSourceStack = !manual
        && (job.analysis?.qualities.length ?? 0) > 0
        && entry.selectedIds.length > 1;
    if (automaticSameSourceStack) return { automaticSameSourceStack: true, standaloneSelectionIds: [] };
    const grouped = new Set(batch.stackGroups.flatMap(group => group.members
        .filter(member => member.entryId === entry.id)
        .map(member => member.selectionId)));
    return {
        automaticSameSourceStack: false,
        standaloneSelectionIds: entry.selectedIds.filter(selectionId => !grouped.has(selectionId)),
    };
}

export function resolveGroupMembers(
    batch: RemoteImportBatch,
    group: RemoteImportStackGroup,
    jobsById: Map<string, RemoteImportJob>,
): ResolvedBatchGroupMember[] {
    const resolved: ResolvedBatchGroupMember[] = [];
    for (const ref of group.members) {
        const entry = batch.entries.find(candidate => candidate.id === ref.entryId);
        if (!entry?.jobId) continue;
        const job = jobsById.get(entry.jobId);
        if (!job) continue;
        const outcome = Object.values(job.uploadOutcomes ?? {}).find(value =>
            value.selectionId === ref.selectionId
            && (value.status === 'success' || value.status === 'skipped')
            && typeof value.logicalFileId === 'string'
            && value.logicalFileId.length > 0
        );
        if (!outcome?.logicalFileId) continue;
        resolved.push({
            ref: { ...ref },
            logicalFileId: outcome.logicalFileId,
            label: outcome.label,
            height: outcome.height,
        });
    }
    return resolved;
}


export function selectCrossSourceStackSeed(plan: CrossSourceStackPlan): ResolvedBatchGroupMember[] {
    if (!plan.ready || !plan.primary) return [];
    const second = plan.members.find(member => member.logicalFileId !== plan.primary!.logicalFileId);
    return second ? [plan.primary, second] : [];
}

export function missingCrossSourceStackMembers(
    group: RemoteImportStackGroup,
    plan: CrossSourceStackPlan,
): ResolvedBatchGroupMember[] {
    const stacked = new Set(group.stackedFileIds ?? []);
    return plan.members.filter(member => !stacked.has(member.logicalFileId));
}

export function planCrossSourceStackFinalization(
    batch: RemoteImportBatch,
    group: RemoteImportStackGroup,
    jobsById: Map<string, RemoteImportJob>,
): CrossSourceStackPlan {
    const members = resolveGroupMembers(batch, group, jobsById);
    const missing = group.members.filter(ref => !members.some(member => sameMember(member.ref, ref)));
    const explicitPrimary = group.primary
        ? members.find(member => sameMember(member.ref, group.primary!)) ?? null
        : null;
    const suggestedPrimary = [...members].sort((left, right) => (right.height ?? 0) - (left.height ?? 0))[0] ?? null;
    return {
        ready: missing.length === 0 && members.length >= 2 && (group.primaryConfirmed ? !!explicitPrimary : false),
        members,
        primary: explicitPrimary ?? (group.primaryConfirmed ? suggestedPrimary : null),
        missing,
    };
}

let batchStoreTail: Promise<unknown> = Promise.resolve();

function serializeBatchStore<T>(operation: () => Promise<T>): Promise<T> {
    const next = batchStoreTail.catch(() => {}).then(operation);
    batchStoreTail = next;
    return next;
}

export function loadPersistedRemoteImportBatches(store: Store | null): Promise<RemoteImportBatch[]> {
    if (!store) return Promise.resolve([]);
    return serializeBatchStore(async () => {
        const value = await store.get<unknown>(REMOTE_IMPORT_BATCH_STORE_KEY);
        return Array.isArray(value) ? value.filter(isBatch).map(batch => ({ ...batch })) : [];
    });
}

export function persistRemoteImportBatches(store: Store | null, batches: RemoteImportBatch[]): Promise<void> {
    if (!store) return Promise.resolve();
    return serializeBatchStore(async () => {
        await store.set(REMOTE_IMPORT_BATCH_STORE_KEY, batches);
        await store.save();
    });
}
