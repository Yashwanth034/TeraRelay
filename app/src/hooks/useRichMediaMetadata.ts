import { useQuery } from '@tanstack/react-query';
import { invoke } from '@tauri-apps/api/core';
import type { RichMediaMetadata } from '../types';
import { isMediaFile } from '../utils';

const RICH_METADATA_STALE_TIME = 60 * 60 * 1000;

export function richMediaMetadataQueryKey(
    folderId: number | null,
    messageId: number,
    fileName: string,
) {
    return ['rich-media-metadata', folderId, messageId, fileName] as const;
}

export function useRichMediaMetadata(
    messageId: number,
    folderId: number | null,
    fileName: string,
    enabled = true,
) {
    const eligible = messageId > 0 && isMediaFile(fileName);

    return useQuery({
        queryKey: richMediaMetadataQueryKey(folderId, messageId, fileName),
        queryFn: () => invoke<RichMediaMetadata>('cmd_get_rich_media_metadata', {
            messageId,
            folderId,
            fileName,
        }),
        enabled: eligible && enabled,
        staleTime: RICH_METADATA_STALE_TIME,
        retry: 1,
    });
}
