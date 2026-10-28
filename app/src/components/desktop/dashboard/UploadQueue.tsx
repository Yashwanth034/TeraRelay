import { QueueItem } from "../../../types";
import { AlertCircle, Pause, Play, RotateCcw, X } from "lucide-react";
import { formatBytes } from "../../../utils";

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
}: UploadQueueProps) {
    if (items.length === 0 && !driveSyncWarning) return null;

    const activeCount = items.filter(i => i.status === 'pending' || i.status === 'pausing' || i.status === 'uploading' || i.status === 'downloading').length;
    const pausedCount = items.filter(i => i.status === 'paused').length;

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
                {items.map(item => {
                    const active = item.status === 'uploading' || item.status === 'downloading';
                    const showProgress = active || item.status === 'pausing' || item.status === 'paused';
                    const filename = item.driveFileName || (item.url || item.path).split('/').pop();

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

                                {(item.status === 'success' || item.status === 'error' || item.status === 'cancelled') && (
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
                })}
            </div>}
        </div>
    );
}
