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
            className="tr-modal-backdrop fixed inset-0 z-[240] flex items-center justify-center p-4"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget && !busy) onClose();
            }}
        >
            <form
                onSubmit={submit}
                className="tr-modal w-full max-w-[500px] overflow-hidden"
            >
                <div className="tr-modal-header px-5 py-4 flex items-center justify-between">
                    <div className="flex items-center gap-3 min-w-0">
                        <div className="tr-modal-icon w-9 h-9 grid place-items-center flex-shrink-0">
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
                        className="tr-modal-close disabled:opacity-40"
                        aria-label="Close"
                    >
                        <X className="w-4 h-4" />
                    </button>
                </div>

                <div className="p-5 space-y-4">
                    <div>
                        <label className="tr-modal-label">
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
                            className="tr-modal-input w-full resize-none px-3.5 py-3 text-sm font-mono placeholder:text-telegram-subtext/50"
                        />
                    </div>

                    <div className="tr-modal-note">
                        You’ll join with your current Telegram account. After joining, the channel’s files will appear here automatically.
                    </div>

                    {error && (
                        <div className="tr-modal-note tr-modal-note--danger break-words">
                            {error}
                        </div>
                    )}

                    <div className="flex justify-end gap-2">
                        <button
                            type="button"
                            onClick={onClose}
                            disabled={busy}
                            className="tr-button tr-button--secondary tr-button--md disabled:opacity-40"
                        >
                            Cancel
                        </button>
                        <button
                            type="submit"
                            disabled={!invite.trim() || busy}
                            className="tr-button tr-button--primary tr-button--md min-w-[120px] disabled:opacity-50 disabled:cursor-not-allowed"
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
