import { useMemo } from 'react';
import {
    Check,
    Download,
    FolderInput,
    Paperclip,
    Pencil,
    Search,
    Share2,
    Trash2,
} from 'lucide-react';
import { TelegramFile } from '../../../types';
import { displayFileName, formatBytes } from '../../../utils';
import { FileTypeIcon } from '../../shared/FileTypeIcon';

interface ChannelFeedProps {
    channelName: string;
    files: TelegramFile[];
    loading: boolean;
    error: Error | null;
    selectedIds: number[];
    onFileClick: (e: React.MouseEvent, id: number) => void;
    onToggleSelection: (id: number) => void;
    onDownload: (id: number, name: string) => void;
    onPreview: (file: TelegramFile, orderedFiles?: TelegramFile[]) => void;
    onDelete: (id: number) => void;
    onRename: (file: TelegramFile) => void;
    onFileMove: (file: TelegramFile) => void;
    onShare?: (file: TelegramFile) => void;
    onManualUpload: () => void;
    onFolderUpload: () => void;
    onChannelInfo: () => void;
    totalFileCount: number;
    activeCategoryLabel?: string;
    showFolderUpload: boolean;
    onDrop?: (e: React.DragEvent, folderId: number) => void;
    activeFolderId: number;
    readOnly?: boolean;
    searchTerm: string;
    onSearchChange: (term: string) => void;
}

function parseTelegramDate(value?: string) {
    if (!value) return null;
    const normalized = value.endsWith(' UTC')
        ? value.replace(' ', 'T').replace(' UTC', 'Z')
        : value;
    const date = new Date(normalized);
    return Number.isNaN(date.getTime()) ? null : date;
}

function displayTime(value?: string) {
    const date = parseTelegramDate(value);
    if (!date) return '';
    return date.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
}

function displayDay(value?: string) {
    const date = parseTelegramDate(value);
    if (!date) return '';
    return date.toLocaleDateString([], { month: 'long', day: 'numeric' });
}

