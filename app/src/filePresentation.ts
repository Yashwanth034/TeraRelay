import type { RichMediaMetadata, TelegramFile } from './types';
import { displayFileName } from './utils';

export function fileExtension(name: string) {
    const extension = name.split('.').pop()?.toLowerCase() || '';
    return extension.length <= 8 ? extension : '';
}

function humanizeFileLikeTitle(name: string) {
    const withoutExtension = displayFileName(name).replace(/\.[^.]+$/, '');
    const tokens = withoutExtension
        .replace(/[._]+/g, ' ')
        .replace(/\s+-\s+/g, ' ')
        .replace(/\s+/g, ' ')
        .trim()
        .split(' ');

    const technicalToken = /^(?:2160p|1080p|720p|480p|4k|uhd|hdr10\+?|hdr|dv|dolbyvision|bluray|blu-ray|brrip|bdrip|web-dl|webdl|webrip|remux|x26[45]|h26[45]|hevc|av1|aac|dts|truehd)$/i;
    const technicalIndex = tokens.findIndex(token =>
        technicalToken.test(token.replace(/[^a-z0-9+_-]/gi, '')),
    );
    const titleTokens = technicalIndex > 0 ? tokens.slice(0, technicalIndex) : tokens;

    return {
        title: titleTokens.join(' ') || withoutExtension,
        hasTechnicalSuffix: technicalIndex > 0,
    };
}

export function getPremiumFileTitle(file: TelegramFile) {
    const stackName = file.stack_name?.trim();
    if (stackName) {
        const cleaned = humanizeFileLikeTitle(stackName);
        // Auto-created stack names are physical filenames. Humanize those,
        // while preserving deliberate custom names exactly as the user wrote them.
        if (cleaned.hasTechnicalSuffix || stackName === file.name) return cleaned.title;
        return stackName;
    }

    return humanizeFileLikeTitle(file.name).title;
}

export function getPremiumFileMeta(file: TelegramFile) {
    if (file.stack_label?.trim()) return file.stack_label.trim();

    const name = file.name;
    const resolution = /(?:^|[._\-\s])(2160p|1080p|720p|480p|4k|uhd)(?:[._\-\s]|$)/i.exec(name)?.[1];
    const dynamicRange = /(?:^|[._\-\s])(hdr10\+?|hdr|dolby[._\-\s]?vision|dv)(?:[._\-\s]|$)/i.exec(name)?.[1];
    const source = /(?:^|[._\-\s])(bluray|blu-ray|brrip|bdrip|web-dl|webdl|webrip|remux)(?:[._\-\s]|$)/i.exec(name)?.[1];

    const labels: string[] = [];
    if (resolution) labels.push(/2160p|4k|uhd/i.test(resolution) ? '4K' : resolution.toUpperCase());
    if (dynamicRange) labels.push(/dolby|^dv$/i.test(dynamicRange) ? 'Dolby Vision' : dynamicRange.toUpperCase());
    if (!dynamicRange && source) {
        labels.push(
            source
                .replace(/^webdl$/i, 'WEB-DL')
                .replace(/^webrip$/i, 'WEBRip')
                .replace(/^bluray$/i, 'BluRay')
                .replace(/^brrip$/i, 'BRRip')
                .replace(/^bdrip$/i, 'BDRip'),
        );
    }

    if (labels.length) return labels.slice(0, 2).join(' · ');

    const extension = fileExtension(file.name);
    return extension ? extension.toUpperCase() : 'FILE';
}

export function formatCompactMediaDuration(seconds?: number | null) {
    if (!seconds || !Number.isFinite(seconds) || seconds <= 0) return null;
    const totalMinutes = Math.max(1, Math.round(seconds / 60));
    const hours = Math.floor(totalMinutes / 60);
    const minutes = totalMinutes % 60;
    return hours > 0 ? `${hours}h ${String(minutes).padStart(2, '0')}m` : `${minutes}m`;
}

export function formatVideoCodec(codec?: string | null) {
    switch (codec?.toLowerCase()) {
        case 'hevc':
        case 'h265':
            return 'HEVC';
        case 'h264':
        case 'avc':
            return 'H.264';
        case 'av1':
            return 'AV1';
        case 'vp9':
            return 'VP9';
        default:
            return codec?.toUpperCase() || null;
    }
}

export function getRichFileMeta(file: TelegramFile, metadata?: RichMediaMetadata | null) {
    if (!metadata) return getPremiumFileMeta(file);

    const labels = getPremiumFileMeta(file)
        .split(' · ')
        .map(label => label.trim())
        .filter(Boolean);
    const codec = formatVideoCodec(metadata.video_codec);
    const duration = formatCompactMediaDuration(metadata.duration_secs);

    if (metadata.dynamic_range && !labels.some(label => /HDR|DOLBY|HLG/i.test(label))) {
        labels.push(metadata.dynamic_range);
    }
    if (codec && !labels.some(label => label.toUpperCase() === codec.toUpperCase())) {
        labels.push(codec);
    }
    if (duration) labels.push(duration);

    return labels.slice(0, 4).join(' · ');
}
