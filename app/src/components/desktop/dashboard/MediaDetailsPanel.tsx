import {
    AudioLines,
    Captions,
    Clock3,
    File,
    Film,
    Folder,
    Gauge,
    HardDrive,
    Info,
    Maximize2,
    Package,
    X,
} from 'lucide-react';
import type { MediaTrackInfo, TelegramFile } from '../../../types';
import {
    fileExtension,
    formatCompactMediaDuration,
    formatVideoCodec,
    getPremiumFileMeta,
} from '../../../filePresentation';
import { formatBytes, isAudioFile, isImageFile, isMediaFile, isPdfFile, isVideoFile } from '../../../utils';
import { useRichMediaMetadata } from '../../../hooks/useRichMediaMetadata';
import { PremiumFileThumbnail } from '../../shared/PremiumFileThumbnail';

interface MediaDetailsPanelProps {
    file: TelegramFile;
    folderId: number | null;
    folderName: string;
    onClose: () => void;
    onPreview?: () => void;
}

interface DetailRowProps {
    icon: React.ReactNode;
    label: string;
    value: string;
}

function DetailRow({ icon, label, value }: DetailRowProps) {
    return (
        <div className="tr-media-details__row">
            <span className="tr-media-details__row-icon">{icon}</span>
            <span className="tr-media-details__row-label">{label}</span>
            <span className="tr-media-details__row-value" title={value}>{value}</span>
        </div>
    );
}

function fileTypeLabel(file: TelegramFile) {
    if (isVideoFile(file.name)) return 'Video';
    if (isAudioFile(file.name)) return 'Audio';
    if (isImageFile(file.name)) return 'Image';
    if (isPdfFile(file.name)) return 'PDF';
    const extension = fileExtension(file.name);
    return extension ? `${extension.toUpperCase()} file` : 'File';
}

function languageLabel(language?: string | null) {
    if (!language || language === 'und') return 'Unknown';
    const names: Record<string, string> = {
        eng: 'English',
        en: 'English',
        tel: 'Telugu',
        te: 'Telugu',
        hin: 'Hindi',
        hi: 'Hindi',
        tam: 'Tamil',
        ta: 'Tamil',
        spa: 'Spanish',
        es: 'Spanish',
        fre: 'French',
        fra: 'French',
        fr: 'French',
        ger: 'German',
        deu: 'German',
        de: 'German',
        jpn: 'Japanese',
        ja: 'Japanese',
    };
    return names[language.toLowerCase()] || language.toUpperCase();
}

function audioSummary(track: MediaTrackInfo | undefined) {
    if (!track) return 'None';
    const channel = track.channel_layout
        || (track.channels ? `${track.channels} channels` : null);
    return [languageLabel(track.language), channel].filter(Boolean).join(' · ');
}

function inferredDynamicRange(file: TelegramFile) {
    const match = /(?:^|[._\-\s])(hdr10\+?|hdr|dolby[._\-\s]?vision|dv)(?:[._\-\s]|$)/i.exec(file.name)?.[1];
    if (!match) return null;
    if (/dolby|^dv$/i.test(match)) return 'Dolby Vision';
    return match.toUpperCase();
}

export function MediaDetailsPanel({
    file,
    folderId,
    folderName,
    onClose,
    onPreview,
}: MediaDetailsPanelProps) {
    const shouldProbe = isMediaFile(file.name);
    const { data: metadata, isLoading, error } = useRichMediaMetadata(
        file.id,
        folderId,
        file.name,
        shouldProbe,
    );

    const resolution = metadata?.width && metadata?.height
        ? `${metadata.width} × ${metadata.height}`
        : null;
    const dynamicRange = metadata?.dynamic_range || inferredDynamicRange(file);
    const codec = formatVideoCodec(metadata?.video_codec);
    const container = metadata?.container || fileExtension(file.name).toUpperCase() || null;
    const duration = formatCompactMediaDuration(metadata?.duration_secs);
    const firstAudio = metadata?.audio_tracks[0];
    const subtitleCount = metadata?.subtitle_tracks.length ?? 0;

    return (
        <aside className="tr-media-details" aria-label={`File details for ${file.name}`}>
            <div className="tr-media-details__head">
                <div className="min-w-0">
                    <div className="tr-media-details__eyebrow">File details</div>
                    <h2 className="tr-media-details__title" title={file.name}>{file.name}</h2>
                </div>
                <button
                    type="button"
                    className="tr-media-details__close"
                    onClick={onClose}
                    aria-label="Close file details"
                    title="Close"
                >
                    <X />
                </button>
            </div>

            <div className="tr-media-details__preview">
                <PremiumFileThumbnail
                    file={file}
                    folderId={folderId}
                    variant="grid"
                    className="tr-media-details__thumbnail"
                    onOpen={onPreview}
                />
                <div className="min-w-0">
                    <div className="tr-media-details__identity">{fileTypeLabel(file)}</div>
                    <div className="tr-media-details__summary">{getPremiumFileMeta(file)}</div>
                </div>
            </div>

            <section className="tr-media-details__section">
                <div className="tr-media-details__section-title">
                    <Info />
                    File information
                </div>
                <DetailRow icon={<File />} label="Type" value={fileTypeLabel(file)} />
                {resolution && <DetailRow icon={<Maximize2 />} label="Resolution" value={resolution} />}
                {dynamicRange && <DetailRow icon={<Gauge />} label="Dynamic range" value={dynamicRange} />}
                {codec && <DetailRow icon={<Film />} label="Codec" value={codec} />}
                {container && <DetailRow icon={<Package />} label="Container" value={container} />}
                {duration && <DetailRow icon={<Clock3 />} label="Duration" value={duration} />}
                <DetailRow icon={<HardDrive />} label="Size" value={file.sizeStr || formatBytes(file.size)} />
            </section>

            {shouldProbe && (
                <section className="tr-media-details__section">
                    <div className="tr-media-details__section-title">
                        <Film />
                        Media details
                    </div>
                    {isLoading ? (
                        <div className="tr-media-details__status">Reading media metadata…</div>
                    ) : error ? (
                        <div className="tr-media-details__status">
                            Technical track details aren’t available for this file.
                        </div>
                    ) : (
                        <>
                            <DetailRow
                                icon={<AudioLines />}
                                label="Audio"
                                value={audioSummary(firstAudio)}
                            />
                            <DetailRow
                                icon={<Captions />}
                                label="Subtitles"
                                value={subtitleCount === 1 ? '1 track' : `${subtitleCount} tracks`}
                            />
                        </>
                    )}
                </section>
            )}

            <section className="tr-media-details__section">
                <div className="tr-media-details__section-title">
                    <Folder />
                    Location
                </div>
                <DetailRow icon={<Folder />} label="Channel" value={folderName} />
            </section>
        </aside>
    );
}
