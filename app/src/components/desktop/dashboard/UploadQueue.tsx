import { useState } from "react";
import { QueueItem } from "../../../types";
import { AlertCircle, ChevronDown, ChevronRight, Folder, Pause, Play, RotateCcw, X } from "lucide-react";
import { formatBytes } from "../../../utils";
import { FolderUploadEpisodeRow } from './FolderUploadEpisodeRow';

interface UploadQueueProps {
    items: QueueItem[];
    driveSyncWarning?: string | null;
    driveSyncRetryBusy?: boolean;
    onRetryDriveSync?: () => void;
    onCancelAll: () => void;
    onCancelItem: (id: string) => void;
    onPauseItem: (id: string) => void;
    onResumeItem: (id: string) => void;
    onRetryItem: (id: string) => void;
    onDismissItem: (id: string) => void;
    onPauseGroup: (groupId: string) => void;
    onResumeGroup: (groupId: string) => void;
    onCancelGroup: (groupId: string) => void;
    onDismissGroup: (groupId: string) => void;
}

export function UploadQueue({
    items,
    driveSyncWarning = null,
    driveSyncRetryBusy = false,
    onRetryDriveSync,
    onCancelAll,
    onCancelItem,
    onPauseItem,
    onResumeItem,
    onRetryItem,
    onDismissItem,
    onPauseGroup,
    onResumeGroup,
    onCancelGroup,
    onDismissGroup,
}: UploadQueueProps) {
    const [expandedGroups, setExpandedGroups] = useState<Record<string, boolean>>({});
    if (items.length === 0 && !driveSyncWarning) return null;

    const activeCount = items.filter(i => i.status === 'pending' || i.status === 'pausing' || i.status === 'uploading' || i.status === 'downloading').length;
    const pausedCount = items.filter(i => i.status === 'paused').length;

    const groupedItems = new Map<string, QueueItem[]>();
    for (const item of items) {
        if (!item.uploadGroupId) continue;
        const members = groupedItems.get(item.uploadGroupId) ?? [];
        members.push(item);
        groupedItems.set(item.uploadGroupId, members);
    }
    type UploadRow = { kind: 'item'; item: QueueItem } | { kind: 'group'; id: string };
    const uploadRows: UploadRow[] = [];
    const emittedGroups = new Set<string>();
    for (const item of items) {
        if (!item.uploadGroupId) {
            uploadRows.push({ kind: 'item', item });
        } else if (!emittedGroups.has(item.uploadGroupId)) {
            emittedGroups.add(item.uploadGroupId);
            uploadRows.push({ kind: 'group', id: item.uploadGroupId });
        }
    }

    const renderItem = (item: QueueItem, grouped = false) => {
                    const active = item.status === 'uploading' || item.status === 'downloading';
                    const showProgress = active || item.status === 'pausing' || item.status === 'paused';
                    const filename = item.uploadGroupRelativePath || item.driveFileName || (item.url || item.path).split('/').pop();

                    return (
                        <div key={item.id} role="listitem" aria-label={`Upload ${filename || 'transfer'}, ${item.status}`} className="tr-transfer-item flex flex-col gap-1.5 p-2.5">
                            <div className="flex items-center gap-2.5 text-sm">
                                <div className={`w-2 h-2 rounded-full flex-shrink-0 ${
                                    item.status === 'pending' ? 'bg-yellow-500' :
                                    item.status === 'pausing' ? 'bg-yellow-400 animate-pulse' :
                                    item.status === 'paused' ? 'bg-yellow-400' :
                                    item.status === 'downloading' ? 'bg-cyan-500 animate-pulse' :
                                    item.status === 'uploading' ? 'bg-blue-500 animate-pulse' :
                                    item.status === 'cancelled' ? 'bg-gray-500' :
                                    item.status === 'error' ? 'bg-red-500' : 'bg-green-500'
                                }`} />

                                <div className="flex-1 truncate text-telegram-subtext" title={item.url || item.path}>
                                    {filename}
                                </div>

                                {active && (
                                    <>
                                        <button onClick={() => onPauseItem(item.id)} className="p-1 text-gray-400 hover:text-yellow-400 transition-colors" title="Pause" aria-label={`Pause ${filename || 'transfer'}`}>
                                            <Pause className="w-3.5 h-3.5" />
                                        </button>
                                        <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel" aria-label={`Cancel ${filename || 'transfer'}`}>
                                            <X className="w-3.5 h-3.5" />
                                        </button>
                                    </>
                                )}

                                {item.status === 'pausing' && (
                                    <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel" aria-label={`Cancel ${filename || 'transfer'}`}>
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}

                                {item.status === 'paused' && (
                                    <>
                                        <button onClick={() => onResumeItem(item.id)} className="p-1 text-gray-400 hover:text-green-400 transition-colors" title="Resume" aria-label={`Resume ${filename || 'transfer'}`}>
                                            <Play className="w-3.5 h-3.5" />
                                        </button>
                                        <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel" aria-label={`Cancel ${filename || 'transfer'}`}>
                                            <X className="w-3.5 h-3.5" />
                                        </button>
                                    </>
                                )}

                                {item.status === 'pending' && (
                                    <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel" aria-label={`Cancel ${filename || 'transfer'}`}>
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}

                                {(item.status === 'error' || item.status === 'cancelled') && (
                                    <button onClick={() => onRetryItem(item.id)} className="p-1 text-gray-400 hover:text-blue-400 transition-colors" title="Retry" aria-label={`Retry ${filename || 'transfer'}`}>
                                        <RotateCcw className="w-3.5 h-3.5" />
                                    </button>
                                )}

                                {!grouped && (item.status === 'success' || item.status === 'error' || item.status === 'cancelled') && (
                                    <button onClick={() => onDismissItem(item.id)} className="p-1 text-gray-400 hover:text-telegram-text transition-colors" title="Dismiss" aria-label={`Dismiss ${filename || 'transfer'}`}>
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}
                            </div>

                            {showProgress && (
                                <>
                                    <div className="tr-transfer-progress relative w-full h-1 overflow-hidden"
                                        role="progressbar"
                                        aria-label={`Upload progress for ${filename || 'transfer'}`}
                                        aria-valuemin={0}
                                        aria-valuemax={100}
                                        aria-valuenow={item.totalBytes !== undefined && item.totalBytes > 0 && item.uploadedBytes !== undefined
                                            ? Math.max(0, Math.min(100, Math.round(item.uploadedBytes / item.totalBytes * 100)))
                                            : item.progress !== undefined ? Math.max(0, Math.min(100, Math.round(item.progress))) : undefined}
                                        aria-valuetext={item.uploadPhase === 'preparing' ? 'Preparing upload' : item.status === 'paused' ? 'Paused' : item.status === 'pausing' ? 'Pausing' : 'Uploading'}
                                    >
                                        {active && (item.uploadedBytes ?? 0) === 0 ? (
                                            <div className="bg-blue-500 h-full w-1/2 animate-progress-indeterminate" />
                                        ) : (
                                            <>
                                                <div
                                                    className={`${item.status === 'downloading' ? 'bg-cyan-500' : 'bg-blue-500'} h-full rounded-full transition-[width] duration-700 ease-out`}
                                                    style={{ width: `${Math.max(0, Math.min(100, item.progress ?? 0))}%` }}
                                                />
                                                {active && (item.speedBytesPerSec ?? 0) > 0 && (
                                                    <div className="absolute inset-y-0 w-1/4 bg-white/20 animate-progress-indeterminate" />
                                                )}
                                            </>
                                        )}
                                    </div>

                                    <div className="flex justify-between text-[10px] text-telegram-subtext">
                                        <span className="tabular-nums truncate pr-2">
                                            {item.status === 'pausing'
                                                ? 'Pausing…'
                                                : item.status === 'paused'
                                                    ? 'Paused'
                                                    : item.status === 'downloading'
                                                    ? `Caching: ${item.uploadedBytes !== undefined && item.totalBytes !== undefined ? `${formatBytes(item.uploadedBytes)} / ${formatBytes(item.totalBytes)}` : ''}`
                                                    : item.uploadPhase === 'preparing'
                                                        ? ((item.uploadedBytes ?? 0) > 0
                                                            ? `Preparing next part… ${formatBytes(item.uploadedBytes!)} uploaded`
                                                            : 'Preparing first upload part…')
                                                        : (active && (item.uploadedBytes ?? 0) === 0
                                                            ? 'Starting upload…'
                                                            : `Uploading: ${item.uploadedBytes !== undefined && item.totalBytes !== undefined ? `${formatBytes(item.uploadedBytes)} / ${formatBytes(item.totalBytes)}` : item.progress !== undefined ? `${item.progress}%` : ''}`)}
                                        </span>
                                        <span className="w-[92px] flex-shrink-0 text-right tabular-nums">
                                            {item.status === 'pausing'
                                                ? 'Pausing…'
                                                : item.status === 'paused'
                                                    ? ''
                                                    : item.uploadPhase !== 'preparing' && item.speedBytesPerSec !== undefined
                                                        ? (active && item.speedBytesPerSec === 0 ? 'Waiting…' : `${formatBytes(item.speedBytesPerSec)}/s avg`)
                                                        : ''}
                                        </span>
                                    </div>
                                </>
                            )}

                            {item.status === 'success' && <div role="status" className="text-[10px] text-green-400">Completed</div>}
                            {item.status === 'error' && item.error && (
                                <div role="alert" className="flex items-center gap-1 text-xs text-red-400">
                                    <AlertCircle className="w-3 h-3 flex-shrink-0" />
                                    <span className="truncate">{item.error}</span>
                                </div>
                            )}
                            {item.status === 'cancelled' && <div role="status" className="text-xs text-gray-400">Cancelled</div>}
                        </div>
                    );
    };

    return (
        <div className="tr-transfer-dock w-full overflow-hidden">
            <div className="tr-transfer-header flex justify-between items-center">
                <div className="flex items-center gap-2">
                    <h4 className="text-sm font-medium text-telegram-text">Uploads</h4>
                    {activeCount > 0 && (
                        <span className="text-xs px-1.5 py-0.5 bg-blue-500/15 text-blue-400 rounded-full">
                            {activeCount} active
                        </span>
                    )}
                    {pausedCount > 0 && (
                        <span className="text-xs px-1.5 py-0.5 bg-yellow-500/15 text-yellow-400 rounded-full">
                            {pausedCount} paused
                        </span>
                    )}
                </div>
                {(activeCount > 0 || pausedCount > 0) && (
                    <button onClick={onCancelAll} className="tr-transfer-cancel-all" aria-label="Cancel all uploads">
                        Cancel All
                    </button>
                )}
            </div>

            {driveSyncWarning && (
                <div role="alert" className="m-2 rounded-md border border-amber-500/30 p-2.5 text-xs text-telegram-text">
                    <div className="flex items-start gap-2">
                        <AlertCircle className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" aria-hidden="true" />
                        <div className="min-w-0 flex-1 space-y-1.5">
                            <p>{driveSyncWarning}</p>
                            <p className="text-telegram-subtext">Your uploaded file is not marked failed. Only cross-device metadata publication needs verification.</p>
                            <button
                                type="button"
                                aria-label="Retry TeraRelay Drive metadata sync"
                                onClick={onRetryDriveSync}
                                disabled={!onRetryDriveSync || driveSyncRetryBusy}
                                className="inline-flex items-center gap-1 rounded-md bg-telegram-primary/15 px-2 py-1.5 font-semibold text-telegram-primary disabled:opacity-50"
                            >
                                <RotateCcw className={`h-3.5 w-3.5 ${driveSyncRetryBusy ? 'animate-spin' : ''}`} aria-hidden="true" />
                                {driveSyncRetryBusy ? 'Syncing…' : 'Retry Drive sync'}
                            </button>
                        </div>
                    </div>
                </div>
            )}
            {items.length > 0 && <div className="tr-transfer-list max-h-64 overflow-y-auto p-2 space-y-2" role="list" aria-label="Upload transfer status">
                {uploadRows.map(entry => {
                    if (entry.kind === 'item') return renderItem(entry.item);
                    const members = groupedItems.get(entry.id) ?? [];
                    const first = members[0];
                    if (!first) return null;
                    const totalFiles = first.uploadGroupFileCount ?? members.length;
                    // Terminal file rows remain durable until the whole group
                    // is dismissed, preserving accurate counts across restarts.
                    const succeeded = members.filter(item => item.status === 'success').length;
                    const uploadedCount = Math.min(totalFiles, succeeded);
                    const failedCount = members.filter(item => item.status === 'error').length;
                    const cancelledCount = members.filter(item => item.status === 'cancelled').length;
                    const activeGroupCount = members.filter(item =>
                        item.status === 'uploading' || item.status === 'downloading'
                    ).length;
                    const pausingGroupCount = members.filter(item => item.status === 'pausing').length;
                    const queuedCount = members.filter(item => item.status === 'pending').length;
                    const pausedGroupCount = members.filter(item => item.status === 'paused').length;
                    const hasUnfinished = activeGroupCount + pausingGroupCount + queuedCount + pausedGroupCount > 0;
                    const canPauseGroup = activeGroupCount > 0 || queuedCount > 0;
                    const canResumeGroup = pausedGroupCount > 0;
                    const canCancelGroup = hasUnfinished;
                    const canDismissGroup = !hasUnfinished;
                    const totalBytesFromFiles = members.reduce((sum, item) =>
                        sum + (typeof item.totalBytes === 'number' && Number.isFinite(item.totalBytes)
                            ? Math.max(0, item.totalBytes) : 0), 0);
                    const sizesComplete = members.length > 0 && members.every(item =>
                        typeof item.totalBytes === 'number' && Number.isFinite(item.totalBytes) && item.totalBytes >= 0
                    );
                    const totalBytes = first.uploadGroupTotalBytes ?? totalBytesFromFiles;
                    const transferredBytes = members.reduce((sum, item) => {
                        const size = typeof item.totalBytes === 'number' && Number.isFinite(item.totalBytes)
                            ? Math.max(0, item.totalBytes) : 0;
                        if (item.status === 'success') return sum + size;
                        if (['uploading', 'downloading', 'pausing', 'paused'].includes(item.status)) {
                            const sent = typeof item.uploadedBytes === 'number' && Number.isFinite(item.uploadedBytes)
                                ? Math.max(0, item.uploadedBytes) : 0;
                            return sum + Math.min(size, sent);
                        }
                        return sum;
                    }, 0);
                    const allFilesDone = members.length >= totalFiles && uploadedCount >= totalFiles;
                    const percent = allFilesDone ? 100 : sizesComplete && totalBytes > 0
                        ? Math.round(Math.min(totalBytes, transferredBytes) / totalBytes * 100)
                        : totalFiles > 0 ? Math.round(uploadedCount / totalFiles * 100) : 0;
                    const expanded = expandedGroups[entry.id] === true;
                    const detailId = `folder-upload-files-${entry.id}`;
                    return (
                        <div key={`folder-upload-${entry.id}`} role="listitem"
                            aria-label={`Folder upload ${first.uploadGroupName}, ${uploadedCount} of ${totalFiles} files uploaded`}
                            className="tr-transfer-item overflow-hidden rounded-2xl border border-telegram-primary/20 bg-telegram-surface/70 shadow-sm">
                            <div className="flex items-start gap-2.5 p-3">
                                <button type="button"
                                    className="flex min-w-0 flex-1 items-start gap-3 text-left"
                                    aria-expanded={expanded}
                                    aria-controls={detailId}
                                    aria-label={`${expanded ? 'Collapse' : 'Expand'} folder upload ${first.uploadGroupName}`}
                                    onClick={() => setExpandedGroups(current => ({ ...current, [entry.id]: !expanded }))}>
                                    <span className="grid h-11 w-11 shrink-0 place-items-center rounded-xl border border-telegram-primary/20 bg-telegram-primary/10 text-telegram-primary">
                                        <Folder className="h-5 w-5" aria-hidden="true" />
                                    </span>
                                    <span className="min-w-0 flex-1 pt-0.5">
                                        <span className="block text-[9px] font-bold uppercase tracking-[0.16em] text-telegram-primary">
                                            Folder upload
                                        </span>
                                        <span className="mt-0.5 block truncate text-sm font-semibold text-telegram-text">
                                            {first.uploadGroupName}
                                        </span>
                                        <span className="mt-1 block text-[10px] text-telegram-subtext">
                                            {uploadedCount} of {totalFiles} files completed
                                            {sizesComplete && totalBytes > 0 ? ` · ${formatBytes(transferredBytes)} / ${formatBytes(totalBytes)}` : ''}
                                        </span>
                                        <span className="mt-2 flex flex-wrap gap-1.5">
                                            {activeGroupCount > 0 && <span className="rounded-full bg-blue-500/10 px-2 py-0.5 text-[9px] font-semibold text-blue-400">{activeGroupCount} active</span>}
                                            {queuedCount > 0 && <span className="rounded-full bg-telegram-bg px-2 py-0.5 text-[9px] font-semibold text-telegram-subtext">{queuedCount} queued</span>}
                                            {pausedGroupCount > 0 && <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[9px] font-semibold text-amber-400">{pausedGroupCount} paused</span>}
                                            {failedCount > 0 && <span className="rounded-full bg-red-500/10 px-2 py-0.5 text-[9px] font-semibold text-red-400">{failedCount} failed</span>}
                                            {cancelledCount > 0 && <span className="rounded-full bg-telegram-bg px-2 py-0.5 text-[9px] font-semibold text-telegram-subtext">{cancelledCount} cancelled</span>}
                                        </span>
                                    </span>
                                    <span className="flex shrink-0 flex-col items-end gap-1 pt-1">
                                        <span className="text-lg font-bold tabular-nums text-telegram-text">{percent}%</span>
                                        <span className="flex items-center gap-1 text-[9px] font-medium text-telegram-subtext">
                                            {expanded ? 'Hide files' : 'View files'}
                                            {expanded
                                                ? <ChevronDown className="h-3 w-3" aria-hidden="true" />
                                                : <ChevronRight className="h-3 w-3" aria-hidden="true" />}
                                        </span>
                                    </span>
                                </button>
                                <div className="flex shrink-0 items-center gap-0.5">
                                    {canPauseGroup && (
                                        <button type="button" className="rounded-lg p-1.5 text-telegram-subtext hover:bg-telegram-bg hover:text-telegram-text"
                                            title="Pause folder upload"
                                            aria-label={`Pause folder upload ${first.uploadGroupName}`}
                                            onClick={() => onPauseGroup(entry.id)}>
                                            <Pause className="h-3.5 w-3.5" aria-hidden="true" />
                                        </button>
                                    )}
                                    {canResumeGroup && (
                                        <button type="button" className="rounded-lg p-1.5 text-telegram-subtext hover:bg-telegram-bg hover:text-green-400"
                                            title="Resume folder upload"
                                            aria-label={`Resume folder upload ${first.uploadGroupName}`}
                                            onClick={() => onResumeGroup(entry.id)}>
                                            <Play className="h-3.5 w-3.5" aria-hidden="true" />
                                        </button>
                                    )}
                                    {canCancelGroup && (
                                        <button type="button" className="rounded-lg p-1.5 text-telegram-subtext hover:bg-red-500/10 hover:text-red-400"
                                            title="Cancel folder upload"
                                            aria-label={`Cancel folder upload ${first.uploadGroupName}`}
                                            onClick={() => onCancelGroup(entry.id)}>
                                            <X className="h-3.5 w-3.5" aria-hidden="true" />
                                        </button>
                                    )}
                                    {canDismissGroup && (
                                        <button type="button" className="rounded-lg p-1.5 text-telegram-subtext hover:bg-telegram-bg hover:text-telegram-text"
                                            title="Dismiss folder upload"
                                            aria-label={`Dismiss folder upload ${first.uploadGroupName}`}
                                            onClick={() => onDismissGroup(entry.id)}>
                                            <X className="h-3.5 w-3.5" aria-hidden="true" />
                                        </button>
                                    )}
                                </div>
                            </div>
                            <div className="px-3 pb-3">
                                <div className="h-1.5 overflow-hidden rounded-full bg-telegram-border"
                                    role="progressbar" aria-label={`Folder upload progress for ${first.uploadGroupName}`}
                                    aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}
                                    aria-valuetext={`${uploadedCount} of ${totalFiles} files uploaded, ${percent}%`}>
                                    <div className="h-full rounded-full bg-gradient-to-r from-blue-500 to-telegram-primary transition-[width] duration-500"
                                        style={{ width: `${percent}%` }} />
                                </div>
                            </div>
                            {expanded && (
                                <div id={detailId} className="border-t border-telegram-border/80 bg-telegram-bg/40 p-3"
                                    role="list" aria-label={`Files in folder upload ${first.uploadGroupName}`}>
                                    <div className="mb-2 flex items-center justify-between gap-2">
                                        <span className="text-[9px] font-bold uppercase tracking-[0.16em] text-telegram-subtext">
                                            Folder contents
                                        </span>
                                        <span className="text-[10px] tabular-nums text-telegram-subtext">
                                            {members.length} / {totalFiles} tracked
                                        </span>
                                    </div>
                                    <div className="max-h-56 space-y-2 overflow-y-auto pr-1">
                                        {members.map((item, index) => (
                                            <FolderUploadEpisodeRow
                                                key={item.id}
                                                index={index}
                                                item={item}
                                                onPause={onPauseItem}
                                                onResume={onResumeItem}
                                                onCancel={onCancelItem}
                                                onRetry={onRetryItem}
                                            />
                                        ))}
                                    </div>
                                </div>
                            )}
                        </div>
                    );
                })}
            </div>}
        </div>
    );
}
