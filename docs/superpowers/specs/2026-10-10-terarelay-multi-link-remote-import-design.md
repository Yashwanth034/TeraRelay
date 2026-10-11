# TeraRelay Multi-Link Remote Import Design

Date: 2026-10-10
Status: Design approved conversationally; written-spec review pending
Depends on: `docs/superpowers/specs/2026-10-10-terarelay-remote-import-design.md`

## Intent

Extend the completed single-source Remote Import feature so a user can paste multiple heterogeneous sources at once and configure each source independently before import.

Supported mixed batches include website/media URLs, direct files, magnet links, and `.torrent` links handled by the already-finalized Remote Import engine stack. The feature must reuse the existing single-source analyzer/acquisition/upload pipeline rather than duplicate it.

The user-facing goal is:

1. Open the writable TeraRelay channel that should receive the import.
2. Paste multiple links in one Remote Import dialog.
3. TeraRelay analyzes the links in the pasted order.
4. The user explicitly chooses what to import for every link.
5. The user may manually group selected outputs from different links as one Version Stack.
6. TeraRelay executes the batch in deterministic order, survives restart, and never guesses cross-link version relationships.

## Non-Negotiable Product Rules

- Current open channel remains the implicit destination. There is no destination-channel dropdown.
- Multi-link mode has **no default selection**. Analysis may show recommended metadata, but no quality/file/item checkbox is selected automatically.
- Every source keeps independent selections.
- One source may choose one quality while another chooses several.
- Multiple qualities of the same analyzed media source continue to use the existing automatic Version Stack behavior.
- Cross-source Version Stacks are **manual only**. TeraRelay never groups different links merely because filenames, titles, dimensions, hashes, or media metadata look similar.
- Users manually select compatible outputs and choose **Group as versions**.
- One output may belong to at most one cross-source Version Stack group within a batch.
- Version Stack creation/finalization must reuse the existing Version Stack commands and durable finalization logic.
- The batch queue must preserve the user's source order.
- The implementation must not modify Device Continuity, Drive crypto/metadata, sharing, TDLib session handling, or unrelated UI.

## Existing Architecture to Reuse

The finished Remote Import system already provides:

- source classification;
- yt-dlp/gallery-dl/direct/torrent analysis and acquisition;
- per-job staging ownership;
- safe subprocess invocation;
- cancellation isolation;
- librqbit file selection;
- durable Remote Import jobs;
- handoff into the normal TeraRelay upload queue;
- upload outcomes with logical file IDs;
- same-source multi-quality Version Stack finalization;
- helper packaging and managed yt-dlp updates.

Multi-Link Remote Import adds a batch/orchestration layer above those existing jobs. It does not add another downloader, uploader, torrent client, or stack implementation.

## Data Model

### Batch

Add a durable `RemoteImportBatch` model with at least:

- `id`
- immutable destination `folderId`
- ordered `sourceEntries`
- `stackGroups`
- batch phase/status
- current active source index
- created/updated timestamps
- durable error/partial-success information

### Source Entry

Each `RemoteImportBatchEntry` owns:

- stable entry ID
- ordinal/source order
- original source string
- linked single-source Remote Import job ID after analysis starts
- analysis status
- explicit selected IDs
- import status
- terminal uploaded outputs/logical file IDs
- error/retry state

The existing `RemoteImportJob` remains the execution unit. A batch entry references a normal job instead of replacing it.

### Cross-Source Version Stack Group

Add a durable batch-level grouping record such as:

- `groupId`
- optional user-facing stack name/title
- ordered member references `(entryId, selectionId)`
- optional explicit primary member reference
- persisted `stackId` after creation
- persisted `stackedFileIds` for restart-safe idempotent finalization

A member reference must resolve to exactly one selected output. Invalid, duplicate, or already-grouped references are rejected before execution.

## Input Parsing

The dialog accepts multiple source lines.

Rules:

- trim whitespace;
- ignore blank lines;
- preserve first-seen order exactly;
- duplicate links remain separate entries if the user pasted them more than once, because they may intentionally configure them differently;
- reject an entirely empty batch;
- apply a conservative maximum batch size to prevent accidental huge paste/DoS; the UI must explain the limit rather than silently truncate.

No shell parsing is involved. Every source continues to be passed as a literal argument/value to its existing engine path.

## Analysis Flow

Analysis is sequential in source order.

For each entry:

1. create/reuse its normal single-source Remote Import job;
2. run the existing analyzer;
3. persist analysis result to the job and batch entry;
4. move to the next entry.

