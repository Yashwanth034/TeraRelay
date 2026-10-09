import { useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import {
    ChevronDown,
    ChevronRight,
    Download,
    MoreVertical,
} from 'lucide-react';
import type { FileStackView, TelegramFile } from '../../../types';
import { displayFileName, formatBytes } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';
import { RichFileMetaText } from '../../shared/RichFileMetaText';

interface StackedFileRowProps {
    file: TelegramFile;
    folderId: number;
    expanded: boolean;
    onExpandedChange: (expanded: boolean) => void;
    onPreview: (file: TelegramFile, orderedFiles?: TelegramFile[]) => void;
    onDetails?: (file: TelegramFile) => void;
    onDownload: (id: number, name: string) => void;
    onDownloadAll: (files: TelegramFile[]) => Promise<void> | void;
    onMore: (event: React.MouseEvent<HTMLButtonElement>, file: TelegramFile) => void;
}

function normalizeStack(result: FileStackView): FileStackView {
    return {
        ...result,
        members: result.members
            .map(member => ({
                ...member,
                file: {
                    ...member.file,
                    sizeStr: member.file.sizeStr || formatBytes(member.file.size),
                    type: member.file.type || 'file',
                },
            }))
            .sort((a, b) => {
                if (a.is_primary !== b.is_primary) return a.is_primary ? -1 : 1;
                return a.added_at - b.added_at;
            }),
    };
}

export function StackedFileRow({
    file,
    folderId,
    expanded,
    onExpandedChange,
    onPreview,
    onDetails,
    onDownload,
    onDownloadAll,
    onMore,
}: StackedFileRowProps) {
    const [stack, setStack] = useState<FileStackView | null>(null);
    const [loading, setLoading] = useState(false);
    const [failed, setFailed] = useState(false);

    const title = getPremiumFileTitle(file);

    const load = async (): Promise<FileStackView | null> => {
        if (!file.stack_id) return null;
        if (stack) return stack;
        if (loading) return null;

        setLoading(true);
        setFailed(false);
        try {
            const result = normalizeStack(await invoke<FileStackView>('cmd_get_file_stack', {
                stackId: file.stack_id,
                folderId,
            }));
            setStack(result);
            return result;
        } catch {
            setFailed(true);
            return null;
        } finally {
            setLoading(false);
        }
    };

    const toggle = async (event: React.MouseEvent) => {
        event.stopPropagation();
        const next = !expanded;
        onExpandedChange(next);
        if (next && !stack) await load();
    };

    const downloadGroup = async (event: React.MouseEvent) => {
        event.stopPropagation();
        const current = stack ?? await load();
        if (current) {
            await onDownloadAll(current.members.map(member => member.file));
        }
    };

    return (
        <div
            className={[
                'tr-version-group',
                expanded ? 'tr-version-group--expanded' : '',
            ].join(' ')}
        >
            <article className="tr-version-group__parent group relative flex items-center gap-2 px-2.5 select-none">
                <button
                    type="button"
                    className="tr-version-group__toggle"
                    onClick={toggle}
                    aria-expanded={expanded}
                    aria-label={expanded ? 'Collapse grouped files' : 'Expand grouped files'}
                    title={expanded ? 'Collapse' : 'Expand'}
                >
                    {expanded ? <ChevronDown /> : <ChevronRight />}
                </button>

                <button
                    type="button"
                    onClick={toggle}
                    className="tr-version-group__title min-w-0 flex-1 truncate text-left"
                    title={title}
                >
                    {title}
                </button>

                <button
                    type="button"
                    onClick={downloadGroup}
                    disabled={loading}
                    className="tr-file-action tr-file-action--primary ml-auto grid h-7 w-7 shrink-0 place-items-center"
                    title="Download all"
                    aria-label={`Download all files in ${title}`}
                >
                    <Download className="h-3.5 w-3.5" />
                </button>

                <button
                    type="button"
                    onClick={(event) => onMore(event, file)}
                    className="tr-file-action grid h-7 w-7 shrink-0 place-items-center"
                    title="More actions"
                    aria-label={`More actions for ${title}`}
                >
                    <MoreVertical className="h-3.5 w-3.5" />
                </button>
            </article>

            {expanded && (
                <div className="tr-version-group__body">
                    {loading && (
                        <div className="tr-version-group__loading" aria-label="Loading grouped files">
                            <span />
                            <span />
                        </div>
                    )}

                    {!loading && failed && (
                        <div className="tr-version-group__error">
                            <span>Couldn’t load grouped files.</span>
                            <button type="button" onClick={() => void load()}>Retry</button>
                        </div>
                    )}

                    {!loading && stack && stack.members.map(member => {
                        const memberFile = member.file;
                        const memberLabel = member.label?.trim();

                        return (
                            <div
                                key={memberFile.logical_file_id || memberFile.id}
                                className={[
                                    'tr-version-group__member',
                                    member.is_primary ? 'tr-version-group__member--primary' : '',
                                ].join(' ')}
                                onDoubleClick={() => onPreview(memberFile, stack.members.map(item => item.file))}
                            >
                                <PremiumFileThumbnail
                                    file={memberFile}
                                    folderId={folderId}
                                    onOpen={() => onPreview(memberFile, stack.members.map(item => item.file))}
                                />

                                <button
                                    type="button"
                                    className="tr-version-group__member-copy"
                                    onClick={() => {
                                        if (onDetails) {
                                            onDetails({
                                                ...memberFile,
                                                stack_id: undefined,
                                                stack_name: undefined,
                                                stack_version_count: 0,
                                            });
                                        } else {
                                            onPreview(memberFile, stack.members.map(item => item.file));
                                        }
                                    }}
                                >
                                    <span
                                        className="tr-version-group__member-title"
                                        title={displayFileName(memberFile.name)}
                                    >
                                        {displayFileName(memberFile.name)}
                                    </span>
                                    <span className="tr-version-group__member-meta">
                                        <span>
                                            {memberLabel || (
                                                <RichFileMetaText file={memberFile} folderId={folderId} />
                                            )}
                                        </span>
                                        <span className="tr-file-meta-dot">•</span>
                                        <span>{formatBytes(memberFile.size)}</span>
                                    </span>
                                </button>

                                <button
                                    type="button"
                                    onClick={(event) => {
                                        event.stopPropagation();
                                        onDownload(memberFile.id, memberFile.name);
                                    }}
                                    className="tr-version-group__member-action"
                                    title="Download"
                                    aria-label={`Download ${memberFile.name}`}
                                >
                                    <Download />
                                </button>

                                <button
                                    type="button"
                                    onClick={(event) => onMore(event, memberFile)}
                                    className="tr-version-group__member-action"
                                    title="More actions"
                                    aria-label={`More actions for ${memberFile.name}`}
                                >
                                    <MoreVertical />
                                </button>
                            </div>
                        );
                    })}
                </div>
            )}
        </div>
    );
}
