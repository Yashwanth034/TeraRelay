# TeraRelay Remote Import Design

## Goal

Add a complete Remote Import subsystem that lets a user paste a supported source into TeraRelay, choose one or more available media qualities when applicable, and import the resulting file(s) directly into the currently open writable TeraRelay channel without manually downloading anything first.

The user-facing contract is:

> Open destination channel → Upload from Link → paste source → choose quality/file selection when needed → Import → TeraRelay handles acquisition, processing, upload, version stacking, recovery, and cleanup.

## Product Constraints

1. Destination is always the currently open writable TeraRelay channel. The import UI must not show a channel/folder dropdown.
2. If no writable channel is open, Remote Import is unavailable rather than asking the user to choose a destination in the modal.
3. One selected video quality uploads as one normal TeraRelay file.
4. Two or more selected video qualities upload as members of one existing TeraRelay Version Stack automatically. The highest selected quality becomes primary.
5. The quality list itself is multi-select. There is no separate "single/multiple/version stack" option.
6. Normal default selection is a recommended quality, preferring 1080p when a suitable 1080p format exists. The user can change it before import.
7. Remote Import is not a separate download-manager product. Acquisition is an internal stage whose result enters the existing TeraRelay upload system.
8. Imported files must use the normal TeraRelay logical-file path, including multipart files, hashing, duplicate detection, recovery journal, Boost/TDLib transport choice, manifests, and Version Stacks.
9. The old Remote Upload 2 GB ceiling must not apply to the new subsystem. Large imported files use the existing logical multipart upload path.
10. Jobs must survive an application restart whenever the source engine supports resumable acquisition and the staged file still exists.
11. Temporary artifacts are removed only after the final TeraRelay upload/stack operation is durably complete or after explicit cancellation when safe.
12. Do not touch Device Continuity code, TDLib session/profile data, unrelated sharing features, Drive metadata, or unrelated UI.

## Engine Stack

Remote Import uses specialized acquisition/processing engines behind one TeraRelay interface:

- `yt-dlp`: primary media-site extractor for YouTube, Instagram, X/Twitter, TikTok, Facebook, Reddit, Vimeo, Twitch, and other supported sites.
- `yt-dlp-ejs` + Deno runtime: current YouTube JavaScript challenge support.
- `FFmpeg` + `ffprobe`: merge separate audio/video streams, remux, inspect, and post-process HLS/DASH outputs.
- `aria2c`: segmented/resumable HTTP(S) acquisition for direct downloads and as a yt-dlp external downloader where useful.
- `librqbit`: native Rust BitTorrent/magnet engine with DHT/PEX, resume, selective files, and rate limiting.
- `gallery-dl`: fallback/specialist for image galleries/carousels and sources that are better represented as collections than single videos.
- existing Rust `reqwest` downloader: reliability fallback for ordinary direct HTTP(S) files.

The UI never exposes engine names as normal workflow choices. Engine identity is diagnostic information only.

## Source Routing

A new source router classifies the pasted input before acquisition:

1. `magnet:` → librqbit.
2. Local/selected `.torrent` file → librqbit.
3. Direct downloadable HTTP(S) resource → aria2c, with existing Rust downloader as fallback.
4. Known/unknown web media page → yt-dlp analysis.
5. Gallery/carousel source that yt-dlp cannot represent correctly → gallery-dl fallback when applicable.
6. Unsupported/DRM-only source → clear unsupported-source error; do not attempt DRM circumvention.

Fallbacks are explicit and bounded. TeraRelay must not recursively try random engines until something produces bytes.

## Analysis Before Import

Pasting a web-media source starts an analysis stage, not a download.

For video sources, analysis returns normalized metadata suitable for UI and execution:

- title
- source/site label
- duration when known
- thumbnail URL/path when safely available
- list of importable quality choices
- approximate size when known
- video codec/container hints when needed for execution
- whether audio and video must be merged
- stable extractor format identifiers required to reproduce the user's selection

Quality choices are normalized into user-facing labels such as:

- 480p
- 720p
- 1080p
- 1080p60
- 1440p
- 4K
- 4K60
- 4K HDR
- 8K

The UI preserves the exact engine format IDs internally rather than trying to reconstruct a yt-dlp format expression from only the display label.

## Quality Selection and Version Stacks

The Remote Import modal uses checkboxes/multi-select directly in the quality list.

Example:

```text
Choose quality

□ 480p        ~38 MB
□ 720p        ~74 MB
☑ 1080p       ~142 MB     Recommended
□ 1440p       ~280 MB
□ 4K          ~620 MB
□ 8K          ~1.8 GB

[ Import ]
```

Rules:

