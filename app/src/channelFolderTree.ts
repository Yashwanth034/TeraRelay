import type { TelegramFile } from './types';

export interface ChannelFolderNode {
    name: string;
    path: string;
    directory_id?: string;
    folders: ChannelFolderNode[];
    files: TelegramFile[];
    file_count: number;
    total_bytes: number;
    /** Earliest contained file timestamp, used to position the folder in the channel timeline. */
    timeline_timestamp?: number;
    timeline_message_id?: number;
    timeline_date_value?: string;
}

export interface ChannelFolderTree {
    folders: ChannelFolderNode[];
    looseFiles: TelegramFile[];
}

export type ChannelTimelineEntry =
    | {
        kind: 'folder';
        key: string;
        timestamp: number;
        messageId: number;
        dateValue?: string;
        folder: ChannelFolderNode;
    }
    | {
        kind: 'file';
        key: string;
        timestamp: number;
        messageId: number;
        dateValue?: string;
        file: TelegramFile;
    };

function parseFolderTimelineDate(value?: string): number | null {
    if (!value) return null;
    const normalized = value.endsWith(' UTC')
        ? value.replace(' ', 'T').replace(' UTC', 'Z')
        : value;
    const timestamp = new Date(normalized).getTime();
    return Number.isNaN(timestamp) ? null : timestamp;
}

function validRelativeFolderPath(value: unknown): value is string {
    if (typeof value !== 'string' || value.length === 0 || value.length > 4096
        || value.startsWith('/') || value.includes('\\')) return false;
    const parts = value.split('/');
    return parts.length <= 64 && parts.every(part =>
        part.length > 0 && part !== '.' && part !== '..' && !/[\u0000-\u001f\u007f]/.test(part)
    );
}

/**
 * Builds the compact channel-feed tree from the persisted Drive directory
 * inventory. Files without a verified Drive path stay visible at channel root.
 */
export function buildChannelFolderTree(
    files: TelegramFile[],
    directoryPaths: string[] = [],
    directoryIdsByPath: Record<string, string> = {},
    directoryCreatedAtByPath: Record<string, number> = {},
): ChannelFolderTree {
    const roots = new Map<string, ChannelFolderNode>();
    const allNodes = new Map<string, ChannelFolderNode>();

    const ensurePath = (path: string): ChannelFolderNode | null => {
        if (!validRelativeFolderPath(path)) return null;
        let parent = roots;
        let prefix = '';
        let node: ChannelFolderNode | undefined;
        for (const part of path.split('/')) {
            prefix = prefix ? prefix + '/' + part : part;
            const key = part.toLocaleLowerCase();
            node = parent.get(key);
            if (!node) {
                node = {
                    name: part,
                    path: prefix,
                    directory_id: directoryIdsByPath[prefix],
                    timeline_timestamp: Number.isFinite(directoryCreatedAtByPath[prefix])
                        ? directoryCreatedAtByPath[prefix]
                        : undefined,
                    timeline_date_value: Number.isFinite(directoryCreatedAtByPath[prefix])
                        ? new Date(directoryCreatedAtByPath[prefix]).toISOString()
                        : undefined,
                    folders: [],
                    files: [],
                    file_count: 0,
                    total_bytes: 0,
                };
                parent.set(key, node);
                if (prefix.includes('/')) {
                    const parentPath = prefix.slice(0, prefix.lastIndexOf('/'));
                    const parentNode = allNodes.get(parentPath.toLocaleLowerCase());
                    parentNode?.folders.push(node);
                }
                allNodes.set(prefix.toLocaleLowerCase(), node);
            }
            parent = new Map(node.folders.map(child => [child.name.toLocaleLowerCase(), child]));
        }
        return node ?? null;
    };

    // Directory inventory also includes empty folders and nested folders that
    // currently contain no uploaded files.
    for (const path of directoryPaths) ensurePath(path);

    const looseFiles: TelegramFile[] = [];
    for (const file of files) {
        const path = file.drive_relative_path;
        if (!validRelativeFolderPath(path)) {
            looseFiles.push(file);
            continue;
        }
        const folder = ensurePath(path);
        if (!folder) looseFiles.push(file);
        else folder.files.push(file);
    }

    const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
    const finalize = (node: ChannelFolderNode): { count: number; bytes: number } => {
        node.folders.sort((a, b) => collator.compare(a.name, b.name));
        node.files.sort((a, b) => collator.compare(a.name, b.name));
        let count = node.files.length;
        let bytes = node.files.reduce((sum, file) =>
            sum + (Number.isFinite(file.size) ? Math.max(0, file.size) : 0), 0);
        for (const child of node.folders) {
            const totals = finalize(child);
            count += totals.count;
            bytes += totals.bytes;
        }
        node.file_count = count;
        node.total_bytes = bytes;

        // If the entire subtree is empty, keep this directory's own
        // creation time. Otherwise anchor it to the earliest real file.
        if (count > 0) {
            const candidates = [
                ...node.files.map(file => ({
                    timestamp: parseFolderTimelineDate(file.created_at),
                    messageId: Number.isFinite(file.id) ? file.id : 0,
                    dateValue: file.created_at,
                })),
                ...node.folders.map(child => ({
                    timestamp: child.timeline_timestamp ?? null,
                    messageId: child.timeline_message_id ?? 0,
                    dateValue: child.timeline_date_value,
                })),
            ].filter((candidate): candidate is {
                timestamp: number;
                messageId: number;
                dateValue: string | undefined;
            } => candidate.timestamp !== null);
            candidates.sort((a, b) => a.timestamp - b.timestamp || a.messageId - b.messageId);
            const first = candidates[0];
            if (first) {
                node.timeline_timestamp = first.timestamp;
                node.timeline_message_id = first.messageId;
                node.timeline_date_value = first.dateValue;
            }
        }
        return { count, bytes };
    };

    const folders = [...roots.values()].sort((a, b) => collator.compare(a.name, b.name));
    folders.forEach(finalize);
    return { folders, looseFiles };
}

/**
 * Merge folder collections and ordinary files into the channel's existing
 * oldest-to-newest timeline. A folder is anchored to its earliest contained
 * Telegram message (message ID breaks same-second ties); empty folders use
 * their persisted Drive creation timestamp.
 */
export function buildChannelTimeline(
    folders: ChannelFolderNode[],
    looseFiles: TelegramFile[],
): ChannelTimelineEntry[] {
    const entries: ChannelTimelineEntry[] = [
        ...folders.map(folder => ({
            kind: 'folder' as const,
            key: `folder-${folder.directory_id || folder.path}`,
            timestamp: folder.timeline_timestamp ?? 0,
            messageId: folder.timeline_message_id ?? 0,
            dateValue: folder.timeline_date_value,
            folder,
        })),
        ...looseFiles.map(file => ({
            kind: 'file' as const,
            key: `file-${file.logical_file_id || file.id}`,
            timestamp: parseFolderTimelineDate(file.created_at) ?? 0,
            messageId: Number.isFinite(file.id) ? file.id : 0,
            dateValue: file.created_at,
            file,
        })),
    ];
    entries.sort((a, b) =>
        a.timestamp - b.timestamp
        || a.messageId - b.messageId
        || (a.kind === b.kind ? 0 : a.kind === 'folder' ? -1 : 1)
        || a.key.localeCompare(b.key),
    );
    return entries;
}
