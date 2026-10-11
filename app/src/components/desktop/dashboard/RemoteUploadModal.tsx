import React, { useEffect, useMemo, useState } from 'react';
import { Globe, Layers3, Loader2, RotateCcw, X } from 'lucide-react';
import { toast } from 'sonner';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';
import type { RemoteImportJob } from '../../../remoteImport';
import type { RemoteImportBatch, RemoteImportMemberRef } from '../../../remoteImportBatch';
import { formatBytes } from '../../../utils';

interface RemoteImportBatchController {
    batches: RemoteImportBatch[];
    analyzeBatch: (rawInput: string) => Promise<RemoteImportBatch>;
    resumeBatchAnalysis: (batchId: string) => Promise<RemoteImportBatch>;
    setEntrySelections: (batchId: string, entryId: string, selectedIds: string[]) => Promise<void>;
    toggleEntrySelection: (batchId: string, entryId: string, selectionId: string) => Promise<void>;
    skipEntry: (batchId: string, entryId: string) => Promise<void>;
    retryEntryAnalysis: (batchId: string, entryId: string) => Promise<void>;
    groupMembers: (batchId: string, members: RemoteImportMemberRef[], primary?: RemoteImportMemberRef) => Promise<void>;
    setGroupPrimary: (batchId: string, groupId: string, primary: RemoteImportMemberRef) => Promise<void>;
    ungroup: (batchId: string, groupId: string) => Promise<void>;
    startBatch: (batchId: string) => Promise<void>;
    retryBatchEntry: (batchId: string, entryId: string) => Promise<void>;
    cancelBatch: (batchId: string) => Promise<void>;
}

interface RemoteUploadModalProps {
    isOpen: boolean;
    onClose: () => void;
    activeFolderId: number | null;
    currentChannelName: string;
    jobs: RemoteImportJob[];
    onAnalyze: (source: string) => Promise<RemoteImportJob>;
    onImport: (jobId: string, selectedIds: string[]) => Promise<RemoteImportJob>;
    onBatchSettled?: () => void | Promise<void>;
    remoteImportBatch: RemoteImportBatchController;
}

function errorMessage(error: unknown) {
    return error instanceof Error ? error.message : String(error);
}

function sourceLines(value: string): string[] {
    return value.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
}

function phaseLabel(job: RemoteImportJob) {
    switch (job.phase) {
        case 'analyzing': return 'Analyzing…';
        case 'downloading': return 'Downloading…';
        case 'processing': return 'Processing…';
        case 'ready-to-upload': return 'Preparing upload…';
        case 'uploading': return 'Uploading to TeraRelay…';
        case 'finalizing': return 'Finalizing…';
        case 'success': return 'Import complete';
        case 'partial-error': return 'Partially complete';
        case 'error': return 'Import needs attention';
        case 'cancelled': return 'Import cancelled';
        default: return 'Ready';
    }
}

function selectionLabel(job: RemoteImportJob | undefined, selectionId: string): string {
    const quality = job?.analysis?.qualities.find(value => value.format_id === selectionId);
    if (quality) return quality.label;
    const item = job?.analysis?.items.find(value => value.id === selectionId);
    return item?.label ?? selectionId;
}

function memberKey(member: RemoteImportMemberRef): string {
    return `${member.entryId}\u0000${member.selectionId}`;
}

