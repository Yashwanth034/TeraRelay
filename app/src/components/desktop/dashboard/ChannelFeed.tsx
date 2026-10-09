import { useMemo, useState } from 'react';
import {
    Check,
    Download,
    FolderInput,
    Paperclip,
    MoreVertical,
} from 'lucide-react';
import { TelegramFile, TelegramFolder } from '../../../types';
import { ContextMenu } from './ContextMenu';
import { displayFileName, formatBytes } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';
import { RichFileMetaText } from '../../shared/RichFileMetaText';
import { PremiumButton } from '../../ui/PremiumPrimitives';
import { StackedFileRow } from './StackedFileRow';

interface ChannelFeedProps {
    files: TelegramFile[];
    loading: boolean;
    error: Error | null;
    selectedIds: number[];
    onFileClick: (e: React.MouseEvent, id: number) => void;
    onToggleSelection: (id: number) => void;
    onDownload: (id: number, name: string) => void;
    onDownloadAll: (files: TelegramFile[]) => Promise<void> | void;
    onPreview: (file: TelegramFile, orderedFiles?: TelegramFile[]) => void;
    onDetails?: (file: TelegramFile) => void;
    onDelete: (id: number) => void;
    onRename: (file: TelegramFile) => void;
    onFileMove: (file: TelegramFile) => void;
    onShare?: (file: TelegramFile) => void;
    onAddVersion?: (file: TelegramFile) => void;
    onManageVersions?: (file: TelegramFile) => void;
    folders?: TelegramFolder[];
    onManualUpload: () => void;
    onFolderUpload: () => void;
    activeCategoryLabel?: string;
    showFolderUpload: boolean;
    onDrop?: (e: React.DragEvent, folderId: number) => void;
    activeFolderId: number;
    readOnly?: boolean;
}

function parseTelegramDate(value?: string) {
    if (!value) return null;
    const normalized = value.endsWith(' UTC')
        ? value.replace(' ', 'T').replace(' UTC', 'Z')
        : value;
    const date = new Date(normalized);
    return Number.isNaN(date.getTime()) ? null : date;
}

