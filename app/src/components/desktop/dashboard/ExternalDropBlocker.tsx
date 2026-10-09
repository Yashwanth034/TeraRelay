import { useState, useEffect, useRef } from 'react';
import { motion, AnimatePresence } from 'framer-motion';
import { Upload, CheckCircle2 } from 'lucide-react';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { DragDropOverlay } from './DragDropOverlay';

/**
 * ExternalDropBlocker - Intercepts external file drops and triggers uploads directly.
 * 
 * With Tauri's dragDropEnabled: false, we handle DOM drag events ourselves.
 * On drop, file paths are extracted from File objects (Tauri webviews expose .path)
 * and passed to the onFilesDropped callback for direct upload queueing.
 * 
 * Falls back to showing the Upload dialog prompt only if file paths cannot be extracted.
 */
export function ExternalDropBlocker({ onFilesDropped, onUploadClick, disabled = false }: { onFilesDropped?: (paths: string[]) => void; onUploadClick?: () => void; disabled?: boolean }) {
    const [isDragging, setIsDragging] = useState(false);
    const [droppedCount, setDroppedCount] = useState<number | null>(null);
    const [showFallback, setShowFallback] = useState(false);
    
    // Use refs for values accessed inside stable event listeners
    const onFilesDroppedRef = useRef(onFilesDropped);
    const disabledRef = useRef(disabled);
    onFilesDroppedRef.current = onFilesDropped;
    disabledRef.current = disabled;

    // Native Tauri drag/drop supplies real absolute filesystem paths.
    // Linux enables this at the webview level in tauri.linux.conf.json; platforms
    // that keep native drag/drop disabled continue to use the DOM fallback below.
    useEffect(() => {
        let unlisten: (() => void) | undefined;
        let messageTimeout: ReturnType<typeof setTimeout>;

        (async () => {
            try {
                unlisten = await getCurrentWebview().onDragDropEvent((event) => {
                    if (disabledRef.current) return;

                    if (event.payload.type === 'enter' || event.payload.type === 'over') {
                        setIsDragging(true);
                        return;
                    }

                    if (event.payload.type === 'leave') {
                        setIsDragging(false);
                        return;
                    }

                    if (event.payload.type === 'drop') {
                        setIsDragging(false);
                        const paths = Array.from(new Set(
                            event.payload.paths.filter(path => typeof path === 'string' && path.length > 0),
                        ));

                        if (paths.length > 0) {
                            onFilesDroppedRef.current?.(paths);
                            clearTimeout(messageTimeout);
                            setDroppedCount(paths.length);
                            messageTimeout = setTimeout(() => setDroppedCount(null), 2000);
                        }
                    }
                });
            } catch (error) {
                // Native drag/drop is intentionally disabled on some platforms;
                // the DOM fallback below remains available there.
                void error;
            }
        })();

        return () => {
            unlisten?.();
            clearTimeout(messageTimeout);
        };
    }, []);

    useEffect(() => {
        let dragEnterCount = 0;
        let hideTimeout: ReturnType<typeof setTimeout>;
        let messageTimeout: ReturnType<typeof setTimeout>;

        const handleDragEnter = (e: DragEvent) => {
            if (disabledRef.current) return;
            if (e.dataTransfer?.types.includes('Files')) {
                e.preventDefault();
                e.stopPropagation();
                dragEnterCount++;
                setIsDragging(true);
                clearTimeout(hideTimeout);
            }
        };

        const handleDragOver = (e: DragEvent) => {
            if (disabledRef.current) return;
            if (e.dataTransfer?.types.includes('Files')) {
                e.preventDefault();
                e.stopPropagation();
                e.dataTransfer.dropEffect = 'copy';
                clearTimeout(hideTimeout);
            }
        };

        const handleDragLeave = (e: DragEvent) => {
            if (e.dataTransfer?.types.includes('Files')) {
                dragEnterCount--;
                // Only hide when truly leaving the window
                if (dragEnterCount <= 0 &&
                    (e.clientX <= 0 || e.clientY <= 0 ||
                     e.clientX >= window.innerWidth || e.clientY >= window.innerHeight)) {
                    dragEnterCount = 0;
                    hideTimeout = setTimeout(() => {
                        setIsDragging(false);
                    }, 150);
                }
            }
        };

        const handleDrop = (e: DragEvent) => {
            if (disabledRef.current) return;
            if (!e.dataTransfer?.types.includes('Files')) return;

            e.preventDefault();
            e.stopPropagation();
            dragEnterCount = 0;
            setIsDragging(false);
            clearTimeout(hideTimeout);
            clearTimeout(messageTimeout);

            const files = e.dataTransfer.files;
            const paths: string[] = [];

            for (let i = 0; i < files.length; i++) {
                // In Tauri webviews, File objects expose a non-standard .path property
                const path = (files[i] as any).path as string | undefined;
                if (path && typeof path === 'string' && path.length > 0) {
                    paths.push(path);
                }
            }

            if (paths.length > 0 && onFilesDroppedRef.current) {
                onFilesDroppedRef.current(paths);
                setDroppedCount(paths.length);
                messageTimeout = setTimeout(() => setDroppedCount(null), 2000);
            } else {
                // Fallback: file paths not available (e.g., non-Tauri browser during dev)
                setShowFallback(true);
                messageTimeout = setTimeout(() => setShowFallback(false), 4000);
            }
        };

        // Capture phase ensures we intercept before the webview's default handler
        document.addEventListener('dragenter', handleDragEnter, true);
        document.addEventListener('dragover', handleDragOver, true);
        document.addEventListener('dragleave', handleDragLeave, true);
        document.addEventListener('drop', handleDrop, true);

        return () => {
            document.removeEventListener('dragenter', handleDragEnter, true);
            document.removeEventListener('dragover', handleDragOver, true);
            document.removeEventListener('dragleave', handleDragLeave, true);
            document.removeEventListener('drop', handleDrop, true);
            clearTimeout(hideTimeout);
            clearTimeout(messageTimeout);
        };
    }, []);

    return (
        <>
            {/* Drag overlay - shown while files are being dragged over the window */}
            <AnimatePresence>
                {isDragging && <DragDropOverlay />}
            </AnimatePresence>

            {/* Brief success confirmation after drop */}
            <AnimatePresence>
                {droppedCount !== null && (
                    <motion.div
                        initial={{ opacity: 0, y: 20 }}
                        animate={{ opacity: 1, y: 0 }}
                        exit={{ opacity: 0, y: 20 }}
                        className="fixed bottom-20 right-4 z-[110] pointer-events-none"
                    >
                        <div className="tr-toast-surface px-3.5 py-3 flex items-center gap-3">
                            <CheckCircle2 className="w-5 h-5 text-green-400 flex-shrink-0" />
                            <span className="text-sm text-telegram-text">
                                Queued {droppedCount} file{droppedCount !== 1 ? 's' : ''} for upload
                            </span>
                        </div>
                    </motion.div>
                )}
            </AnimatePresence>

            {/* Fallback message when file paths cannot be extracted */}
            <AnimatePresence>
                {showFallback && (
                    <motion.div
                        initial={{ opacity: 0 }}
                        animate={{ opacity: 1 }}
                        exit={{ opacity: 0 }}
                        className="tr-modal-backdrop fixed inset-0 z-50 flex items-center justify-center pointer-events-none p-4"
                    >
                        <div className="tr-modal w-full max-w-[420px] p-6 pointer-events-auto">
                            <div className="flex flex-col items-center text-center gap-4">
                                <div className="tr-modal-icon !h-12 !w-12 !rounded-2xl flex items-center justify-center">
                                    <Upload className="w-5 h-5 text-telegram-primary" />
                                </div>
                                <div>
                                    <h3 className="text-lg font-semibold text-telegram-text mb-2">
                                        Drag-and-drop not available
                                    </h3>
                                    <p className="text-telegram-subtext text-sm">
                                        File paths could not be read from the drag event.
                                        <br />
                                        Use the button below or the <strong>Upload File</strong> button in the toolbar.
                                    </p>
                                </div>
                                <div className="flex gap-3">
                                    <button
                                        onClick={() => setShowFallback(false)}
                                        className="tr-button tr-button--secondary tr-button--sm mt-2"
                                    >
                                        Dismiss
                                    </button>
                                    <button
                                        onClick={() => {
                                            setShowFallback(false);
                                            onUploadClick?.();
                                        }}
                                        className="tr-button tr-button--primary tr-button--sm mt-2"
                                    >
                                        Open Upload Dialog
                                    </button>
                                </div>
                            </div>
                        </div>
                    </motion.div>
                )}
            </AnimatePresence>
        </>
    );
}