export function RemoteUploadModal({
    isOpen,
    onClose,
    activeFolderId,
    currentChannelName,
    jobs,
    onAnalyze,
    onImport,
    onBatchSettled,
    remoteImportBatch,
}: RemoteUploadModalProps) {
    const [url, setUrl] = useState('');
    const [jobId, setJobId] = useState<string | null>(null);
    const [batchId, setBatchId] = useState<string | null>(null);
    const [selectedIds, setSelectedIds] = useState<string[]>([]);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [groupBuilderOpen, setGroupBuilderOpen] = useState(false);
    const [groupCandidates, setGroupCandidates] = useState<RemoteImportMemberRef[]>([]);
    const [groupPrimaryKey, setGroupPrimaryKey] = useState<string | null>(null);

    useEscapeToClose(isOpen, onClose);

    const latestActiveBatch = useMemo(() => [...remoteImportBatch.batches]
        .filter(batch => batch.folderId === activeFolderId && !['success', 'cancelled'].includes(batch.phase))
        .sort((left, right) => right.updatedAt - left.updatedAt)[0] ?? null,
    [activeFolderId, remoteImportBatch.batches]);

    useEffect(() => {
        if (!isOpen) return;
        setUrl('');
        setJobId(null);
        setBatchId(latestActiveBatch?.id ?? null);
        setSelectedIds([]);
        setBusy(false);
        setError(null);
        setGroupBuilderOpen(false);
        setGroupCandidates([]);
        setGroupPrimaryKey(null);
    }, [isOpen, latestActiveBatch?.id]);

    const lines = useMemo(() => sourceLines(url), [url]);
    const multiLinkMode = lines.length >= 2 || batchId !== null;
    const job = useMemo(() => jobs.find(candidate => candidate.id === jobId) ?? null, [jobs, jobId]);
    const batch = useMemo(
        () => remoteImportBatch.batches.find(candidate => candidate.id === batchId) ?? null,
        [batchId, remoteImportBatch.batches],
    );
    const jobsById = useMemo(() => new Map(jobs.map(value => [value.id, value] as const)), [jobs]);
    const analysis = job?.analysis;
    const qualities = useMemo(
        () => [...(analysis?.qualities ?? [])].sort((left, right) =>
            (left.height ?? 0) - (right.height ?? 0) || (left.fps ?? 0) - (right.fps ?? 0)
        ),
        [analysis],
    );
    const items = analysis?.items ?? [];
    const activePhase = job && !['waiting-for-selection', 'error', 'partial-error', 'success', 'cancelled'].includes(job.phase);

    const selectedBatchMembers = useMemo(() => {
        if (!batch) return [] as Array<RemoteImportMemberRef & { label: string; grouped: boolean }>;
        const grouped = new Set(batch.stackGroups.flatMap(group => group.members.map(memberKey)));
        return batch.entries.flatMap(entry => {
            const entryJob = entry.jobId ? jobsById.get(entry.jobId) : undefined;
            return entry.selectedIds.map(selectionId => ({
                entryId: entry.id,
                selectionId,
                label: selectionLabel(entryJob, selectionId),
                grouped: grouped.has(memberKey({ entryId: entry.id, selectionId })),
            }));
        });
    }, [batch, jobsById]);

    const ungroupedBatchMembers = selectedBatchMembers.filter(member => !member.grouped);
    const pendingAnalysis = !!batch?.entries.some(entry => !entry.jobId && entry.phase === 'pending');
    const analysisReady = !!batch && batch.entries.every(entry => {
        if (entry.phase === 'skipped') return true;
        if (!entry.jobId) return false;
        return !!jobsById.get(entry.jobId)?.analysis;
    });
    const selectionReady = analysisReady && !!batch && batch.entries.every(entry =>
        entry.phase === 'skipped' || entry.selectedIds.length > 0
    );
    const configurationLocked = batch?.startedAt !== undefined;
    const groupingEligible = !configurationLocked
        && ungroupedBatchMembers.length >= 2
        && new Set(ungroupedBatchMembers.map(member => member.entryId)).size >= 2;

    if (!isOpen) return null;

    const toggleSelection = (id: string) => {
        setSelectedIds(current => current.includes(id) ? current.filter(value => value !== id) : [...current, id]);
    };

    const handleAnalyze = async (event: React.FormEvent) => {
        event.preventDefault();
        if (activeFolderId === null) {
            toast.error('Open a writable TeraRelay channel first');
            return;
        }
        if (!lines.length) {
            setError('Paste a URL or magnet link first');
            return;
        }
        setBusy(true);
        setError(null);
        try {
            if (lines.length >= 2) {
                const next = await remoteImportBatch.analyzeBatch(url);
                setBatchId(next.id);
                setJobId(null);
                setSelectedIds([]);
            } else {
                const next = await onAnalyze(lines[0]);
                setJobId(next.id);
                setBatchId(null);
                setSelectedIds(next.selectedIds);
            }
        } catch (cause) {
            setError(errorMessage(cause));
        } finally {
            setBusy(false);
        }
    };

    const handleSingleImport = async () => {
        if (!job) return;
        const requiresSelection = (analysis?.qualities.length ?? 0) > 0 || (analysis?.items.length ?? 0) > 1;
        if (requiresSelection && selectedIds.length === 0) {
            setError('Choose at least one item to import');
            return;
        }
        setBusy(true);
        setError(null);
        try {
            await onImport(job.id, selectedIds);
            onClose();
        } catch (cause) {
            setError(errorMessage(cause));
        } finally {
            setBusy(false);
        }
    };

    const handleResumeBatchAnalysis = async () => {
        if (!batch) return;
        setBusy(true);
        setError(null);
        try {
            await remoteImportBatch.resumeBatchAnalysis(batch.id);
        } catch (cause) {
            setError(errorMessage(cause));
        } finally {
            setBusy(false);
        }
    };

    const handleBatchImport = () => {
        if (!batch) return;
        setError(null);
        void remoteImportBatch.startBatch(batch.id)
            .catch(cause => {
                toast.error(`Remote Import needs attention: ${errorMessage(cause)}`);
            })
            .finally(() => {
                void onBatchSettled?.();
            });
        onClose();
    };

    const toggleBatchSelection = async (entryId: string, selectionId: string) => {
        if (!batch) return;
        await remoteImportBatch.toggleEntrySelection(batch.id, entryId, selectionId);
    };

    const openGroupBuilder = () => {
        setGroupBuilderOpen(true);
        setGroupCandidates([]);
        setGroupPrimaryKey(null);
    };

    const toggleGroupCandidate = (member: RemoteImportMemberRef) => {
        const key = memberKey(member);
        setGroupCandidates(current => current.some(value => memberKey(value) === key)
            ? current.filter(value => memberKey(value) !== key)
            : [...current, member]);
        if (groupPrimaryKey === key) setGroupPrimaryKey(null);
    };

    const createVersionGroup = async () => {
        if (!batch) return;
        if (groupCandidates.length < 2 || new Set(groupCandidates.map(member => member.entryId)).size < 2) {
            setError('Choose at least two selected outputs from different links');
            return;
        }
        const primary = groupCandidates.find(member => memberKey(member) === groupPrimaryKey);
        if (!primary) {
            setError('Choose the Primary version for this group');
            return;
        }
        try {
            await remoteImportBatch.groupMembers(batch.id, groupCandidates, primary);
            setGroupBuilderOpen(false);
            setGroupCandidates([]);
            setGroupPrimaryKey(null);
            setError(null);
        } catch (cause) {
            setError(errorMessage(cause));
        }
    };

    const progressPercent = job?.progress?.total_bytes
        ? Math.min(100, Math.round((job.progress.downloaded_bytes / job.progress.total_bytes) * 100))
        : null;

    return (
        <div className="tr-modal-backdrop fixed inset-0 z-[200] flex items-center justify-center p-4" onClick={onClose}>
            <form
                onSubmit={handleAnalyze}
                className="tr-modal w-full max-w-[620px] overflow-hidden flex flex-col animate-in fade-in zoom-in-95 duration-150"
                onClick={event => event.stopPropagation()}
            >
                <div className="tr-modal-header p-4 flex items-center justify-between">
                    <div className="min-w-0">
                        <h3 className="text-telegram-text font-medium flex items-center gap-2">
                            <Globe className="w-5 h-5 text-telegram-primary" />
                            Upload from Link
                        </h3>
                        <p className="text-xs text-telegram-subtext mt-0.5 truncate">To {currentChannelName}</p>
                    </div>
                    <button type="button" onClick={onClose} className="tr-modal-close" aria-label="Close">
                        <X className="w-5 h-5" />
                    </button>
                </div>

                <div className="p-4 space-y-4 overflow-y-auto max-h-[72vh]">
                    {!batch && (
                        <div className="space-y-1.5">
                            <label className="text-xs text-telegram-subtext font-medium">Link{lines.length >= 2 ? 's' : ''}</label>
                            <div className="flex gap-2 items-start">
                                <textarea
                                    placeholder={'YouTube, Instagram, X, magnet, direct file…\nPaste one link per line for multiple imports'}
                                    value={url}
                                    onChange={event => {
                                        setUrl(event.target.value);
                                        if (jobId) { setJobId(null); setSelectedIds([]); }
                                        setError(null);
                                    }}
                                    className="tr-modal-input flex-1 min-w-0 px-3 py-2 text-sm placeholder:text-telegram-subtext/60 resize-none min-h-[42px] max-h-32"
                                    rows={lines.length >= 2 ? Math.min(5, Math.max(2, lines.length)) : 1}
                                    autoFocus
                                    disabled={busy || !!activePhase}
                                />
                                <button
                                    type="submit"
                                    className="tr-button tr-button--secondary tr-button--sm whitespace-nowrap"
                                    disabled={busy || !!activePhase || !lines.length}
                                >
                                    {busy && !job ? <Loader2 className="w-4 h-4 animate-spin" /> : null}
                                    Analyze
                                </button>
                            </div>
                            {lines.length >= 2 && (
                                <div className="text-[11px] text-telegram-subtext">{lines.length} links · choices stay independent</div>
                            )}
                        </div>
                    )}

                    {!multiLinkMode && analysis && (
                        <div className="space-y-3">
                            <div className="min-w-0">
                                <div className="text-sm font-medium text-telegram-text truncate" title={analysis.title}>{analysis.title}</div>
                                <div className="text-xs text-telegram-subtext mt-0.5">
                                    {analysis.source_label}
                                    {analysis.duration_seconds ? ` · ${Math.floor(analysis.duration_seconds / 60)}:${String(Math.round(analysis.duration_seconds % 60)).padStart(2, '0')}` : ''}
                                </div>
                            </div>

                            {qualities.length > 0 && (
                                <div className="space-y-1.5">
                                    <div className="text-xs font-medium text-telegram-subtext">Choose quality</div>
                                    <div className="space-y-1">
                                        {qualities.map(quality => (
                                            <label key={quality.format_id} className="flex items-center gap-3 px-3 py-2 rounded-lg hover:bg-telegram-hover cursor-pointer">
                                                <input type="checkbox" checked={selectedIds.includes(quality.format_id)} onChange={() => toggleSelection(quality.format_id)} disabled={busy || !!activePhase} />
                                                <span className="text-sm text-telegram-text min-w-[72px]">{quality.label}</span>
                                                <span className="text-xs text-telegram-subtext flex-1">{quality.estimated_size ? `~${formatBytes(quality.estimated_size)}` : ''}</span>
                                                {quality.recommended && <span className="text-xs text-telegram-primary">Recommended</span>}
                                            </label>
                                        ))}
                                    </div>
                                </div>
                            )}

                            {qualities.length === 0 && items.length > 1 && (
                                <div className="space-y-1.5">
                                    <div className="flex items-center justify-between gap-3">
                                        <div className="text-xs font-medium text-telegram-subtext">Choose items</div>
                                        <button type="button" className="text-xs text-telegram-primary" onClick={() => setSelectedIds(selectedIds.length === items.length ? [] : items.map(item => item.id))} disabled={busy || !!activePhase}>
                                            {selectedIds.length === items.length ? 'Clear' : 'Select all'}
                                        </button>
                                    </div>
                                    <div className="space-y-1 max-h-60 overflow-y-auto">
                                        {items.map(item => (
                                            <label key={item.id} className="flex items-center gap-3 px-3 py-2 rounded-lg hover:bg-telegram-hover cursor-pointer">
                                                <input type="checkbox" checked={selectedIds.includes(item.id)} onChange={() => toggleSelection(item.id)} disabled={busy || !!activePhase} />
                                                <span className="text-sm text-telegram-text flex-1 truncate" title={item.label}>{item.label}</span>
                                                {item.size ? <span className="text-xs text-telegram-subtext">{formatBytes(item.size)}</span> : null}
                                            </label>
                                        ))}
                                    </div>
                                </div>
                            )}

                            {qualities.length === 0 && items.length === 1 && (
                                <div className="px-3 py-2 rounded-lg bg-telegram-hover/50 flex items-center gap-3">
                                    <span className="text-sm text-telegram-text flex-1 truncate">{items[0].label}</span>
                                    {items[0].size ? <span className="text-xs text-telegram-subtext">{formatBytes(items[0].size)}</span> : null}
                                </div>
                            )}
                        </div>
                    )}

                    {batch && (
                        <div className="space-y-3">
                            <div className="flex items-center justify-between gap-3 text-xs text-telegram-subtext">
                                <span>{batch.entries.length} sources · processed in order</span>
                                {batch.activeEntryIndex !== null && (
                                    <span>current entry {batch.activeEntryIndex + 1} / {batch.entries.length}</span>
                                )}
                            </div>

                            {batch.entries.map(entry => {
                                const entryJob = entry.jobId ? jobsById.get(entry.jobId) : undefined;
                                const entryAnalysis = entryJob?.analysis;
                                const entryQualities = [...(entryAnalysis?.qualities ?? [])].sort((a, b) => (a.height ?? 0) - (b.height ?? 0));
                                const entryItems = entryAnalysis?.items ?? [];
                                const isActive = batch.activeEntryIndex === entry.ordinal;
                                return (
                                    <div key={entry.id} className="rounded-xl border border-telegram-divider/70 p-3 space-y-2">
                                        <div className="flex items-start gap-3">
                                            <div className="text-xs text-telegram-subtext w-5 pt-0.5">{entry.ordinal + 1}.</div>
                                            <div className="min-w-0 flex-1">
                                                <div className="text-sm text-telegram-text truncate" title={entryAnalysis?.title ?? entry.source}>{entryAnalysis?.title ?? entry.source}</div>
                                                <div className="text-[11px] text-telegram-subtext truncate">{entryAnalysis?.source_label ?? entry.source}</div>
                                            </div>
                                            <div className="text-[11px] text-telegram-subtext flex items-center gap-1">
                                                {isActive ? <Loader2 className="w-3 h-3 animate-spin" /> : null}
                                                {entry.phase}
                                            </div>
                                        </div>

                                        {entryQualities.length > 0 && (
                                            <div className="pl-8 space-y-1">
                                                {entryQualities.map(quality => (
                                                    <label key={quality.format_id} className="flex items-center gap-2 py-1.5 cursor-pointer">
                                                        <input
                                                            type="checkbox"
                                                            checked={entry.selectedIds.includes(quality.format_id)}
                                                            onChange={() => void toggleBatchSelection(entry.id, quality.format_id)}
                                                            disabled={configurationLocked}
                                                        />
                                                        <span className="text-sm text-telegram-text min-w-[68px]">{quality.label}</span>
                                                        <span className="text-xs text-telegram-subtext flex-1">{quality.estimated_size ? `~${formatBytes(quality.estimated_size)}` : ''}</span>
                                                        {quality.recommended ? <span className="text-[11px] text-telegram-primary">Recommended</span> : null}
                                                    </label>
                                                ))}
                                            </div>
                                        )}

                                        {entryQualities.length === 0 && entryItems.length > 0 && (
                                            <div className="pl-8 space-y-1 max-h-44 overflow-y-auto">
                                                {entryItems.map(item => (
                                                    <label key={item.id} className="flex items-center gap-2 py-1.5 cursor-pointer">
                                                        <input
                                                            type="checkbox"
                                                            checked={entry.selectedIds.includes(item.id)}
                                                            onChange={() => void toggleBatchSelection(entry.id, item.id)}
                                                            disabled={configurationLocked}
                                                        />
                                                        <span className="text-sm text-telegram-text flex-1 truncate" title={item.label}>{item.label}</span>
                                                        {item.size ? <span className="text-xs text-telegram-subtext">{formatBytes(item.size)}</span> : null}
                                                    </label>
                                                ))}
                                            </div>
                                        )}

                                        {entry.error && <div className="pl-8 text-xs text-red-500">{entry.error}</div>}
                                        {entry.phase === 'error' && (
                                            <div className="pl-8 flex gap-3">
                                                <button
                                                    type="button"
                                                    className="text-xs text-telegram-primary flex items-center gap-1"
                                                    onClick={() => void (entryJob?.analysis
                                                        ? remoteImportBatch.retryBatchEntry(batch.id, entry.id)
                                                        : remoteImportBatch.retryEntryAnalysis(batch.id, entry.id)
                                                    ).catch(cause => setError(errorMessage(cause)))}
                                                >
                                                    <RotateCcw className="w-3 h-3" /> {entryJob?.analysis ? 'Retry import' : 'Retry analysis'}
                                                </button>
                                                <button type="button" className="text-xs text-telegram-subtext" onClick={() => void remoteImportBatch.skipEntry(batch.id, entry.id)}>Skip</button>
                                            </div>
                                        )}
                                    </div>
                                );
                            })}

                            {batch.stackGroups.length > 0 && (
                                <div className="space-y-2">
                                    <div className="text-xs font-medium text-telegram-subtext">Version groups</div>
                                    {batch.stackGroups.map((group, index) => (
                                        <div key={group.id} className="rounded-lg bg-telegram-hover/50 px-3 py-2 space-y-1.5">
                                            <div className="flex items-center gap-2">
                                                <Layers3 className="w-4 h-4 text-telegram-primary" />
                                                <span className="text-sm text-telegram-text flex-1">Version group {index + 1}</span>
                                                {!configurationLocked && (
                                                    <button type="button" className="text-xs text-telegram-subtext" onClick={() => void remoteImportBatch.ungroup(batch.id, group.id)}>Ungroup</button>
                                                )}
                                            </div>
                                            {group.members.map(member => {
                                                const memberEntry = batch.entries.find(entry => entry.id === member.entryId);
                                                const memberJob = memberEntry?.jobId ? jobsById.get(memberEntry.jobId) : undefined;
                                                const primary = group.primary && memberKey(group.primary) === memberKey(member);
                                                return (
                                                    <div key={memberKey(member)} className="flex items-center gap-2 pl-6 text-xs">
                                                        <span className="text-telegram-text flex-1 truncate">{selectionLabel(memberJob, member.selectionId)}</span>
                                                        {primary ? (
                                                            <span className="text-telegram-primary">Primary</span>
                                                        ) : !configurationLocked ? (
                                                            <button type="button" className="text-telegram-subtext hover:text-telegram-primary" onClick={() => void remoteImportBatch.setGroupPrimary(batch.id, group.id, member)}>Make Primary</button>
                                                        ) : null}
                                                    </div>
                                                );
                                            })}
                                        </div>
                                    ))}
                                </div>
                            )}

                            {groupBuilderOpen ? (
                                <div className="rounded-xl border border-telegram-primary/30 p-3 space-y-2">
                                    <div className="text-sm font-medium text-telegram-text">Group as versions</div>
                                    <div className="text-xs text-telegram-subtext">Choose outputs from at least two links, then choose the Primary version.</div>
                                    {ungroupedBatchMembers.map(member => {
                                        const selected = groupCandidates.some(value => memberKey(value) === memberKey(member));
                                        return (
                                            <div key={memberKey(member)} className="flex items-center gap-2 text-sm py-1">
                                                <input type="checkbox" checked={selected} onChange={() => toggleGroupCandidate(member)} />
                                                <span className="flex-1 text-telegram-text truncate">{member.label}</span>
                                                {selected && (
                                                    <label className="text-xs text-telegram-subtext flex items-center gap-1 cursor-pointer">
                                                        <input type="radio" name="remote-import-primary" checked={groupPrimaryKey === memberKey(member)} onChange={() => setGroupPrimaryKey(memberKey(member))} />
                                                        Primary
                                                    </label>
                                                )}
                                            </div>
                                        );
                                    })}
                                    <div className="flex justify-end gap-2 pt-1">
                                        <button type="button" className="tr-button tr-button--secondary tr-button--sm" onClick={() => setGroupBuilderOpen(false)}>Cancel</button>
                                        <button type="button" className="tr-button tr-button--primary tr-button--sm" onClick={() => void createVersionGroup()}>Create group</button>
                                    </div>
                                </div>
                            ) : (
                                <button type="button" className="tr-button tr-button--secondary tr-button--sm" disabled={!groupingEligible} onClick={openGroupBuilder}>
                                    <Layers3 className="w-4 h-4" /> Group as versions
                                </button>
                            )}

                            {batch.error && <div className="text-xs text-red-500 rounded-lg border border-red-500/20 px-3 py-2">{batch.error}</div>}
                        </div>
                    )}

                    {!multiLinkMode && job && job.phase !== 'waiting-for-selection' && (
                        <div className="text-xs text-telegram-subtext flex items-center gap-2">
                            {activePhase ? <Loader2 className="w-3.5 h-3.5 animate-spin" /> : null}
                            <span>{phaseLabel(job)}{progressPercent !== null ? ` · ${progressPercent}%` : ''}</span>
                        </div>
                    )}

                    {(error || (!multiLinkMode && job?.error)) && (
                        <div className="text-xs text-red-500 rounded-lg border border-red-500/20 px-3 py-2">{error || job?.error}</div>
                    )}
                </div>

                <div className="tr-modal-footer">
                    <button type="button" onClick={onClose} className="tr-button tr-button--secondary tr-button--sm">Cancel</button>
                    {batch?.phase === 'importing' && (
                        <button
                            type="button"
                            className="tr-button tr-button--secondary tr-button--sm"
                            onClick={() => void remoteImportBatch.cancelBatch(batch.id).catch(cause => setError(errorMessage(cause)))}
                        >
                            Stop import
                        </button>
                    )}
                    {batch && pendingAnalysis && !configurationLocked && (
                        <button
                            type="button"
                            onClick={() => void handleResumeBatchAnalysis()}
                            className="tr-button tr-button--primary tr-button--sm"
                            disabled={busy}
                        >
                            {busy ? <Loader2 className="w-4 h-4 animate-spin" /> : null}
                            Resume analysis
                        </button>
                    )}
                    {!multiLinkMode && analysis && job?.phase === 'waiting-for-selection' && (
                        <button type="button" onClick={handleSingleImport} className="tr-button tr-button--primary tr-button--sm" disabled={busy || (qualities.length > 0 && selectedIds.length === 0)}>
                            {busy ? <Loader2 className="w-4 h-4 animate-spin" /> : null} Import
                        </button>
                    )}
                    {batch && analysisReady && !pendingAnalysis && !['importing', 'success', 'cancelled'].includes(batch.phase) && (
                        <button
                            type="button"
                            onClick={handleBatchImport}
                            className="tr-button tr-button--primary tr-button--sm"
                            disabled={!selectionReady}
                        >
                            Import
                        </button>
                    )}
                </div>
            </form>
        </div>
    );
}
