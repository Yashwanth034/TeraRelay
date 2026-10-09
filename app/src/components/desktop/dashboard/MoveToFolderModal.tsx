import { X, HardDrive, Folder } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { TelegramFolder } from '../../../types';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface MoveToFolderModalProps {
    folders: TelegramFolder[];
    onClose: () => void;
    onSelect: (id: number | null) => void;
    activeFolderId: number | null;
    fileName?: string;
    allowPersonalVault?: boolean;
    writableOnly?: boolean;
}

export function MoveToFolderModal({ folders, onClose, onSelect, activeFolderId, fileName, allowPersonalVault = true, writableOnly = false }: MoveToFolderModalProps) {
    const { t } = useTranslation();
    useEscapeToClose(true, onClose);

    return (
        <div className="tr-modal-backdrop fixed inset-0 z-[100] flex items-center justify-center p-4" onClick={onClose}>
            <div className="tr-modal w-full max-w-[360px] overflow-hidden flex flex-col max-h-[80vh]" onClick={e => e.stopPropagation()}>
                <div className="tr-modal-header p-4 flex justify-between items-center">
                    <h3 className="text-telegram-text font-medium truncate max-w-[220px]">
                        {fileName ? t('files.move_file_to_folder', { name: fileName }) : t('files.move_to_folder')}
                    </h3>
                    <button onClick={onClose} className="tr-modal-close" aria-label="Close"><X className="w-4 h-4" /></button>
                </div>
                <div className="flex-1 overflow-y-auto p-2 space-y-1">
                    {allowPersonalVault && activeFolderId !== null && (
                        <button
                            onClick={() => onSelect(null)}
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
                                key={f.id}
                                onClick={() => onSelect(f.id)}
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
