import type { Store } from '@tauri-apps/plugin-store';

export const REMOTE_IMPORT_STORE_KEY = 'remoteImportJobsV1';

export type RemoteImportSourceKind = 'web_media' | 'direct_file' | 'torrent';

export type RemoteImportPhase =
    | 'analyzing'
    | 'waiting-for-selection'
    | 'downloading'
    | 'processing'
    | 'ready-to-upload'
    | 'uploading'
    | 'finalizing'
    | 'success'
    | 'partial-error'
    | 'error'
    | 'cancelled';

export interface RemoteImportQuality {
    format_id: string;
    label: string;
    height?: number | null;
    fps?: number | null;
    hdr: boolean;
    ext?: string | null;
    estimated_size?: number | null;
    recommended: boolean;
    has_audio: boolean;
}

export interface RemoteImportItem {
    id: string;
    label: string;
    path?: string | null;
    size?: number | null;
    kind: string;
}

export interface RemoteImportAnalysis {
    source_kind: RemoteImportSourceKind;
    title: string;
    source_label: string;
    duration_seconds?: number | null;
    thumbnail?: string | null;
    qualities: RemoteImportQuality[];
    items: RemoteImportItem[];
}

export interface RemoteImportSelection {
    id: string;
    label?: string | null;
    source?: string | null;
    format_id?: string | null;
    has_audio?: boolean | null;
    height?: number | null;
    estimated_size?: number | null;
}

export interface RemoteImportOutput {
    selection_id: string;
    staging_path: string;
    display_name: string;
    size: number;
    label?: string | null;
    height?: number | null;
}

export interface RemoteImportAcquireResult {
    job_id: string;
    outputs: RemoteImportOutput[];
}

export interface RemoteImportProgress {
    job_id: string;
    phase: string;
    downloaded_bytes: number;
    total_bytes: number;
    speed_bytes_per_sec: number;
    selection_id?: string | null;
}

export interface RemoteImportUploadOutcome {
    queueId: string;
    selectionId: string;
    logicalFileId?: string | null;
    status: 'success' | 'skipped' | 'error' | 'cancelled';
    error?: string;
    label?: string | null;
    height?: number | null;
}

export interface RemoteImportJob {
    id: string;
    source: string;
    folderId: number;
    phase: RemoteImportPhase;
    analysis?: RemoteImportAnalysis;
    selectedIds: string[];
    outputs: RemoteImportOutput[];
    uploadQueueIds: string[];
    uploadOutcomes?: Record<string, RemoteImportUploadOutcome>;
    /** Existing Version Stack state, persisted so finalization can resume safely. */
    stackId?: string;
    stackedFileIds?: string[];
    /** Batch orchestration owns finalization when true; single-link jobs default to automatic finalization. */
    deferAutomaticFinalization?: boolean;
    /** Hide a dismissed batch row while retaining its recovery/finalization journal. */
    dismissedAt?: number;
    progress?: RemoteImportProgress;
    error?: string;
    createdAt: number;
    updatedAt: number;
}

export interface PreparedImportInput {
    path: string;
    folderId: number;
    uploadName: string;
    remoteImportJobId: string;
    remoteImportSelectionId: string;
    remoteImportLabel?: string | null;
    remoteImportHeight?: number | null;
}

export interface RemoteImportEngineStatus {
    name: string;
    available: boolean;
    version?: string | null;
    required: boolean;
    purpose: string;
}

const INTERRUPTED_PHASES = new Set<RemoteImportPhase>([
    'analyzing',
    'downloading',
    'processing',
    'ready-to-upload',
]);

export function pickRecommendedSelection(qualities: RemoteImportQuality[]): string | null {
    if (qualities.length === 0) return null;
    const sorted = sortQualitiesHighestFirst(qualities);
    return qualities.find(quality => quality.recommended)?.format_id
        ?? sorted[0]?.format_id
        ?? qualities[0].format_id;
}

export function sortQualitiesHighestFirst(qualities: RemoteImportQuality[]): RemoteImportQuality[] {
    return [...qualities].sort((left, right) => {
        const height = (right.height ?? 0) - (left.height ?? 0);
        if (height !== 0) return height;
        const fps = (right.fps ?? 0) - (left.fps ?? 0);
        if (fps !== 0) return fps;
        if (left.hdr !== right.hdr) return right.hdr ? 1 : -1;
        return left.label.localeCompare(right.label);
    });
}

