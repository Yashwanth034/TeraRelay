import { DownloadItem } from "../../../types";
import { AlertCircle, Check, Download, Pause, Play, RotateCcw, X } from "lucide-react";
import { formatBytes } from "../../../utils";

interface DownloadQueueProps {
    items: DownloadItem[];
    onCancelAll: () => void;
    onCancelItem: (id: string) => void;
    onPauseItem: (id: string) => void;
    onResumeItem: (id: string) => void;
    onRetryItem: (id: string) => void;
    onDismissItem: (id: string) => void;
}

export function DownloadQueue({
    items,
    onCancelAll,
    onCancelItem,
    onPauseItem,
    onResumeItem,
    onRetryItem,
    onDismissItem,
}: DownloadQueueProps) {
    if (items.length === 0) return null;

    const activeCount = items.filter(i => i.status === 'pending' || i.status === 'downloading' || i.status === 'pausing').length;
    const pausedCount = items.filter(i => i.status === 'paused').length;

    return (
        <div className="tr-transfer-dock w-full overflow-hidden">
            <div className="tr-transfer-header flex justify-between items-center">
                <div className="flex items-center gap-2">
                    <Download className="w-4 h-4 text-telegram-secondary" />
                    <h4 className="text-sm font-medium text-telegram-text">Downloads</h4>
                    {activeCount > 0 && (
                        <span className="text-xs px-1.5 py-0.5 bg-telegram-secondary/20 text-telegram-secondary rounded-full">
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
                    const showProgress = item.status === 'downloading' || item.status === 'pausing' || item.status === 'paused';
                    const active = item.status === 'downloading';
                    return (
                        <div key={item.id} className="tr-transfer-item flex flex-col gap-1.5 p-2.5">
                            <div className="flex items-center gap-2.5 text-sm">
                                <div className="flex-shrink-0">
                                    {item.status === 'pending' && <div className="w-4 h-4 rounded-full bg-yellow-500/20 flex items-center justify-center"><div className="w-2 h-2 bg-yellow-500 rounded-full" /></div>}
                                    {item.status === 'downloading' && <div className="w-4 h-4 rounded-full border-2 border-telegram-secondary border-t-transparent animate-spin" />}
                                    {item.status === 'pausing' && <div className="w-4 h-4 rounded-full border-2 border-yellow-400 border-t-transparent animate-spin" />}
                                    {item.status === 'paused' && <Pause className="w-4 h-4 text-yellow-400" />}
                                    {item.status === 'success' && <div className="w-4 h-4 rounded-full bg-green-500/20 flex items-center justify-center"><Check className="w-3 h-3 text-green-500" /></div>}
                                    {item.status === 'error' && <div className="w-4 h-4 rounded-full bg-red-500/20 flex items-center justify-center"><X className="w-3 h-3 text-red-500" /></div>}
                                    {item.status === 'cancelled' && <div className="w-4 h-4 rounded-full bg-gray-500/20 flex items-center justify-center"><X className="w-3 h-3 text-gray-400" /></div>}
                                </div>

                                <div className="flex-1 truncate text-telegram-subtext" title={item.filename}>
                                    {item.filename}
                                </div>

                                {item.status === 'downloading' && (
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
                                    <button onClick={() => onDismissItem(item.id)} className="p-1 text-gray-400 hover:text-telegram-text transition-colors" title="Dismiss" aria-label={`Dismiss ${item.filename ?? 'transfer'}`}>
                                        <X className="w-3.5 h-3.5" />
                                    </button>
                                )}
                            </div>

                            {showProgress && (
                                <>
                                    <div className="tr-transfer-progress relative w-full h-1 overflow-hidden">
                                        {(active && (item.downloadedBytes ?? 0) === 0) ? (
                                            <div className="bg-telegram-secondary h-full w-1/2 animate-progress-indeterminate" />
                                        ) : (
                                            <>
                                                <div
                                                    className="bg-telegram-secondary h-full rounded-full transition-[width] duration-300 ease-out"
                                                    style={{ width: `${Math.max(0, Math.min(100,
                                                        item.downloadedBytes !== undefined && (item.totalBytes ?? 0) > 0
                                                            ? item.downloadedBytes / item.totalBytes! * 100
                                                            : item.progress ?? 0))}%` }}
                                                />
                                            </>
                                        )}
                                    </div>
                                    <div className="flex justify-between text-[10px] text-telegram-subtext">
                                        <span className="tabular-nums">
                                            {active && (item.downloadedBytes ?? 0) === 0
                                                ? 'Starting download…'
                                                : item.downloadedBytes !== undefined && item.totalBytes !== undefined
                                                    ? `${formatBytes(item.downloadedBytes)} / ${formatBytes(item.totalBytes)}`
                                                    : item.progress !== undefined ? `${item.progress}%` : ''}
                                        </span>
                                        <span className="min-w-[88px] text-right tabular-nums">
                                            {item.status === 'pausing'
                                                ? 'Pausing…'
                                                : item.status === 'paused'
                                                    ? 'Paused'
                                                    : item.speedBytesPerSec !== undefined
                                                        ? (active && item.speedBytesPerSec === 0 ? 'Waiting…' : `${formatBytes(item.speedBytesPerSec)}/s avg`)
                                                        : ''}
                                        </span>
                                    </div>
                                </>
                            )}

                            {item.status === 'success' && (
                                <div className="text-[10px] text-green-400">Completed</div>
                            )}
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
