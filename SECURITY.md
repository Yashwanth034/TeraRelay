# TeraRelay security

## Sensitive local data

TeraRelay stores authentication/session material locally. Never commit or publish:

- Telegram session databases
- TDLib databases or authorization state
- Telegram API hashes
- login codes or 2-step verification passwords
- local SQLite user/share databases
- signing keys, certificates, provisioning profiles, or Android keystores
- local environment files, logs, caches, or QA fixtures

The repository ignore rules and `scripts/check-source.sh` provide release-time guardrails, but maintainers must still review the final Git diff before publishing.

## Telegram storage model

TeraRelay uses Telegram cloud storage as its backing transport/storage layer. Telegram cloud chats are not end-to-end encrypted. Users with a threat model requiring end-to-end confidentiality should encrypt sensitive files before uploading them.

## Local services

TeraRelay may run local HTTP services for application streaming/API features. These are designed for local application use unless a feature explicitly documents otherwise. Do not expose a local TeraRelay port directly to the public Internet.

## Dependency security

Before a release, run:

```bash
cd app
npm audit --audit-level=high
cd src-tauri
cargo audit
```

Known upstream advisories that cannot be resolved without breaking required functionality must be documented rather than hidden.

### v0.2.1 dependency remediation (2026-10-28)

This checkpoint supersedes the historical unresolved-Hickory/ringbuf snapshots below. The locked graph now uses `hickory-proto`, `hickory-net` and `hickory-resolver 0.26.3`, and `ringbuf 0.5.3`. A fresh non-waived `cargo audit --file Cargo.lock` completed successfully against the current RustSec database; no advisory ignores were added.

The narrow vendored Grammers transport patch retains version 0.8.1 and the original `d07f96f` Grammers session/auth/TL family, rather than migrating stored sessions to a new client family. SOCKS support is retained. The vendored uTP 0.7.0 library retains its production API and transport features while moving to the patched ringbuf line. Both source directories include licenses and patch provenance.

The dependency regression crates share the application's workspace and lockfile. Upstream demonstration targets and external-reference-library comparison tests are excluded because their development-only dependencies reintroduced ringbuf 0.4; the native uTP buffer and loopback transport tests remain. This does not disable torrent or proxy functionality in TeraRelay.

Cargo audit still prints its default non-fatal upstream warnings, including unmaintained packages, GLib/Tokio unsoundness advisories and a yanked clipboard-win version. They remain visible; a successful mandatory vulnerability gate is not a claim that every dependency or platform is risk-free. Actual release/CI evidence must still be checked before publishing.

### JavaScript build dependencies

The `source-map-js` fix introduced in v0.1.1 remains in the locked graph. On 2026-10-12, `npm audit --json` for v0.2.0 reported zero vulnerabilities at every severity. Continue checking the locked dependency graph before every release.

**2026-10-21 frontend security recheck:** A fresh `npm audit --audit-level=high --json` identified **one high-severity Vite vulnerability** in the locked `vite 7.3.6` (Windows `launch-editor` argument/command handling, advisories `GHSA-698v-4r3x-3vcv`, `GHSA-r825-w8mm-5pxw`, and `GHSA-587h-7wmj-h5pm`). A **lockfile-only** `npm update vite --package-lock-only --ignore-scripts --no-audit --no-fund` moved the single `node_modules/vite` lock entry to **7.3.7**, preserving `package.json` (range `^7.0.4`) and installed `node_modules` byte-for-byte. The new lock entry includes npm's resolved tarball integrity. A second real `npm audit --audit-level=high --json` **exited 0 with zero vulnerabilities at all severities**. `scripts/check-source.sh` also passed after moving the synthetic native CI Vite log to the ephemeral GitHub runner's `RUNNER_TEMP` rather than embedding a private QA path in tracked workflow source. This is an **audited lockfile repair**, not a local runtime check of the newly locked Vite binary; no new npm packages were installed. CI already contains `npm ci` and `npm audit --audit-level=high` and must perform actual runtime verification when authorized to run remotely. The unrelated three nonwaived **RustSec advisories remain**.

### Historical upstream Rust advisories (before v0.2.1)

The current locked graph contains `hickory-proto 0.25.2` through Grammers' optional proxy DNS resolver and is reported by RustSec for `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119`.

