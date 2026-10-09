import { useState } from 'react';
import { Check, Folder, MoreVertical } from 'lucide-react';
import { TelegramFile } from '../../../types';
import { createDragGhost, displayFileName } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';
import { RichFileMetaText } from '../../shared/RichFileMetaText';

interface FileListItemProps {
    file: TelegramFile;
    selectedIds: number[];
    onFileClick: (e: React.MouseEvent, id: number) => void;
    handleContextMenu: (e: React.MouseEvent, file: TelegramFile) => void;
    onDragStart?: (fileIds: number[]) => void;
    onDragEnd?: () => void;
    onDrop?: (e: React.DragEvent, folderId: number) => void;
}

export function FileListItem({
    file,
    selectedIds,
    onFileClick,
    handleContextMenu,
    onDragStart,
    onDragEnd,
    onDrop,
}: FileListItemProps) {
    const [isDragOver, setIsDragOver] = useState(false);
    const isFolder = file.type === 'folder';
    const isSelected = selectedIds.includes(file.id);
    const title = getPremiumFileTitle(file);

    return (
        <div
            onClick={(event) => onFileClick(event, file.id)}
            onContextMenu={(event) => handleContextMenu(event, file)}
            draggable
            onDragStart={(event) => {
                const idsToDrag = isSelected ? selectedIds : [file.id];
                onDragStart?.(idsToDrag);
                event.dataTransfer.setData('application/x-telegram-file-ids', JSON.stringify(idsToDrag));
                event.dataTransfer.effectAllowed = 'move';
                const ghost = createDragGhost(file.name, isFolder, idsToDrag.length);
                event.dataTransfer.setDragImage(ghost, 0, 0);
                requestAnimationFrame(() => ghost.remove());
            }}
            onDragEnd={() => onDragEnd?.()}
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
            className={[
                'tr-library-list-row group grid grid-cols-[2rem_minmax(0,1fr)_5rem_2rem] gap-2.5 items-center px-2.5 py-1.5 cursor-pointer',
                isSelected ? 'tr-library-list-row--selected' : '',
                isDragOver ? 'tr-library-list-row--drop' : '',
            ].join(' ')}
        >
            <div className="relative flex justify-center">
                {isFolder ? (
                    <span className="tr-library-list-folder">
                        <Folder className="h-4 w-4" />
                    </span>
                ) : (
                    <PremiumFileThumbnail
                        file={file}
                        folderId={file.folder_id ?? null}
                        className="tr-library-list-thumb"
                    />
                )}
                {isSelected && (
                    <span className="tr-library-list-check" aria-hidden="true">
                        <Check className="h-2.5 w-2.5" />
                    </span>
                )}
            </div>

            <div className="min-w-0">
                <div
                    className="truncate text-[12px] font-semibold text-telegram-text"
                    title={file.stack_name || displayFileName(file.name)}
                >
                    {title}
                </div>
                <div className="mt-0.5 flex min-w-0 items-center gap-1.5 text-[9px] text-telegram-subtext">
                    <span className="tr-file-quality truncate">
                        <RichFileMetaText file={file} folderId={file.folder_id ?? null} />
                    </span>

                </div>
            </div>

            <div className="truncate text-right text-[10px] font-medium text-telegram-subtext">
                {file.sizeStr}
            </div>

            <button
                type="button"
                onClick={(event) => {
                    event.stopPropagation();
                    handleContextMenu(event, file);
                }}
                className="tr-library-list-more"
                aria-label="File actions"
            >
                <MoreVertical className="h-3.5 w-3.5" />
            </button>
        </div>
    );
}
