export interface DriveSnapshotLike {
  revision: number;
  dirty: boolean;
  directories: unknown[];
  files: unknown[];
  pending: unknown[];
}

export interface DriveSyncResultLike {
  revision: number;
  dirty: boolean;
  source: string;
}

export interface DriveCryptoStatusLike {
  configured: boolean;
  unlocked: boolean;
  version?: number | null;
}

/** Native IPC is a trust boundary even when invoke<T>() advertises types.
 * Reject malformed metadata instead of treating an invalid snapshot as a new
 * pristine Drive or reporting restored cloud data from invented fields.
 */
export function validateDriveSnapshot(value: unknown): DriveSnapshotLike {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('Native Drive snapshot is not an object');
  }
  const row = value as Record<string, unknown>;
  if (!Number.isSafeInteger(row.revision) || (row.revision as number) < 0
      || typeof row.dirty !== 'boolean'
      || !Array.isArray(row.directories) || !Array.isArray(row.files)
      || !Array.isArray(row.pending)) {
    throw new TypeError('Native Drive snapshot has invalid revision, dirty state or item arrays');
  }
  return {
    revision: row.revision as number,
    dirty: row.dirty,
    directories: row.directories,
    files: row.files,
    pending: row.pending,
  };
}

export function validateDriveSyncResult(value: unknown): DriveSyncResultLike {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('Native Drive metadata sync did not return an outcome');
  }
  const row = value as Record<string, unknown>;
  const sources = ['current', 'remote', 'remote-merged', 'merged-published', 'qa-local'];
  if (!Number.isSafeInteger(row.revision) || (row.revision as number) < 0
      || typeof row.dirty !== 'boolean'
      || typeof row.source !== 'string' || !sources.includes(row.source)) {
    throw new TypeError('Native Drive metadata sync returned an unverified result');
  }
  return { revision: row.revision as number, dirty: row.dirty, source: row.source };
}

export function validateDriveCryptoStatus(value: unknown): DriveCryptoStatusLike {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new TypeError('Native Drive encryption status is not an object');
  }
  const row = value as Record<string, unknown>;
  if (typeof row.configured !== 'boolean' || typeof row.unlocked !== 'boolean'
      || (row.unlocked && !row.configured)
      || (row.version !== undefined && row.version !== null
          && (!Number.isSafeInteger(row.version) || (row.version as number) < 0))) {
    throw new TypeError('Native Drive encryption status is invalid');
  }
  return {
    configured: row.configured,
    unlocked: row.unlocked,
    version: row.version as number | null | undefined,
  };
}

export interface DeviceContinuityAssessment {
  restoredFromRemote: boolean;
  directoryCount: number;
  fileCount: number;
  needsDriveUnlock: boolean;
  syncSource: string;
}

export function isPristineDriveSnapshot(snapshot: DriveSnapshotLike): boolean {
  return snapshot.revision === 0
    && !snapshot.dirty
    && snapshot.directories.length === 0
    && snapshot.files.length === 0
    && snapshot.pending.length === 0;
}

export function isRemoteDriveSyncSource(source: string): boolean {
  return source === 'remote' || source === 'remote-merged';
}

export function classifyDeviceContinuity(
  before: DriveSnapshotLike,
  sync: DriveSyncResultLike,
  after: DriveSnapshotLike,
  crypto: DriveCryptoStatusLike,
): DeviceContinuityAssessment {
  const remoteStateExists = after.revision > 0
    || after.directories.length > 0
    || after.files.length > 0
    || crypto.configured;

  return {
    restoredFromRemote:
      isPristineDriveSnapshot(before)
      && isRemoteDriveSyncSource(sync.source)
      && remoteStateExists,
    directoryCount: after.directories.length,
    fileCount: after.files.length,
    needsDriveUnlock: crypto.configured && !crypto.unlocked,
    syncSource: sync.source,
  };
}
