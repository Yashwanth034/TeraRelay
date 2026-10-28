export interface DriveSafeMountStatus {
  read_only: boolean;
  mounted: boolean;
  in_flight_uploads_may_finish: true;
  scope: 'linux_fuse_mount_only';
}

export function parseDriveSafeMountStatus(value: unknown): DriveSafeMountStatus {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('Invalid Drive Safe Mode response.');
  }
  const result = value as Record<string, unknown>;
  if (typeof result.read_only !== 'boolean' || typeof result.mounted !== 'boolean'
      || result.in_flight_uploads_may_finish !== true
      || result.scope !== 'linux_fuse_mount_only') {
    throw new Error('Invalid Drive Safe Mode status. The native state is uncertain.');
  }
  return {
    read_only: result.read_only,
    mounted: result.mounted,
    in_flight_uploads_may_finish: true,
    scope: 'linux_fuse_mount_only',
  };
}
