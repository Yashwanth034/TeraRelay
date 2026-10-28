import { validateDriveSyncResult } from './deviceContinuity';

// A tiny renderer-local notification bridge. It carries no file names,
// credentials, account details or remote data; it never starts a sync.
// Multiple frontend components use the same native metadata backend but may
// otherwise leave an obsolete "unsynchronized" warning on screen.
const verifiedSyncListeners = new Set<() => void>();

export function subscribeToVerifiedDriveSync(listener: () => void): () => void {
  verifiedSyncListeners.add(listener);
  return () => { verifiedSyncListeners.delete(listener); };
}

/** Must only publish after the native IPC result passes strict validation.
 * Resolved promises, QA-local, dirty and malformed states do NOT mean that
 * remote Telegram metadata was verified/synchronized.
 */
export function publishVerifiedDriveSync(value: unknown): boolean {
  const result = validateDriveSyncResult(value);
  if (result.dirty || result.source === 'qa-local') return false;
  for (const listener of [...verifiedSyncListeners]) {
    try {
      listener();
    } catch {
      // One unmounted or broken UI listener must not misreport a successfully
      // validated native metadata sync as an operation failure.
    }
  }
  return true;
}
