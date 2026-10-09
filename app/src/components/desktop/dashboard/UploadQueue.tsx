import { QueueItem } from "../../../types";
import { AlertCircle, Pause, Play, RotateCcw, X } from "lucide-react";
import { formatBytes } from "../../../utils";

interface UploadQueueProps {
    items: QueueItem[];
    onCancelAll: () => void;
    onCancelItem: (id: string) => void;
    onPauseItem: (id: string) => void;
    onResumeItem: (id: string) => void;
    onRetryItem: (id: string) => void;
    onDismissItem: (id: string) => void;
}

export function UploadQueue({
    items,
    onCancelAll,
    onCancelItem,
    onPauseItem,
    onResumeItem,
    onRetryItem,
    onDismissItem,
}: UploadQueueProps) {
    if (items.length === 0) return null;

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
                    <button onClick={onCancelAll} className="tr-transfer-cancel-all">
                        Cancel All
                    </button>
                )}
            </div>

            <div className="tr-transfer-list max-h-64 overflow-y-auto p-2 space-y-2">
                {items.map(item => {
                    const active = item.status === 'uploading' || item.status === 'downloading';
                    const showProgress = active || item.status === 'pausing' || item.status === 'paused';
                    const filename = item.driveFileName || (item.url || item.path).split('/').pop();

                    return (
                        <div key={item.id} className="tr-transfer-item flex flex-col gap-1.5 p-2.5">
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
                                        <button onClick={() => onPauseItem(item.id)} className="p-1 text-gray-400 hover:text-yellow-400 transition-colors" title="Pause">
                                            <Pause className="w-3.5 h-3.5" />
                                        </button>
                                        <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel">
                                            <X className="w-3.5 h-3.5" />
                                        </button>
                                    </>
                                )}

                                {item.status === 'pausing' && (
                                    <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel">
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}

                                {item.status === 'paused' && (
                                    <>
                                        <button onClick={() => onResumeItem(item.id)} className="p-1 text-gray-400 hover:text-green-400 transition-colors" title="Resume">
                                            <Play className="w-3.5 h-3.5" />
                                        </button>
                                        <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel">
                                            <X className="w-3.5 h-3.5" />
                                        </button>
                                    </>
                                )}

                                {item.status === 'pending' && (
                                    <button onClick={() => onCancelItem(item.id)} className="p-1 text-gray-400 hover:text-red-400 transition-colors" title="Cancel">
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}

                                {(item.status === 'error' || item.status === 'cancelled') && (
                                    <button onClick={() => onRetryItem(item.id)} className="p-1 text-gray-400 hover:text-blue-400 transition-colors" title="Retry">
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
                                    <div className="tr-transfer-progress relative w-full h-1 overflow-hidden">
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

                            {item.status === 'success' && <div className="text-[10px] text-green-400">Completed</div>}
                            {item.status === 'error' && item.error && (
                                <div className="flex items-center gap-1 text-xs text-red-400">
                                    <AlertCircle className="w-3 h-3 flex-shrink-0" />
                                    <span className="truncate">{item.error}</span>
                                </div>
                            )}
                            {item.status === 'cancelled' && <div className="text-xs text-gray-400">Cancelled</div>}
                        </div>
                    );
                })}
            </div>
        </div>
    );
}
