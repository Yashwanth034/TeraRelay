import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { load } from '@tauri-apps/plugin-store';
import { open } from '@tauri-apps/plugin-shell';
import { AnimatePresence, motion } from 'framer-motion';
import {
    ArrowLeft,
    ArrowRight,
    CircleHelp,
    ExternalLink,
    KeyRound,
    LockKeyhole,
    Moon,
    Phone,
    ShieldCheck,
    Sun,
    X,
} from 'lucide-react';
import { useTheme } from '../../context/ThemeContext';
import { usePlatform } from '../../hooks/usePlatform';
import { TeraRelayBrand } from './TeraRelayBrand';

type Step = 'setup' | 'phone' | 'code' | 'password';

interface SecureCredentialStatus {
    available: boolean;
    has_credentials: boolean;
    migrated: boolean;
    api_id?: number | null;
}

function AuthThemeToggle() {
    const { theme, toggleTheme } = useTheme();

    return (
        <button
            type="button"
            onClick={toggleTheme}
            className="absolute right-5 top-5 z-10 rounded-full p-2.5 text-telegram-subtext transition hover:bg-telegram-hover hover:text-telegram-text"
            title={theme === 'dark' ? 'Switch to Light Mode' : 'Switch to Dark Mode'}
            aria-label={theme === 'dark' ? 'Switch to Light Mode' : 'Switch to Dark Mode'}
        >
            {theme === 'dark' ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
        </button>
    );
}

function normalizeError(error: unknown) {
    if (error instanceof Error) return error.message;
    if (typeof error === 'string') return error;
    try {
        return JSON.stringify(error);
    } catch {
        return 'Something went wrong. Please try again.';
    }
}

export function AuthWizard({ onLogin }: { onLogin: () => void }) {
    const isBrowser = typeof window !== 'undefined' && !('__TAURI_INTERNALS__' in window);
    const qaEphemeralCredentialsAllowed =
        import.meta.env.DEV && import.meta.env.VITE_TERA_REAL_E2E_QA === '1';
    const { isMobile } = usePlatform();
    const [step, setStep] = useState<Step>('setup');
    const [checkingCredentials, setCheckingCredentials] = useState(true);
    const [hasSavedCredentials, setHasSavedCredentials] = useState(false);
    const [secureStoreAvailable, setSecureStoreAvailable] = useState(true);
    const [apiId, setApiId] = useState('');
    const [apiHash, setApiHash] = useState('');
    const [phone, setPhone] = useState('');
    const [code, setCode] = useState('');
    const [password, setPassword] = useState('');
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [floodWait, setFloodWait] = useState<number | null>(null);
    const [showHelp, setShowHelp] = useState(false);

    useEffect(() => {
        if (isBrowser) {
            setCheckingCredentials(false);
            return;
        }

        const initialize = async () => {
            try {
                const status = await invoke<SecureCredentialStatus>('cmd_secure_credential_status');
                setSecureStoreAvailable(status.available);
                setHasSavedCredentials(status.has_credentials);
                if (status.api_id) setApiId(String(status.api_id));
                if (status.has_credentials) setStep('phone');
            } catch {
                setHasSavedCredentials(false);
                setStep('setup');
            } finally {
                setCheckingCredentials(false);
            }
        };

        void initialize();
    }, [isBrowser]);

    useEffect(() => {
        if (!floodWait) return;
        const timer = window.setInterval(() => {
            setFloodWait((current) => {
                if (!current || current <= 1) {
                    window.clearInterval(timer);
                    return null;
                }
                return current - 1;
            });
        }, 1000);
        return () => window.clearInterval(timer);
    }, [floodWait]);

    const goToSetup = () => {
        setStep('setup');
        setHasSavedCredentials(false);
        setApiHash('');
        setError(null);
    };

    const handleSetupSubmit = (event: React.FormEvent) => {
        event.preventDefault();
        const trimmedId = apiId.trim();
        const trimmedHash = apiHash.trim();

        if (!trimmedId || !trimmedHash) {
            setError('Enter both your API ID and API Hash.');
            return;
        }
        if (!/^\d+$/.test(trimmedId)) {
            setError('API ID must be a number.');
            return;
        }
        if (/\s/.test(trimmedHash)) {
            setError('API Hash cannot contain spaces.');
            return;
        }
        // Desktop keeps the API hash in the operating-system credential store.
        // Mobile deliberately keeps the API hash memory-only: Telegram only needs it
        // for the login-code request, while later session restores need only api_id.
        if (!isMobile && !secureStoreAvailable && !qaEphemeralCredentialsAllowed) {
            setError('Secure credential storage is unavailable on this device. Unlock or enable your operating system credential store and try again.');
            return;
        }

        setApiId(trimmedId);
        setApiHash(trimmedHash);
        setError(null);
        setStep('phone');
    };

    const handlePhoneSubmit = async (event: React.FormEvent) => {
        event.preventDefault();
        const trimmedPhone = phone.trim();
        if (!trimmedPhone) return;

        setLoading(true);
        setError(null);

        try {
            if (hasSavedCredentials) {
                await invoke('cmd_auth_request_code_saved', { phone: trimmedPhone });
            } else {
                const id = Number.parseInt(apiId, 10);
                if (Number.isNaN(id) || !apiHash) {
                    setStep('setup');
                    throw new Error('Enter your Telegram API credentials first.');
                }

                let useEphemeralQaCredentials = false;
                if (!isMobile) {
                    const status = await invoke<SecureCredentialStatus>('cmd_secure_credential_status');
                    if (!status.available) {
                        if (!qaEphemeralCredentialsAllowed) {
                            throw new Error('Secure credential storage is unavailable on this device. Unlock or enable your operating system credential store and try again.');
                        }
                        useEphemeralQaCredentials = true;
                    }
                }

                await invoke(
                    useEphemeralQaCredentials
                        ? 'cmd_auth_request_code_ephemeral'
                        : 'cmd_auth_request_code',
                    {
                        phone: trimmedPhone,
                        apiId: id,
                        apiHash,
                    },
                );

                const store = await load('config.json');
                await store.set('api_id', String(id));
                await store.delete('api_hash');
                await store.save();

                setHasSavedCredentials(!useEphemeralQaCredentials);
                if (!useEphemeralQaCredentials) setApiHash('');
            }

            setStep('code');
        } catch (requestError) {
            const message = normalizeError(requestError);
            if (message.includes('API_ID_INVALID')) {
                setStep('setup');
                setHasSavedCredentials(false);
                setError('Telegram rejected this API ID / API Hash pair. Check both values at my.telegram.org and try again.');
            } else {
                const match = message.match(/FLOOD_WAIT_(\d+)/);
                if (match) {
                    setFloodWait(Number.parseInt(match[1], 10));
                } else {
                    setError(message);
                }
            }
        } finally {
            setLoading(false);
        }
    };

    const handleCodeSubmit = async (event: React.FormEvent) => {
        event.preventDefault();
        const trimmedCode = code.replace(/\s/g, '');
        if (!trimmedCode) return;

        setLoading(true);
        setError(null);
        try {
            const result = await invoke<{ success: boolean; next_step?: string }>('cmd_auth_sign_in', {
                code: trimmedCode,
            });
            setCode('');

            if (result.success) {
                setPassword('');
                onLogin();
                return;
            }
            if (result.next_step === 'password') {
                setStep('password');
                return;
            }
            setError('Telegram could not complete sign in. Please try again.');
        } catch (signInError) {
            setError(normalizeError(signInError));
        } finally {
            setLoading(false);
        }
    };

    const handlePasswordSubmit = async (event: React.FormEvent) => {
        event.preventDefault();
        if (!password) return;

        setLoading(true);
        setError(null);
        try {
            const result = await invoke<{ success: boolean }>('cmd_auth_check_password', { password });
            setPassword('');
            if (result.success) {
                onLogin();
            } else {
                setError('Two-step verification failed.');
            }
        } catch (passwordError) {
            setPassword('');
            setError(normalizeError(passwordError));
        } finally {
            setLoading(false);
        }
    };

    if (isBrowser) {
        return (
            <div className="grid h-full place-items-center bg-telegram-bg p-6">
                <div className="max-w-sm text-center">
                    <TeraRelayBrand size="lg" className="justify-center" />
                    <p className="mt-5 text-sm text-telegram-subtext">
                        Open TeraRelay as the desktop application to sign in.
                    </p>
                </div>
            </div>
        );
    }

    const title =
        step === 'setup'
            ? 'Connect Telegram'
            : step === 'phone'
              ? 'Your phone number'
              : step === 'code'
                ? 'Verification code'
                : 'Two-step verification';

    const subtitle =
        step === 'setup'
            ? 'Use your own Telegram API credentials.'
            : step === 'phone'
              ? 'Enter the number connected to your Telegram account.'
              : step === 'code'
                ? 'Enter the code Telegram sent to you.'
                : 'Enter your Telegram cloud password.';

    return (
        <div className="relative flex h-full w-full items-center justify-center overflow-y-auto bg-telegram-bg px-5 py-10">
            <AuthThemeToggle />

            <div className="w-full max-w-[420px]">
                <div className="mb-7 flex justify-center">
                    <TeraRelayBrand size="lg" className="justify-center" />
                </div>

                <motion.div
                    initial={{ opacity: 0, y: 12 }}
                    animate={{ opacity: 1, y: 0 }}
                    className="rounded-3xl border border-telegram-border bg-telegram-surface p-7 shadow-2xl"
                >
                    <div className="mb-6 text-center">
                        <h1 className="text-xl font-semibold tracking-tight text-telegram-text">
                            Sign in to TeraRelay
                        </h1>
                        <p className="mt-1.5 text-sm font-medium text-telegram-text">{title}</p>
                        <p className="mt-1 text-xs leading-relaxed text-telegram-subtext">{subtitle}</p>
                    </div>

                    {checkingCredentials ? (
                        <div className="grid min-h-[180px] place-items-center">
                            <div className="h-7 w-7 animate-spin rounded-full border-2 border-telegram-primary border-t-transparent" />
                        </div>
                    ) : floodWait ? (
                        <div className="py-5 text-center">
                            <div className="text-3xl font-semibold tabular-nums text-telegram-text">
                                {Math.floor(floodWait / 60)}:{String(floodWait % 60).padStart(2, '0')}
                            </div>
                            <p className="mt-2 text-sm text-telegram-subtext">
                                Telegram asked you to wait before trying again.
                            </p>
                        </div>
                    ) : (
                        <AnimatePresence mode="wait">
                            {step === 'setup' && (
                                <motion.form
                                    key="setup"
                                    initial={{ opacity: 0, x: 10 }}
                                    animate={{ opacity: 1, x: 0 }}
                                    exit={{ opacity: 0, x: -10 }}
                                    onSubmit={handleSetupSubmit}
                                    className="space-y-4"
                                >
                                    <label className="block">
                                        <span className="mb-1.5 block text-xs font-medium text-telegram-subtext">API ID</span>
                                        <div className="relative">
                                            <KeyRound className="absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                            <input
                                                autoFocus
                                                inputMode="numeric"
                                                autoComplete="off"
                                                value={apiId}
                                                onChange={(event) => setApiId(event.target.value)}
                                                placeholder="12345678"
                                                className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-3 pl-10 pr-3 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                            />
                                        </div>
                                    </label>

                                    <label className="block">
                                        <span className="mb-1.5 block text-xs font-medium text-telegram-subtext">API Hash</span>
                                        <div className="relative">
                                            <LockKeyhole className="absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                            <input
                                                type="password"
                                                autoComplete="off"
                                                value={apiHash}
                                                onChange={(event) => setApiHash(event.target.value)}
                                                placeholder="Your API Hash"
                                                className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-3 pl-10 pr-3 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                            />
                                        </div>
                                    </label>

                                    <button
                                        type="button"
                                        onClick={() => setShowHelp(true)}
                                        className="inline-flex items-center gap-1.5 text-xs font-medium text-telegram-primary hover:underline"
                                    >
                                        <CircleHelp className="h-3.5 w-3.5" />
                                        Get API ID and API Hash
                                    </button>

                                    <button
                                        type="submit"
                                        className="mt-2 flex w-full items-center justify-center gap-2 rounded-xl bg-telegram-primary py-3 text-sm font-semibold text-white transition hover:brightness-110 active:scale-[0.99]"
                                    >
                                        Continue
                                        <ArrowRight className="h-4 w-4" />
                                    </button>
                                </motion.form>
                            )}

                            {step === 'phone' && (
                                <motion.form
                                    key="phone"
                                    initial={{ opacity: 0, x: 10 }}
                                    animate={{ opacity: 1, x: 0 }}
                                    exit={{ opacity: 0, x: -10 }}
                                    onSubmit={handlePhoneSubmit}
                                    className="space-y-4"
                                >
                                    <label className="block">
                                        <span className="mb-1.5 block text-xs font-medium text-telegram-subtext">Phone number</span>
                                        <div className="relative">
                                            <Phone className="absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                            <input
                                                autoFocus
                                                type="tel"
                                                autoComplete="tel"
                                                value={phone}
                                                onChange={(event) => setPhone(event.target.value)}
                                                placeholder="+91 98765 43210"
                                                className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-3 pl-10 pr-3 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                            />
                                        </div>
                                        <span className="mt-1.5 block text-[11px] leading-relaxed text-telegram-subtext">
                                            Include your country code, for example +91 98765 43210.
                                        </span>
                                    </label>

                                    {hasSavedCredentials && (
                                        <div className="flex items-center justify-between gap-3 rounded-xl bg-telegram-primary/5 px-3 py-2.5">
                                            <span className="inline-flex items-center gap-1.5 text-[11px] text-telegram-subtext">
                                                <ShieldCheck className="h-3.5 w-3.5 text-telegram-primary" />
                                                API credentials saved securely
                                            </span>
                                            <button
                                                type="button"
                                                onClick={goToSetup}
                                                className="text-[11px] font-medium text-telegram-primary hover:underline"
                                            >
                                                Change
                                            </button>
                                        </div>
                                    )}

                                    <button
                                        type="submit"
                                        disabled={loading || !phone.trim()}
                                        className="flex w-full items-center justify-center gap-2 rounded-xl bg-telegram-primary py-3 text-sm font-semibold text-white transition hover:brightness-110 active:scale-[0.99] disabled:cursor-not-allowed disabled:opacity-50"
                                    >
                                        {loading ? 'Sending…' : 'Continue'}
                                        {!loading && <ArrowRight className="h-4 w-4" />}
                                    </button>

                                    {!hasSavedCredentials && (
                                        <button
                                            type="button"
                                            onClick={() => setStep('setup')}
                                            className="mx-auto flex items-center gap-1.5 text-xs text-telegram-subtext hover:text-telegram-text"
                                        >
                                            <ArrowLeft className="h-3.5 w-3.5" />
                                            Back
                                        </button>
                                    )}
                                </motion.form>
                            )}

                            {step === 'code' && (
                                <motion.form
                                    key="code"
                                    initial={{ opacity: 0, x: 10 }}
                                    animate={{ opacity: 1, x: 0 }}
                                    exit={{ opacity: 0, x: -10 }}
                                    onSubmit={handleCodeSubmit}
                                    className="space-y-4"
                                >
                                    <input
                                        autoFocus
                                        inputMode="numeric"
                                        autoComplete="one-time-code"
                                        value={code}
                                        onChange={(event) => setCode(event.target.value)}
                                        placeholder="Verification code"
                                        className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 px-4 py-3 text-center text-xl tracking-[0.25em] text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                    />

                                    <button
                                        type="submit"
                                        disabled={loading || !code.trim()}
                                        className="w-full rounded-xl bg-telegram-primary py-3 text-sm font-semibold text-white transition hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-50"
                                    >
                                        {loading ? 'Verifying…' : 'Continue'}
                                    </button>

                                    <button
                                        type="button"
                                        onClick={() => {
                                            setCode('');
                                            setError(null);
                                            setStep('phone');
                                        }}
                                        className="mx-auto flex items-center gap-1.5 text-xs text-telegram-subtext hover:text-telegram-text"
                                    >
                                        <ArrowLeft className="h-3.5 w-3.5" />
                                        Change phone number
                                    </button>
                                </motion.form>
                            )}

                            {step === 'password' && (
                                <motion.form
                                    key="password"
                                    initial={{ opacity: 0, x: 10 }}
                                    animate={{ opacity: 1, x: 0 }}
                                    exit={{ opacity: 0, x: -10 }}
                                    onSubmit={handlePasswordSubmit}
                                    className="space-y-4"
                                >
                                    <div className="relative">
                                        <LockKeyhole className="absolute left-3.5 top-1/2 h-4 w-4 -translate-y-1/2 text-telegram-subtext" />
                                        <input
                                            autoFocus
                                            type="password"
                                            autoComplete="current-password"
                                            value={password}
                                            onChange={(event) => setPassword(event.target.value)}
                                            placeholder="Telegram password"
                                            className="w-full rounded-xl border border-telegram-border bg-telegram-bg/70 py-3 pl-10 pr-3 text-sm text-telegram-text outline-none transition focus:border-telegram-primary/60"
                                        />
                                    </div>

                                    <button
                                        type="submit"
                                        disabled={loading || !password}
                                        className="w-full rounded-xl bg-telegram-primary py-3 text-sm font-semibold text-white transition hover:brightness-110 disabled:cursor-not-allowed disabled:opacity-50"
                                    >
                                        {loading ? 'Verifying…' : 'Continue'}
                                    </button>
                                </motion.form>
                            )}
                        </AnimatePresence>
                    )}

                    {error && (
                        <div className="mt-4 rounded-xl border border-red-500/20 bg-red-500/10 px-3.5 py-3 text-xs leading-relaxed text-red-400">
                            {error}
                        </div>
                    )}
                </motion.div>

                <p className="mt-4 text-center text-[11px] text-telegram-subtext">
                    Your Telegram session stays on this device.
                </p>
            </div>

            <AnimatePresence>
                {showHelp && (
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        className="fixed inset-0 z-50 grid place-items-center bg-black/60 p-5"
                        onClick={() => setShowHelp(false)}
                    >
                        <motion.div
                            initial={{ opacity: 0, scale: 0.98, y: 8 }}
                            animate={{ opacity: 1, scale: 1, y: 0 }}
                            exit={{ opacity: 0, scale: 0.98, y: 8 }}
                            className="w-full max-w-[390px] rounded-2xl border border-telegram-border bg-telegram-surface p-5 shadow-2xl"
                            onClick={(event) => event.stopPropagation()}
                        >
                            <div className="flex items-center justify-between">
                                <h2 className="text-base font-semibold text-telegram-text">Get Telegram API credentials</h2>
                                <button
                                    type="button"
                                    onClick={() => setShowHelp(false)}
                                    className="rounded-lg p-1.5 text-telegram-subtext hover:bg-telegram-hover hover:text-telegram-text"
                                    aria-label="Close guide"
                                >
                                    <X className="h-4 w-4" />
                                </button>
                            </div>

                            <ol className="mt-4 space-y-3 text-sm text-telegram-subtext">
                                <li><span className="font-medium text-telegram-text">1.</span> Open my.telegram.org and sign in.</li>
                                <li><span className="font-medium text-telegram-text">2.</span> Open <span className="text-telegram-text">API development tools</span> and create an application.</li>
                                <li><span className="font-medium text-telegram-text">3.</span> Copy the API ID and API Hash into TeraRelay.</li>
                            </ol>

                            <button
                                type="button"
                                onClick={() => void open('https://my.telegram.org')}
                                className="mt-5 flex w-full items-center justify-center gap-2 rounded-xl bg-telegram-primary py-2.5 text-sm font-semibold text-white"
                            >
                                Open my.telegram.org
                                <ExternalLink className="h-4 w-4" />
                            </button>
                        </motion.div>
                    </motion.div>
                )}
            </AnimatePresence>
        </div>
    );
}
