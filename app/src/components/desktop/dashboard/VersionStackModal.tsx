import { useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import {
    Download, Edit3, FilePlus2, Layers3, Loader2,
    Play, Plus, Save, Search, Trash2, Upload, X,
} from 'lucide-react';
import { toast } from 'sonner';
import { TelegramFile, FileStackView } from '../../../types';
import { formatBytes, displayFileName, sanitizeFilename } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { useConfirm } from '../../../context/ConfirmContext';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';
import type { VersionUploadTarget } from '../../../hooks/useFileUpload';

type Mode = 'manage' | 'add';

interface VersionStackModalProps {
    file: TelegramFile;
    allFiles: TelegramFile[];
    folderId: number;
    initialMode?: Mode;
    readOnly?: boolean;
    onClose: () => void;
    onChanged: () => Promise<void> | void;
    onUploadNew: (target: VersionUploadTarget) => Promise<void> | void;
    onPreview: (file: TelegramFile) => void;
    onDownload: (file: TelegramFile) => void;
    onDownloadAll: (files: TelegramFile[]) => Promise<void> | void;
    onMoveStack?: (file: TelegramFile) => void;
}

function cleanError(error: unknown): string {
    return error instanceof Error ? error.message : String(error);
}

function withSize(file: TelegramFile): TelegramFile {
    return {
        ...file,
        sizeStr: file.sizeStr || formatBytes(file.size),
        type: file.type || 'file',
    };
}

function addFilenameSuffix(filename: string, suffix: string): string {
    const dot = filename.lastIndexOf('.');
    if (dot > 0) return `${filename.slice(0, dot)} (${suffix})${filename.slice(dot)}`;
    return `${filename} (${suffix})`;
}

function uniqueDownloadFiles(members: FileStackView['members']): TelegramFile[] {
    const used = new Set<string>();
    return members.map((member, index) => {
        const original = member.file.name;
        let candidate = original;
        let key = sanitizeFilename(candidate).toLocaleLowerCase();
        if (used.has(key)) {
            const preferred = member.label?.trim() || `version ${index + 1}`;
            candidate = addFilenameSuffix(original, preferred);
            key = sanitizeFilename(candidate).toLocaleLowerCase();
            let collision = 2;
            while (used.has(key)) {
                candidate = addFilenameSuffix(original, `${preferred} ${collision++}`);
                key = sanitizeFilename(candidate).toLocaleLowerCase();
            }
        }
        used.add(key);
        return { ...member.file, name: candidate };
    });
}

export function VersionStackModal({
    file,
    allFiles,
    folderId,
    initialMode = 'manage',
    readOnly = false,
    onClose,
    onChanged,
    onUploadNew,
    onPreview,
    onDownload,
    onDownloadAll,
    onMoveStack,
}: VersionStackModalProps) {
    const { confirm } = useConfirm();
    const [stack, setStack] = useState<FileStackView | null>(null);
    const [mode, setMode] = useState<Mode>(file.stack_id ? initialMode : 'add');
    const [addMethod, setAddMethod] = useState<'choose' | 'existing'>('choose');
    const [search, setSearch] = useState('');
    const [makePrimary, setMakePrimary] = useState(false);
    const [loading, setLoading] = useState(!!file.stack_id);
    const [busy, setBusy] = useState(false);
    const [nameDraft, setNameDraft] = useState(file.stack_id ? getPremiumFileTitle(file) : displayFileName(file.name));
    const [labelDrafts, setLabelDrafts] = useState<Record<string, string>>({});

    useEscapeToClose(!busy, onClose);

    const loadStack = async () => {
        if (!file.stack_id) {
            setStack(null);
            return;
        }
        setLoading(true);
        try {
            const result = await invoke<FileStackView>('cmd_get_file_stack', {
                stackId: file.stack_id,
                folderId,
            });
            const normalized: FileStackView = {
                ...result,
                members: result.members.map(member => ({
                    ...member,
                    file: withSize(member.file),
                })),
            };
            setStack(normalized);
            setNameDraft(getPremiumFileTitle({ ...file, stack_name: normalized.display_name }));
            setLabelDrafts(Object.fromEntries(
                normalized.members.map(member => [member.file.logical_file_id || '', member.label || ''])
            ));
        } catch (error) {
            toast.error(`Could not load versions: ${cleanError(error)}`);
        } finally {
            setLoading(false);
        }
    };

    useEffect(() => {
        void loadStack();
    }, [file.stack_id, folderId]);

    const memberIds = useMemo(
        () => new Set((stack?.members || []).map(member => member.file.logical_file_id).filter(Boolean)),
        [stack],
    );

    const candidates = useMemo(() => {
        const query = search.trim().toLowerCase();
        return allFiles
            .filter(candidate =>
                candidate.type !== 'folder'
                && !!candidate.logical_file_id
                && !candidate.stack_id
                && candidate.logical_file_id !== file.logical_file_id
                && !memberIds.has(candidate.logical_file_id)
            )
            .filter(candidate => {
                if (!query) return true;
                const title = (candidate.stack_name || candidate.name).toLowerCase();
                return title.includes(query);
            });
    }, [allFiles, search, file.logical_file_id, memberIds]);

    const refreshAfterChange = async () => {
        await onChanged();
        if (file.stack_id) await loadStack();
    };

    const targetForUpload = (): VersionUploadTarget | null => {
        if (file.stack_id) {
            return { stackId: file.stack_id, makePrimary };
        }
        if (file.logical_file_id) {
            return { baseFileId: file.logical_file_id, makePrimary };
        }
        return null;
    };

    const uploadNew = async () => {
        const target = targetForUpload();
        if (!target) {
            toast.error('This file does not have stable TeraRelay metadata yet.');
            return;
        }
        await onUploadNew(target);
        onClose();
    };

    const addExisting = async (candidate: TelegramFile) => {
        if (!candidate.logical_file_id || busy) return;
        setBusy(true);
        try {
            if (file.stack_id) {
                await invoke('cmd_add_file_to_stack', {
                    folderId,
                    stackId: file.stack_id,
                    fileId: candidate.logical_file_id,
                    makePrimary,
                });
            } else {
                if (!file.logical_file_id) {
                    throw new Error('The base file does not have a stable TeraRelay file ID.');
                }
                await invoke('cmd_create_file_stack', {
                    folderId,
                    baseFileId: file.logical_file_id,
                    versionFileId: candidate.logical_file_id,
                    primaryFileId: makePrimary ? candidate.logical_file_id : file.logical_file_id,
                });
            }
            toast.success('Version added');
            await onChanged();
            onClose();
        } catch (error) {
            toast.error(`Could not add version: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const setPrimary = async (member: TelegramFile) => {
        if (!stack || !member.logical_file_id || busy) return;
        setBusy(true);
        try {
            await invoke('cmd_set_file_stack_primary', {
                folderId,
                stackId: stack.stack_id,
                fileId: member.logical_file_id,
            });
            toast.success('Primary version updated');
            await refreshAfterChange();
        } catch (error) {
            toast.error(`Could not change primary version: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const saveStackName = async () => {
        if (!stack || busy) return;
        const name = nameDraft.trim();
        if (!name || name === stack.display_name) return;
        setBusy(true);
        try {
            await invoke('cmd_rename_file_stack', {
                folderId,
                stackId: stack.stack_id,
                displayName: name,
            });
            toast.success('Stack name updated');
            await refreshAfterChange();
        } catch (error) {
            toast.error(`Could not rename stack: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const saveLabel = async (member: TelegramFile) => {
        if (!stack || !member.logical_file_id || busy) return;
        setBusy(true);
        try {
            await invoke('cmd_update_file_stack_label', {
                folderId,
                stackId: stack.stack_id,
                fileId: member.logical_file_id,
                label: labelDrafts[member.logical_file_id] || null,
            });
            toast.success('Version label updated');
            await refreshAfterChange();
        } catch (error) {
            toast.error(`Could not update label: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const removeMember = async (member: TelegramFile) => {
        if (!stack || !member.logical_file_id || busy) return;
        const ok = await confirm({
            title: 'Remove From Version Stack',
            message: `Remove “${displayFileName(member.name)}” from this stack?\n\nThe file itself will NOT be deleted.`,
            confirmText: 'Remove',
            variant: 'info',
        });
        if (!ok) return;
        setBusy(true);
        try {
            await invoke('cmd_remove_file_from_stack', {
                folderId,
                stackId: stack.stack_id,
                fileId: member.logical_file_id,
            });
            toast.success('Version removed; file kept');
            await onChanged();
            if (stack.members.length <= 2) {
                onClose();
            } else {
                await loadStack();
            }
        } catch (error) {
            toast.error(`Could not remove version: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const deleteMember = async (member: TelegramFile) => {
        if (!stack || !member.logical_file_id || busy) return;
        const ok = await confirm({
            title: 'Delete This Version',
            message: `Permanently delete “${displayFileName(member.name)}” from Telegram?\n\nOther versions will be kept.`,
            confirmText: 'Delete Version',
            variant: 'danger',
        });
        if (!ok) return;
        setBusy(true);
        try {
            await invoke('cmd_remove_file_from_stack', {
                folderId,
                stackId: stack.stack_id,
                fileId: member.logical_file_id,
            });
            await invoke('cmd_delete_file', {
                messageId: member.id,
                folderId,
            });
            await Promise.all([
                invoke('cmd_delete_image_thumbnail', { messageId: member.id, folderId }).catch(() => {}),
                invoke('cmd_delete_preview_for_message', { messageId: member.id, folderId }).catch(() => {}),
            ]);
            toast.success('Version deleted');
            await onChanged();
            if (stack.members.length <= 2) onClose();
            else await loadStack();
        } catch (error) {
            toast.error(`Delete did not fully complete: ${cleanError(error)}. Refresh to see the safe current state.`);
            await onChanged();
        } finally {
            setBusy(false);
        }
    };

    const unstack = async () => {
        if (!stack || busy) return;
        const ok = await confirm({
            title: 'Remove Version Stack',
            message: 'Separate these versions back into normal files?\n\nNo uploaded files will be deleted.',
            confirmText: 'Remove Stack',
            variant: 'info',
        });
        if (!ok) return;
        setBusy(true);
        try {
            await invoke('cmd_unstack_file_stack', {
                folderId,
                stackId: stack.stack_id,
            });
            toast.success('Stack removed; all files kept');
            await onChanged();
            onClose();
        } catch (error) {
            toast.error(`Could not remove stack: ${cleanError(error)}`);
        } finally {
            setBusy(false);
        }
    };

    const deleteAll = async () => {
        if (!stack || busy) return;
        const ok = await confirm({
            title: 'Delete All Versions',
            message: `Permanently delete all ${stack.members.length} versions in “${stack.display_name}” from Telegram?\n\nThis cannot be undone.`,
            confirmText: 'Delete All Versions',
            variant: 'danger',
        });
        if (!ok) return;
        setBusy(true);
        try {
            await invoke('cmd_delete_file_stack_all', {
                folderId,
                stackId: stack.stack_id,
            });
            await Promise.all(stack.members.flatMap(member => [
                invoke('cmd_delete_image_thumbnail', { messageId: member.file.id, folderId }).catch(() => {}),
                invoke('cmd_delete_preview_for_message', { messageId: member.file.id, folderId }).catch(() => {}),
            ]));
            toast.success('All versions deleted');
            await onChanged();
            onClose();
        } catch (error) {
            toast.error(cleanError(error));
            await onChanged();
            onClose();
        } finally {
            setBusy(false);
        }
    };

    const title = stack
        ? getPremiumFileTitle({ ...file, stack_name: stack.display_name })
        : getPremiumFileTitle(file);

    return (
        <div
            className="tr-modal-backdrop fixed inset-0 z-[240] flex items-center justify-center p-3 sm:p-5"
            onMouseDown={(event) => {
                if (event.target === event.currentTarget && !busy) onClose();
            }}
        >
            <div className="tr-modal flex max-h-[88vh] w-full max-w-[760px] flex-col overflow-hidden">
                <div className="tr-modal-header flex items-center gap-3 px-4 py-3.5 sm:px-5">
                    <div className="tr-modal-icon grid h-10 w-10 shrink-0 place-items-center">
                        <Layers3 className="h-5 w-5" />
                    </div>
                    <div className="min-w-0 flex-1">
                        <h2 className="truncate text-base font-semibold text-telegram-text">{title}</h2>
                        <p className="text-xs text-telegram-subtext">
                            {stack ? 'Manage grouped files' : 'Add another file'}
                        </p>
                    </div>
                    {busy && <Loader2 className="h-4 w-4 animate-spin text-telegram-primary" />}
                    <button
                        type="button"
                        onClick={onClose}
                        disabled={busy}
                        className="tr-modal-close disabled:opacity-40"
                        aria-label="Close"
                    >
                        <X className="h-4 w-4" />
                    </button>
                </div>

                {loading ? (
                    <div className="grid min-h-[260px] place-items-center">
                        <Loader2 className="h-7 w-7 animate-spin text-telegram-primary" />
                    </div>
                ) : (
                    <div className="flex-1 overflow-y-auto">
                        {stack && mode === 'manage' && (
                            <div className="space-y-5 p-4 sm:p-5">
                                {!readOnly && (
                                    <div className="rounded-xl border border-telegram-border bg-telegram-bg/35 p-3.5">
                                        <div className="mb-2 flex items-center gap-2 text-xs font-semibold text-telegram-subtext">
                                            <Edit3 className="h-3.5 w-3.5" />
                                            Stack name
                                        </div>
                                        <div className="flex gap-2">
                                            <input
                                                value={nameDraft}
                                                onChange={event => setNameDraft(event.target.value)}
                                                maxLength={512}
                                                className="tr-modal-input min-w-0 flex-1 px-3 py-2 text-sm"
                                            />
                                            <button
                                                onClick={saveStackName}
                                                disabled={busy || !nameDraft.trim() || nameDraft.trim() === title}
                                                className="grid h-10 w-10 place-items-center rounded-xl bg-telegram-primary text-white transition hover:brightness-110 disabled:opacity-35"
                                                title="Save stack name"
                                            >
                                                <Save className="h-4 w-4" />
                                            </button>
                                        </div>
                                    </div>
                                )}

                                <div className="space-y-2">
                                    {stack.members.map(member => {
                                        const memberFile = member.file;
                                        const id = memberFile.logical_file_id || '';
                                        const labelChanged = (labelDrafts[id] || '') !== (member.label || '');
                                        return (
                                            <div
                                                key={id || memberFile.id}
                                                className={[
                                                    'rounded-xl border p-3',
                                                    member.is_primary
                                                        ? 'border-telegram-primary/45 bg-telegram-primary/[0.06]'
                                                        : 'border-telegram-border bg-telegram-bg/25',
                                                ].join(' ')}
                                            >
                                                <div className="flex items-start gap-3">
                                                    <div className="min-w-0 flex-1">
                                                        <div className="flex flex-wrap items-center gap-2">
                                                            <span className="truncate text-sm font-semibold text-telegram-text">
                                                                {displayFileName(memberFile.name)}
                                                            </span>

                                                        </div>
                                                        <div className="mt-1 text-[11px] text-telegram-subtext">
                                                            {formatBytes(memberFile.size)}
                                                        </div>
                                                    </div>
                                                    <div className="flex shrink-0 items-center gap-1">
                                                        <button
                                                            onClick={() => onPreview(memberFile)}
                                                            className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-primary"
                                                            title="Preview this version"
                                                        >
                                                            <Play className="h-4 w-4" />
                                                        </button>
                                                        <button
                                                            onClick={() => onDownload(memberFile)}
                                                            className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-emerald-400"
                                                            title="Download this version"
                                                        >
                                                            <Download className="h-4 w-4" />
                                                        </button>
                                                    </div>
                                                </div>

                                                {!readOnly && (
                                                    <div className="mt-3 flex flex-col gap-2 border-t border-telegram-border/70 pt-3 sm:flex-row sm:items-center">
                                                        <div className="flex min-w-0 flex-1 gap-2">
                                                            <input
                                                                value={labelDrafts[id] || ''}
                                                                onChange={event => setLabelDrafts(prev => ({ ...prev, [id]: event.target.value }))}
                                                                maxLength={64}
                                                                placeholder="Label, e.g. 4K HDR"
                                                                className="min-w-0 flex-1 rounded-lg border border-telegram-border bg-telegram-bg px-2.5 py-1.5 text-xs text-telegram-text outline-none focus:border-telegram-primary/50"
                                                            />
                                                            <button
                                                                onClick={() => saveLabel(memberFile)}
                                                                disabled={busy || !labelChanged}
                                                                className="rounded-lg border border-telegram-border px-2.5 text-xs text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text disabled:opacity-30"
                                                            >
                                                                Save
                                                            </button>
                                                        </div>
                                                        <div className="flex flex-wrap gap-1.5">
                                                            {!member.is_primary && (
                                                                <button
                                                                    onClick={() => setPrimary(memberFile)}
                                                                    disabled={busy}
                                                                    className="rounded-lg border border-amber-500/20 bg-amber-500/10 px-2.5 py-1.5 text-[11px] font-medium text-amber-400 transition hover:bg-amber-500/15"
                                                                >
                                                                    Set primary
                                                                </button>
                                                            )}
                                                            <button
                                                                onClick={() => removeMember(memberFile)}
                                                                disabled={busy}
                                                                className="rounded-lg border border-telegram-border px-2.5 py-1.5 text-[11px] text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                                            >
                                                                Remove from stack
                                                            </button>
                                                            <button
                                                                onClick={() => deleteMember(memberFile)}
                                                                disabled={busy}
                                                                className="rounded-lg border border-red-500/20 px-2.5 py-1.5 text-[11px] text-red-400 transition hover:bg-red-500/10"
                                                            >
                                                                Delete version
                                                            </button>
                                                        </div>
                                                    </div>
                                                )}
                                            </div>
                                        );
                                    })}
                                </div>

                                <button
                                    onClick={() => onDownloadAll(uniqueDownloadFiles(stack.members))}
                                    disabled={busy}
                                    className="flex w-full items-center justify-center gap-2 rounded-xl border border-emerald-500/20 bg-emerald-500/[0.05] px-4 py-2.5 text-xs font-medium text-emerald-400 transition hover:bg-emerald-500/[0.09] disabled:opacity-40"
                                >
                                    <Download className="h-4 w-4" />
                                    Download all
                                </button>

                                {!readOnly && (
                                    <>
                                        <button
                                            onClick={() => {
                                                setMode('add');
                                                setAddMethod('choose');
                                                setSearch('');
                                                setMakePrimary(false);
                                            }}
                                            className="flex w-full items-center justify-center gap-2 rounded-xl border border-dashed border-telegram-primary/35 bg-telegram-primary/[0.04] px-4 py-3 text-sm font-medium text-telegram-primary transition hover:bg-telegram-primary/[0.08]"
                                        >
                                            <Plus className="h-4 w-4" />
                                            Add another version
                                        </button>

                                        <div className="flex flex-wrap items-center justify-between gap-2 border-t border-telegram-border pt-4">
                                            <div className="flex flex-wrap gap-2">
                                                {onMoveStack && (
                                                    <button
                                                        onClick={() => onMoveStack(file)}
                                                        disabled={busy}
                                                        className="rounded-xl border border-telegram-border px-3 py-2 text-xs text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                                    >
                                                        Move stack
                                                    </button>
                                                )}
                                                <button
                                                    onClick={unstack}
                                                    disabled={busy}
                                                    className="rounded-xl border border-telegram-border px-3 py-2 text-xs text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                                >
                                                    Remove stack only
                                                </button>
                                            </div>
                                            <button
                                                onClick={deleteAll}
                                                disabled={busy}
                                                className="inline-flex items-center gap-1.5 rounded-xl border border-red-500/20 bg-red-500/[0.06] px-3 py-2 text-xs text-red-400 transition hover:bg-red-500/10"
                                            >
                                                <Trash2 className="h-3.5 w-3.5" />
                                                Delete all files
                                            </button>
                                        </div>
                                    </>
                                )}
                            </div>
                        )}

                        {mode === 'add' && !readOnly && (
                            <div className="space-y-4 p-4 sm:p-5">
                                {stack && (
                                    <button
                                        type="button"
                                        onClick={() => setMode('manage')}
                                        className="text-xs font-medium text-telegram-primary hover:underline"
                                    >
                                        ← Back
                                    </button>
                                )}

                                <div>
                                    <h3 className="text-sm font-semibold text-telegram-text">Add version</h3>
                                    <p className="mt-1 text-xs leading-relaxed text-telegram-subtext">
                                        Upload a new file or choose one already stored in this TeraRelay channel.
                                    </p>
                                </div>

                                <label className="flex cursor-pointer items-start gap-3 rounded-xl border border-telegram-border bg-telegram-bg/30 p-3">
                                    <input
                                        type="checkbox"
                                        checked={makePrimary}
                                        onChange={event => setMakePrimary(event.target.checked)}
                                        className="mt-0.5"
                                    />
                                    <span>
                                        <span className="block text-sm font-medium text-telegram-text">Make added file primary</span>
                                        <span className="mt-0.5 block text-[11px] text-telegram-subtext">
                                            Off by default. The current primary stays unchanged unless you choose this.
                                        </span>
                                    </span>
                                </label>

                                {addMethod === 'choose' ? (
                                    <div className="grid gap-3 sm:grid-cols-2">
                                        <button
                                            onClick={uploadNew}
                                            disabled={busy}
                                            className="group rounded-2xl border border-telegram-primary/25 bg-telegram-primary/[0.06] p-4 text-left transition hover:border-telegram-primary/50 hover:bg-telegram-primary/[0.09]"
                                        >
                                            <div className="mb-3 grid h-10 w-10 place-items-center rounded-xl bg-telegram-primary/12 text-telegram-primary">
                                                <Upload className="h-5 w-5" />
                                            </div>
                                            <div className="text-sm font-semibold text-telegram-text">Upload new version</div>
                                            <div className="mt-1 text-xs leading-relaxed text-telegram-subtext">
                                                Choose one file from your device. It uses the normal resumable TeraRelay upload queue.
                                            </div>
                                        </button>
                                        <button
                                            onClick={() => setAddMethod('existing')}
                                            disabled={busy}
                                            className="group rounded-2xl border border-telegram-border bg-telegram-bg/30 p-4 text-left transition hover:border-telegram-primary/35 hover:bg-telegram-hover/40"
                                        >
                                            <div className="mb-3 grid h-10 w-10 place-items-center rounded-xl bg-telegram-hover text-telegram-text">
                                                <FilePlus2 className="h-5 w-5" />
                                            </div>
                                            <div className="text-sm font-semibold text-telegram-text">Use existing TeraRelay file</div>
                                            <div className="mt-1 text-xs leading-relaxed text-telegram-subtext">
                                                Search this channel and attach a file that is already uploaded.
                                            </div>
                                        </button>
                                    </div>
                                ) : (
                                    <div className="space-y-3">
                                        <button
                                            onClick={() => setAddMethod('choose')}
                                            className="text-xs font-medium text-telegram-primary hover:underline"
                                        >
                                            ← Choose another method
                                        </button>
                                        <div className="relative">
                                            <Search className="absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                            <input
                                                autoFocus
                                                value={search}
                                                onChange={event => setSearch(event.target.value)}
                                                placeholder="Search existing files"
                                                className="tr-modal-input w-full py-2.5 pl-9 pr-3 text-sm"
                                            />
                                        </div>
                                        <div className="max-h-[340px] space-y-1.5 overflow-y-auto">
                                            {candidates.length === 0 ? (
                                                <div className="rounded-xl border border-dashed border-telegram-border px-4 py-8 text-center text-xs text-telegram-subtext">
                                                    No available unstacked files match your search.
                                                </div>
                                            ) : candidates.map(candidate => (
                                                <button
                                                    key={candidate.logical_file_id || candidate.id}
                                                    onClick={() => addExisting(candidate)}
                                                    disabled={busy}
                                                    className="flex w-full items-center gap-3 rounded-xl border border-transparent px-3 py-2.5 text-left transition hover:border-telegram-border hover:bg-telegram-hover/50 disabled:opacity-50"
                                                >
                                                    <div className="grid h-9 w-9 shrink-0 place-items-center rounded-lg bg-telegram-hover text-telegram-subtext">
                                                        <FilePlus2 className="h-4 w-4" />
                                                    </div>
                                                    <div className="min-w-0 flex-1">
                                                        <div className="truncate text-sm font-medium text-telegram-text">
                                                            {candidate.stack_name || displayFileName(candidate.name)}
                                                        </div>
                                                        <div className="mt-0.5 text-[11px] text-telegram-subtext">
                                                            {formatBytes(candidate.size)}
                                                        </div>
                                                    </div>
                                                </button>
                                            ))}
                                        </div>
                                    </div>
                                )}
                            </div>
                        )}

                        {readOnly && stack && (
                            <div className="border-t border-telegram-border px-5 py-3 text-xs text-telegram-subtext">
                                This channel is read-only. You can preview and download files, but only the owner can change the group.
                            </div>
                        )}
                    </div>
                )}
            </div>
        </div>
    );
}
