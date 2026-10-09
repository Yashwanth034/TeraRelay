import React, { useEffect, useState } from 'react';
import { X, Globe, ChevronDown } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { TelegramFolder } from '../../../types';
import { toast } from 'sonner';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface RemoteUploadModalProps {
    isOpen: boolean;
    onClose: () => void;
    folders: TelegramFolder[];
    defaultFolderId: number | null;
    onUpload: (url: string, folderId: number | null) => void;
}

export function RemoteUploadModal({ isOpen, onClose, folders, defaultFolderId, onUpload }: RemoteUploadModalProps) {
    const [url, setUrl] = useState('');
    const [folderId, setFolderId] = useState<number | null>(defaultFolderId);
    const { t } = useTranslation();

    useEscapeToClose(isOpen, onClose);

    useEffect(() => {
        if (!isOpen) return;
        setUrl('');
        setFolderId(defaultFolderId);
    }, [isOpen, defaultFolderId]);

    if (!isOpen) return null;

    const handleSubmit = (e: React.FormEvent) => {
        e.preventDefault();
        if (!url.trim()) {
            toast.error(t('files.please_enter_url'));
            return;
        }
        if (!url.startsWith('http://') && !url.startsWith('https://')) {
            toast.error(t('files.url_must_start'));
            return;
        }
        onUpload(url.trim(), folderId);
        setUrl('');
        setFolderId(defaultFolderId);
        onClose();
    };

    return (
        <div className="tr-modal-backdrop fixed inset-0 z-[200] flex items-center justify-center p-4" onClick={onClose}>
            <form
                onSubmit={handleSubmit}
                className="tr-modal w-full max-w-[440px] overflow-hidden flex flex-col animate-in fade-in zoom-in-95 duration-150"
                onClick={e => e.stopPropagation()}
            >
                <div className="tr-modal-header p-4 flex items-center justify-between">
                    <h3 className="text-telegram-text font-medium flex items-center gap-2">
                        <Globe className="w-5 h-5 text-telegram-primary" />
                        {t('files.remote_upload')}
                    </h3>
                    <button type="button" onClick={onClose} className="tr-modal-close">
                        <X className="w-5 h-5" />
                    </button>
                </div>

                <div className="p-4 space-y-4">
                    <div className="space-y-1">
                        <label className="text-xs text-telegram-subtext font-medium">{t('files.remote_file_url')}</label>
                        <input
                            type="text"
                            placeholder="https://example.com/file.zip"
                            value={url}
                            onChange={e => setUrl(e.target.value)}
                            className="tr-modal-input w-full px-3 py-2 text-sm placeholder:text-telegram-subtext/60"
                            autoFocus
                        />
                    </div>

                    <div className="space-y-1">
                        <label className="text-xs text-telegram-subtext font-medium">{t('files.destination_folder')}</label>
                        <div className="relative">
                            <select
                                value={folderId === null ? '' : folderId}
                                onChange={e => setFolderId(e.target.value === '' ? null : Number(e.target.value))}
                                className="tr-modal-input appearance-none w-full pl-3 pr-8 py-2 text-sm cursor-pointer"
                            >
                                <option value="">{t('common.personal_vault')}</option>
                                {folders.map(folder => (
                                    <option key={folder.id} value={folder.id}>
                                        {folder.name}
                                    </option>
                                ))}
                            </select>
                            <ChevronDown className="w-4 h-4 text-telegram-subtext absolute right-2.5 top-1/2 -translate-y-1/2 pointer-events-none" />
                        </div>
                    </div>
                </div>

                <div className="tr-modal-footer">
                    <button
                        type="button"
                        onClick={onClose}
                        className="tr-button tr-button--secondary tr-button--sm"
                    >
                        {t('common.cancel')}
                    </button>
                    <button
                        type="submit"
                        className="tr-button tr-button--primary tr-button--sm"
                    >
                        {t('files.start_upload')}
                    </button>
                </div>
            </form>
        </div>
    );
}
