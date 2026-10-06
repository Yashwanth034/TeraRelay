import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { KeyRound, Loader2, LockKeyhole, Phone, Zap } from 'lucide-react';
import { isAndroidPlatform } from '../utils';

interface FastTransferStatus {
    supported: boolean;
    runtime_installed: boolean;
    ready: boolean;
    auth_state: string;
    backend: string;
    detail: string;
}

type AuthStep = 'phone' | 'code' | 'password';
type AuthFlow = 'auto-2fa' | 'manual';

interface FastTransferAuthContextValue {
    ensureFastTransferReady: () => Promise<boolean>;
}

const FastTransferAuthContext = createContext<FastTransferAuthContextValue | undefined>(undefined);

function cleanError(error: unknown): string {
    const message = error instanceof Error ? error.message : String(error);
    return message.replace(/^Error:\s*/i, '');
}

function stepFromStatus(status: FastTransferStatus): AuthStep | null {
    if (status.auth_state === 'phone') return 'phone';
    if (status.auth_state === 'code') return 'code';
    if (status.auth_state === 'password') return 'password';
    return null;
}

export function FastTransferAuthProvider({ children }: { children: ReactNode }) {
    const [open, setOpen] = useState(false);
    const [step, setStep] = useState<AuthStep>('phone');
    const [flow, setFlow] = useState<AuthFlow>('manual');
    const [value, setValue] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const resolveRef = useRef<((value: boolean) => void) | null>(null);
    const activeCheckRef = useRef<Promise<boolean> | null>(null);

    const finish = useCallback((success: boolean) => {
        setOpen(false);
        setBusy(false);
        setError(null);
        setValue('');
        const resolve = resolveRef.current;
        resolveRef.current = null;
        resolve?.(success);
    }, []);

    const waitForAuth = useCallback((status: FastTransferStatus, nextFlow: AuthFlow) => {
        const nextStep = stepFromStatus(status);
        if (!nextStep) {
            return Promise.reject(
                new Error(`TDLib cannot continue from authorization state: ${status.auth_state}`),
            );
        }

        setStep(nextStep);
        setFlow(nextFlow);
        setValue('');
        setError(null);
        setBusy(false);
        setOpen(true);

        return new Promise<boolean>((resolve) => {
            resolveRef.current = resolve;
        });
    }, []);

    const ensureFastTransferReady = useCallback(async (): Promise<boolean> => {
        if (isAndroidPlatform) return true;
        if (activeCheckRef.current) return activeCheckRef.current;

        const check = (async () => {
            const current = await invoke<FastTransferStatus>('cmd_fast_transfer_status');
            if (!current.supported || current.ready) return true;

            // First choice: silently authorize TDLib from the already authenticated
            // TeraRelay/Grammers session. A transient startup/timeout must never
            // erase a possibly valid saved TDLib authorization database.
            let prepared: FastTransferStatus | null = null;
            let automaticError: unknown = null;
            try {
                prepared = await invoke<FastTransferStatus>('cmd_fast_transfer_prepare_saved', {
                    install: !current.runtime_installed,
                });
            } catch (error) {
                automaticError = error;
                console.warn('Automatic TDLib authorization did not finish yet.', error);
            }

            const settleSavedSession = async (initial: FastTransferStatus | null) => {
                let settled = initial;
                for (let attempt = 0; attempt < 8; attempt += 1) {
                    if (
                        settled?.ready
                        || settled?.auth_state === 'password'
                        || settled?.auth_state === 'code'
                        || settled?.auth_state === 'phone'
                    ) {
                        break;
                    }

                    await new Promise(resolve => window.setTimeout(resolve, 400));
                    try {
                        settled = await invoke<FastTransferStatus>('cmd_fast_transfer_status');
                    } catch {
                        // The worker may still be reopening its TDLib database.
                    }
                }
                return settled;
            };

            const settled = await settleSavedSession(prepared);

            if (settled?.ready) return true;
            if (settled?.auth_state === 'password') {
                return await waitForAuth(settled, 'auto-2fa');
            }
            if (settled?.auth_state === 'code') {
                return await waitForAuth(settled, 'manual');
            }

            if (settled?.auth_state !== 'phone') {
                const suffix = automaticError ? ` Last error: ${cleanError(automaticError)}` : '';
                throw new Error(
                    `TDLib is still restoring its saved session. Please retry in a moment.${suffix}`,
                );
            }

            // Only a definitive phone state means there is no reusable TDLib
            // authorization to continue. Now reset and offer direct login.
            const manual = await invoke<FastTransferStatus>('cmd_fast_transfer_prepare_manual_saved', {
                install: !current.runtime_installed,
            });

            if (manual.ready) return true;
            if (manual.auth_state !== 'phone') {
                const nextStep = stepFromStatus(manual);
                if (nextStep) return await waitForAuth(manual, 'manual');
                throw new Error(
                    `TDLib manual login expected a phone number but returned: ${manual.auth_state}`,
                );
            }

            return await waitForAuth(manual, 'manual');
        })();

        activeCheckRef.current = check;
        try {
            return await check;
        } finally {
            if (activeCheckRef.current === check) {
                activeCheckRef.current = null;
            }
        }
    }, [waitForAuth]);

    const submit = async (event: React.FormEvent) => {
        event.preventDefault();
        if (busy) return;

        let submitted = value;
        if (step === 'phone') {
            submitted = value.trim();
            if (!submitted.startsWith('+')) {
                setError('Include your country code, for example +91 98765 43210.');
                return;
            }
        } else if (step === 'code') {
            submitted = value.replace(/\s/g, '');
        }

        if (!submitted) return;

        setBusy(true);
        setError(null);

        try {
            let status = step === 'phone'
                ? await invoke<FastTransferStatus>('cmd_fast_transfer_phone', { phone: submitted })
                : step === 'code'
                    ? await invoke<FastTransferStatus>('cmd_fast_transfer_code', { code: submitted })
                    : await invoke<FastTransferStatus>('cmd_fast_transfer_password', { password: submitted });

            if (status.ready) {
                // Confirm the newly authorized TDLib session once before
                // releasing the queued transfer. This catches a late 401 and
                // avoids treating a half-settled login as durable.
                await new Promise(resolve => window.setTimeout(resolve, 250));
                const confirmed = await invoke<FastTransferStatus>('cmd_fast_transfer_status');
                if (confirmed.ready) {
                    finish(true);
                    return;
                }
                status = confirmed;
            }

            const nextStep = stepFromStatus(status);
            if (!nextStep) {
                throw new Error(`Telegram returned an unsupported TDLib authorization state: ${status.auth_state}`);
            }

            setStep(nextStep);
            setValue('');
        } catch (submitError) {
            setError(cleanError(submitError));
        } finally {
            setBusy(false);
        }
    };

    const title = flow === 'auto-2fa'
        ? 'Confirm Telegram 2FA'
        : step === 'phone'
            ? 'Connect fast transfers'
            : step === 'code'
                ? 'Enter Telegram code'
                : 'Enter Telegram 2FA';

    const description = flow === 'auto-2fa'
        ? 'Your main TeraRelay login was reused successfully. Telegram only needs your 2-step verification password to finish TDLib authorization.'
        : step === 'phone'
            ? 'Automatic TDLib authorization was unavailable. Sign in once to the fast-transfer engine; this TDLib session will then be saved for future app launches.'
            : step === 'code'
                ? 'Enter the Telegram login code sent for the TDLib fast-transfer session.'
                : 'Telegram requires your 2-step verification password to finish the TDLib login.';

    const label = step === 'phone'
        ? 'Phone number'
        : step === 'code'
            ? 'Telegram code'
            : '2FA password';

    const placeholder = step === 'phone'
        ? '+91 98765 43210'
        : step === 'code'
            ? '12345'
            : 'Your Telegram 2FA password';

    const Icon = step === 'phone' ? Phone : step === 'code' ? KeyRound : LockKeyhole;

    return (
        <FastTransferAuthContext.Provider value={{ ensureFastTransferReady }}>
            {children}

            {open && (
                <div className="fixed inset-0 z-[300] flex items-center justify-center bg-black/70 p-5 backdrop-blur-sm">
                    <form
                        onSubmit={submit}
                        className="w-full max-w-[460px] overflow-hidden rounded-2xl border border-telegram-border bg-telegram-surface shadow-2xl"
                    >
                        <div className="border-b border-telegram-border px-5 py-4">
                            <div className="flex items-start gap-3">
                                <div className="grid h-10 w-10 shrink-0 place-items-center rounded-xl border border-telegram-primary/20 bg-telegram-primary/10 text-telegram-primary">
                                    <Zap className="h-4.5 w-4.5" />
                                </div>
                                <div>
                                    <h2 className="text-sm font-semibold text-telegram-text">{title}</h2>
                                    <p className="mt-1 text-xs leading-relaxed text-telegram-subtext">{description}</p>
                                </div>
                            </div>
                        </div>

                        <div className="space-y-4 p-5">
                            <label className="block">
                                <span className="mb-2 block text-[11px] font-semibold uppercase tracking-wider text-telegram-subtext">
                                    {label}
                                </span>
                                <div className="relative">
                                    <Icon className="absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                    <input
                                        autoFocus
                                        type={step === 'password' ? 'password' : step === 'phone' ? 'tel' : 'text'}
                                        autoComplete={step === 'password' ? 'current-password' : step === 'phone' ? 'tel' : 'one-time-code'}
                                        value={value}
                                        onChange={(event) => {
                                            setValue(event.target.value);
                                            setError(null);
                                        }}
                                        placeholder={placeholder}
                                        className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-3 pl-10 pr-3 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                    />
                                </div>
                                {step === 'phone' && (
                                    <span className="mt-1.5 block text-[11px] text-telegram-subtext">
                                        Include the country code. Example: +91 98765 43210.
                                    </span>
                                )}
                            </label>

                            {error && (
                                <div className="break-words rounded-xl border border-red-500/20 bg-red-500/10 px-3.5 py-3 text-xs text-red-400">
                                    {error}
                                </div>
                            )}

                            <div className="rounded-xl border border-telegram-border/70 bg-telegram-hover/30 px-3.5 py-3 text-[11px] leading-relaxed text-telegram-subtext">
                                This authorizes only TeraRelay's TDLib/C++ fast-transfer engine. Your saved API ID and API Hash are reused securely.
                            </div>

                            <div className="flex justify-end gap-2">
                                <button
                                    type="button"
                                    onClick={() => finish(false)}
                                    disabled={busy}
                                    className="rounded-xl border border-telegram-border px-4 py-2.5 text-sm text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text disabled:opacity-40"
                                >
                                    Cancel
                                </button>
                                <button
                                    type="submit"
                                    disabled={busy || !value}
                                    className="min-w-[120px] rounded-xl bg-telegram-primary px-4 py-2.5 text-sm font-semibold text-white transition hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-50"
                                >
                                    <span className="flex items-center justify-center gap-2">
                                        {busy && <Loader2 className="h-4 w-4 animate-spin" />}
                                        {busy ? 'Checking…' : step === 'phone' ? 'Send code' : 'Continue'}
                                    </span>
                                </button>
                            </div>
                        </div>
                    </form>
                </div>
            )}
        </FastTransferAuthContext.Provider>
    );
}

export function useFastTransferAuth() {
    const context = useContext(FastTransferAuthContext);
    if (!context) {
        throw new Error('useFastTransferAuth must be used within FastTransferAuthProvider');
    }
    return context;
}
