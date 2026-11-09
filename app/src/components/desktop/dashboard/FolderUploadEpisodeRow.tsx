import { AlertCircle, File, Pause, Play, RotateCcw, X } from 'lucide-react';
import type { QueueItem } from '../../../types';
import { formatBytes } from '../../../utils';

interface FolderUploadEpisodeRowProps {
    index: number;
    item: QueueItem;
    onPause: (id: string) => void;
    onResume: (id: string) => void;
    onCancel: (id: string) => void;
    onRetry: (id: string) => void;
}

function displayStatus(item: QueueItem) {
    if (item.status === 'success') return 'Completed';
    if (item.status === 'error') return 'Failed';
    if (item.status === 'cancelled') return 'Cancelled';
    if (item.status === 'paused') return 'Paused';
    if (item.status === 'pausing') return 'Pausing';
    if (item.status === 'pending') return 'Queued';
    if (item.status === 'downloading') return 'Preparing';
    if (item.uploadPhase === 'preparing') return 'Preparing';
    return 'Uploading';
}

function statusTone(item: QueueItem) {
    if (item.status === 'success') return 'border-green-500/25 bg-green-500/10 text-green-400';
    if (item.status === 'error') return 'border-red-500/25 bg-red-500/10 text-red-400';
    if (item.status === 'cancelled') return 'border-telegram-border bg-telegram-surface text-telegram-subtext';
    if (item.status === 'paused' || item.status === 'pausing') return 'border-amber-500/25 bg-amber-500/10 text-amber-400';
    if (item.status === 'pending') return 'border-telegram-border bg-telegram-surface text-telegram-subtext';
    return 'border-blue-500/25 bg-blue-500/10 text-blue-400';
}

export function FolderUploadEpisodeRow({
    index,
    item,
    onPause,
    onResume,
    onCancel,
    onRetry,
}: FolderUploadEpisodeRowProps) {
    const active = item.status === 'uploading' || item.status === 'downloading';
    const canPause = active || item.status === 'pending';
    const canResume = item.status === 'paused';
    const canCancel = canPause || item.status === 'paused' || item.status === 'pausing';
    const canRetry = item.status === 'error' || item.status === 'cancelled';
    const filename = item.uploadGroupRelativePath || item.driveFileName || (item.url || item.path).split('/').pop() || item.path;
    const status = displayStatus(item);
    const transferred = typeof item.uploadedBytes === 'number' ? Math.max(0, item.uploadedBytes) : 0;
    const total = typeof item.totalBytes === 'number' ? Math.max(0, item.totalBytes) : 0;
    const percent = item.status === 'success'
        ? 100
        : total > 0
            ? Math.max(0, Math.min(100, Math.round(transferred / total * 100)))
            : Math.max(0, Math.min(100, Math.round(item.progress ?? 0)));
    const showProgress = active || item.status === 'paused' || item.status === 'pausing' || item.status === 'success';

    return (
        <div
            className="tr-folder-upload-episode rounded-xl border border-telegram-border/80 bg-telegram-bg/70 p-2.5"
            role="listitem"
            aria-label={`File ${index + 1}: ${filename}, ${status}`}
        >
            <div className="flex min-w-0 items-center gap-2.5">
                <div className="grid h-8 w-8 shrink-0 place-items-center rounded-lg bg-telegram-surface text-[10px] font-bold tabular-nums text-telegram-subtext">
                    {String(index + 1).padStart(2, '0')}
                </div>
                <div className="grid h-8 w-8 shrink-0 place-items-center rounded-lg bg-blue-500/10 text-blue-400">
                    <File className="h-4 w-4" aria-hidden="true" />
                </div>
                <div className="min-w-0 flex-1">
                    <p className="truncate text-xs font-semibold text-telegram-text" title={filename}>{filename}</p>
                    <p className="mt-0.5 text-[10px] text-telegram-subtext">
                        {total > 0 ? formatBytes(total) : 'Size pending'}
                        {item.speedBytesPerSec && item.speedBytesPerSec > 0
                            ? ` · ${formatBytes(item.speedBytesPerSec)}/s`
                            : ''}
                        {item.status === 'error' && item.error ? ` · ${item.error}` : ''}
                    </p>
                </div>
                <span className={`shrink-0 rounded-full border px-2 py-1 text-[10px] font-semibold ${statusTone(item)}`}>
                    {status}
                </span>
                <div className="flex shrink-0 items-center gap-0.5">
                    {canPause && (
                        <button type="button" className="rounded-md p-1.5 text-telegram-subtext hover:bg-amber-500/10 hover:text-amber-400"
                            title="Pause file" aria-label={`Pause ${filename}`} onClick={() => onPause(item.id)}>
                            <Pause className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                    )}
                    {canResume && (
                        <button type="button" className="rounded-md p-1.5 text-telegram-subtext hover:bg-green-500/10 hover:text-green-400"
                            title="Resume file" aria-label={`Resume ${filename}`} onClick={() => onResume(item.id)}>
                            <Play className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                    )}
                    {canCancel && (
                        <button type="button" className="rounded-md p-1.5 text-telegram-subtext hover:bg-red-500/10 hover:text-red-400"
                            title="Cancel file" aria-label={`Cancel ${filename}`} onClick={() => onCancel(item.id)}>
                            <X className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                    )}
                    {canRetry && (
                        <button type="button" className="rounded-md p-1.5 text-telegram-subtext hover:bg-blue-500/10 hover:text-blue-400"
                            title="Retry file" aria-label={`Retry ${filename}`} onClick={() => onRetry(item.id)}>
                            <RotateCcw className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                    )}
                </div>
            </div>
            {showProgress && (
                <div className="ml-[5.25rem] mt-2">
                    <div className="h-1.5 overflow-hidden rounded-full bg-telegram-border">
                        <div
                            className={`h-full rounded-full transition-[width] duration-500 ${item.status === 'success' ? 'bg-green-500' : item.status === 'error' ? 'bg-red-500' : 'bg-blue-500'}`}
                            style={{ width: `${percent}%` }}
                        />
                    </div>
                    <div className="mt-1 flex items-center justify-between gap-2 text-[10px] text-telegram-subtext">
                        <span className="truncate">
                            {item.status === 'success'
                                ? 'Uploaded successfully'
                                : item.status === 'paused'
                                    ? 'Transfer paused'
                                    : item.status === 'pausing'
                                        ? 'Pausing transfer…'
                                        : item.status === 'downloading'
                                            ? 'Preparing file for upload'
                                            : item.uploadPhase === 'preparing'
                                                ? 'Preparing next part…'
                                                : active && transferred === 0
                                                    ? 'Starting transfer…'
                                                    : `${formatBytes(transferred)} of ${total > 0 ? formatBytes(total) : 'unknown'} uploaded`}
                        </span>
                        <span className="shrink-0 tabular-nums">{percent}%</span>
                    </div>
                </div>
            )}
            {item.status === 'error' && item.error && (
                <div role="alert" className="ml-[5.25rem] mt-1 flex items-center gap-1 text-[10px] text-red-400">
                    <AlertCircle className="h-3 w-3 shrink-0" aria-hidden="true" />
                    <span className="truncate">{item.error}</span>
                </div>
            )}
        </div>
    );
}
