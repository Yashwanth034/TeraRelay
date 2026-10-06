import { useCallback, useEffect, useRef, useState } from 'react';
import type { Dispatch, SetStateAction } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { Store } from '@tauri-apps/plugin-store';
import { toast } from 'sonner';
import type { DownloadItem, QueueItem } from './types';

export type TransferQueueKind = 'upload' | 'download';
type TransferItem = QueueItem | DownloadItem;
type Normalize<T> = (items: T[]) => T[];

/** Fail closed before filtering anything from untrusted saved or legacy records. */
function validateSavedQueue(kind: TransferQueueKind, value: unknown): void {
    const invalid = (detail: string): never => {
        throw new Error(`Invalid saved ${kind} queue: ${detail}. Stored work was left unchanged.`);
    };
    if (!Array.isArray(value)) invalid('expected an array');
    const statuses = new Set(['pending', 'pausing', 'paused', 'downloading', 'success', 'error', 'cancelled']);
    if (kind === 'upload') statuses.add('uploading');
    const ids = new Set<string>();
    for (const [index, entry] of (value as unknown[]).entries()) {
        if (!entry || typeof entry !== 'object' || Array.isArray(entry)) invalid(`item ${index + 1} is not a record`);
        const item = entry as Record<string, unknown>;
        const nonempty = (field: string) => typeof item[field] === 'string' && (item[field] as string).trim().length > 0;
        const optionalPath = (field: string) => item[field] === undefined || nonempty(field);
        if (!nonempty('id')) invalid(`item ${index + 1} has no transfer ID`);
        const id = item.id as string;
        if (ids.has(id)) invalid(`duplicate transfer ID ${id}`);
        ids.add(id);
        if (typeof item.status !== 'string' || !statuses.has(item.status)) invalid(`item ${index + 1} has an unknown status`);
        if (item.folderId !== null && !Number.isSafeInteger(item.folderId)) invalid(`item ${index + 1} has an invalid folder ID`);
        if (item.error !== undefined && typeof item.error !== 'string') invalid(`item ${index + 1} has an invalid error`);
        if (kind === 'upload') {
            if (!optionalPath('path') || !optionalPath('url') || !optionalPath('tempZipPath')) {
                invalid(`item ${index + 1} has an invalid source path`);
            }
            if (!nonempty('path') && !nonempty('url')) invalid(`item ${index + 1} has no upload source`);
        } else {
            // DownloadFileRequest accepts signed i32 message IDs; keep that existing contract.
            if (!Number.isSafeInteger(item.messageId) || (item.messageId as number) < -2147483648 ||
                (item.messageId as number) > 2147483647) invalid(`item ${index + 1} has an invalid message ID`);
            if (!nonempty('filename')) invalid(`item ${index + 1} has no filename`);
            if (!optionalPath('savePath')) invalid(`item ${index + 1} has an invalid destination`);
        }
    }
}

function recoverable(item: TransferItem) {
    return item.status === 'pending' || item.status === 'uploading' ||
        item.status === 'downloading' || item.status === 'pausing' ||
        item.status === 'paused' || item.status === 'error';
}

function savedStatus(item: TransferItem): 'pending' | 'paused' | 'error' {
    if (item.status === 'error') return 'error';
    return item.status === 'paused' || item.status === 'pausing' ? 'paused' : 'pending';
}

export const normalizeUploadQueue: Normalize<QueueItem> = items => items.filter(recoverable).map(item => ({
    id: item.id,
    path: item.path || item.url || '',
    url: item.url,
    folderId: item.folderId,
    tempZipPath: item.tempZipPath,
    status: savedStatus(item),
    ...(['error', 'paused', 'pausing'].includes(item.status) && item.error !== undefined ? { error: item.error } : {}),
}));

export const normalizeDownloadQueue: Normalize<DownloadItem> = items => items.filter(recoverable).map(item => ({
    id: item.id,
    messageId: item.messageId,
    filename: item.filename,
    folderId: item.folderId,
    savePath: item.savePath,
    status: savedStatus(item),
    ...(['error', 'paused', 'pausing'].includes(item.status) && item.error !== undefined ? { error: item.error } : {}),
}));

export interface TransferQueuePersistence<T> {
    load(legacy: () => Promise<T[] | null | undefined>): Promise<T[]>;
    persist(items: T[]): Promise<void>;
    isDurable(items: T[]): boolean;
}

