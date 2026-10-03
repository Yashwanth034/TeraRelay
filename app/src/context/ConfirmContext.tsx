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
                    className="fixed inset-0 z-[260] flex items-center justify-center bg-black/60 p-5 backdrop-blur-sm"
                    onMouseDown={(event) => {
                        if (event.target === event.currentTarget) handleCancel();
                    }}
                >
                    <div
                        className="w-full max-w-[420px] overflow-hidden rounded-2xl border border-telegram-border bg-telegram-surface shadow-2xl animate-in zoom-in-95"
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
                        <div className="mt-5 flex justify-end gap-2 border-t border-telegram-border bg-telegram-hover/20 px-5 py-4">
                            <button
                                onClick={handleCancel}
                                className="rounded-xl border border-telegram-border px-4 py-2 text-sm font-medium text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
                            >
                                {options.cancelText || 'Cancel'}
                            </button>
                            <button
                                onClick={handleConfirm}
                                className={`rounded-xl px-4 py-2 text-sm font-semibold transition ${
                                    options.variant === 'danger'
                                        ? 'bg-red-500/15 text-red-400 hover:bg-red-500/25'
                                        : 'bg-telegram-primary text-white hover:brightness-110'
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
