import React, { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Zap, Cpu, X } from 'lucide-react';

export type TransferEngine = 'boost' | 'tdlib';

interface TransferMethodContextValue {
    chooseTransferMethod: (action: 'upload' | 'download') => Promise<TransferEngine | null>;
}

interface PendingChoice {
    action: 'upload' | 'download';
    resolve: (engine: TransferEngine | null) => void;
}

const TransferMethodContext = createContext<TransferMethodContextValue | null>(null);

export function TransferMethodProvider({ children }: { children: React.ReactNode }) {
    const [pending, setPending] = useState<PendingChoice | null>(null);
    const pendingRef = useRef<PendingChoice | null>(null);
    const [tdlibSupported, setTdlibSupported] = useState<boolean | null>(null);

    useEffect(() => {
        let disposed = false;
        invoke<boolean>('cmd_tdlib_transfer_supported')
            .then((supported) => {
                if (!disposed) setTdlibSupported(supported);
            })
            .catch(() => {
                if (!disposed) setTdlibSupported(false);
            });
        return () => {
            disposed = true;
        };
    }, []);

    const finish = useCallback((engine: TransferEngine | null) => {
        const current = pendingRef.current;
        pendingRef.current = null;
        setPending(null);
        current?.resolve(engine);
    }, []);

    const chooseTransferMethod = useCallback((action: 'upload' | 'download') => {
        // A native file picker or a rapid double click must never open two
        // competing engine dialogs. Resolve any abandoned request as cancelled.
        if (pendingRef.current) {
            pendingRef.current.resolve(null);
        }
        return new Promise<TransferEngine | null>((resolve) => {
            const choice = { action, resolve };
            pendingRef.current = choice;
            setPending(choice);
        });
    }, []);

    return (
        <TransferMethodContext.Provider value={{ chooseTransferMethod }}>
            {children}
            {pending && (
                <div
                    className="tr-modal-backdrop fixed inset-0 z-[10000] flex items-center justify-center p-4"
                    onMouseDown={(event) => {
                        if (event.currentTarget === event.target) finish(null);
                    }}
                >
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-label="Choose transfer method"
                        className="tr-modal w-full max-w-sm p-4"
                    >
                        <div className="mb-4 flex items-start justify-between gap-3">
                            <div>
                                <h2 className="text-base font-semibold text-telegram-text">
                                    {pending.action === 'download' ? 'Download' : 'Upload'}
                                </h2>
                                <p className="mt-1 text-xs text-telegram-subtext">
                                    Choose transfer method
                                </p>
                            </div>
                            <button
                                type="button"
                                aria-label="Cancel transfer method"
                                onClick={() => finish(null)}
                                className="tr-modal-close"
                            >
                                <X className="h-4 w-4" />
                            </button>
                        </div>

                        <div className="space-y-2">
                            <button
                                type="button"
                                onClick={() => finish('boost')}
                                className="tr-modal-option tr-modal-option--primary flex w-full items-center gap-3 px-4 py-3 text-left"
                            >
                                <span className="tr-modal-icon flex h-10 w-10 shrink-0 items-center justify-center">
                                    <Zap className="h-5 w-5" />
                                </span>
                                <span className="min-w-0">
                                    <span className="block text-sm font-semibold text-telegram-text">
                                        TeraRelay Boost
                                    </span>
                                    <span className="block text-xs text-telegram-subtext">
                                        Fast MTProto transfer
                                    </span>
                                </span>
                            </button>

                            <button
                                type="button"
                                onClick={() => finish('tdlib')}
                                disabled={tdlibSupported === false}
                                className="tr-modal-option flex w-full items-center gap-3 px-4 py-3 text-left disabled:cursor-not-allowed disabled:opacity-45"
                            >
                                <span className="tr-modal-icon flex h-10 w-10 shrink-0 items-center justify-center">
                                    <Cpu className="h-5 w-5" />
                                </span>
                                <span className="min-w-0">
                                    <span className="block text-sm font-semibold text-telegram-text">
                                        TDLib / C++
                                    </span>
                                    <span className="block text-xs text-telegram-subtext">
                                        {tdlibSupported === false
                                            ? 'Not available on this platform'
                                            : 'Native transfer engine'}
                                    </span>
                                </span>
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </TransferMethodContext.Provider>
    );
}

export function useTransferMethod() {
    const context = useContext(TransferMethodContext);
    if (!context) {
        throw new Error('useTransferMethod must be used within TransferMethodProvider');
    }
    return context;
}