Sequential analysis is intentional:

- predictable UI/order;
- avoids launching many extractor/torrent metadata jobs simultaneously;
- reduces rate-limit pressure on websites;
- makes failure/retry state obvious.

A failed source does not erase successful analyses of earlier sources. The user may retry that source or remove it before import.

## Selection UX

After analysis, display one compact card/section per source in original order.

Examples:

### Website/video source

- title/platform/thumbnail/duration when available;
- quality checkboxes such as 480p / 720p / 1080p / 4K / 8K;
- recommendation may be visually labeled, but **nothing is checked automatically in multi-link mode**.

### Torrent/magnet source

- torrent name;
- file list with sizes;
- no file selected automatically in multi-link mode.

### Direct file

- one selectable output row;
- still not auto-selected in multi-link mode.

The Import button remains disabled until every non-removed source has either:

- at least one explicit selection, or
- been explicitly skipped/removed by the user.

This prevents accidental imports caused by hidden defaults.

## Manual Cross-Link Version Grouping

The user can enter/select a grouping action after making per-link selections.

Recommended UX:

1. Select two or more selected output rows across source cards.
2. Click **Group as versions**.
3. TeraRelay shows a compact group summary.
4. User may choose the primary member manually; if omitted, the highest video height is suggested but not silently committed until the group is confirmed.
5. Confirm the group.

After grouping, each member row shows a lightweight group marker/name. There is no automatic grouping heuristic.

### Compatibility

Cross-link Version Stack groups are intended for alternate versions of one logical file/media item. The UI should permit grouping arbitrary files because the existing Version Stack feature is user-controlled, but it should show available labels/quality metadata to help the user make the right choice.

For torrent/magnet scenarios, this enables:

- Torrent A -> select `Movie.1080p.mkv`
- Torrent B -> select `Movie.2160p.mkv`
- manually group those two selected outputs as one Version Stack

Likewise a torrent output and a YouTube output may be grouped if the user deliberately chooses to do so.

## Same-Source vs Cross-Source Stack Rule

Same-source behavior remains automatic and unchanged:

- exactly one quality from one media source -> normal file;
- 2+ qualities selected from that same source -> existing automatic Version Stack.

Cross-source behavior is different:

- outputs from different links are never auto-stacked;
- only an explicit batch stack group may combine them.

If a selected output is manually assigned to a cross-source group, it must not also participate in that source job's automatic same-source stack. The batch planner must resolve ownership before imports start so one uploaded logical file is finalized into exactly one stack relationship.

If a source has three selected qualities and the user manually groups only one of those qualities with an output from another link, the remaining same-source qualities are handled independently according to the explicit planning rules rather than creating overlapping stacks.

Recommended deterministic rule: any source with at least one member manually assigned to a cross-source group disables automatic same-source stacking for that source; its ungrouped selected outputs remain normal standalone files unless the user creates another explicit group. This avoids ambiguous overlapping stack membership.

## Execution Order

Imports execute in deterministic batch order, one source job at a time by default.

For each entry:

1. acquire selected outputs through its existing engine path;
2. hand prepared files into the existing durable TeraRelay upload queue;
3. wait for terminal upload outcomes needed by the batch planner;
4. persist resulting logical file IDs;
5. continue to the next source.

After all required members of a manual cross-source Version Stack group have terminal successful/skipped upload outcomes, finalize that group using the existing stack commands.

A group's finalization does not have to wait for unrelated later sources once all of its own members are available, but durable state must make repeated finalization idempotent.

## Failure and Recovery

The batch layer must be restart-safe.

- Batch state is persisted separately from the existing single-job store or with an explicitly versioned schema; do not overload a plain job array with incompatible objects.
- On restart, recover batch entries and reference their existing Remote Import jobs.
- Never recreate/re-upload a file when a durable logical file ID is already known.
- Never create a second Version Stack if the group already persisted its `stackId`.
- Persist every successfully added stack member so retries add only missing members.
- A failed source pauses that source/batch position with Retry/Skip/Remove options; completed earlier sources remain completed.
- A partial batch remains visibly partial rather than being reported as success.
- Cancelling a batch cancels only its currently active acquisition/upload work and keeps unrelated TeraRelay uploads untouched.
- Cleanup removes only staging owned by the relevant Remote Import jobs after their outputs are durably handed off/finalized according to existing policy.

## User Actions

Required multi-link actions:

- Add/paste multiple links
- Analyze
- Per-source select/deselect outputs
- Retry failed analysis
- Remove/skip a source before import
- Group selected outputs as versions
- Ungroup before import
- Choose/adjust a group's primary member
- Import batch
- Cancel active batch
- Retry failed source/import step

No destination chooser and no separate downloader UI are added.

## Concurrency and Safety

Initial implementation is deliberately serial at the batch-source level.

- one active analyzer/acquisition source at a time;
- existing upload queue controls transfer concurrency internally;
- no uncontrolled fan-out of yt-dlp/aria2/gallery-dl/rqbit processes;
- cancellation token ownership remains per normal Remote Import job;
- cross-source grouping is metadata orchestration only and never merges/re-encodes files.

Serial orchestration is preferred over parallel source downloads for correctness, predictable disk use, website rate limits, and simpler crash recovery. Parallel batch-source acquisition can be considered later if justified by measured need.

## UI Scope

Modify only the existing Remote Import surface and the minimum supporting batch presentation.

Single-link flow must remain clean and unchanged:

- one source pasted -> existing single-link UX, including the existing recommended default selection behavior.

Multi-link flow activates only when 2+ nonblank sources are present:

- ordered source cards;
- no default selections;
- manual grouping controls;
- one Import action;
- compact progress showing current entry / total entries and per-entry status.

Do not add a global channel list or a second Remote Import screen elsewhere in the app.

## Testing Requirements

### Pure model tests

- parse multiple links while preserving order;
- blank-line handling;
- duplicate-source preservation;
- multi-link default selection is empty for every source type;
- single-link default behavior remains unchanged;
- stable batch/entry/group IDs;
- manual group validation rejects duplicate membership and missing selections;
- cross-source groups never form automatically;
- same-source auto-stack remains unchanged when no manual group touches that source;
- manual group involvement disables overlapping automatic stack planning;
- primary-member selection is deterministic/persisted;
- queue ordering is stable.

### Recovery tests

- app restart during analysis;
- restart during source N acquisition;
- restart after upload but before batch outcome persistence;
- restart after stack creation but before all members are added;
- retry does not re-upload known logical files;
- retry does not create duplicate stacks;
- earlier completed entries remain complete after later source failure;
- batch cancellation does not cancel unrelated uploads.

### UI contract tests

- 2+ links activate multi-link mode;
- no checkbox is preselected in multi-link mode;
- each source has independent selections;
- grouping control is available only for 2+ eligible selected outputs;
- group/ungroup updates visible membership;
- no destination dropdown appears;
- one-link flow retains existing compact UI.

### Backend/integration tests

Reuse existing Remote Import engine tests; add only orchestration tests needed for batch behavior. Do not duplicate yt-dlp/rqbit/aria2 engine tests that already exist.

### Final regression

At minimum:

- multi-link focused tests;
- existing Remote Import Rust tests;
- existing Remote Import UI/model checks;
- helper packaging checks;
- frontend production build;
- full Rust test suite;
- `git diff --check`;
- `scripts/prebuild-qa.sh`.

Real Telegram E2E must only use the existing preserved isolated QA profile under the already-established safety rules; never inspect or expose TDLib session contents and never log out/delete the preserved session.

## Files Expected to Change

Likely in-scope:

- `app/src/remoteImport.ts`
- `app/src/hooks/useRemoteImport.ts`
- `app/src/components/desktop/dashboard/RemoteUploadModal.tsx`
- a new focused batch model/module if separating batch logic improves safety
- focused Remote Import test scripts
- possibly existing upload outcome plumbing only if batch orchestration requires a minimal generic callback/event extension

Backend acquisition engine files should remain unchanged unless a concrete batch orchestration defect proves a backend change is necessary.

Out of scope:

- Device Continuity files
- Drive metadata/crypto/mount logic
- TDLib auth/session code
- sharing/public-link code
- unrelated dashboard/feed behavior
- Git staging/committing/pushing/releasing/version bumping

## Completion Criteria

The extension is complete only when:

1. mixed multiple links can be pasted and analyzed in original order;
2. every link has independent explicit choices;
3. multi-link mode has no automatic selected quality/file/item;
4. imports execute/recover deterministically;
5. same-source multiple qualities retain existing automatic stack behavior when not manually overridden;
6. outputs from different links can be manually grouped into one Version Stack;
7. manual cross-source groups survive restart and finalize idempotently;
8. no output can end up in overlapping stacks;
9. current-channel implicit destination remains intact;
10. full focused and project regression verification passes without touching unrelated product areas.
