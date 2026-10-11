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
