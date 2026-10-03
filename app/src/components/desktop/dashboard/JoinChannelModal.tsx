import { useEffect, useState } from 'react';
import { Link2, Loader2, X } from 'lucide-react';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface Props {
    open: boolean;
    onClose: () => void;
    onJoin: (invite: string) => Promise<void>;
}

export function JoinChannelModal({ open, onClose, onJoin }: Props) {
    const [invite, setInvite] = useState('');
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        if (!open) {
            setInvite('');
            setBusy(false);
            setError(null);
        }
    }, [open]);

    useEscapeToClose(open, onClose, busy);

    if (!open) return null;

    const submit = async (event: React.FormEvent) => {
        event.preventDefault();
        const value = invite.trim();
        if (!value || busy) return;

        setBusy(true);
        setError(null);
        try {
            await onJoin(value);
            onClose();
        } catch (e) {
            setError(String(e).replace(/^Error:\s*/i, ''));
        } finally {
            setBusy(false);
        }
    };

    return (
        <div
            className="fixed inset-0 z-[240] bg-black/65 backdrop-blur-sm flex items-center justify-center p-5"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget && !busy) onClose();
            }}
        >
            <form
                onSubmit={submit}
                className="w-full max-w-[500px] rounded-2xl border border-telegram-border bg-telegram-surface shadow-2xl overflow-hidden"
            >
                <div className="px-5 py-4 border-b border-telegram-border flex items-center justify-between">
                    <div className="flex items-center gap-3 min-w-0">
                        <div className="w-9 h-9 rounded-xl grid place-items-center bg-telegram-primary/10 border border-telegram-primary/20 flex-shrink-0">
                            <Link2 className="w-4 h-4 text-telegram-primary" />
                        </div>
                        <div className="min-w-0">
                            <h2 className="text-sm font-semibold text-telegram-text">Join TeraRelay Channel</h2>
                            <p className="text-[11px] text-telegram-subtext mt-0.5">
                                Paste the TeraRelay invite sent by the channel owner.
                            </p>
                        </div>
                    </div>
                    <button
                        type="button"
                        onClick={onClose}
                        disabled={busy}
                        className="p-1.5 rounded-lg text-telegram-subtext hover:text-telegram-text hover:bg-telegram-hover disabled:opacity-40"
                        aria-label="Close"
                    >
                        <X className="w-4 h-4" />
                    </button>
                </div>

                <div className="p-5 space-y-4">
                    <div>
                        <label className="block text-[11px] font-semibold uppercase tracking-wider text-telegram-subtext mb-2">
                            Channel invite
                        </label>
                        <textarea
                            autoFocus
                            value={invite}
                            onChange={(e) => {
                                setInvite(e.target.value);
                                setError(null);
                            }}
                            rows={4}
                            spellCheck={false}
                            placeholder="terarelay://join/..."
                            className="w-full resize-none rounded-xl border border-telegram-border bg-telegram-bg/70 px-3.5 py-3 text-sm text-telegram-text font-mono outline-none focus:border-telegram-primary/60 placeholder:text-telegram-subtext/50"
                        />
                    </div>

                    <div className="rounded-xl border border-telegram-border/70 bg-telegram-hover/30 px-3.5 py-3 text-xs leading-relaxed text-telegram-subtext">
                        You’ll join with your current Telegram account. After joining, the channel’s files will appear here automatically.
                    </div>

                    {error && (
                        <div className="rounded-xl border border-red-500/20 bg-red-500/10 px-3.5 py-3 text-xs text-red-400 break-words">
                            {error}
                        </div>
                    )}

                    <div className="flex justify-end gap-2">
                        <button
                            type="button"
                            onClick={onClose}
                            disabled={busy}
                            className="px-4 py-2.5 rounded-xl border border-telegram-border text-sm text-telegram-subtext hover:text-telegram-text hover:bg-telegram-hover disabled:opacity-40"
                        >
                            Cancel
                        </button>
                        <button
                            type="submit"
                            disabled={!invite.trim() || busy}
                            className="min-w-[120px] px-4 py-2.5 rounded-xl bg-telegram-primary text-white text-sm font-medium flex items-center justify-center gap-2 disabled:opacity-50 disabled:cursor-not-allowed"
                        >
                            {busy && <Loader2 className="w-4 h-4 animate-spin" />}
                            {busy ? 'Joining…' : 'Join Channel'}
                        </button>
                    </div>
                </div>
            </form>
        </div>
    );
}
