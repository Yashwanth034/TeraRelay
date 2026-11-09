import { useState } from 'react';
import {
    Check,
    ChevronDown,
    ChevronRight,
    Download,
    Folder,
    FolderOpen,
    MoreVertical,
    Trash2,
} from 'lucide-react';
import type { TelegramFile } from '../../../types';
import { displayFileName, formatBytes } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';
import { RichFileMetaText } from '../../shared/RichFileMetaText';
import { StackedFileRow } from './StackedFileRow';
import type { ChannelFolderNode } from '../../../channelFolderTree';

interface ChannelFolderGroupsProps {
    folders: ChannelFolderNode[];
    allFiles: TelegramFile[];
    activeFolderId: number;
    selectedIds: number[];
    onFileClick: (event: React.MouseEvent, id: number) => void;
    onToggleSelection: (id: number) => void;
    onDownload: (id: number, name: string) => void;
    onDownloadAll: (files: TelegramFile[]) => Promise<void> | void;
    onPreview: (file: TelegramFile, orderedFiles?: TelegramFile[]) => void;
    onDetails?: (file: TelegramFile) => void;
    onDeleteFolder?: (folder: ChannelFolderNode) => void;
    expandedFolders?: Set<string>;
    onExpandedFoldersChange?: (folders: Set<string>) => void;
    readOnly?: boolean;
    onMore: (event: React.MouseEvent<HTMLButtonElement>, file: TelegramFile) => void;
}

