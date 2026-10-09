import { createContext, useContext, useState, ReactNode } from 'react';
import { AlertTriangle, Info } from 'lucide-react';
import { useEscapeToClose } from '../hooks/useEscapeToClose';

interface ConfirmOptions {
    title: string;
    message: string;
    confirmText?: string;
    cancelText?: string;
    variant?: 'danger' | 'info';
}

interface ConfirmContextType {
    confirm: (options: ConfirmOptions) => Promise<boolean>;
}

const ConfirmContext = createContext<ConfirmContextType | undefined>(undefined);

export function ConfirmProvider({ children }: { children: ReactNode }) {
    const [isOpen, setIsOpen] = useState(false);
    const [options, setOptions] = useState<ConfirmOptions>({ title: '', message: '' });
    const [resolveRef, setResolveRef] = useState<((value: boolean) => void) | null>(null);

    const confirm = (opts: ConfirmOptions) => {
        setOptions(opts);
        setIsOpen(true);
        return new Promise<boolean>((resolve) => {
            setResolveRef(() => resolve);
        });
    };

    const handleConfirm = () => {
        setIsOpen(false);
        if (resolveRef) resolveRef(true);
    };

    const handleCancel = () => {
        setIsOpen(false);
        if (resolveRef) resolveRef(false);
    };

    useEscapeToClose(isOpen, handleCancel);

    return (
        <ConfirmContext.Provider value={{ confirm }}>
            {children}
            {isOpen && (
                <div
                    className="tr-modal-backdrop fixed inset-0 z-[260] flex items-center justify-center p-4"
                    onMouseDown={(event) => {
                        if (event.target === event.currentTarget) handleCancel();
                    }}
                >
                    <div
                        className="tr-modal w-full max-w-[420px] overflow-hidden animate-in zoom-in-95"
                        onClick={event => event.stopPropagation()}
                    >
                        <div className="flex gap-3 px-5 pt-5">
                            <div className={`grid h-10 w-10 shrink-0 place-items-center rounded-xl border ${
                                options.variant === 'danger'
                                    ? 'border-red-500/20 bg-red-500/10 text-red-400'
                                    : 'border-telegram-primary/20 bg-telegram-primary/10 text-telegram-primary'
                            }`}>
                                {options.variant === 'danger'
                                    ? <AlertTriangle className="h-4.5 w-4.5" />
                                    : <Info className="h-4.5 w-4.5" />}
                            </div>
                            <div className="min-w-0 flex-1">
                                <h3 className="text-base font-semibold text-telegram-text">{options.title}</h3>
                                <p className="mt-1 text-sm leading-relaxed text-telegram-subtext whitespace-pre-line">{options.message}</p>
                            </div>
                        </div>
                        <div className="tr-modal-footer mt-5">
                            <button
                                onClick={handleCancel}
                                className="tr-button tr-button--secondary tr-button--sm"
                            >
                                {options.cancelText || 'Cancel'}
                            </button>
                            <button
                                onClick={handleConfirm}
                                className={`tr-button tr-button--sm ${
                                    options.variant === 'danger'
                                        ? 'tr-button--danger'
                                        : 'tr-button--primary'
                                }`}
                            >
                                {options.confirmText || 'Confirm'}
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </ConfirmContext.Provider>
    );
}

export const useConfirm = () => {
    const context = useContext(ConfirmContext);
    if (!context) throw new Error('useConfirm must be used within a ConfirmProvider');
    return context;
};
