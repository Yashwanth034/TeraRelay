import { motion } from 'framer-motion';
import { UploadCloud } from 'lucide-react';

export function DragDropOverlay() {
    return (
        <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            className="tr-modal-backdrop fixed inset-0 z-50 flex items-center justify-center pointer-events-none p-4"
        >
            <motion.div
                initial={{ scale: 0.9, opacity: 0 }}
                animate={{ scale: 1, opacity: 1 }}
                exit={{ scale: 0.9, opacity: 0 }}
                className="tr-drop-surface flex flex-col items-center gap-4"
            >
                <div className="tr-empty-state__icon !h-14 !w-14 !rounded-2xl">
                    <UploadCloud className="w-6 h-6 text-telegram-primary" />
                </div>
                <div className="text-center">
                    <h3 className="text-base font-semibold tracking-tight text-telegram-text">Drop files to upload</h3>
                    <p className="text-telegram-subtext text-[11px] mt-1">Release anywhere to add them to the current folder.</p>
                </div>
            </motion.div>
        </motion.div>
    );
}
