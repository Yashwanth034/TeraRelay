/** Verify the native mount response before showing a healthy Drive badge or
 * using its path with the host shell, clipboard, or Telegram metadata sync.
 * Tauri invoke<T>() does not validate IPC data at runtime.
 */
export interface NativeDriveMountStatus {
    supported: boolean;
    mounted: boolean;
    native_recovery: boolean;
    mount_point: string | null;
    provider_kind?: string;
}

export function validateNativeDriveMountStatus(value: unknown): NativeDriveMountStatus {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new TypeError('Invalid native Drive mount status: response is not an object');
    }
    const row = value as Record<string, unknown>;
    if (typeof row.supported !== 'boolean' || typeof row.mounted !== 'boolean'
        || typeof row.native_recovery !== 'boolean'
        || (row.mount_point !== null && typeof row.mount_point !== 'string')
        || (typeof row.mount_point === 'string' && row.mount_point.trim().length === 0)
        || (row.provider_kind !== undefined && typeof row.provider_kind !== 'string')
        || (row.mounted && (!row.supported
            || (typeof row.mount_point !== 'string'
                && !(row.mount_point === null && row.provider_kind === 'macos-file-provider'))))) {
        throw new TypeError('Invalid native Drive mount status: inconsistent mount fields');
    }
    return {
        supported: row.supported,
        mounted: row.mounted,
        native_recovery: row.native_recovery,
        mount_point: row.mount_point,
        provider_kind: row.provider_kind as string | undefined,
    };
}
