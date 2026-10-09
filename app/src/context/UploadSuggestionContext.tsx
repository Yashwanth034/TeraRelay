import React, { createContext, useCallback, useContext, useRef, useState } from 'react';
import { FileCheck2, Layers3, ShieldCheck, X } from 'lucide-react';
import { formatBytes } from '../utils';

export interface UploadCandidateFile {
    logical_file_id: string;
    message_id: number;
    name: string;
    size: number;
}

export interface UploadPreflightResult {
    file_name: string;
    size: number;
    source_sha256: string;
    source_identity: string;
    exact_duplicate?: UploadCandidateFile | null;
    similar_files: UploadCandidateFile[];
}

export type UploadSuggestionDecision =
    | 'skip'
    | 'upload_anyway'
    | 'add_version'
    | 'keep_separate'
    | 'cancel';

interface UploadSuggestionRequest {
    kind: 'duplicate' | 'version';
    incomingName: string;
    incomingSize: number;
    candidate: UploadCandidateFile;
    stackName?: string | null;
}

interface PendingReview extends UploadSuggestionRequest {
    resolve: (decision: UploadSuggestionDecision) => void;
}

interface UploadSuggestionContextValue {
    reviewUploadSuggestion: (request: UploadSuggestionRequest) => Promise<UploadSuggestionDecision>;
}

const UploadSuggestionContext = createContext<UploadSuggestionContextValue | null>(null);

export function UploadSuggestionProvider({ children }: { children: React.ReactNode }) {
    const [pending, setPending] = useState<PendingReview | null>(null);
    const pendingRef = useRef<PendingReview | null>(null);

    const finish = useCallback((decision: UploadSuggestionDecision) => {
        const current = pendingRef.current;
        pendingRef.current = null;
        setPending(null);
        current?.resolve(decision);
    }, []);

    const reviewUploadSuggestion = useCallback((request: UploadSuggestionRequest) => {
        if (pendingRef.current) {
            pendingRef.current.resolve('cancel');
        }
        return new Promise<UploadSuggestionDecision>((resolve) => {
            const next = { ...request, resolve };
            pendingRef.current = next;
            setPending(next);
        });
    }, []);

    const dismiss = () => finish(pending?.kind === 'duplicate' ? 'skip' : 'cancel');

    return (
        <UploadSuggestionContext.Provider value={{ reviewUploadSuggestion }}>
            {children}
            {pending && (
                <div
                    className="tr-modal-backdrop fixed inset-0 z-[10020] flex items-center justify-center p-4"
                    onMouseDown={(event) => {
                        if (event.target === event.currentTarget) dismiss();
                    }}
                >
                    <div
                        role="dialog"
                        aria-modal="true"
                        aria-label={pending.kind === 'duplicate' ? 'Exact duplicate found' : 'Possible version found'}
                        className="tr-modal w-full max-w-[520px] overflow-hidden"
                    >
                        <div className="flex items-start justify-between gap-4 px-5 pt-5">
                            <div className="flex min-w-0 gap-3">
                                <div className="tr-modal-icon mt-0.5 grid h-10 w-10 shrink-0 place-items-center">
                                    {pending.kind === 'duplicate'
                                        ? <FileCheck2 className="h-5 w-5 text-emerald-400" />
                                        : <Layers3 className="h-5 w-5 text-telegram-primary" />}
                                </div>
                                <div className="min-w-0">
                                    <h2 className="text-base font-semibold text-telegram-text">
                                        {pending.kind === 'duplicate' ? 'Exact duplicate found' : 'Possible version found'}
                                    </h2>
                                    <p className="mt-1 text-xs leading-relaxed text-telegram-subtext">
                                        {pending.kind === 'duplicate'
                                            ? 'TeraRelay compared a SHA-256 fingerprint calculated from every byte. The stored file has the same content.'
                                            : 'The files have a matching cleaned title and media type, but their verified SHA-256 fingerprints are different.'}
                                    </p>
                                </div>
                            </div>
                            <button
                                type="button"
                                aria-label="Close"
                                onClick={dismiss}
                                className="tr-modal-close"
                            >
                                <X className="h-4 w-4" />
                            </button>
                        </div>

                        <div className="mx-5 mt-4 grid gap-2">
                            <div className="rounded-xl border border-telegram-border bg-telegram-bg/35 px-3.5 py-3">
                                <div className="text-[10px] font-semibold uppercase tracking-[0.08em] text-telegram-subtext">
                                    New file
                                </div>
                                <div className="mt-1 truncate text-sm font-medium text-telegram-text" title={pending.incomingName}>
                                    {pending.incomingName}
                                </div>
                                <div className="mt-1 text-[11px] text-telegram-subtext">
                                    {formatBytes(pending.incomingSize)}
                                </div>
                            </div>

                            <div className="rounded-xl border border-telegram-border bg-telegram-bg/35 px-3.5 py-3">
                                <div className="text-[10px] font-semibold uppercase tracking-[0.08em] text-telegram-subtext">
                                    Existing {pending.stackName ? 'stack' : 'file'}
                                </div>
                                <div className="mt-1 truncate text-sm font-medium text-telegram-text" title={pending.candidate.name}>
                                    {pending.stackName || pending.candidate.name}
                                </div>
                                <div className="mt-1 text-[11px] text-telegram-subtext">
                                    {formatBytes(pending.candidate.size)}
                                </div>
                            </div>

                            <div className="flex items-center gap-2 px-1 pt-1 text-[11px] text-telegram-subtext">
                                <ShieldCheck className="h-3.5 w-3.5 text-emerald-400" />
                                {pending.kind === 'duplicate'
                                    ? 'Content fingerprint: exact match'
                                    : 'Content fingerprint: different bytes'}
                            </div>
                        </div>

                        <div className="tr-modal-footer mt-5 flex-wrap">
                            {pending.kind === 'duplicate' ? (
                                <>
                                    <button
                                        type="button"
                                        onClick={() => finish('upload_anyway')}
                                        className="tr-button tr-button--secondary tr-button--sm"
                                    >
                                        Upload anyway
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => finish('skip')}
                                        className="tr-button tr-button--primary tr-button--sm"
                                    >
                                        Skip duplicate
                                    </button>
                                </>
                            ) : (
                                <>
                                    <button
                                        type="button"
                                        onClick={() => finish('cancel')}
                                        className="tr-button tr-button--secondary tr-button--sm"
                                    >
                                        Cancel
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => finish('keep_separate')}
                                        className="tr-button tr-button--secondary tr-button--sm"
                                    >
                                        Keep separate
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => finish('add_version')}
                                        className="tr-button tr-button--primary tr-button--sm"
                                    >
                                        Add as version
                                    </button>
                                </>
                            )}
                        </div>
                    </div>
                </div>
            )}
        </UploadSuggestionContext.Provider>
    );
}

export function useUploadSuggestion() {
    const context = useContext(UploadSuggestionContext);
    if (!context) {
        throw new Error('useUploadSuggestion must be used within UploadSuggestionProvider');
    }
    return context;
}
