import { useEffect, useRef, useState } from 'react';
import { X, HardDrive, Folder } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { TelegramFolder } from '../../../types';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface MoveToFolderModalProps {
    folders: TelegramFolder[];
    onClose: () => void;
    onSelect: (id: number | null) => Promise<void> | void;
    activeFolderId: number | null;
    fileName?: string;
    allowPersonalVault?: boolean;
    writableOnly?: boolean;
}

export function MoveToFolderModal({ folders, onClose, onSelect, activeFolderId, fileName, allowPersonalVault = true, writableOnly = false }: MoveToFolderModalProps) {
    const { t } = useTranslation();
    const dialogRef = useRef<HTMLDivElement>(null);
    const closeButtonRef = useRef<HTMLButtonElement>(null);
    const selectingRef = useRef(false);
    const [selecting, setSelecting] = useState(false);
    const closeIfIdle = () => {
        // State updates render later; the ref also closes the same-tick race.
        if (!selectingRef.current) onClose();
    };
    useEscapeToClose(true, closeIfIdle, selecting);

    useEffect(() => {
        const active = document.activeElement;
        const previousFocus = active instanceof HTMLElement && active !== document.body ? active : null;
        const frame = requestAnimationFrame(() => closeButtonRef.current?.focus({ preventScroll: true }));
        return () => {
            cancelAnimationFrame(frame);
            requestAnimationFrame(() => {
                if (previousFocus?.isConnected) previousFocus.focus({ preventScroll: true });
            });
        };
    }, []);

    const selectOnce = async (id: number | null) => {
        // Use a ref so two clicks in the same render cannot submit twice.
        if (selectingRef.current) return;
        selectingRef.current = true;
        setSelecting(true);
        try {
            await onSelect(id);
        } finally {
            selectingRef.current = false;
            setSelecting(false);
        }
    };

    const trapMoveDialogTab = (event: React.KeyboardEvent<HTMLDivElement>) => {
        if (event.key !== 'Tab') return;
        const dialog = dialogRef.current;
        if (!dialog) return;
        const focusable = Array.from(dialog.querySelectorAll<HTMLElement>(
            'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
        )).filter(element => element.getClientRects().length > 0);
        if (focusable.length === 0) {
            event.preventDefault();
            dialog.focus();
            return;
        }
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        const active = document.activeElement;
        if (event.shiftKey && (active === first || !dialog.contains(active))) {
            event.preventDefault();
            last.focus();
        } else if (!event.shiftKey && (active === last || !dialog.contains(active))) {
            event.preventDefault();
            first.focus();
        }
    };

    return (
        <div className="tr-modal-backdrop fixed inset-0 z-[100] flex items-center justify-center p-4" onClick={closeIfIdle}>
            <div ref={dialogRef} tabIndex={-1} onKeyDown={trapMoveDialogTab} className="tr-modal w-full max-w-[360px] overflow-hidden flex flex-col max-h-[80vh]" role="dialog" aria-modal="true" aria-labelledby="tr-move-folder-title" onClick={e => e.stopPropagation()}>
                <div className="tr-modal-header p-4 flex justify-between items-center">
                    <h3 id="tr-move-folder-title" className="text-telegram-text font-medium truncate max-w-[220px]">
                        {fileName ? t('files.move_file_to_folder', { name: fileName }) : t('files.move_to_folder')}
                    </h3>
                    <button ref={closeButtonRef} type="button" onClick={closeIfIdle} disabled={selecting} className="tr-modal-close" aria-label={t('common.close')}><X className="w-4 h-4" /></button>
                </div>
                <div className="flex-1 overflow-y-auto p-2 space-y-1">
                    {allowPersonalVault && activeFolderId !== null && (
                        <button
                            type="button"
                            disabled={selecting}
                            onClick={() => { void selectOnce(null); }}
                            className="tr-modal-option tr-modal-option--primary w-full flex items-center gap-3 px-3 py-3 text-sm text-left text-telegram-text"
                        >
                            <div className="tr-modal-icon w-9 h-9 flex items-center justify-center">
                                <HardDrive className="w-4 h-4" />
                            </div>
                            <span className="font-medium">{t('common.personal_vault')}</span>
                        </button>
                    )}

                    {folders.map((f: any) => {
                        if (f.id === activeFolderId || (writableOnly && f.role === 'member')) return null;
                        return (
                            <button
                                type="button"
                                key={f.id}
                                disabled={selecting}
                                onClick={() => { void selectOnce(f.id); }}
                                className="tr-modal-option w-full flex items-center gap-3 px-3 py-3 text-sm text-left text-telegram-text"
                            >
                                <div className="tr-modal-icon w-9 h-9 flex items-center justify-center">
                                    <Folder className="w-4 h-4" />
                                </div>
                                <span className="font-medium">{f.name}</span>
                            </button>
                        )
                    })}

                    {folders.length === 0 && activeFolderId === null && (
                        <div className="p-4 text-center text-xs text-telegram-subtext">{t('files.no_other_folders')}</div>
                    )}
                </div>
            </div>
        </div>
    )
}
