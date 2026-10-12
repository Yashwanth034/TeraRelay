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

### JavaScript build dependencies

The `source-map-js` fix introduced in v0.1.1 remains in the locked graph. On 2026-10-12, `npm audit --json` for v0.2.0 reported zero vulnerabilities at every severity. Continue checking the locked dependency graph before every release.

### Current upstream Rust advisories

The current locked graph contains `hickory-proto 0.25.2` through Grammers' optional proxy DNS resolver and is reported by RustSec for `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119`.

- The DNSSEC validation features required for the `RUSTSEC-2026-0118` vulnerable path are not enabled in TeraRelay's resolved feature graph.
- `RUSTSEC-2026-0119` remains present transitively. Hickory fixes it in the 0.26 line, but the current Grammers proxy dependency still pins `hickory-resolver 0.25.2`; forcing an incompatible transitive upgrade is not treated as a safe release fix.
- Proxy support is retained rather than silently removed. Re-check this dependency on every release and upgrade as soon as Grammers supports the fixed Hickory line.

### Torrent ring buffer advisory

The locked graph contains `ringbuf 0.4.8` through `librqbit-utp 0.7.0` and `librqbit 9.0.1` on non-iOS targets. [RUSTSEC-2026-0293](https://rustsec.org/advisories/RUSTSEC-2026-0293.html) reports a double free/use-after-free when an element's destructor panics inside `Consumer::skip` or `Consumer::clear`; the patched release is `ringbuf 0.5.2` or later.

Inspection of the published `librqbit-utp 0.7.0` source found its ring buffer is `SharedRb<Heap<u8>>` (`src/stream_tx.rs`). `truncate_front` calls `skip`, but stored bytes have no destructor that can panic. Therefore, the advisory's required trigger is not reachable through this inspected use. This is a source-based reachability assessment, not a claim that the dependency is patched or that every memory-safety issue has been excluded. Reassess if the dependency or stored element type changes. The current library requires the `ringbuf 0.4` line; upgrading to 0.5 requires a compatible upstream dependency update or a separately tested patch.

### Audit snapshot and follow-up

On 2026-10-12, `cargo audit --json` reported three vulnerability advisories: `RUSTSEC-2026-0118`, `RUSTSEC-2026-0119`, and `RUSTSEC-2026-0293`. They remain visible to the audit; no ignore rule was added. The resolved Hickory features contain no DNSSEC validation feature. In pinned Grammers revision `d07f96f`, Hickory is used by the SOCKS5 connection path to resolve domain-name proxy hosts; literal IPv4/IPv6 proxy addresses bypass that resolver. The encoding CPU-exhaustion advisory (`RUSTSEC-2026-0119`) remains unresolved, and this review does not establish that it is unreachable. A trusted proxy configured by IP avoids this particular Grammers DNS lookup, but is not a dependency fix.

The audit also reports `RUSTSEC-2024-0370` (unmaintained `proc-macro-error`) and `RUSTSEC-2024-0429` (unsound `glib::VariantStrIter`) as warnings. Review these on upstream upgrades; warning classification does not imply harmlessness.

Follow up with compatible Grammers/Hickory and librqbit/ringbuf updates, then rerun feature analysis, dependency audits, proxy/torrent regression tests, and cross-platform CI. This assessment does not require a binary release; a later code/dependency fix will require updated binaries.

## Reporting

Use [GitHub private vulnerability reporting](https://github.com/Yashwanth034/TeraRelay/security/advisories/new) for security vulnerabilities. Do not include credentials, Telegram session files, signing material, or private user data in a public issue.