- exactly one selected quality → normal file upload;
- two or more selected qualities → one Version Stack;
- highest selected quality is primary after every member succeeds;
- each stack member receives a quality label derived from the normalized selection (for example `1080p`, `4K HDR`);
- stack creation is automatic and never asks the user a second confirmation;
- if one member fails, successful members remain valid normal files and the job reports partial failure rather than deleting good uploads;
- stack metadata is created only after enough members have uploaded successfully to form a meaningful stack;
- audio-only outputs remain separate files rather than becoming video-quality stack members.

The existing Version Stack implementation remains the source of truth. Remote Import only orchestrates the existing stack commands after upload results return stable logical file IDs.

## Destination UX

Remote Import is launched from inside a channel.

The existing modal's destination dropdown is removed. The modal receives the active channel ID as immutable destination context. The user should not be able to change channels behind the modal and accidentally redirect the job; the destination is captured when the import is queued.

If the active location is root/Personal Vault or a read-only joined channel, the Remote Import action is disabled or gives a concise "Open a writable TeraRelay channel first" message.

## Job Model

Remote Import receives a durable job model separate from the ordinary upload queue while acquisition/processing is still in progress. Once a staged output is ready, it enters the existing upload queue through a defined handoff.

A job records at minimum:

- job ID
- original source
- source kind/engine
- destination channel ID captured at creation
- analysis metadata needed to display/recover the job
- selected item/quality IDs
- per-output state
- staging paths owned by the job
- acquisition resume metadata/reference when applicable
- processing state
- upload queue IDs / resulting logical file IDs
- desired Version Stack relationship and labels
- timestamps and last error

Normalized phases:

- analyzing
- waiting-for-selection
- downloading
- processing
- ready-to-upload
- uploading
- finalizing
- success
- partial-error
- error
- cancelled

Frontend progress should present these phases as one import operation, not unrelated downloader and uploader cards.

## Acquisition and Staging

Each selected output is acquired into a TeraRelay-owned temporary/staging location with collision-free job-specific paths.

Requirements:

- preserve source filenames only as display/upload names, never as trusted filesystem paths;
- prevent path traversal from torrent/gallery/archive-provided names;
- reserve/check disk space when size is known and react clearly when it is not;
- support cancellation without deleting unrelated files;
- resume from existing engine state where safe;
- verify an acquired output exists and is non-empty before upload handoff;
- use `ffprobe`/engine metadata to verify media output when processing produced a media file;
- never expose cookies, auth headers, signed media URLs, or engine credentials in logs/UI.

For multiple selected video qualities, outputs may be acquired sequentially initially to bound temporary disk usage. Parallel acquisition is an optimization, not a correctness requirement.

## Upload Handoff

The new subsystem must not upload directly with Grammers `upload_stream()` like the current legacy `cmd_upload_from_url` flow.

Instead each complete staged output is handed to the same logical upload path used by local files. This preserves:

- whole-file SHA-256 preflight and source identity verification
- duplicate/version suggestions where appropriate
- upload journaling and crash recovery
- TeraRelay Boost or TDLib transport selection
- logical multipart files beyond Telegram's per-document limit
- remote logical manifests
- stable logical file IDs required for Version Stacks

Remote Import may provide an explicit display/upload filename so opaque staging filenames do not leak into TeraRelay.

The transfer engine should follow the user's normal upload-engine choice/policy rather than silently forcing a third Telegram transport.

## Existing Remote Upload Migration

The current direct-URL feature and `cmd_upload_from_url` contain useful URL-download behavior but have architectural limitations:

- direct HTTP(S) only
- rejects HTML pages by design
- downloads and uploads in one backend command
- directly calls Grammers upload stream
- enforces a 2 GB ceiling
- does not reuse the complete logical upload path

Remote Import replaces its user-facing workflow. Direct URLs become one source type inside Remote Import. The old command may remain temporarily only as an internal compatibility path until all callers are migrated, then can be removed in a later cleanup if no longer referenced.

## Tool Packaging and Health

A normal user should not need to install Python, FFmpeg, aria2, Deno, yt-dlp, or gallery-dl manually.

TeraRelay must have a platform-aware helper strategy for supported desktop targets. Engine discovery reports explicit availability/version/capability state, and Remote Import fails with an actionable message when a required helper is missing or incompatible.

Because website extractors change frequently, yt-dlp-related helper updates must be independently refreshable within the application's release/security model rather than requiring TeraRelay product code changes for every extractor update.

No helper update process may overwrite arbitrary files outside TeraRelay-owned application/helper directories.

## Authentication and Restricted Sources

Remote Import supports only content the user is authorized to access.

