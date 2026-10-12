import { useEffect, useMemo, useRef, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { invoke } from '@tauri-apps/api/core';
import { FileText, Play } from 'lucide-react';
import { TelegramFile } from '../../types';
import { FileTypeIcon } from './FileTypeIcon';

const IMAGE_EXTS = new Set(['jpg', 'jpeg', 'png', 'gif', 'webp', 'bmp', 'avif']);
const VIDEO_EXTS = new Set(['mp4', 'mkv', 'webm', 'mov', 'm4v', 'avi', 'mpeg', 'mpg']);

function extensionOf(name: string) {
    const ext = name.split('.').pop()?.toLowerCase() || '';
    return ext.length <= 8 ? ext : '';
}

export function isVisualMediaFile(name: string) {
    const ext = extensionOf(name);
    return IMAGE_EXTS.has(ext) || VIDEO_EXTS.has(ext) || ext === 'pdf';
}

interface PremiumFileThumbnailProps {
    file: TelegramFile;
    folderId: number | null;
    onOpen?: () => void;
    variant?: 'row' | 'grid';
    className?: string;
}

export function PremiumFileThumbnail({
    file,
    folderId,
    onOpen,
    variant = 'row',
    className,
}: PremiumFileThumbnailProps) {
    const ext = useMemo(() => extensionOf(file.name), [file.name]);
    const isVideo = VIDEO_EXTS.has(ext);
    const isImage = IMAGE_EXTS.has(ext);
    const isPdf = ext === 'pdf';
    const shouldFetch = isVideo || isImage || isPdf;
    const thumbnailRef = useRef<HTMLButtonElement>(null);
    const [nearViewport, setNearViewport] = useState(false);

    useEffect(() => {
        setNearViewport(false);
        const element = thumbnailRef.current;
        if (!shouldFetch || !element) return;
        if (typeof IntersectionObserver === 'undefined') {
            setNearViewport(true);
            return;
        }
        const observer = new IntersectionObserver(([entry]) => {
            setNearViewport(Boolean(entry?.isIntersecting));
        }, { rootMargin: '180px' });
        observer.observe(element);
        return () => observer.disconnect();
    }, [file.id, folderId, shouldFetch]);

    const { data: thumbnail, isFetching: loading } = useQuery({
        queryKey: ['file-thumbnail', folderId, file.id],
        queryFn: async () => {
            try {
                return await invoke<string>('cmd_get_thumbnail', {
                    messageId: file.id,
                    folderId,
                }) || null;
            } catch {
                // Optional media artwork must never block browsing.
                return null;
            }
        },
        enabled: shouldFetch && nearViewport,
        staleTime: 30 * 60 * 1000,
        gcTime: 5 * 60 * 1000,
        refetchOnWindowFocus: false,
        retry: false,
    });

    return (
        <button
            ref={thumbnailRef}
            type="button"
            className={[
                'tr-media-thumb',
                variant === 'grid' ? 'tr-media-thumb--grid' : '',
                thumbnail ? 'tr-media-thumb--image' : '',
                isVideo ? 'tr-media-thumb--video' : '',
                isPdf ? 'tr-media-thumb--pdf' : '',
                className || '',
            ].join(' ')}
            onClick={(event) => {
                event.stopPropagation();
                onOpen?.();
            }}
            title="Open file"
            aria-label={'Open ' + file.name}
        >
            {thumbnail ? (
                <img src={thumbnail} alt="" loading="lazy" decoding="async" className="tr-media-thumb__image" />
            ) : loading ? (
                <span className="tr-media-thumb__skeleton" aria-hidden="true" />
            ) : (
                <span className="tr-media-thumb__fallback" aria-hidden="true">
                    <FileTypeIcon filename={file.name} className="h-5 w-5" />
                </span>
            )}

            <span className="tr-media-thumb__shade" aria-hidden="true" />

            {isVideo && (
                <span className="tr-media-thumb__play" aria-hidden="true">
                    <Play className="h-3 w-3 fill-current" />
                </span>
            )}

            {isPdf && !thumbnail && (
                <span className="tr-media-thumb__pdf" aria-hidden="true">
                    <FileText className="h-4 w-4" />
                    PDF
                </span>
            )}

            {ext && !isPdf && (
                <span className="tr-media-thumb__type" aria-hidden="true">
                    {ext.toUpperCase()}
                </span>
            )}
        </button>
    );
}
