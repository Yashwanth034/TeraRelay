import { useState, useRef, useEffect } from 'react';
import { Pencil, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';

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
    const { t } = useTranslation();

    useEffect(() => {
        inputRef.current?.focus();
        inputRef.current?.select();
    }, []);

    const handleSubmit = async () => {
        if (isSubmitting) return;
        const trimmed = name.trim();
        if (!trimmed || trimmed === currentName) {
            onClose();
            return;
        }
        setIsSubmitting(true);
        try {
            await onRename(folderId, currentName, trimmed);
            onClose();
        } catch {
            // error handled by parent
            setIsSubmitting(false);
        }
    };

    const handleKeyDown = (e: React.KeyboardEvent) => {
        if (e.key === 'Enter') {
            e.preventDefault();
            handleSubmit();
        } else if (e.key === 'Escape') {
            onClose();
        }
    };

    return (
        <div
            className="tr-modal-backdrop fixed inset-0 z-[250] flex items-center justify-center p-4"
            onClick={onClose}
        >
            <div
                className="tr-modal w-full max-w-[380px] overflow-hidden animate-in fade-in zoom-in-95 duration-150"
                onClick={e => e.stopPropagation()}
            >
                {/* Header */}
                <div className="tr-modal-header p-4 flex items-center justify-between">
                    <h3 className="text-telegram-text font-medium flex items-center gap-2">
                        <Pencil className="w-4 h-4 text-blue-400" />
                        {t('files.rename_folder')}
                    </h3>
                    <button
                        onClick={onClose}
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
                        onClick={onClose}
                        className="tr-button tr-button--secondary tr-button--sm"
                        disabled={isSubmitting}
                    >
                        {t('common.cancel')}
                    </button>
                    <button
                        onClick={handleSubmit}
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
