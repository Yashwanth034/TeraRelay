import { useState } from 'react';
import { ChevronDown, ChevronUp, Globe, Loader2, RotateCcw, X } from 'lucide-react';
import { toast } from 'sonner';
import type { RemoteImportJob } from '../../../remoteImport';
import type { RemoteImportBatch } from '../../../remoteImportBatch';
import type { QueueItem } from '../../../types';
import { formatBytes } from '../../../utils';

interface Props {
    jobs: RemoteImportJob[];
    batches: RemoteImportBatch[];
    uploadItems: QueueItem[];
    onReview: (jobId: string, batchId?: string) => void;
    onCancel: (jobId: string, batchId?: string) => void | Promise<unknown>;
    onDismiss: (jobId: string, batchId?: string) => void | Promise<unknown>;
    onRetry: (jobId: string, batchId?: string) => void | Promise<unknown>;
}

function statusLabel(job: RemoteImportJob): string {
    // A terminal result takes precedence over an older acquisition progress tick.
    switch (job.phase) {
        case 'error': return 'Import failed';
        case 'partial-error': return 'Partially complete';
        case 'cancelled': return 'Cancelled';
        case 'success': return 'Saved to Telegram';
        case 'waiting-for-selection': return 'Ready · choose items';
        case 'uploading': return 'Uploading to Telegram';
        case 'finalizing': return 'Finalizing';
    }
    if (job.progress?.phase === 'connecting') return 'Connecting to torrent peers';
    if (job.progress?.phase === 'waiting_for_peers') return 'Waiting for torrent data';
    switch (job.phase) {
        case 'analyzing': return 'Analyzing link';
        case 'processing': return 'Processing media';
        case 'ready-to-upload': return 'Preparing upload';
        default: return 'Downloading source';
    }
}

export function RemoteImportQueue({ jobs, batches, uploadItems, onReview, onCancel, onDismiss, onRetry }: Props) {
    const [expanded, setExpanded] = useState(true);
    const [busy, setBusy] = useState<string[]>([]);
    const removed = new Set(batches.flatMap(batch => batch.entries.filter(entry => entry.phase === 'skipped').map(entry => entry.jobId)));
    const visible = jobs.filter(job => job.dismissedAt === undefined && !removed.has(job.id));
    if (!visible.length) return null;
    const perform = async (jobId: string, action: () => void | Promise<unknown>) => {
        setBusy(current => [...current, jobId]);
        try {
            await action();
        } catch (error) {
            toast.error(error instanceof Error ? error.message : String(error));
        } finally {
            setBusy(current => current.filter(id => id !== jobId));
        }
    };
    return (
        <section className="tr-transfer-dock w-full shrink-0 overflow-hidden" aria-label="Link imports">
            <div className="tr-transfer-header flex items-center justify-between gap-2">
                <h4 className="text-sm font-medium text-telegram-text flex items-center gap-2">
                    <Globe className="w-4 h-4 text-telegram-primary" /> Link imports
                    <span className="text-xs text-telegram-subtext">{visible.length}</span>
                </h4>
                <button type="button" className="p-1 text-telegram-subtext" onClick={() => setExpanded(value => !value)} aria-label={expanded ? 'Collapse imports' : 'Expand imports'} aria-expanded={expanded}>
                    {expanded ? <ChevronDown className="w-4 h-4" /> : <ChevronUp className="w-4 h-4" />}
                </button>
            </div>
            {expanded && <div className="tr-transfer-list max-h-52 overflow-y-auto p-2 space-y-2">
                {visible.map(original => {
                    const batch = batches.find(value => value.entries.some(entry => entry.jobId === original.id && entry.phase !== 'skipped'));
                    const entry = batch?.entries.find(value => value.jobId === original.id);
                    const job = batch?.phase === 'cancelled' && original.phase === 'waiting-for-selection'
                        ? { ...original, phase: 'cancelled' as const } : original;
                    const terminal = ['success', 'partial-error', 'error', 'cancelled'].includes(job.phase);
                    const active = !terminal && job.phase !== 'waiting-for-selection';
                    const failed = job.phase === 'error' || job.phase === 'partial-error';
                    const uploading = uploadItems.filter(item => job.uploadQueueIds.includes(item.id));
                    const bytes = job.phase === 'uploading'
                        ? uploading.reduce((sum, item) => sum + (item.uploadedBytes ?? 0), 0)
                        : job.progress?.downloaded_bytes ?? 0;
                    const total = job.phase === 'uploading'
                        ? uploading.reduce((sum, item) => sum + (item.totalBytes ?? 0), 0)
                        : job.progress?.total_bytes ?? 0;
                    const speed = job.phase === 'uploading'
                        ? uploading.reduce((sum, item) => sum + (item.speedBytesPerSec ?? 0), 0)
                        : job.progress?.speed_bytes_per_sec ?? 0;
                    const percent = total > 0 ? Math.min(100, Math.round(bytes / total * 100)) : null;
                    const title = job.analysis?.title || job.source;
                    const label = !active && !terminal && batch?.phase === 'importing' && batch.activeEntryIndex !== entry?.ordinal
                        ? 'Queued' : statusLabel(job);
                    return <div key={job.id} className="tr-transfer-item p-2.5 space-y-1.5">
                        <div className="flex items-center gap-2">
                            {active ? <Loader2 className="w-3.5 h-3.5 shrink-0 animate-spin text-telegram-primary" /> : <span className={failed ? 'text-red-400' : 'text-telegram-primary'}>●</span>}
                            <div className="min-w-0 flex-1 text-sm text-telegram-text truncate" title={title}>{title}</div>
                            {failed && <button type="button" aria-label="Retry import" title="Retry import" disabled={busy.includes(job.id)} className="p-1 text-telegram-primary" onClick={() => void perform(job.id, () => onRetry(job.id, batch?.id))}><RotateCcw className="w-3.5 h-3.5" /></button>}
                            <button type="button" className="p-1 text-telegram-subtext hover:text-red-400" disabled={busy.includes(job.id)}
                                aria-label={terminal ? 'Dismiss import' : batch ? 'Cancel import and remaining links in this batch' : 'Cancel import'}
                                title={terminal ? 'Dismiss' : batch ? 'Cancel remaining links in this batch' : 'Cancel import'}
                                onClick={() => void perform(job.id, () => terminal ? onDismiss(job.id, batch?.id) : onCancel(job.id, batch?.id))}>
                                <X className="w-3.5 h-3.5" />
                            </button>
                        </div>
                        <div className="flex items-center justify-between gap-2 text-xs text-telegram-subtext">
                            <span>{label}{active && percent !== null ? ' · ' + percent + '%' : ''}</span>
                            {!terminal && <button type="button" className="text-telegram-primary" onClick={() => onReview(job.id, batch?.id)}>Details</button>}
                        </div>
                        {active && (bytes > 0 || total > 0) && <div className="text-xs text-telegram-subtext">
                            {formatBytes(bytes)}{total > 0 ? ' / ' + formatBytes(total) : ''}{speed > 0 ? ' · ' + formatBytes(speed) + '/s' : ''}
                        </div>}
                        {active && percent !== null && <div className="h-1 rounded-full bg-telegram-hover overflow-hidden" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}>
                            <div className="h-full bg-telegram-primary transition-[width]" style={{ width: percent + '%' }} />
                        </div>}
                        {failed && (job.error || entry?.error) && <p className="text-xs text-red-400 break-words">{job.error || entry?.error}</p>}
                    </div>;
                })}
            </div>}
        </section>
    );
}
