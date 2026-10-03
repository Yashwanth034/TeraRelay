# TeraRelay transfer performance

Transfer performance depends on the Telegram account, Telegram data-center path, network, storage, and platform.

## Verified Linux baseline

The current Linux x86_64 release path has been validated with multi-gigabyte logical files.

Observed real transfer results during release validation included:

- large uploads around 18-20 MB/s on the tested connection
- native TDLib downloads around 16-17 MB/s on the tested non-Premium account
- exact large-file reconstruction with byte-for-byte and SHA-256 verification
- upload resume and one-part-ahead preparation for split files
- download recovery for stale TDLib local-cache state

These measurements are test results, not guaranteed speeds.

## Cross-platform transfer path

Linux x86_64 currently has the additional TDLib/C++ fast-transfer path used for the verified benchmark above. Windows, macOS, Android, and iOS use TeraRelay's Grammers/MTProto fallback path. The fallback preserves the same logical split-file manifests, resume metadata, chunk ordering, and reconstructed-file validation, but performance should not be assumed to match the Linux TDLib benchmark until each platform is measured independently.

The Linux TDLib helper is optional platform-specific acceleration; Python and the pinned Ubuntu TDLib runtime are not requirements for normal transfers on the other platforms.

## Benchmarking rules

For meaningful comparisons, use the same file, account class, network, storage device, and Telegram region/path. Record:

- elapsed time
- acknowledged upload/download bytes
- average throughput
- interruption/resume behavior
- Telegram flood/rate-limit responses
- reconstructed size and hash

Do not compare smoothed UI numbers or different network/account conditions as if they were equivalent.

TeraRelay uses Telegram's MTProto/TDLib data path for Telegram transfers; ordinary HTTP download accelerators are not used for the core Telegram storage path.
