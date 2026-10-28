import { useState, useRef, useEffect } from 'react';
import { Pencil, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { containDialogTab } from '../../../dialogKeyboard';

interface RenameFolderModalProps {
    folderId: number;
    currentName: string;
    onRename: (folderId: number, oldName: string, newName: string) => Promise<void>;
    onClose: () => void;
}

export function RenameFolderModal({ folderId, currentName, onRename, onClose }: RenameFolderModalProps) {
    const [name, setName] = useState(currentName);
    const [isSubmitting, setIsSubmitting] = useState(false);
    const inputRef = useRef<HTMLInputElement>(null);
    const dialogRef = useRef<HTMLDivElement>(null);
    const submittingRef = useRef(false);
    const { t } = useTranslation();

    const closeIfIdle = () => {
        if (!submittingRef.current) onClose();
    };

    useEffect(() => {
        const active = document.activeElement;
        const previousFocus = active instanceof HTMLElement && active !== document.body ? active : null;
        inputRef.current?.focus();
        inputRef.current?.select();
        return () => {
            requestAnimationFrame(() => {
                if (previousFocus?.isConnected) previousFocus.focus({ preventScroll: true });
            });
        };
    }, []);

    const handleSubmit = async () => {
        if (submittingRef.current) return;
        const trimmed = name.trim();
        if (!trimmed || trimmed === currentName) {
            closeIfIdle();
            return;
        }
        // Ref is immediate; React's isSubmitting update can render later.
        submittingRef.current = true;
        setIsSubmitting(true);
        try {
            await onRename(folderId, currentName, trimmed);
            onClose();
        } catch {
            // The parent owns the error notification; leave the dialog open.
        } finally {
            submittingRef.current = false;
            setIsSubmitting(false);
        }
    };

    const handleDialogKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
        if (event.key === 'Escape') {
            event.preventDefault();
            event.stopPropagation();
            closeIfIdle();
            return;
        }
        containDialogTab(event, dialogRef.current, document.activeElement);
    };

    const handleKeyDown = (e: React.KeyboardEvent) => {
        if (e.key === 'Enter') {
            e.preventDefault();
            void handleSubmit();
        }
    };

    return (
        <div
            className="tr-modal-backdrop fixed inset-0 z-[250] flex items-center justify-center p-4"
            onClick={closeIfIdle}
        >
            <div
                ref={dialogRef}
                tabIndex={-1}
                onKeyDown={handleDialogKeyDown}
                className="tr-modal w-full max-w-[380px] overflow-hidden animate-in fade-in zoom-in-95 duration-150"
                role="dialog"
                aria-modal="true"
                aria-busy={isSubmitting}
                aria-labelledby="tr-rename-folder-title"
                onClick={e => e.stopPropagation()}
            >
                {/* Header */}
                <div className="tr-modal-header p-4 flex items-center justify-between">
                    <h3 id="tr-rename-folder-title" className="text-telegram-text font-medium flex items-center gap-2">
                        <Pencil className="w-4 h-4 text-blue-400" />
                        {t('files.rename_folder')}
                    </h3>
                    <button
                        type="button"
                        onClick={closeIfIdle}
                        aria-label={t('common.close')}
                        className="tr-modal-close"
                        disabled={isSubmitting}
                    >
                        <X className="w-4 h-4" />
                    </button>
                </div>

                {/* Body */}
                <div className="p-4 space-y-3">
                    <div className="text-sm text-telegram-subtext">
                        {t('files.enter_new_name', { name: currentName })}
                    </div>
                    <input
                        ref={inputRef}
                        type="text"
                        aria-label={t('files.folder_name')}
                        value={name}
                        onChange={e => setName(e.target.value)}
                        onKeyDown={handleKeyDown}
                        maxLength={100}
                        className="tr-modal-input w-full px-3 py-2 text-sm placeholder:text-telegram-subtext/50"
                        placeholder={t('files.folder_name')}
                        disabled={isSubmitting}
                    />
                </div>

                {/* Footer */}
                <div className="tr-modal-footer">
                    <button
                        type="button"
                        onClick={closeIfIdle}
                        className="tr-button tr-button--secondary tr-button--sm"
                        disabled={isSubmitting}
                    >
                        {t('common.cancel')}
                    </button>
                    <button
                        type="button"
                        onClick={() => { void handleSubmit(); }}
                        disabled={isSubmitting || !name.trim() || name.trim() === currentName}
                        className="tr-button tr-button--primary tr-button--sm disabled:opacity-40 disabled:cursor-not-allowed"
                    >
                        {isSubmitting ? t('files.renaming') : t('files.rename')}
                    </button>
                </div>
            </div>
        </div>
    );
}
