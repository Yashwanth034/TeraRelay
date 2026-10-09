import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Eye, HardDrive, Trash2, FolderOpen, Pencil, Play, FileText, Link, Copy, ArrowRightLeft, Layers3, Plus, Info } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { TelegramFile, TelegramFolder } from '../../../types';
import { isMediaFile, isPdfFile } from '../../../utils';
import { getPremiumFileTitle } from '../../../filePresentation';
import { toast } from 'sonner';

interface ContextMenuProps {
    x: number;
    y: number;
    file: TelegramFile;
    onClose: () => void;
    onDownload: () => void;
    onDelete?: () => void;
    onPreview: () => void;
    onDetails?: () => void;
    onShare?: () => void;
    onRename?: () => void;
    onMove?: () => void;
    onAddVersion?: () => void;
    onManageVersions?: () => void;
    folders?: TelegramFolder[];
    activeFolderId?: number | null;
}

export function ContextMenu({ x, y, file, onClose, onDownload, onDelete, onPreview, onDetails, onShare, onRename, onMove, onAddVersion, onManageVersions, folders, activeFolderId }: ContextMenuProps) {
    const [adjustedPos, setAdjustedPos] = useState({ x, y });
    const menuRef = useRef<HTMLDivElement>(null);
    const { t } = useTranslation();

    // Adjust position to stay in bounds
    useLayoutEffect(() => {
        if (menuRef.current) {
            const rect = menuRef.current.getBoundingClientRect();
            let newX = x;
            let newY = y;

            if (x + rect.width > window.innerWidth) {
                newX = x - rect.width;
            }
            if (y + rect.height > window.innerHeight) {
                newY = y - rect.height;
            }
            setAdjustedPos({ x: newX, y: newY });
        }
    }, [x, y]);

    // Close on outside click — ignore clicks inside the menu so button handlers can fire
    useEffect(() => {
        const handleClick = (e: MouseEvent) => {
            if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
                onClose();
            }
        };
        const handleResize = () => onClose();
        const handleContextMenu = (e: MouseEvent) => {
            if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
                onClose();
            }
        };

        window.addEventListener('click', handleClick, true);
        window.addEventListener('resize', handleResize);
        window.addEventListener('contextmenu', handleContextMenu, true);

        return () => {
            window.removeEventListener('click', handleClick, true);
            window.removeEventListener('resize', handleResize);
            window.removeEventListener('contextmenu', handleContextMenu, true);
        };
    }, [onClose]);

    return (
        <div
            ref={menuRef}
            className="tr-popover fixed z-50 min-w-[218px] p-1.5 animate-in fade-in zoom-in-95 duration-100 flex flex-col gap-0.5"
            style={{ left: adjustedPos.x, top: adjustedPos.y }}
            onClick={(e) => e.stopPropagation()}
            onContextMenu={(e) => e.preventDefault()}
        >
            <div className="tr-popover-title px-2 py-2 truncate max-w-[200px] mb-1">
                {file.stack_id ? getPremiumFileTitle(file) : file.name}
            </div>

            {file.type !== 'folder' && (
                <button onClick={onPreview} className="tr-menu-item">
                    {isMediaFile(file.name) ? (
                        <>
                            <Play className="w-4 h-4 text-telegram-primary" />
                            {t('common.play')}
                        </>
                    ) : isPdfFile(file.name) ? (
                        <>
                            <FileText className="w-4 h-4 text-red-400" />
                            {t('files.view_pdf')}
                        </>
                    ) : (
                        <>
                            <Eye className="w-4 h-4 text-blue-500" />
                            {t('files.preview')}
                        </>
                    )}
                </button>
            )}

            {file.type === 'folder' && (
                <button onClick={onPreview} className="tr-menu-item">
                    <FolderOpen className="w-4 h-4 text-yellow-500" />
                    {t('files.open')}
                </button>
            )}

            {file.type !== 'folder' && !file.stack_id && onDetails && (
                <button onClick={onDetails} className="tr-menu-item">
                    <Info className="w-4 h-4 text-violet-400" />
                    File details
                </button>
            )}

            <button onClick={onDownload} className="tr-menu-item">
                <HardDrive className="w-4 h-4 text-green-500" />
                {t('files.download')}
            </button>

            {file.type !== 'folder' && onShare && (
                <button onClick={onShare} className="tr-menu-item">
                    <Link className="w-4 h-4 text-telegram-primary" />
                    {t('files.share_link')}
                </button>
            )}

            {file.type !== 'folder' && (
                (() => {
                    const folder = folders?.find(f => f.id === file.folder_id) || folders?.find(f => f.id === activeFolderId);
                    const username = folder?.username || (folder as any)?.chat?.username || (folder as any)?.channel?.username;
                    
                    if (username) {
                        const handleCopyLink = async () => {
                            const url = `https://t.me/${username}/${file.id}`;
                            try {
                                await navigator.clipboard.writeText(url);
                                toast.success(t('notifications.telegram_link_copied'));
                            } catch (e) {
                                toast.error(t('notifications.copy_link_failed'));
                            }
                            onClose();
                        };
                        return (
                            <button onClick={handleCopyLink} className="tr-menu-item">
                                <Copy className="w-4 h-4 text-telegram-primary" />
                                {t('files.copy_telegram_link')}
                            </button>
                        );
                    } else {
                        return (
                            <button 
                                disabled 
                                title="Only available for public channels" 
                                className="tr-menu-item"
                            >
                                <Copy className="w-4 h-4" />
                                {t('files.copy_telegram_link')}
                            </button>
                        );
                    }
                })()
            )}

            {file.type !== 'folder' && file.logical_file_id && onAddVersion && (
                <button onClick={onAddVersion} className="tr-menu-item">
                    <Plus className="w-4 h-4 text-violet-400" />
                    Add version
                </button>
            )}

            {file.type !== 'folder' && file.stack_id && onManageVersions && (
                <button onClick={onManageVersions} className="tr-menu-item">
                    <Layers3 className="w-4 h-4 text-violet-400" />
                    Manage versions
                </button>
            )}

            {file.type !== 'folder' && onMove && (
                <button onClick={onMove} className="tr-menu-item">
                    <ArrowRightLeft className="w-4 h-4 text-amber-400" />
                    {file.stack_id ? 'Move stack' : t('files.move_to_folder')}
                </button>
            )}

            {file.type !== 'folder' && onRename && (
                <button onClick={onRename} className="tr-menu-item">
                    <Pencil className="w-4 h-4 text-blue-400" />
                    {file.stack_id ? 'Rename stack' : t('files.rename')}
                </button>
            )}

            {file.type !== 'folder' && !onRename && (
                <button disabled className="tr-menu-item">
                    <Pencil className="w-4 h-4" />
                    {t('files.rename')}
                </button>
            )}

            {onDelete && (
                <>
                    <div className="tr-menu-separator" />
                    <button onClick={onDelete} className="tr-menu-item tr-menu-item--danger">
                        <Trash2 className="w-4 h-4" />
                        {file.stack_id ? 'Delete / unstack…' : t('files.delete')}
                    </button>
                </>
            )}
        </div>
    );
}
