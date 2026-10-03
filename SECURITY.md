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

### Current upstream Rust advisories

The current locked graph contains `hickory-proto 0.25.2` through Grammers' optional proxy DNS resolver and is reported by RustSec for `RUSTSEC-2026-0118` and `RUSTSEC-2026-0119`.

- The DNSSEC validation features required for the `RUSTSEC-2026-0118` vulnerable path are not enabled in TeraRelay's resolved feature graph.
- `RUSTSEC-2026-0119` remains present transitively. Hickory fixes it in the 0.26 line, but the current Grammers proxy dependency still pins `hickory-resolver 0.25.2`; forcing an incompatible transitive upgrade is not treated as a safe release fix.
- Proxy support is retained rather than silently removed. Re-check this dependency on every release and upgrade as soon as Grammers supports the fixed Hickory line.

`cargo audit` also reports transitive maintenance/unsoundness warnings. They should be reviewed on dependency upgrades even when they are not classified as active vulnerabilities.

## Reporting

For a public repository, use GitHub's private vulnerability-reporting/security-advisory flow when available. Do not include credentials, session files, or private user data in a public issue.
