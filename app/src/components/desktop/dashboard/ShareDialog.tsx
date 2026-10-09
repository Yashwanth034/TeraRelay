import { useState } from 'react';
import { X, Link, Copy, Check, Shield, Clock, AlertCircle, Share2 } from 'lucide-react';
import { TelegramFile, ShareInfo } from '../../../types';
import { invoke } from '@tauri-apps/api/core';
import { motion, AnimatePresence } from 'framer-motion';
import { nativeShareOrCopy } from '../../../utils';
import { useTranslation } from 'react-i18next';
import { useEscapeToClose } from '../../../hooks/useEscapeToClose';

interface ShareDialogProps {
    file: TelegramFile;
    folderId: number | null;
    onClose: () => void;
}

const SHOW_NETWORK_OVERRIDE_UI = false;

export function ShareDialog({ file, folderId, onClose }: ShareDialogProps) {
    const { t } = useTranslation();
    const [password, setPassword] = useState('');
    const [requirePassword, setRequirePassword] = useState(false);
    const [expiryType, setExpiryType] = useState<'never' | '1h' | '1d' | '7d' | 'custom'>('1d');
    const [customHours, setCustomHours] = useState('24');
    
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [shareInfo, setShareInfo] = useState<ShareInfo | null>(null);
    const [copied, setCopied] = useState(false);
    const [customDomain, setCustomDomain] = useState('');

    useEscapeToClose(true, onClose, loading);

    const handleGenerate = async () => {
        setLoading(true);
        setError(null);
        try {
            let expiryHours: number | null = null;
            if (expiryType === '1h') expiryHours = 1;
            else if (expiryType === '1d') expiryHours = 24;
            else if (expiryType === '7d') expiryHours = 168;
            else if (expiryType === 'custom') {
                const parsed = parseInt(customHours, 10);
                if (isNaN(parsed) || parsed <= 0) {
                    throw new Error('Please enter a valid number of hours');
                }
                expiryHours = parsed;
            }

            const pwdParam = requirePassword && password.trim() ? password : null;

            const res = await invoke<ShareInfo>('cmd_create_share', {
                folderId,
                messageId: file.id, // In TeraRelay, file.id is the message id
                fileName: file.name,
                fileSize: file.size,
                password: pwdParam,
                expiryHours,
            });

            setShareInfo(res);
        } catch (err: any) {
            setError(err.toString());
        } finally {
            setLoading(false);
        }
    };

    const getDisplayLink = () => {
        if (!shareInfo) return '';
        if (customDomain.trim()) {
            try {
                // Replace the host part (localhost:14201) with the custom domain
                const url = new URL(shareInfo.link);
                return `${url.protocol}//${customDomain.trim()}${url.pathname}`;
            } catch {
                return shareInfo.link;
            }
        }
        return shareInfo.link;
    };

    const handleCopy = () => {
        const link = getDisplayLink();
        if (link) {
            navigator.clipboard.writeText(link);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        }
    };

    // Native Android/iOS share sheet via Web Share API
    const handleNativeShare = () => {
        if (!shareInfo) return;
        nativeShareOrCopy(file.name, file.sizeStr, getDisplayLink(), () => {
            navigator.clipboard.writeText(getDisplayLink());
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
        });
    };

    return (
        <div className="tr-modal-backdrop fixed inset-0 z-[100] flex items-center justify-center p-4" onClick={onClose}>
            <div className="tr-modal w-full max-w-[440px] overflow-hidden flex flex-col animate-in fade-in zoom-in-95 duration-150" onClick={e => e.stopPropagation()}>
                <div className="tr-modal-header p-4 flex justify-between items-center">
                    <h3 className="text-telegram-text font-medium flex items-center gap-2">
                        <Link className="w-5 h-5 text-telegram-primary" />
                        {t('share.title')}
                    </h3>
                    <button onClick={onClose} className="tr-modal-close" aria-label="Close">
                        <X className="w-4 h-4" />
                    </button>
                </div>

                <div className="p-5 flex-1 overflow-y-auto space-y-4 max-h-[75vh]">
                    <div className="tr-modal-note">
                        <div className="text-xs text-telegram-subtext uppercase font-semibold tracking-wider mb-1">{t('share.sharing_file')}</div>
                        <div className="text-sm font-medium text-telegram-text truncate">{file.name}</div>
                        <div className="text-xs text-telegram-subtext mt-0.5">{file.sizeStr}</div>
                    </div>

                    {!shareInfo ? (
                        <>
                            {/* Security Option */}
                            <div className="space-y-2">
                                <div className="flex items-center justify-between py-1">
                                    <span className="text-sm font-medium text-telegram-text flex items-center gap-2 select-none">
                                        <Shield className="w-4 h-4 text-emerald-400" />
                                        {t('share.password_protection')}
                                    </span>
                                    <button
                                        type="button"
                                        onClick={() => setRequirePassword(!requirePassword)}
                                        className={`relative w-10 h-5.5 rounded-full transition-colors duration-200 shrink-0 ${
                                            requirePassword ? 'bg-telegram-primary' : 'bg-telegram-border'
                                        }`}
                                    >
                                        <span
                                            className={`absolute top-0.5 left-0.5 w-4.5 h-4.5 rounded-full bg-white shadow transition-transform duration-200 ${
                                                requirePassword ? 'translate-x-4.5' : 'translate-x-0'
                                            }`}
                                        />
                                    </button>
                                </div>
                                
                                <AnimatePresence>
                                    {requirePassword && (
                                        <motion.div
                                            initial={{ height: 0, opacity: 0, marginTop: 0 }}
                                            animate={{ height: 'auto', opacity: 1, marginTop: 8 }}
                                            exit={{ height: 0, opacity: 0, marginTop: 0 }}
                                            transition={{ duration: 0.2, ease: 'easeInOut' }}
                                            className="overflow-hidden"
                                        >
                                            <input
                                                type="password"
                                                placeholder={t('share.enter_password')}
                                                value={password}
                                                onChange={(e) => setPassword(e.target.value)}
                                                className="tr-modal-input w-full px-3 py-2 text-sm placeholder:text-telegram-subtext/60"
                                                autoFocus
                                            />
                                        </motion.div>
                                    )}
                                </AnimatePresence>
                            </div>

                            {/* Expiry Option */}
                            <div className="space-y-2">
                                <span className="text-sm font-medium text-telegram-text flex items-center gap-2">
                                    <Clock className="w-4 h-4 text-amber-400" />
                                    {t('share.expiration')}
                                </span>
                                <div className="grid grid-cols-3 gap-2">
                                    {(['1h', '1d', '7d'] as const).map((type) => (
                                        <button
                                            key={type}
                                            type="button"
                                            onClick={() => setExpiryType(type)}
                                            className={`tr-choice-chip ${expiryType === type ? 'tr-choice-chip--active' : ''}`}
                                        >
                                            {type === '1h' ? t('share.one_hour') : type === '1d' ? t('share.one_day') : t('share.seven_days')}
                                        </button>
                                    ))}
                                    <button
                                        type="button"
                                        onClick={() => setExpiryType('never')}
                                        className={`tr-choice-chip ${expiryType === 'never' ? 'tr-choice-chip--active' : ''}`}
                                    >
                                        {t('share.never')}
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => setExpiryType('custom')}
                                        className={`tr-choice-chip col-span-2 ${expiryType === 'custom' ? 'tr-choice-chip--active' : ''}`}
                                    >
                                        {t('share.custom_hours')}
                                    </button>
                                </div>

                                {expiryType === 'custom' && (
                                    <div className="flex gap-2 items-center mt-2 animate-in slide-in-from-top-1 duration-100">
                                        <input
                                            type="number"
                                            min="1"
                                            value={customHours}
                                            onChange={(e) => setCustomHours(e.target.value)}
                                            className="tr-modal-input w-24 px-3 py-2 text-sm"
                                        />
                                        <span className="text-xs text-telegram-subtext">{t('share.hours_from_now')}</span>
                                    </div>
                                )}
                            </div>

                            {error && (
                                <div className="tr-modal-note tr-modal-note--danger flex gap-2 items-start">
                                    <AlertCircle className="w-4 h-4 shrink-0 mt-0.5" />
                                    <span>{error}</span>
                                </div>
                            )}

                            <button
                                onClick={handleGenerate}
                                disabled={loading}
                                className="tr-button tr-button--primary tr-button--md w-full mt-4"
                            >
                                {loading ? (
                                    <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin"></div>
                                ) : t('share.generate_link')}
                            </button>
                        </>
                    ) : (
                        <div className="space-y-4 animate-in fade-in duration-200">
                            <div className="tr-modal-note tr-modal-note--success flex gap-2 items-center">
                                <Check className="w-4 h-4 shrink-0" />
                                <span>{t('share.link_created')}</span>
                            </div>

                            {/* Shareable Link Display */}
                            <div className="space-y-1.5">
                                <label className="text-xs font-semibold text-telegram-subtext">{t('files.share_link')}</label>
                                <div className="flex gap-2">
                                    <input
                                        type="text"
                                        readOnly
                                        value={getDisplayLink()}
                                        className="tr-modal-input flex-1 px-3 py-2 text-sm select-all"
                                    />
                                    <button
                                        onClick={handleCopy}
                                        className={`px-3 py-2 rounded-lg border flex items-center justify-center transition-all ${
                                            copied 
                                                ? 'bg-emerald-500 border-emerald-500 text-white' 
                                                : 'bg-telegram-hover border-telegram-border text-telegram-text hover:bg-white/10'
                                        }`}
                                    >
                                        {copied ? <Check className="w-4 h-4" /> : <Copy className="w-4 h-4" />}
                                    </button>
                                </div>
                            </div>

                            {/* Native Share Button (Android/iOS) */}
                            {typeof navigator !== 'undefined' && typeof navigator.share === 'function' && (
                                <button
                                    onClick={handleNativeShare}
                                    className="tr-button tr-button--secondary tr-button--md w-full text-telegram-primary"
                                >
                                    <Share2 className="w-4 h-4" />
                                    {t('share.share_via')}
                                </button>
                            )}

                            {SHOW_NETWORK_OVERRIDE_UI && (<>
                            {/* Tailscale / Network Share Customizer */}
                            <div className="tr-modal-note space-y-2">
                                <div className="text-xs font-semibold text-telegram-text flex items-center gap-1.5">
                                    <span>🌐</span> {t('share.share_externally')}
                                </div>
                                <p className="text-xs text-telegram-subtext leading-relaxed">
                                    {t('share.tailscale_help')}
                                </p>
                                <div className="flex gap-2 items-center">
                                    <input
                                        type="text"
                                        placeholder="e.g. 100.115.22.45 or tailscale-pc:14201"
                                        value={customDomain}
                                        onChange={(e) => setCustomDomain(e.target.value)}
                                        className="tr-modal-input flex-1 px-3 py-2 text-xs placeholder:text-telegram-subtext/40"
                                    />
                                </div>
                            </div>

                            </>)}

                            <button
                                onClick={onClose}
                                className="tr-button tr-button--secondary tr-button--md w-full"
                            >
                                {t('share.done')}
                            </button>
                        </div>
                    )}
                </div>
            </div>
        </div>
    );
}