export function defaultRemoteImportSelectionIds(analysis: RemoteImportAnalysis): string[] {
    if (analysis.qualities.length > 0) {
        const recommended = pickRecommendedSelection(analysis.qualities);
        return recommended ? [recommended] : [];
    }
    return analysis.items.map(item => item.id);
}

export function buildRemoteImportSelections(
    analysis: RemoteImportAnalysis,
    selectedIds: string[],
): RemoteImportSelection[] {
    const selected = new Set(selectedIds);
    if (analysis.qualities.length > 0) {
        return analysis.qualities
            .filter(quality => selected.has(quality.format_id))
            .map(quality => ({
                id: quality.format_id,
                label: quality.label,
                format_id: quality.format_id,
                has_audio: quality.has_audio,
                height: quality.height ?? null,
                estimated_size: quality.estimated_size ?? null,
            }));
    }
    return analysis.items
        .filter(item => selected.has(item.id))
        .map(item => ({
            id: item.id,
            label: item.label,
            source: item.path ?? null,
            estimated_size: item.size ?? null,
        }));
}

export function normalizeBackendImportPhase(phase: string): RemoteImportPhase | null {
    switch (phase) {
        case 'analyzing': return 'analyzing';
        case 'connecting':
        case 'waiting_for_peers':
        case 'downloading': return 'downloading';
        case 'processing': return 'processing';
        case 'ready_to_upload': return 'ready-to-upload';
        case 'uploading': return 'uploading';
        case 'finalizing': return 'finalizing';
        default: return null;
    }
}

function isValidJob(value: unknown): value is RemoteImportJob {
    if (!value || typeof value !== 'object') return false;
    const job = value as Partial<RemoteImportJob>;
    return typeof job.id === 'string'
        && job.id.length > 0
        && typeof job.source === 'string'
        && Number.isFinite(job.folderId)
        && typeof job.phase === 'string'
        && Array.isArray(job.selectedIds)
        && Array.isArray(job.outputs)
        && Array.isArray(job.uploadQueueIds)
        && typeof job.createdAt === 'number'
        && typeof job.updatedAt === 'number';
}

export function normalizeRecoveredRemoteImportJobs(value: unknown): RemoteImportJob[] {
    if (!Array.isArray(value)) return [];
    const now = Date.now();
    return value.filter(isValidJob).map(job => {
        if (INTERRUPTED_PHASES.has(job.phase)) {
            return {
                ...job,
                phase: 'error',
                error: 'Remote Import was interrupted. Retry this job to resume from its existing staging data.',
                progress: undefined,
                updatedAt: now,
            };
        }
        if ((job.phase === 'uploading' || job.phase === 'finalizing') && job.uploadQueueIds.length === 0) {
            return {
                ...job,
                phase: 'error',
                error: 'Remote Import upload handoff was interrupted. Retry this job to continue.',
                updatedAt: now,
            };
        }
        return { ...job };
    });
}

export function highestQualitySelectionId(
    analysis: RemoteImportAnalysis | undefined,
    selectedIds: string[],
): string | null {
    if (!analysis || analysis.qualities.length === 0) return null;
    const selected = new Set(selectedIds);
    return sortQualitiesHighestFirst(analysis.qualities)
        .find(quality => selected.has(quality.format_id))?.format_id ?? null;
}

export type SuccessfulRemoteImportOutcome = RemoteImportUploadOutcome & { logicalFileId: string };

export interface RemoteImportFinalizationPlan {
    ready: boolean;
    shouldStack: boolean;
    available: SuccessfulRemoteImportOutcome[];
    failures: RemoteImportUploadOutcome[];
    primary: SuccessfulRemoteImportOutcome | null;
    terminalPhase: 'success' | 'partial-error' | 'error';
}