- Public sources work without authentication when the engine supports them.
- Sources requiring user authentication may use explicitly user-authorized engine cookie/session input in a later/contained authentication surface.
- TeraRelay must not silently scrape browser credential databases or steal browser sessions.
- DRM circumvention is out of scope.
- Engine errors such as login required, age gate, geo restriction, private source, extractor update required, or DRM-only content should be surfaced as meaningful categories where possible.

## Torrent Behavior

For a magnet or `.torrent` source:

- analyze torrent metadata first;
- present files when more than one meaningful file exists;
- selected torrent files become separate TeraRelay imports unless the product later defines a folder/archive grouping mode;
- acquisition uses librqbit and can resume from job-owned state;
- completed files enter the same upload handoff as every other source;
- torrent seeding is not a product goal for Remote Import; stop the torrent session after selected outputs are safely imported/cleaned according to job policy.

## Gallery/Playlist Behavior

A source that contains multiple independent items (playlist, carousel, gallery) presents item selection with Select All initially enabled. Each selected item follows the same output pipeline.

A multi-item source is not automatically represented as one Version Stack. Version Stack is reserved for multiple quality variants of the same logical video/media item.

## Failure and Recovery Semantics

- Analysis failure creates no staged file and no upload.
- Download failure keeps resumable engine state/staging only when it is useful for Retry.
- Processing failure keeps the acquired input needed for Retry when safe.
- Upload failure preserves the staged output and the existing TeraRelay upload journal for Retry.
- Application restart reconstructs durable Remote Import jobs and resumes from the earliest incomplete safe phase.
- A completed Telegram upload must never be re-uploaded solely because stack finalization or local queue persistence failed; store the resulting logical file ID before moving to stack finalization.
- Cancellation stops active engine work, cancels any active TeraRelay upload through the existing cancellation mechanism, and cleans only job-owned temporary files when no longer needed for safe recovery.
- Partial success is explicit for multi-quality/multi-item jobs.

## UI Scope

Modify only the existing upload-from-link surface and the transfer/import presentation required for this feature.

Expected desktop flow:

1. User opens writable channel.
2. Upload → Upload from Link.
3. Paste source.
4. Analyze.
5. If a single direct file requires no choice, Import can proceed immediately after validation.
6. If quality/item selection exists, show compact selectable rows.
7. Import.
8. Modal closes once the durable job is queued; progress remains visible through the existing transfer area or a minimal Remote Import extension of it.

Do not add a permanent channel picker, separate downloader page, or unrelated library/organization UI.

Mobile support should use the same backend job model, but desktop correctness is the first implementation target unless existing shared components make mobile support essentially free. Do not regress current mobile uploads.

## Testing Requirements

The feature is not complete until tests cover the routing/orchestration semantics without depending entirely on live third-party sites.

Required automated coverage includes:

1. source classification: HTTP direct, webpage, magnet, torrent file;
2. quality normalization and deterministic recommended-quality selection;
3. one selected quality → no Version Stack;
4. multiple selected qualities → Version Stack request with highest quality primary and correct labels;
5. destination captured from active writable channel with no modal channel selector;
6. root/read-only destination rejection;
7. durable job serialization/recovery across every phase boundary;
8. cancellation cleans only job-owned temp paths;
9. staged output handoff enters the normal logical upload path, including files larger than 2 GB in logic/tests without allocating real multi-GB data;
10. completed upload ID survives stack-finalization failure/retry without duplicate re-upload;
11. path sanitization for hostile torrent/gallery filenames;
12. engine errors are normalized into user-actionable categories;
13. existing local file/folder uploads and Version Stack workflows remain unchanged;
14. focused frontend tests for analysis/selection/progress states;
15. production frontend build, Rust test suite, transfer recovery tests, and existing prebuild QA remain green.

Live smoke tests should use stable/public test media where legally and practically appropriate, but automated unit/integration tests must use mocked engine adapters/fixtures so CI does not depend on YouTube/Instagram/TikTok availability.

## Non-Goals

- hosting or sharing imported media publicly;
- DRM circumvention;
- background cloud execution while the user's device is off;
- building a general standalone download manager;
- changing TeraRelay Drive encryption, Device Continuity, sharing, channel storage architecture, or Telegram authentication;
- automatic import of every available quality without user selection;
- unrelated UI redesign.

## Success Criteria

The implementation is successful when a user can open a writable TeraRelay channel, paste a supported source, choose one or several available qualities/items, and walk away while TeraRelay acquires/processes/uploads the output through the existing reliable storage layer. One quality appears as a normal file; multiple qualities of the same video appear as one existing Version Stack with the highest selected quality primary. Restart, retry, cancellation, large files, and helper failures behave predictably without damaging unrelated TeraRelay state.