function channelDayKey(value?: string) {
    const date = parseTelegramDate(value);
    if (!date) return '';
    return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

function formatChannelDay(value?: string) {
    const date = parseTelegramDate(value);
    if (!date) return '';

    const today = new Date();
    const yesterday = new Date(today);
    yesterday.setDate(today.getDate() - 1);

    if (channelDayKey(value) === channelDayKey(today.toISOString())) return 'Today';
    if (channelDayKey(value) === channelDayKey(yesterday.toISOString())) return 'Yesterday';

    return date.toLocaleDateString([], {
        weekday: 'short',
        month: 'short',
        day: 'numeric',
        ...(date.getFullYear() === today.getFullYear() ? {} : { year: 'numeric' }),
    });
}

export function ChannelFeed({
    files,
    loading,
    error,
    selectedIds,
    onFileClick,
    onToggleSelection,
    onDownload,
    onDownloadAll,
    onPreview,
    onDetails,
    onDelete,
    onRename,
    onFileMove,
    onShare,
    onAddVersion,
    onManageVersions,
    folders,
    onManualUpload,
    onFolderUpload,
    activeCategoryLabel,
    showFolderUpload,
    onDrop,
    activeFolderId,
    readOnly = false,
}: ChannelFeedProps) {
    const [contextMenu, setContextMenu] = useState<{ x: number; y: number; file: TelegramFile } | null>(null);
    const [expandedStackIds, setExpandedStackIds] = useState<Set<string>>(() => new Set());

    const orderedFiles = useMemo(
        () => [...files].sort((a, b) => {
            const aTime = parseTelegramDate(a.created_at)?.getTime() ?? 0;
            const bTime = parseTelegramDate(b.created_at)?.getTime() ?? 0;
            return aTime === bTime ? (a.id ?? 0) - (b.id ?? 0) : aTime - bTime;
        }),
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
            className="tr-channel-feed flex min-h-0 flex-1 flex-col"
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
            <div className="tr-channel-scroll custom-scrollbar flex-1 overflow-y-auto px-4 py-3">
                <div className="mx-auto max-w-[820px] space-y-1.5">
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
                            const title = getPremiumFileTitle(file);
                            const dayKey = channelDayKey(file.created_at);
                            const previousDayKey = index > 0 ? channelDayKey(orderedFiles[index - 1].created_at) : '';
                            const dayLabel = formatChannelDay(file.created_at);
                            const showDay = Boolean(dayKey && dayLabel && dayKey !== previousDayKey);
                            const dayDivider = showDay ? (
                                <div
                                    className="tr-channel-day-divider flex items-center gap-2 px-1 pb-1 pt-3"
                                    role="separator"
                                    aria-label={`Uploads from ${dayLabel}`}
                                >
                                    <span className="shrink-0 text-[10px] font-semibold uppercase tracking-[0.12em] text-telegram-subtext/80">
                                        {dayLabel}
                                    </span>
                                    <span className="h-px flex-1 bg-telegram-border/60" />
                                </div>
                            ) : null;

                            if (file.stack_id && (file.stack_version_count ?? 0) > 1) {
                                return (
                                    <div key={`stack-${file.stack_id}`}>
                                        {dayDivider}
                                        <StackedFileRow
                                            file={file}
                                            folderId={activeFolderId}
                                            expanded={expandedStackIds.has(file.stack_id)}
                                            onExpandedChange={(nextExpanded) => {
                                                setExpandedStackIds(current => {
                                                    const next = new Set(current);
                                                    if (nextExpanded) next.add(file.stack_id!);
                                                    else next.delete(file.stack_id!);
                                                    return next;
                                                });
                                            }}
                                            onPreview={onPreview}
                                            onDetails={onDetails}
                                            onDownload={onDownload}
                                            onDownloadAll={onDownloadAll}
                                            onMore={(event, targetFile) => {
                                                event.stopPropagation();
                                                const rect = event.currentTarget.getBoundingClientRect();
                                                setContextMenu({
                                                    x: Math.min(rect.right, window.innerWidth - 8),
                                                    y: rect.bottom + 4,
                                                    file: targetFile,
                                                });
                                            }}
                                        />
                                    </div>
                                );
                            }

                            return (
                                <div key={file.id}>
                                    {dayDivider}
                                    <article
                                        onClick={(event) => onFileClick(event, file.id)}
                                        onDoubleClick={() => onPreview(file, orderedFiles)}
                                        className={[
                                            'tr-file-row tr-file-card group relative flex items-center gap-2.5 px-2.5 py-1.5 select-none',
                                            selected ? 'tr-file-row--selected' : '',
                                        ].join(' ')}
                                    >
                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onToggleSelection(file.id);
                                            }}
                                            className={[
                                                'tr-file-select',
                                                selected ? 'tr-file-select--active' : '',
                                            ].join(' ')}
                                            title={selected ? 'Deselect file' : 'Select file'}
                                            aria-label={selected ? `Deselect ${displayFileName(file.name)}` : `Select ${displayFileName(file.name)}`}
                                        >
                                            <Check className="h-3.5 w-3.5" />
                                        </button>

                                        <PremiumFileThumbnail
                                            file={file}
                                            folderId={activeFolderId}
                                            onOpen={() => onPreview(file, orderedFiles)}
                                        />

                                        <div
                                            role="button"
                                            tabIndex={0}
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                if (onDetails) onDetails(file);
                                                else onPreview(file, orderedFiles);
                                            }}
                                            onKeyDown={(event) => {
                                                if (event.key === 'Enter' || event.key === ' ') {
                                                    event.preventDefault();
                                                    event.stopPropagation();
                                                    if (onDetails) onDetails(file);
                                                    else onPreview(file, orderedFiles);
                                                }
                                            }}
                                            className="min-w-0 flex-1 cursor-pointer text-left"
                                        >
                                            <div className="flex min-w-0 items-center gap-2">
                                                <div className="tr-file-title truncate text-sm font-semibold text-telegram-text" title={file.stack_name || displayFileName(file.name)}>
                                                    {title}
                                                </div>
                                                {!!file.stack_version_count && file.stack_version_count > 1 && (
                                                    <button
                                                        type="button"
                                                        onClick={(event) => {
                                                            event.stopPropagation();
                                                            onManageVersions?.(file);
                                                        }}
                                                        className="tr-version-badge shrink-0 px-2 py-0.5 text-[9px] font-semibold"
                                                        title="Manage versions"
                                                    >
                                                        {file.stack_version_count} versions
                                                    </button>
                                                )}
                                            </div>
                                            <div className="tr-file-meta mt-0.5 flex items-center gap-1.5 text-[10px] text-telegram-subtext">
                                                <span className="tr-file-quality">
                                                    <RichFileMetaText file={file} folderId={activeFolderId} />
                                                </span>
                                                <span className="tr-file-meta-dot">•</span>
                                                <span>{formatBytes(file.size)}</span>
                                            </div>
                                        </div>

                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                onDownload(file.id, file.name);
                                            }}
                                            className="tr-file-action tr-file-action--primary ml-auto grid h-7 w-7 shrink-0 place-items-center"
                                            title="Download"
                                            aria-label={`Download ${file.name}`}
                                        >
                                            <Download className="h-3.5 w-3.5" />
                                        </button>

                                        <button
                                            type="button"
                                            onClick={(event) => {
                                                event.stopPropagation();
                                                const rect = event.currentTarget.getBoundingClientRect();
                                                setContextMenu({
                                                    x: Math.min(rect.right, window.innerWidth - 8),
                                                    y: rect.bottom + 4,
                                                    file,
                                                });
                                            }}
                                            className="tr-file-action grid h-7 w-7 shrink-0 place-items-center"
                                            title="More actions"
                                            aria-label={`More actions for ${file.stack_name || file.name}`}
                                        >
                                            <MoreVertical className="h-3.5 w-3.5" />
                                        </button>


                                    </article>
                                </div>
                            );
                        })
                    )}
                </div>
            </div>

            {contextMenu && (
                <ContextMenu
                    x={contextMenu.x}
                    y={contextMenu.y}
                    file={contextMenu.file}
                    onClose={() => setContextMenu(null)}
                    onDownload={() => {
                        onDownload(contextMenu.file.id, contextMenu.file.name);
                        setContextMenu(null);
                    }}
                    onDelete={!readOnly ? () => {
                        onDelete(contextMenu.file.id);
                        setContextMenu(null);
                    } : undefined}
                    onPreview={() => {
                        onPreview(contextMenu.file, orderedFiles);
                        setContextMenu(null);
                    }}
                    onDetails={onDetails ? () => {
                        onDetails(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    onShare={onShare && !readOnly ? () => {
                        onShare(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    onRename={!readOnly ? () => {
                        onRename(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    onMove={!readOnly ? () => {
                        onFileMove(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    onAddVersion={!readOnly && contextMenu.file.logical_file_id && onAddVersion ? () => {
                        onAddVersion(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    onManageVersions={contextMenu.file.stack_id && onManageVersions ? () => {
                        onManageVersions(contextMenu.file);
                        setContextMenu(null);
                    } : undefined}
                    folders={folders}
                    activeFolderId={activeFolderId}
                />
            )}

            {!readOnly && (
                <div className="tr-channel-composer mx-3 mb-3 px-3 py-2">
                    <div className="mx-auto flex max-w-[820px] items-center justify-between gap-3">
                        <div className="text-xs text-telegram-subtext">
                            Drag files here or add them from your device.
                        </div>
                        <div className="flex items-center gap-2">
                            {showFolderUpload && (
                                <PremiumButton
                                    variant="secondary"
                                    size="sm"
                                    icon={<FolderInput />}
                                    onClick={onFolderUpload}
                                >
                                    Add folder
                                </PremiumButton>
                            )}
                            <PremiumButton
                                variant="primary"
                                size="sm"
                                icon={<Paperclip />}
                                onClick={onManualUpload}
                            >
                                Add files
                            </PremiumButton>
                        </div>
                    </div>
                </div>
            )}
        </section>
    );
}