export function planRemoteImportFinalization(job: RemoteImportJob): RemoteImportFinalizationPlan {
    const outcomes = job.uploadOutcomes ?? {};
    const expected = [...new Set(job.uploadQueueIds)];
    const ready = expected.length > 0 && expected.every(queueId => outcomes[queueId] !== undefined);

    const terminal = expected
        .map(queueId => outcomes[queueId])
        .filter((outcome): outcome is RemoteImportUploadOutcome => outcome !== undefined);

    const available = terminal
        .filter((outcome): outcome is SuccessfulRemoteImportOutcome =>
            (outcome.status === 'success' || outcome.status === 'skipped')
            && typeof outcome.logicalFileId === 'string'
            && outcome.logicalFileId.length > 0
        )
        .sort((left, right) => {
            const height = (right.height ?? 0) - (left.height ?? 0);
            if (height !== 0) return height;
            return left.selectionId.localeCompare(right.selectionId);
        });

    const failures = terminal.filter(outcome =>
        outcome.status === 'error'
        || outcome.status === 'cancelled'
        || ((outcome.status === 'success' || outcome.status === 'skipped') && !outcome.logicalFileId)
    );

    const qualityImport = (job.analysis?.qualities.length ?? 0) > 0;
    const shouldStack = qualityImport && job.selectedIds.length > 1 && available.length >= 2;
    const terminalPhase: RemoteImportFinalizationPlan['terminalPhase'] = failures.length === 0
        ? 'success'
        : available.length > 0
            ? 'partial-error'
            : 'error';

    return {
        ready,
        shouldStack,
        available,
        failures,
        primary: available[0] ?? null,
        terminalPhase,
    };
}

function persistedJobsWithoutRecovery(value: unknown): RemoteImportJob[] {
    if (!Array.isArray(value)) return [];
    return value.filter(isValidJob).map(job => ({
        ...job,
        uploadOutcomes: job.uploadOutcomes ? { ...job.uploadOutcomes } : undefined,
        stackedFileIds: job.stackedFileIds ? [...job.stackedFileIds] : undefined,
    }));
}

let remoteImportStoreTail: Promise<unknown> = Promise.resolve();

function serializeRemoteImportStore<T>(operation: () => Promise<T>): Promise<T> {
    const next = remoteImportStoreTail.catch(() => {}).then(operation);
    remoteImportStoreTail = next;
    return next;
}

export function loadPersistedRemoteImportJobs(store: Store): Promise<RemoteImportJob[]> {
    return serializeRemoteImportStore(async () => {
        const value = await store.get<unknown>(REMOTE_IMPORT_STORE_KEY);
        return persistedJobsWithoutRecovery(value);
    });
}

export function persistRemoteImportJobs(
    store: Store | null,
    snapshot: RemoteImportJob[],
): Promise<RemoteImportJob[]> {
    if (!store) return Promise.resolve(snapshot.map(job => ({ ...job })));
    return serializeRemoteImportStore(async () => {
        const current = persistedJobsWithoutRecovery(await store.get<unknown>(REMOTE_IMPORT_STORE_KEY));
        const currentById = new Map(current.map(job => [job.id, job]));
        const merged = snapshot.map(job => {
            const durable = currentById.get(job.id);
            if (!durable) return { ...job };
            return {
                ...job,
                uploadOutcomes: {
                    ...(job.uploadOutcomes ?? {}),
                    ...(durable.uploadOutcomes ?? {}),
                },
                stackId: job.stackId ?? durable.stackId,
                stackedFileIds: job.stackedFileIds ?? durable.stackedFileIds,
            };
        });
        await store.set(REMOTE_IMPORT_STORE_KEY, merged);
        await store.save();
        return merged;
    });
}

type RemoteImportOutcomeListener = (jobId: string, outcome: RemoteImportUploadOutcome) => void;
const remoteImportOutcomeListeners = new Set<RemoteImportOutcomeListener>();

export function subscribeRemoteImportUploadOutcomes(listener: RemoteImportOutcomeListener): () => void {
    remoteImportOutcomeListeners.add(listener);
    return () => remoteImportOutcomeListeners.delete(listener);
}

function publishRemoteImportUploadOutcome(jobId: string, outcome: RemoteImportUploadOutcome) {
    for (const listener of remoteImportOutcomeListeners) listener(jobId, outcome);
}

export function recordRemoteImportUploadOutcome(
    store: Store | null,
    jobId: string,
    outcome: RemoteImportUploadOutcome,
): Promise<boolean> {
    if (!store) {
        publishRemoteImportUploadOutcome(jobId, outcome);
        return Promise.resolve(true);
    }
    return serializeRemoteImportStore(async () => {
        const current = persistedJobsWithoutRecovery(await store.get<unknown>(REMOTE_IMPORT_STORE_KEY));
        const index = current.findIndex(job => job.id === jobId);
        if (index < 0) return false;
        const job = current[index];
        current[index] = {
            ...job,
            uploadOutcomes: {
                ...(job.uploadOutcomes ?? {}),
                [outcome.queueId]: outcome,
            },
            updatedAt: Date.now(),
        };
        await store.set(REMOTE_IMPORT_STORE_KEY, current);
        await store.save();
        publishRemoteImportUploadOutcome(jobId, outcome);
        return true;
    });
}
