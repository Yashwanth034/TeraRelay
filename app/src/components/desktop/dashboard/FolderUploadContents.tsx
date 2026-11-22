import { useState } from 'react';
import { Maximize2, Minimize2 } from 'lucide-react';
import type { QueueItem } from '../../../types';
import { FolderUploadEpisodeRow } from './FolderUploadEpisodeRow';

const SCROLL_THRESHOLD = 3;

interface FolderUploadContentsProps {
    detailsId: string;
    groupName?: string;
    items: QueueItem[];
    totalFiles: number;
    onPause: (id: string) => void;
    onResume: (id: string) => void;
    onCancel: (id: string) => void;
    onRetry: (id: string) => void;
}

export function FolderUploadContents({
    detailsId,
    groupName,
    items,
    totalFiles,
    onPause,
    onResume,
    onCancel,
    onRetry,
}: FolderUploadContentsProps) {
    // Unset means "use the default": constrain long lists so the dock stays
    // usable, but always let the user reveal every tracked file.
    const [scrollOverride, setScrollOverride] = useState<boolean | null>(null);
    const canScroll = items.length > SCROLL_THRESHOLD;
    const scrollActive = canScroll && (scrollOverride ?? true);
    const listId = `${detailsId}-list`;
    const contentsLabel = groupName ? `Folder contents for ${groupName}` : 'Folder contents';
    const filesLabel = groupName ? `Files in folder upload ${groupName}` : 'Files in folder upload';

    return (
        <div id={detailsId} className="border-t border-telegram-border/80 bg-telegram-bg/40 p-3">
            <div className="mb-2 flex items-center justify-between gap-2">
                <span className="text-[9px] font-bold uppercase tracking-[0.16em] text-telegram-subtext">
                    Folder contents
                </span>
                <div className="flex min-w-0 items-center gap-2">
                    <span className="text-[10px] tabular-nums text-telegram-subtext">
                        {items.length} / {totalFiles} tracked
                    </span>
                    {canScroll && (
                        <button
                            type="button"
                            className="inline-flex h-6 shrink-0 items-center gap-1 rounded-md border border-telegram-border/80 bg-telegram-surface/70 px-1.5 text-[9px] font-semibold text-telegram-subtext transition-colors hover:bg-telegram-surface hover:text-telegram-text"
                            aria-controls={listId}
                            aria-expanded={!scrollActive}
                            title={scrollActive
                                ? 'Show all folder contents'
                                : 'Limit folder contents to a scrollable list'}
                            onClick={() => setScrollOverride(!scrollActive)}
                        >
                            {scrollActive
                                ? <Maximize2 className="h-3 w-3" aria-hidden="true" />
                                : <Minimize2 className="h-3 w-3" aria-hidden="true" />}
                            {scrollActive ? 'Show all' : 'Scroll'}
                        </button>
                    )}
                </div>
            </div>
            <div
                id={listId}
                className={scrollActive ? 'tr-folder-contents-scroll' : undefined}
                role="group"
                aria-label={contentsLabel}
                tabIndex={scrollActive ? 0 : undefined}
            >
                <div className="space-y-2" role="list" aria-label={filesLabel}>
                    {items.map((item, index) => (
                        <FolderUploadEpisodeRow
                            key={item.id}
                            index={index}
                            item={item}
                            onPause={onPause}
                            onResume={onResume}
                            onCancel={onCancel}
                            onRetry={onRetry}
                        />
                    ))}
                </div>
            </div>
        </div>
    );
}
