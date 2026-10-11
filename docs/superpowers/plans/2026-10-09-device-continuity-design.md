# TeraRelay Device Continuity Design

## Goal

After a user signs into the same Telegram account on another TeraRelay installation, TeraRelay should automatically reconcile the cloud-backed TeraRelay state already stored in Telegram and make the same Drive/logical-file/version-stack data available without copying login/session databases or re-uploading files.

## Existing behavior to reuse, not rebuild

- TeraRelay Drive metadata already syncs through Telegram Saved Messages with record-by-record conflict merge.
- TeraRelay Drive encryption metadata already syncs remotely; each device keeps its local unwrapped key separately and can unlock the same Drive with the existing passphrase.
- Logical multipart file manifests already live in Telegram and are resolved when their channel is read.
- File-stack/version manifests already live in Telegram and are resolved from the newest valid channel metadata document.
- Telegram dialogs/channels already come from the same Telegram account.
- The existing debug-only `VITE_TERA_QA_FEATURE_A=1` / `TERARELAY_QA_FEATURE_A=1` path is the login bypass for isolated QA.

## User-visible behavior

1. As soon as authentication succeeds, TeraRelay starts one initial continuity reconciliation.
2. The initial Drive metadata reconciliation happens before the Linux Drive mount is started, so a new device does not mount an empty Drive before remote metadata arrives.
3. Drive metadata continues syncing every 15 seconds on authenticated desktop and mobile clients. Linux mounting remains desktop-only.
4. If the local Drive was pristine and the initial sync pulls an existing remote Drive, TeraRelay reports that the cloud state was restored, including visible Drive folder/file counts.
5. If an existing encrypted Drive is detected but this device has no unlocked local key, TeraRelay shows an unlock dialog that asks only for the existing Drive passphrase. It must never create a new encryption key during restore.
6. The unlock dialog may be dismissed with `Later`; dismissing it leaves Drive locked and does not alter remote metadata.
7. If initial remote sync is temporarily unavailable, cached local data remains usable, the Linux mount still starts, and the existing periodic sync retries automatically. A pristine new device receives a non-destructive warning instead of silently looking empty.
8. Normal Telegram/TDLib authentication databases, local filesystem paths, transfer queues, caches, and mount state remain device-local and are not copied between computers.
9. Existing logical files and version stacks are not reimplemented or duplicated; they continue to materialize through their existing Telegram-backed manifests when their channels are loaded.

## QA strategy

- Keep the production auth path untouched.
- Use the existing debug-only Feature A login bypass for UI/manual QA.
- Use the isolated QA database to configure Drive encryption with a test-only passphrase, lock it, reload, verify the continuity unlock dialog appears without Telegram login, unlock it, and confirm normal dashboard access.
- Use focused source-contract tests for ordering (snapshot -> sync -> snapshot -> crypto -> mount), pristine/remote restore detection, mobile sync, and the unlock dialog contract.
- Run the full prebuild QA after focused tests.

## Protected areas

- Do not copy, print, inspect, delete, or migrate real Telegram/TDLib session contents.
- Do not add a second cloud backend or a second metadata format.
- Do not change version numbers.
- Do not stage, commit, push, tag, or release during implementation.
