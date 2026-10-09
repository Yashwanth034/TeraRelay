import type { TelegramFile } from '../../types';
import { getRichFileMeta } from '../../filePresentation';
import { useRichMediaMetadata } from '../../hooks/useRichMediaMetadata';

interface RichFileMetaTextProps {
    file: TelegramFile;
    folderId: number | null;
}

export function RichFileMetaText({ file, folderId }: RichFileMetaTextProps) {
    const { data } = useRichMediaMetadata(file.id, folderId, file.name, false);
    return <>{getRichFileMeta(file, data)}</>;
}
