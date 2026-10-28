import { useQuery } from '@tanstack/react-query';
import { invoke } from '@tauri-apps/api/core';
import { BandwidthStats } from '../../../types';
import { formatBytes } from '../../../utils';

// Keep the frequent bandwidth poll local to this tiny footer widget.
// Polling must not force the entire file list, sidebar, and thumbnails to re-render.
export function BandwidthWidget({ enabled }: { enabled: boolean }) {
    const { data: bandwidth } = useQuery({
        queryKey: ['bandwidth'],
        queryFn: () => invoke<BandwidthStats>('cmd_get_bandwidth'),
        refetchInterval: 5000,
        enabled,
    });
    if (!bandwidth) return null;

    const totalBytes = bandwidth.up_bytes + bandwidth.down_bytes;

    return (
        <div className="mt-3 text-xs text-telegram-subtext space-y-1">
            <div className="flex justify-between">
                <span>Used Today:</span>
                <span>{formatBytes(totalBytes)}</span>
            </div>
        </div>
    );
}
