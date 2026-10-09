import { motion } from 'framer-motion';
import { useState } from 'react';
import { Check, Download, Eye, Folder, Link, Trash2 } from 'lucide-react';
import { TelegramFile } from '../../../types';
import { createDragGhost, displayFileName } from '../../../utils';
import { getPremiumFileMeta, getPremiumFileTitle } from '../../../filePresentation';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';
import { useVideoMetadata } from '../../../hooks/useVideoMetadata';
import { useCachedVariants } from '../../../hooks/useCachedVariants';
import { VideoMetaBadge } from '../../shared/VideoMetaBadge';

interface FileCardProps {
    file: TelegramFile;
    onDelete: () => void;
    onDownload: () => void;
    onPreview?: () => void;
    onShare?: () => void;
    isSelected: boolean;
    onClick?: (e: React.MouseEvent) => void;
    onContextMenu?: (e: React.MouseEvent) => void;
    onDrop?: (e: React.DragEvent, folderId: number) => void;
    onDragStart?: (fileIds: number[]) => void;
    onDragEnd?: () => void;
    activeFolderId?: number | null;
    height?: number;
    onToggleSelection?: () => void;
    selectedIds?: number[];
}

export function FileCard({
    file,
    onDelete,
    onDownload,
    onPreview,
    onShare,
    isSelected,
    onClick,
    onContextMenu,
    onDrop,
    onDragStart,
    onDragEnd,
    activeFolderId,
    height,
    onToggleSelection,
    selectedIds,
}: FileCardProps) {
    const isFolder = file.type === 'folder';
    const [isDragOver, setIsDragOver] = useState(false);

    const { data: videoMeta, isLoading: videoMetaLoading } = useVideoMetadata(
        file.id,
        file.folder_id ?? null,
        file.name,
    );
    const { data: cachedVariants } = useCachedVariants(
        file.id,
        file.folder_id ?? null,
        file.name,
    );
    const cachedQualities = (cachedVariants || []).filter(v => v.available).map(v => v.quality);
    const premiumTitle = getPremiumFileTitle(file);
    const premiumMeta = getPremiumFileMeta(file);

    return (
        <div
            className="relative"
            draggable={!isFolder}
            onContextMenu={onContextMenu}
            onClick={onClick}
            onDragStart={!isFolder ? (event) => {
                const idsToDrag = selectedIds && selectedIds.includes(file.id) ? selectedIds : [file.id];
                onDragStart?.(idsToDrag);
                event.dataTransfer.setData('application/x-telegram-file-ids', JSON.stringify(idsToDrag));
                event.dataTransfer.effectAllowed = 'move';
                const ghost = createDragGhost(file.name, false, idsToDrag.length);
                event.dataTransfer.setDragImage(ghost, 0, 0);
                requestAnimationFrame(() => ghost.remove());
            } : undefined}
            onDragEnd={!isFolder ? onDragEnd : undefined}
            onDragOver={(event) => {
                if (!isFolder) return;
                event.preventDefault();
                event.stopPropagation();
                if (!isDragOver) setIsDragOver(true);
            }}
            onDragLeave={(event) => {
                if (!isFolder) return;
                event.preventDefault();
                event.stopPropagation();
                setIsDragOver(false);
            }}
            onDrop={(event) => {
                if (!isFolder || !onDrop) return;
                event.preventDefault();
                event.stopPropagation();
                setIsDragOver(false);
                onDrop(event, file.id);
            }}
        >
            <motion.div
                whileHover={{ y: -2 }}
                className={[
                    'tr-library-card group relative cursor-pointer overflow-hidden',
                    isSelected ? 'tr-library-card--selected' : '',
                    isDragOver ? 'tr-library-card--drop' : '',
                ].join(' ')}
                style={height ? { height: `${height}px` } : undefined}
            >
                <div className="tr-library-card__media">
                    {isFolder ? (
                        <div className="tr-library-folder-cover">
                            <span className="tr-library-folder-cover__glow" />
                            <Folder className="h-10 w-10" />
                            <span>Folder</span>
                        </div>
                    ) : (
                        <PremiumFileThumbnail
                            file={file}
                            folderId={activeFolderId ?? file.folder_id ?? null}
                            variant="grid"
                            onOpen={onPreview}
                        />
                    )}

                    <button
                        type="button"
                        className={[
                            'tr-library-select',
                            isSelected ? 'tr-library-select--active' : '',
                        ].join(' ')}
                        onClick={(event) => {
                            event.stopPropagation();
                            onToggleSelection?.();
                        }}
                        aria-label={isSelected ? 'Deselect file' : 'Select file'}
                    >
                        {isSelected && <Check className="h-3 w-3" />}
                    </button>

                    {!isFolder && (
                        <div className="tr-library-card__actions">
                            {onPreview && (
                                <button
                                    type="button"
                                    className="tr-library-card__action"
                                    onClick={(event) => {
                                        event.stopPropagation();
                                        onPreview();
                                    }}
                                    title="Preview"
                                    aria-label="Preview"
                                >
                                    <Eye />
                                </button>
                            )}
                            <button
                                type="button"
                                className="tr-library-card__action"
                                onClick={(event) => {
                                    event.stopPropagation();
                                    onDownload();
                                }}
                                title="Download"
                                aria-label="Download"
                            >
                                <Download />
                            </button>
                            {onShare && (
                                <button
                                    type="button"
                                    className="tr-library-card__action"
                                    onClick={(event) => {
                                        event.stopPropagation();
                                        onShare();
                                    }}
                                    title="Share"
                                    aria-label="Share"
                                >
                                    <Link />
                                </button>
                            )}
                            <button
                                type="button"
                                className="tr-library-card__action tr-library-card__action--danger"
                                onClick={(event) => {
                                    event.stopPropagation();
                                    onDelete();
                                }}
                                title="Delete"
                                aria-label="Delete"
                            >
                                <Trash2 />
                            </button>
                        </div>
                    )}
                </div>

                <div className="tr-library-card__footer">
                    <div className="flex min-w-0 items-center gap-1.5">
                        <h3
                            className="min-w-0 flex-1 truncate text-[12px] font-semibold text-telegram-text"
                            title={file.stack_name || displayFileName(file.name)}
                        >
                            {premiumTitle}
                        </h3>

                    </div>

                    <div className="mt-1 flex min-w-0 items-center gap-1.5 text-[10px] text-telegram-subtext">
                        <span className="tr-file-quality truncate">{premiumMeta}</span>
                        <span className="tr-file-meta-dot">•</span>
                        <span className="shrink-0">{file.sizeStr}</span>
                    </div>

                    <div className="mt-1 flex min-h-[16px] items-center gap-1.5 overflow-hidden">
                        <VideoMetaBadge metadata={videoMeta} isLoading={videoMetaLoading} />
                        {cachedQualities.map(quality => (
                            <span key={quality} className="tr-library-quality">
                                <Check className="h-2.5 w-2.5" />
                                {quality}
                            </span>
                        ))}
                    </div>
                </div>
            </motion.div>
        </div>
    );
}