- The DNSSEC validation features required for the `RUSTSEC-2026-0118` vulnerable path are not enabled in TeraRelay's resolved feature graph.
- `RUSTSEC-2026-0119` remains present transitively. Hickory fixes it in the 0.26 line, but the current Grammers proxy dependency still pins `hickory-resolver 0.25.2`; forcing an incompatible transitive upgrade is not treated as a safe release fix.
- Proxy support is retained rather than silently removed. Re-check this dependency on every release and upgrade as soon as Grammers supports the fixed Hickory line.

### Historical torrent ring buffer advisory

The locked graph contains `ringbuf 0.4.8` through `librqbit-utp 0.7.0` and `librqbit 9.0.1` on non-iOS targets. [RUSTSEC-2026-0293](https://rustsec.org/advisories/RUSTSEC-2026-0293.html) reports a double free/use-after-free when an element's destructor panics inside `Consumer::skip` or `Consumer::clear`; the patched release is `ringbuf 0.5.2` or later.

**Additional 2026-10-23 fail-closed transport policy (worktree, not a dependency fix):** TeraRelay's `torrent_session` and synthetic `torrent_items_from_bytes_for_test` now both use the same explicit `librqbit::ListenerMode::TcpOnly` session options, rather than inheriting the upstream listener default. Published librqbit 9.0.1 presently defaults to TCP-only, but an upstream default change must not silently activate its `librqbit-utp` transport while the transitive ringbuf advisory remains unresolved. TCP torrents are retained. An isolated Rust regression asserts both persisted and non-persisted option configurations keep TCP enabled and uTP disabled. This reduces runtime exposure to the affected uTP implementation but **does not remove the vulnerable crate from the graph**, close RustSec-2026-0293, or prove the memory-safety issue is otherwise impossible. A compatible fixed dependency and true torrent runtime QA remain required.

Inspection of the published `librqbit-utp 0.7.0` source found its ring buffer is `SharedRb<Heap<u8>>` (`src/stream_tx.rs`). `truncate_front` calls `skip`, but stored bytes have no destructor that can panic. Therefore, the advisory's required trigger is not reachable through this inspected use. This is a source-based reachability assessment, not a claim that the dependency is patched or that every memory-safety issue has been excluded. Reassess if the dependency or stored element type changes. The current library requires the `ringbuf 0.4` line; upgrading to 0.5 requires a compatible upstream dependency update or a separately tested patch.

### Audit snapshot and follow-up

On 2026-10-12, `cargo audit --json` reported three vulnerability advisories: `RUSTSEC-2026-0118`, `RUSTSEC-2026-0119`, and `RUSTSEC-2026-0293`. They remain visible to the audit; no ignore rule was added. The resolved Hickory features contain no DNSSEC validation feature. In pinned Grammers revision `d07f96f`, Hickory is used by the SOCKS5 connection path to resolve domain-name proxy hosts; literal IPv4/IPv6 proxy addresses bypass that resolver. The encoding CPU-exhaustion advisory (`RUSTSEC-2026-0119`) remains unresolved, and this review does not establish that it is unreachable. A trusted proxy configured by IP avoids this particular Grammers DNS lookup, but is not a dependency fix.

**2026-10-23 current-worktree mitigation (not an audit fix):** Telegram connection initialization **and** the separate Test Proxy Traffic command now resolve only the configured SOCKS5 **proxy endpoint hostname** using Tokio's system resolver with a five-second timeout before passing a numeric IPv4/IPv6 SOCKS5 URL to pinned Grammers. This avoids the currently identified Hickory proxy-hostname lookup call site while preserving username/password encoding and routing Telegram destination traffic through the SOCKS5 proxy. DNS failure fails closed rather than falling back to Grammers/Hickory. Both commands no longer log credential-bearing proxy URLs. The proxy diagnostic no longer deletes a fixed/shared test session; each attempt uses a randomly named private 0700 Unix directory with RAII cleanup for its own session/WAL files. Focused regressions cover credentials, IPv4 and IPv6 formatting, localhost hostname resolution, malformed configuration, preservation of numeric host URLs, and isolated diagnostic-session cleanup. This is a bounded mitigation of identified call paths, not proof that every Hickory use is unreachable, and `cargo audit` still flags both Hickory advisories. Proper coordinated Grammers 0.10/Hickory 0.26+ migration and real SOCKS5/native-platform tests are still required.

**Additional 2026-10-23 cross-platform proxy and CONNECT-header safety (current worktree):** `NetworkConfig::effective_proxy_url_checked` now fails closed for enabled-but-invalid/missing proxies, unknown types and missing HTTP/HTTPS bridges, instead of treating them as a disabled proxy and silently using direct network routes. SOCKS5 hostnames, bare or bracketed IPv6, username/password and ports use checked URL construction; invalid proxy settings are rejected before application. Telegram login, separate transfer lanes, remote import and proxy diagnostics use the checked API, and transfer lanes resolve the proxy host to a numeric endpoint through the system resolver once per pool rather than allowing direct fallback. The local SOCKS5-to-HTTP CONNECT bridge now rejects CRLF/control/path/userinfo/percent-injected SOCKS destination hostnames before opening an upstream connection and canonically formats IPv6 and IDNA domains, using a single validated HTTP authority in request line and Host header. `cargo test --locked --offline proxy_` passed **8/8** and Rustfmt/diff passed. **This does not clear or suppress the three RustSec advisories**, establish all possible Hickory paths unreachable, or replace actual proxy and native Windows/macOS runtime E2E.

The audit also reports `RUSTSEC-2024-0370` (unmaintained `proc-macro-error`) and `RUSTSEC-2024-0429` (unsound `glib::VariantStrIter`) as warnings. Review these on upstream upgrades; warning classification does not imply harmlessness.

Follow up with compatible Grammers/Hickory and librqbit/ringbuf updates, then rerun feature analysis, dependency audits, proxy/torrent regression tests, and cross-platform CI. This assessment does not require a binary release; a later code/dependency fix will require updated binaries.

### 2026-10-18 development-worktree audit (not a released fix)

Using the RustSec advisory database updated 2026-10-17, the existing lockfile initially reported **18 vulnerability entries**, including newly reported advisories against `rustls`, `quinn-proto`, and two versions of `zip`. A targeted patch-level lockfile update moved `rustls 0.23.45 -> 0.23.46` and `quinn-proto 0.11.15 -> 0.11.18` without changing TeraRelay's declared API dependencies. The subsequent `cargo audit --json` reports **9 remaining vulnerability entries** and still exits nonzero; no advisory was suppressed or ignored. Full serial Rust tests after the code and lockfile edits passed **427/427**, with 2 pre-existing ignored, and the development build succeeded. `npm audit --audit-level=high` reported zero JavaScript vulnerabilities.

The remaining RustSec entries are: `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119` in `hickory-proto 0.25.2`, `RUSTSEC-2026-0293` in `ringbuf 0.4.8`, and `RUSTSEC-2026-0367`, `RUSTSEC-2026-0368`, `RUSTSEC-2026-0369` each reported against both `zip 2.4.2` and `zip 4.6.1`. The audit additionally reports seven unmaintained package warnings and one unsoundness warning. TeraRelay's direct `zip 2.4.2` usage in the ZIP viewer uses `ZipArchive::by_index` rather than the vulnerable bulk `extract()` API. **However, the Windows-only path of `tauri-plugin-updater 2.13.2` depends on `zip 4.6.1` and does call `ZipArchive::extract(&temp_dir)`** (`updater.rs`, Windows `Update::extract_zip`). Its normal `Update::download` verifies the archive's updater signature before returning bytes, and TeraRelay's frontend calls `downloadAndInstall`, which reduces exposure to unauthenticated archives but does not patch the underlying extraction vulnerability. Do not claim that the ZIP advisories are entirely unreachable. Replace or patch this dependency through an upstream-compatible update and verify the Windows installer update flow on an actual Windows runner. Neither this assessment nor the existing Hickory/ringbuf reachability assessments removes the vulnerable packages.

As a separate defense-in-depth change, ZIP and 7z preview extraction now use a shared bounded decoded-entry reader with a maximum of 256 MiB even if the archive download limit is configured as unlimited. Focused regressions check oversized advertised sizes, excess decompressed bytes, truncated bytes and exact-size input. This is **not** a substitute for upgrading the two vulnerable `zip` dependency versions.

**Later source-only RAR fail-closed mitigation, 2026-10-18:** Source review found that the prior `rar::Archive::extract_all` implementation extracted every archive-controlled member name to disk for both listing and selecting a single file, exposing a potential path-traversal/write-outside-destination risk. Both TeraRelay RAR preview commands now refuse in-app listing/extraction with a clear explanation. Storing/downloading RAR files is not disabled. The unsafe extractor implementation and direct `rar 0.4` dependency have been removed from source and lockfile. The expanded source regression (`scripts/test-rar-preview-failclosed.cjs`) and `cargo check --offline` passed with the modified dependency graph. The earlier integrated verification run was intentionally stopped on high host load **7.39 / 6.16 / 4.16**, but was later superseded by an independently completed low-priority, single-core **post-RAR-removal Rust test run: 429 passed, 0 failed, 2 ignored** (process `process-1a14f3aa573-4b`), with 5/5 focused archive tests also green (`process-1a14f3946cc-4a`). A subsequent single-core low-priority `cargo build --locked --offline` also **passed** on the current source (process `process-1a14f4920bd-4e`, 15.69 seconds); no installer was created and the application was not launched. Restore RAR in-app previews only via a separately reviewed safe single-entry decoder, licensing assessment and full cross-platform runtime tests. Other archive processing still requires separate security review.

Additionally, archive download loops were corrected so Telegram transport errors do not become silent end-of-file; the temp-file variant attempts owned cleanup on this error. The latest **five** focused Rust `bounded_archive_tests` passed after the RAR removal, including the fail-closed dispatch regression, and the latest full Rust suite passed **429/429** with 2 existing ignored. This establishes native source-test correctness, not completion of external OS E2E, a production development build, or a passed dependency-security audit.

### Latest 2026-10-18 locked security migration (supersedes earlier ZIP status)

A more recent targeted upgrade **removed both vulnerable ZIP dependency lines**:
- `tauri-plugin-updater 2.13.2` is now built with `default-features = false`, retaining `rustls-tls` and `system-proxy` but disabling the legacy ZIP feature. The current v2 signed updater workflow and frontend `downloadAndInstall` path remain configured; direct EXE/MSI updater compatibility still needs real Windows-runner validation. `zip 4.6.1` is no longer present even in the `--target all` dependency graph.
- The app's direct ZIP dependency was migrated from `zip 2.4.2` to locked `zip 9.0.5` (manifest `9.0.4` lower bound). ZIP filenames are now handled as fallible UTF-8/name results in both the archive viewer and filesystem test. A new **real deflate ZIP round-trip** regression builds, lists, and decompresses an archive with the bounded reader. ZIP9-focused tests passed **6/6**.

After these changes, the **full latest-source Rust suite passed 430 tests, 0 failed, 2 ignored** (`process-1a14f803d69-71`), and a subsequent offline development build passed (`process-1a14f82a9d0-74`, 24.05 seconds). Scoped Rust formatting, ZIP/updater/RAR source-contract tests, and source hygiene passed. No installer was built or installed, and native Windows/macOS/device execution was not performed.

Fresh `cargo audit --json` returned exactly **3 unresolved vulnerability entries** (and correctly exited nonzero): `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119` in `hickory-proto 0.25.2` and `RUSTSEC-2026-0293` in `ringbuf 0.4.8`. RustSec requires Hickory `>=0.26.1` for the CPU exhaustion advisory (the NSEC3 advisory considers the 0.26 prerelease unaffected) and ringbuf `>=0.5.2`. The current pinned Grammers proxy feature requires Hickory 0.25.x and `librqbit-utp 0.7.0` requires ringbuf 0.4.x. No audit ignores, unsafe feature disabling, or unverified transitive forced upgrade was applied. Seven unmaintained warnings and one unsoundness warning remain visible. The dependency vulnerabilities and incomplete cross-platform runtime verification prevent any claim that this is a security-cleared release.

### Exact remaining advisory reachability check (2026-10-18)

The latest `cargo metadata --locked --offline` resolved feature graph was inspected directly: `grammers-mtsender 0.8.1` has `proxy` active; `hickory-resolver 0.25.2` has `default,system-config,tokio`; `hickory-proto 0.25.2` has `futures-io,std,tokio` and **NO DNSSEC feature**. `RUSTSEC-2026-0118` specifically requires the DNSSEC validation path, so its described NSEC3 infinite-loop precondition is **not active in this exact graph**. `RUSTSEC-2026-0119` concerns DNS message encoding and is **not established unreachable** by disabled DNSSEC; it requires a compatible Grammers/Hickory update.

The cached `librqbit-utp 0.7.0` `src/stream_tx.rs` was inspected directly: its only declared ring buffer type is `SharedRb<Heap<u8>>`, and `UserTx::truncate_front` invokes the affected `Consumer::skip` method. Because `u8` has no destructor, the advisory's required **panicking Drop** condition is absent for that source path. This observation lowers demonstrated exploit reachability but is **not a patched ringbuf**; the dependency remains `ringbuf 0.4.8`, which is flagged by the audit. Any change of buffer element type or downstream usages requires renewed review.

Neither reachability assessment removes RustSec entries. **The security audit still fails with three advisories**, without ignores, and this worktree has not passed native Windows/macOS/mobile runtime tests. Follow up through compatible upstream Grammers 0.10/Hickory 0.26+ and librqbit/ringbuf upgrades or reviewed patches, plus proxy/uTP regression tests, before a security-cleared release.

Do not attempt unverified major-version upgrades, blanket audit ignores, or claim a secure release. Resolve remaining advisories through compatible upstream updates or fully reviewed and cross-platform-tested migrations, then rerun the complete test and dependency audit gates. No installer, release or update package was generated in this development verification.

### 2026-10-19 upstream remediation compatibility and fail-closed guard

Fresh advisory recheck and locked local `cargo audit --file Cargo.lock --json` still report exactly **three unresolved** vulnerabilities: Hickory `RUSTSEC-2026-0118` and `0119`, and ringbuf `RUSTSEC-2026-0293`. The audit exits **1**. The required targets are [Hickory 0.26.1 or later](https://rustsec.org/advisories/RUSTSEC-2026-0119.html) (with the 0.26 line moving DNSSEC validation to `hickory-net`) and [ringbuf 0.5.2 or later](https://rustsec.org/advisories/RUSTSEC-2026-0293.html).

The published `grammers-mtsender 0.10.0` manifest retains the SOCKS `proxy` feature and requires `hickory-resolver 0.26.1`. A fresh 2026-10-19 comparison of the published [`grammers-client 0.10.0` manifest](https://docs.rs/crate/grammers-client/0.10.0/source/Cargo.toml) and [`grammers-mtsender 0.10.0` manifest](https://docs.rs/crate/grammers-mtsender/0.10.0/source/Cargo.toml.orig) confirms the coordinated `0.10.0` family (`client`, `session`, `mtsender`, `mtproto`, `tl-types` plus `crypto` transitively). **Do not mix registry 0.10.x crates with the pinned Git 0.8.x family.** The upstream GitHub repository is archived, with the maintained release metadata pointing to Codeberg. The 0.10 family is a **candidate** for a coordinated migration, **not a verified TeraRelay-compatible upgrade**: session persistence, Telegram login/auth, SOCKS proxy, upload/download, TL schema, and all existing real-account fixtures need regression before replacing the pin. The current `librqbit-utp 0.7.0` remains on `ringbuf 0.4.x`; the fixed 0.5.x line requires a compatible upstream uTP/rqbit update or an independently reviewed patch and real torrent regression. No transitive version was forced and no locked dependency changed in this continuation. Explicit offline, `--dry-run` Cargo resolution probes **confirmed the incompatibilities**: `hickory-proto 0.26.1` was rejected because `hickory-resolver 0.25.2` (from pinned `grammers-mtsender 0.8.1`) requires `hickory-proto ^0.25`; `ringbuf 0.5.2` was rejected because `librqbit-utp 0.7.0` (from `librqbit 9.0.1`) requires `ringbuf ^0.4.7`. The `Cargo.lock` SHA-256 was identical before and after both commands. These are constraint failures, not proof that an upgraded and API-migrated application would fail.

Improved `scripts/test-rustsec-reachability.py`: negative tests now cover **every** vulnerable Hickory/ringbuf resolved package ID, including synthetic second-version graphs, rather than selecting the final package by name. A separate metadata-only positive control simulates 0.26.1/0.5.2 to ensure no obsolete reachability guards remain. All controls passed locally. **These are regression guards only.** They do not suppress advisories, upgrade packages, or make the current nonzero cargo audit green.

Do not claim any security-cleared release until both migrations are compatible with Telegram session/auth and SOCKS proxy flows, torrent/uTP flows, and actual OS/CI runtime tests, with a zero-vulnerability non-waived audit.

### 2026-10-21 new RustSec advisories: **six outstanding**, not three

A fresh **non-waived** `cargo audit --file Cargo.lock --json` against the same locked source exited **1** and enumerated **six** advisories. The earlier three-entry checkpoints above are accurate historical results from the then-current advisory database, but are **not the latest security status**:

- `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119` — `hickory-proto 0.25.2` (pinned Grammers/Hickory). Coordinated Hickory/Grammers migration remains necessary.
- `RUSTSEC-2026-0293` — `ringbuf 0.4.8` via `librqbit-utp 0.7.0`. A compatible uTP/ringbuf migration remains necessary.
- **New** `RUSTSEC-2026-0382` — `rustls-webpki 0.103.15`, patched `>=0.103.16, <0.104.0-alpha.1` (or supported later line).
- **New** `RUSTSEC-2026-0383` — `webpki-root-certs 1.0.9`, patched `>=1.0.10`.
- **New** `RUSTSEC-2026-0381` — `webpki-roots 1.0.7`, patched `>=1.0.10`. The direct `webpki-roots 0.26.11` is explicitly marked unaffected, but the separate transitive 1.0.7 is not.

`cargo tree -i` connects these transitive TLS components through rustls/hyper-rustls, reqwest (including 0.12/0.13), librqbit and tauri-plugin-updater. **Offline `cargo update --dry-run -p ... --precise ...` succeeded for all three new patch versions**, showing compatible dependency version constraints without changing `Cargo.lock`; however, the patched crate **source archives were unavailable in the locally cached offline registry**, so no patched rebuild, TLS certificate regression, or new security-cleared result could be verified under the no-install restriction. Do **not** edit only the lockfile and then call the application tested, or suppress these advisories. An authorized fetch of patched source packages plus locked tests/real TLS proxy, updater and torrent checks is required to close those three.

Latest local correctness checks **do not replace the failing audit**: strict all-target `cargo clippy --locked --offline --all-targets -- -D warnings` exited 0 in independent compile/rechecks, serial Rust `cargo test --locked --offline -- --test-threads=1` exited 0 with **430 pass, 0 fail, 2 ignored**, complete CI desktop interaction Node suite **66/66 pass**, and frontend `npm run build` (TypeScript/Vite) exited 0. No normal app/profile or user installation, staged Git change, commit, push or release was involved.

### 2026-10-21 certificate trust advisory remediation — verified current status

The six-advisory audit above was a **historical pre-remediation checkpoint**, not the latest locked-graph status. Three newly reported TLS/certificate-trust advisories were resolved with targeted, offline-resolved **Cargo.lock-only** updates:

| Package | Vulnerable locked version | Patched locked version | Advisory |
|---|---|---|---|
| `rustls-webpki` | `0.103.15` | `0.103.16` | `RUSTSEC-2026-0382` |
| `webpki-root-certs` | `1.0.9` | `1.0.10` | `RUSTSEC-2026-0383` |
| `webpki-roots` (transitive 1.x line) | `1.0.7` | `1.0.10` | `RUSTSEC-2026-0381` |

Each `cargo update --offline --dry-run -p PACKAGE@OLD --precise NEW` resolved cleanly **before** any changes. Then exactly the three package versions were updated with offline Cargo; no broad dependency upgrade or `Cargo.toml` edit. A private snapshot of the previously dirty lockfile was retained outside tracked source; it was not included in any release artifacts. Normal Cargo fetching was permitted **only for the patched crate sources required to compile/test and generate complete metadata**, not an application installation or package distribution.

**Post-update verification:** the real non-waived `cargo audit --file Cargo.lock --json` exits **1 with exactly THREE advisories remaining**, all unchanged: `RUSTSEC-2026-0118` and `0119` for `hickory-proto 0.25.2`, and `RUSTSEC-2026-0293` for `ringbuf 0.4.8`. The three new certificate findings no longer occur in the locked graph. Rust tests passed **430, failed 0, ignored 2**, including a retry after an initial 220-second build+test timeout (the first run was NOT a pass). Strict all-target `cargo clippy --locked --offline --all-targets -- -D warnings` exited **0** against the patched sources. Full Cargo metadata initially failed offline due to the uncached target-specific `webpki-root-certs 1.0.10` crate; it was fetched through locked Cargo metadata and the unwaived `scripts/test-rustsec-reachability.py` guard then passed, including duplicate-vulnerable-version negative controls. Rust formatting and `git diff --check` passed.

**Do not call the overall Rust audit green:** its exit code remains **1**, without ignores. Hickory/Grammers and ringbuf/librqbit need coordinated upstream-compatible migrations plus Telegram login/SOCKS proxy/torrent regressions and actual Windows/macOS/mobile runtime testing. No installed app, normal Telegram profile, installer, commit, push, stage, or release was touched.

### 2026-10-21 repaired WebPKI dependency regression guard

The latest **current-worktree** non-waived `cargo audit --file Cargo.lock --json` still exits **1 with three vulnerabilities** (Hickory `RUSTSEC-2026-0118`/`0119` and ringbuf `RUSTSEC-2026-0293`). This is distinct from the historical six-advisory snapshot taken **before** the WebPKI lockfile repair above.

The existing CI-executed `scripts/test-rustsec-reachability.py` now additionally checks the resolved Cargo metadata for previously repaired `rustls-webpki >=0.103.16` (`RUSTSEC-2026-0382`), `webpki-root-certs >=1.0.10` (`RUSTSEC-2026-0383`) and the affected `webpki-roots 1.x >=1.0.10` (`RUSTSEC-2026-0381`). The explicitly unaffected, separate legacy `webpki-roots 0.26.x` line is not treated as the vulnerable 1.x line. This is a fail-closed project regression floor, **not** a replacement for future RustSec advisories or their upstream fixes.

Local verification passed against the actual locked graph: all three patched versions were observed; **six negative controls** independently detected a downgrade and a duplicate vulnerable version for each package. The existing Hickory/ringbuf DNSSEC/consumer/duplicate-version negative checks and metadata-only simulated patched controls also passed. Python syntax and `git diff --check` passed. The real dependencies were **not changed** by this guard enhancement, and the overall RustSec audit **remains failing**; no advisory is ignored. No installer, installed app, normal Telegram profile, commit/push or release was touched.

### 2026-10-25 upstream compatibility recheck (not an advisory remediation)

Public release metadata lists coordinated [`grammers-client 0.11.0`](https://docs.rs/crate/grammers-client/latest) (published 2026-10-15), newer than the documented 0.10.0 migration candidate. The previously inspected published [`grammers-mtsender 0.10.0`](https://docs.rs/crate/grammers-mtsender/0.10.0) retains the SOCKS5 `proxy` feature with `hickory-resolver ^0.26.1`; this establishes a candidate upstream family, **not** compatibility of a 0.11 migration with TeraRelay's currently pinned Git 0.8.x session/auth/proxy/TL API. No coordinated upgrade or real Telegram integration test was performed.

The newest published [`librqbit-utp 0.7.0`](https://docs.rs/crate/librqbit-utp/latest) still declares `ringbuf ^0.4.7`. This cannot resolve to patched `ringbuf >=0.5.2` under normal Cargo semver, so a reviewed compatible upstream uTP/rqbit release or fully tested source patch is required. No dependency was removed, no security ignore/waiver or forced lockfile edit was applied. A recent local `cargo audit --no-fetch` cannot load its advisory database in the isolated sandbox; the earlier **three unwaived findings remain unresolved**, and no fresh clean audit result is claimed. Preserve Telegram SOCKS5 proxy and torrent capabilities until full migration QA is possible.

## Reporting

Use [GitHub private vulnerability reporting](https://github.com/Yashwanth034/TeraRelay/security/advisories/new) for security vulnerabilities. Do not include credentials, Telegram session files, signing material, or private user data in a public issue.