/** Serialize complete metadata snapshots; failed writes never advance durability. */
export function createTransferQueuePersistence<T>(
    kind: TransferQueueKind,
    normalize: Normalize<T>,
): TransferQueuePersistence<T> {
    let tail: Promise<unknown> = Promise.resolve();
    let durableSnapshot: string | null = null;
    const write = async (items: T[]) => {
        const snapshot = JSON.stringify(items);
        if (snapshot === durableSnapshot) return;
        await invoke('cmd_save_transfer_queue', { kind, items });
        durableSnapshot = snapshot;
    };
    const enqueue = <R,>(operation: () => Promise<R>): Promise<R> => {
        const next = tail.catch(() => {}).then(operation);
        tail = next;
        return next;
    };
    return {
        load: legacy => enqueue(async () => {
            const saved = await invoke<T[] | null>('cmd_load_transfer_queue', { kind });
            if (saved !== null && !Array.isArray(saved)) throw new Error('Invalid saved transfer queue');
            // null means not migrated; [] is an intentionally cleared durable queue.
            const source = saved === null ? (await legacy()) ?? [] : saved;
            validateSavedQueue(kind, source);
            const items = normalize(source);
            durableSnapshot = saved === null ? null : JSON.stringify(saved);
            await write(items);
            return items;
        }),
        persist: items => {
            // Capture metadata now, rather than reading a mutable queue after an older save.
            validateSavedQueue(kind, items);
            const snapshot = normalize(items);
            return enqueue(() => write(snapshot));
        },
        isDurable: items => JSON.stringify(normalize(items)) === durableSnapshot,
    };
}

// Mount changes must not allow two writers for the same queue to race each other.
const writers = new Map<TransferQueueKind, TransferQueuePersistence<TransferItem>>();
function writerFor<T extends TransferItem>(kind: TransferQueueKind, normalize: Normalize<T>) {
    if (!writers.has(kind)) {
        writers.set(kind, createTransferQueuePersistence(kind, normalize) as TransferQueuePersistence<TransferItem>);
    }
    return writers.get(kind)! as TransferQueuePersistence<T>;
}

/** Keep workers behind migration and metadata-save barriers while progress stays in memory. */
export function useRecoverableTransferQueue<T extends TransferItem>(
    kind: TransferQueueKind,
    store: Store | null,
    normalize: Normalize<T>,
) {
    const [queue, updateQueue] = useState<T[]>([]);
    const queueRef = useRef(queue);
    const [initialized, setInitialized] = useState(false);
    const [, setPersistenceRevision] = useState(0);
    const [retryTick, setRetryTick] = useState(0);
    const persistence = useRef(writerFor(kind, normalize)).current;
    const initializationRef = useRef<Promise<T[]> | null>(null);
    const retriesRef = useRef({ key: '', attempts: 0 });

    const setQueue: Dispatch<SetStateAction<T[]>> = useCallback(next => {
        const items = typeof next === 'function' ? next(queueRef.current) : next;
        queueRef.current = items;
        updateQueue(items);
    }, []);

    const persist = useCallback(async (items = queueRef.current) => {
        await persistence.persist(items);
        setPersistenceRevision(revision => revision + 1);
    }, [persistence]);

    const snapshotKey = JSON.stringify(normalize(queue));
    useEffect(() => {
        if (!store) return;
        let disposed = false;
        let retryTimer: number | undefined;
        const key = initialized ? snapshotKey : 'initializing';
        if (retriesRef.current.key !== key) retriesRef.current = { key, attempts: 0 };
        const failed = (error: unknown) => {
            if (disposed) return;
            const attempt = retriesRef.current.attempts++;
            if (attempt === 0) toast.error(`Could not save ${kind} queue: ${error}`);
            // Storage failures cannot spin a worker loop. Keep work in memory and
            // make at most three delayed retries; a new action or restart can retry again.
            if (attempt < 3) {
                retryTimer = window.setTimeout(() => setRetryTick(tick => tick + 1), 1000 * 2 ** attempt);
            }
        };
        if (!initialized) {
            if (!initializationRef.current) {
                initializationRef.current = persistence.load(() => store.get<T[]>(kind + 'Queue'));
            }
            initializationRef.current.then(items => {
                if (disposed) return;
                const queuedIds = new Set(queueRef.current.map(item => item.id));
                setQueue([...items.filter(item => !queuedIds.has(item.id)), ...queueRef.current]);
                setInitialized(true);
                if (items.length) toast.info(`Restored ${items.length} ${kind}${items.length === 1 ? '' : 's'}`);
            }).catch(error => {
                initializationRef.current = null;
                failed(error);
            });
        } else {
            persist().catch(failed);
        }
        return () => {
            disposed = true;
            if (retryTimer !== undefined) window.clearTimeout(retryTimer);
        };
    }, [kind, store, initialized, snapshotKey, retryTick, persistence, persist, setQueue]);

    return { queue, setQueue, queueRef, initialized, durable: initialized && persistence.isDurable(queue), persist };
}