export function ChannelFolderGroups({
    folders,
    allFiles,
    activeFolderId,
    selectedIds,
    onFileClick,
    onToggleSelection,
    onDownload,
    onDownloadAll,
    onPreview,
    onDetails,
    onDeleteFolder,
    expandedFolders: controlledExpandedFolders,
    onExpandedFoldersChange,
    readOnly = false,
    onMore,
}: ChannelFolderGroupsProps) {
    const [localExpandedFolders, setLocalExpandedFolders] = useState<Set<string>>(() => new Set());
    const expandedFolders = controlledExpandedFolders ?? localExpandedFolders;
    const [collapsedStackIds, setCollapsedStackIds] = useState<Set<string>>(() => new Set());
    const updateExpandedFolders = (update: (current: Set<string>) => Set<string>) => {
        const next = update(expandedFolders);
        if (onExpandedFoldersChange) onExpandedFoldersChange(next);
        else setLocalExpandedFolders(next);
    };

    const selectionMode = selectedIds.length > 0;
    const selectedSet = new Set(selectedIds);

    const renderFile = (file: TelegramFile) => {
        const selected = selectedSet.has(file.id);
        if (file.stack_id && (file.stack_version_count ?? 0) > 1) {
            return (
                <div key={file.stack_id} className="py-0.5">
                    <StackedFileRow
                        file={file}
                        folderId={activeFolderId}
                        selected={selected}
                        selectionMode={selectionMode}
                        onToggleSelection={() => onToggleSelection(file.id)}
                        expanded={!collapsedStackIds.has(file.stack_id)}
                        onExpandedChange={(expanded) => {
                            setCollapsedStackIds(current => {
                                const next = new Set(current);
                                if (expanded) next.delete(file.stack_id!);
                                else next.add(file.stack_id!);
                                return next;
                            });
                        }}
                        onPreview={onPreview}
                        onDetails={onDetails}
                        onDownload={onDownload}
                        onDownloadAll={onDownloadAll}
                        onMore={onMore}
                    />
                </div>
            );
        }

        const title = getPremiumFileTitle(file);
        return (
            <article
                key={file.logical_file_id || file.id}
                onClick={(event) => onFileClick(event, file.id)}
                onDoubleClick={() => onPreview(file, allFiles)}
                className={[
                    'tr-file-row tr-file-card group relative flex items-center gap-2.5 px-2.5 py-1.5 select-none',
                    selected ? 'tr-file-row--selected' : '',
                ].join(' ')}
            >
                {selectionMode && (
                    <button
                        type="button"
                        onClick={(event) => {
                            event.stopPropagation();
                            onToggleSelection(file.id);
                        }}
                        className={['tr-file-select', selected ? 'tr-file-select--active' : ''].join(' ')}
                        title={selected ? 'Deselect file' : 'Select file'}
                        aria-label={selected ? `Deselect ${displayFileName(file.name)}` : `Select ${displayFileName(file.name)}`}
                    >
                        {selected && <Check className="h-3.5 w-3.5" />}
                    </button>
                )}

                <PremiumFileThumbnail
                    file={file}
                    folderId={activeFolderId}
                    onOpen={() => onPreview(file, allFiles)}
                />

                <button
                    type="button"
                    onClick={(event) => {
                        event.stopPropagation();
                        if (onDetails) onDetails(file);
                        else onPreview(file, allFiles);
                    }}
                    className="min-w-0 flex-1 cursor-pointer text-left"
                >
                    <span className="tr-file-title block truncate text-sm font-semibold text-telegram-text"
                        title={file.stack_name || displayFileName(file.name)}>
                        {title}
                    </span>
                    <span className="tr-file-meta mt-0.5 flex items-center gap-1.5 text-[10px] text-telegram-subtext">
                        <span className="tr-file-quality"><RichFileMetaText file={file} folderId={activeFolderId} /></span>
                        <span className="tr-file-meta-dot">•</span>
                        <span>{formatBytes(file.size)}</span>
                    </span>
                </button>

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
                        onMore(event, file);
                    }}
                    className="tr-file-action grid h-7 w-7 shrink-0 place-items-center"
                    title="More actions"
                    aria-label={`More actions for ${file.stack_name || file.name}`}
                >
                    <MoreVertical className="h-3.5 w-3.5" />
                </button>
            </article>
        );
    };

    const renderFolder = (folder: ChannelFolderNode, depth: number): React.ReactNode => {
        const expanded = expandedFolders.has(folder.path);
        const disclosureId = `channel-drive-folder-${encodeURIComponent(folder.path)}`;
        return (
            <div key={folder.path} className="space-y-1">
                <div
                    className="tr-file-row tr-file-card flex w-full min-w-0 items-center gap-2 px-2 py-1.5"
                    style={{ marginLeft: depth * 12, width: `calc(100% - ${depth * 12}px)` }}
                >
                    <button
                        type="button"
                        className="flex min-w-0 flex-1 items-center gap-2.5 px-1.5 py-1 text-left"
                        aria-expanded={expanded}
                        aria-controls={disclosureId}
                        aria-label={`${expanded ? 'Collapse' : 'Expand'} folder ${folder.name}, ${folder.file_count} files`}
                        onClick={() => {
                            updateExpandedFolders(current => {
                                const next = new Set(current);
                                if (expanded) next.delete(folder.path);
                                else next.add(folder.path);
                                return next;
                            });
                        }}
                    >
                        {expanded
                            ? <ChevronDown className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
                            : <ChevronRight className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />}
                        {expanded
                            ? <FolderOpen className="h-5 w-5 shrink-0 text-telegram-primary" aria-hidden="true" />
                            : <Folder className="h-5 w-5 shrink-0 text-telegram-primary" aria-hidden="true" />}
                        <span className="min-w-0 flex-1 truncate text-sm font-semibold text-telegram-text" title={folder.name}>
                            {folder.name}
                        </span>
                        <span className="shrink-0 text-[11px] tabular-nums text-telegram-subtext">
                            {folder.file_count} {folder.file_count === 1 ? 'file' : 'files'} · {formatBytes(folder.total_bytes)}
                        </span>
                    </button>
                    {!readOnly && onDeleteFolder && folder.directory_id && (
                        <button
                            type="button"
                            className="tr-file-action grid h-8 w-8 shrink-0 place-items-center text-telegram-subtext hover:bg-red-500/10 hover:text-red-400"
                            title="Delete this folder and all nested files"
                            aria-label={`Delete folder ${folder.name} and ${folder.file_count} files`}
                            onClick={(event) => {
                                event.stopPropagation();
                                onDeleteFolder(folder);
                            }}
                        >
                            <Trash2 className="h-3.5 w-3.5" aria-hidden="true" />
                        </button>
                    )}
                </div>

                {expanded && (
                    <div id={disclosureId} className="space-y-1 border-l border-telegram-border/70 pl-2"
                        style={{ marginLeft: depth * 12 + 12 }}>
                        {folder.files.map(renderFile)}
                        {folder.folders.map(child => renderFolder(child, depth + 1))}
                        {folder.files.length === 0 && folder.folders.length === 0 && (
                            <p className="px-3 py-2 text-xs text-telegram-subtext">This folder is empty.</p>
                        )}
                    </div>
                )}
            </div>
        );
    };

    if (folders.length === 0) return null;
    return (
        <section className="mb-3 space-y-1" aria-label="Folders in this channel">
            {folders.map(folder => renderFolder(folder, 0))}
        </section>
    );
}