export function ChannelFeed({
    channelName,
    files,
    loading,
    error,
    selectedIds,
    onFileClick,
    onToggleSelection,
    onDownload,
    onPreview,
    onDelete,
    onRename,
    onFileMove,
    onShare,
    onManualUpload,
    onFolderUpload,
    onChannelInfo,
    totalFileCount,
    activeCategoryLabel,
    showFolderUpload,
    onDrop,
    activeFolderId,
    readOnly = false,
    searchTerm,
    onSearchChange,
}: ChannelFeedProps) {
    const orderedFiles = useMemo(
        () => [...files].sort((a, b) => (a.id ?? 0) - (b.id ?? 0)),
        [files],
    );

    if (loading) {
        return (
            <div className="flex-1 grid place-items-center text-telegram-subtext">
                <div className="flex flex-col items-center gap-3">
                    <div className="h-7 w-7 animate-spin rounded-full border-2 border-telegram-primary border-t-transparent" />
                    <span className="text-sm">Loading files…</span>
                </div>
            </div>
        );
    }

    if (error) {
        return (
            <div className="flex-1 grid place-items-center px-6 text-center">
                <div>
                    <p className="text-sm font-medium text-red-400">Couldn’t load this channel.</p>
                    <p className="mt-1 text-xs text-telegram-subtext">Try Sync or reopen the channel.</p>
                </div>
            </div>
        );
    }

    return (
        <section
            className="flex min-h-0 flex-1 flex-col bg-telegram-bg"
            onDragOver={(event) => {
                if (readOnly) return;
                event.preventDefault();
                event.dataTransfer.dropEffect = 'copy';
            }}
            onDrop={(event) => {
                if (readOnly) return;
                event.preventDefault();
                onDrop?.(event, activeFolderId);
            }}
        >
            <div className="border-b border-telegram-border bg-telegram-surface/90 px-5 py-3 backdrop-blur-md">
                <div className="mx-auto flex max-w-4xl items-center gap-4">
                    <button
                        type="button"
                        onClick={onChannelInfo}
                        className="min-w-0 shrink-0 rounded-xl px-2 py-1.5 text-left transition hover:bg-telegram-hover"
                        title="Channel info"
                    >
                        <h1 className="max-w-[220px] truncate text-[15px] font-semibold text-telegram-text">
                            {channelName}
                        </h1>
                        <p className="mt-0.5 text-[11px] text-telegram-subtext">
                            {activeCategoryLabel
                                ? `${activeCategoryLabel} · ${files.length} of ${totalFileCount}`
                                : `${totalFileCount} file${totalFileCount === 1 ? '' : 's'}`}
                        </p>
                    </button>

                    <div className="relative mx-auto max-w-md flex-1">
                        <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                        <input
                            type="search"
                            value={searchTerm}
                            onChange={(event) => onSearchChange(event.target.value)}
                            placeholder="Search files"
                            aria-label={`Search files in ${channelName}`}
                            className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-2 pl-9 pr-3 text-sm text-telegram-text outline-none transition placeholder:text-telegram-subtext/65 focus:border-telegram-primary/50"
                        />
                    </div>

                    {readOnly && (
                        <span className="shrink-0 rounded-full border border-telegram-border bg-telegram-hover/45 px-3 py-1.5 text-[11px] font-medium text-telegram-subtext">
                            Download only
                        </span>
                    )}
                </div>
            </div>

            <div className="custom-scrollbar flex-1 overflow-y-auto px-4 py-4">
                <div className="mx-auto max-w-4xl space-y-2">
                    {orderedFiles.length === 0 ? (
                        <div className="flex min-h-[420px] flex-col items-center justify-center px-6 text-center">
                            <div className="mb-4 grid h-14 w-14 place-items-center rounded-2xl border border-telegram-primary/15 bg-telegram-primary/10">
                                <Paperclip className="h-6 w-6 text-telegram-primary" />
                            </div>
                            <h2 className="text-base font-semibold text-telegram-text">
                                {activeCategoryLabel ? `No ${activeCategoryLabel.toLowerCase()} here` : 'No files yet'}
                            </h2>
                            <p className="mt-1 max-w-sm text-sm leading-relaxed text-telegram-subtext">
                                {readOnly
                                    ? 'Files shared by the channel owner will appear here.'
                                    : activeCategoryLabel
                                      ? 'Choose another file type from Channel Info or add new files.'
                                      : 'Add files below or drag and drop them into this channel.'}
                            </p>
                        </div>
                    ) : (
                        orderedFiles.map((file, index) => {
                            const selected = selectedIds.includes(file.id);
                            const day = displayDay(file.created_at);
                            const previousDay = index > 0 ? displayDay(orderedFiles[index - 1].created_at) : '';
                            const showDay = Boolean(day && day !== previousDay);
                            const time = displayTime(file.created_at);

                            return (
                                <div key={file.id}>
                                    {showDay && (
                                        <div className="flex justify-center py-2">
                                            <span className="rounded-full border border-telegram-border bg-telegram-surface/90 px-3 py-1 text-[11px] font-medium text-telegram-subtext">
                                                {day}
                                            </span>
                                        </div>
                                    )}

                                    <article
                                        onClick={(event) => onFileClick(event, file.id)}
                                        onDoubleClick={() => onPreview(file, orderedFiles)}
                                        className={[
                                            'group relative flex items-center gap-3 rounded-2xl border px-3 py-3 transition select-none',
                                            'bg-telegram-surface/90',
                                            selected
                                                ? 'border-telegram-primary bg-telegram-primary/5 ring-1 ring-telegram-primary/25'
                                                : 'border-telegram-border hover:border-telegram-primary/25 hover:bg-telegram-surface',
                                        ].join(' ')}
                                    >
                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onToggleSelection(file.id);
                                            }}
                                            className={[
                                                'grid h-5 w-5 shrink-0 place-items-center rounded-full border transition',
                                                selected
                                                    ? 'border-telegram-primary bg-telegram-primary text-white opacity-100'
                                                    : 'border-telegram-subtext/40 text-transparent opacity-0 group-hover:opacity-100 hover:border-telegram-primary',
                                            ].join(' ')}
                                            title={selected ? 'Deselect file' : 'Select file'}
                                            aria-label={selected ? `Deselect ${displayFileName(file.name)}` : `Select ${displayFileName(file.name)}`}
                                        >
                                            <Check className="h-3.5 w-3.5" />
                                        </button>

                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onPreview(file, orderedFiles);
                                            }}
                                            className="grid h-11 w-11 shrink-0 place-items-center rounded-xl bg-telegram-hover/65"
                                            title="Open file"
                                        >
                                            <FileTypeIcon filename={file.name} className="h-5 w-5" />
                                        </button>

                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onPreview(file, orderedFiles);
                                            }}
                                            className="min-w-0 flex-1 text-left"
                                        >
                                            <div className="truncate text-sm font-semibold text-telegram-text" title={displayFileName(file.name)}>
                                                {displayFileName(file.name)}
                                            </div>
                                            <div className="mt-1 flex items-center gap-1.5 text-[11px] text-telegram-subtext">
                                                <span>{formatBytes(file.size)}</span>
                                                {time && (
                                                    <>
                                                        <span className="opacity-45">•</span>
                                                        <span>{time}</span>
                                                    </>
                                                )}
                                            </div>
                                        </button>

                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onDownload(file.id, file.name);
                                            }}
                                            className="ml-auto grid h-9 w-9 shrink-0 place-items-center rounded-full text-telegram-primary transition hover:bg-telegram-primary/10"
                                            title="Download"
                                            aria-label={`Download ${file.name}`}
                                        >
                                            <Download className="h-4.5 w-4.5" />
                                        </button>

                                        {!readOnly && (
                                            <div className="absolute right-14 top-1/2 flex -translate-y-1/2 items-center gap-0.5 rounded-xl bg-telegram-surface/95 px-1 opacity-0 shadow-sm transition pointer-events-none group-hover:pointer-events-auto group-hover:opacity-100 focus-within:pointer-events-auto focus-within:opacity-100">
                                                {onShare && (
                                                    <button
                                                        type="button"
                                                        disabled={file.is_split}
                                                        onClick={(event) => {
                                                            event.stopPropagation();
                                                            if (!file.is_split) onShare(file);
                                                        }}
                                                        className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-primary disabled:cursor-not-allowed disabled:opacity-35 disabled:hover:bg-transparent disabled:hover:text-telegram-subtext"
                                                        title={file.is_split ? 'Sharing isn’t available for large split files yet' : 'Share'}
                                                    >
                                                        <Share2 className="h-4 w-4" />
                                                    </button>
                                                )}
                                                <button
                                                    type="button"
                                                    onClick={(event) => {
                                                        event.stopPropagation();
                                                        onRename(file);
                                                    }}
                                                    className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                                    title="Rename"
                                                >
                                                    <Pencil className="h-4 w-4" />
                                                </button>
                                                <button
                                                    type="button"
                                                    onClick={(event) => {
                                                        event.stopPropagation();
                                                        onFileMove(file);
                                                    }}
                                                    className="rounded-lg p-2 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                                                    title="Move"
                                                >
                                                    <FolderInput className="h-4 w-4" />
                                                </button>
                                                <button
                                                    type="button"
                                                    onClick={(event) => {
                                                        event.stopPropagation();
                                                        onDelete(file.id);
                                                    }}
                                                    className="rounded-lg p-2 text-telegram-subtext transition hover:bg-red-500/10 hover:text-red-400"
                                                    title="Delete"
                                                >
                                                    <Trash2 className="h-4 w-4" />
                                                </button>
                                            </div>
                                        )}
                                    </article>
                                </div>
                            );
                        })
                    )}
                </div>
            </div>

            {!readOnly && (
                <div className="border-t border-telegram-border bg-telegram-surface/90 px-4 py-3">
                    <div className="mx-auto flex max-w-4xl items-center justify-between gap-3">
                        <div className="text-xs text-telegram-subtext">
                            Drag files here or add them from your device.
                        </div>
                        <div className="flex items-center gap-2">
                            {showFolderUpload && (
                                <button
                                    type="button"
                                    onClick={onFolderUpload}
                                    className="inline-flex h-9 items-center gap-2 rounded-xl border border-telegram-border px-3 text-xs font-medium text-telegram-text transition hover:bg-telegram-hover"
                                >
                                    <FolderInput className="h-4 w-4" />
                                    Add folder
                                </button>
                            )}
                            <button
                                type="button"
                                onClick={onManualUpload}
                                className="inline-flex h-9 items-center gap-2 rounded-xl bg-telegram-primary px-3.5 text-xs font-semibold text-white transition hover:brightness-110"
                            >
                                <Paperclip className="h-4 w-4" />
                                Add files
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </section>
    );
}
