# TeraRelay Grammers transport compatibility patch

Source: `grammers-mtsender` from https://github.com/Lonami/grammers at commit `d07f96f6bee22a3a850a1f3067834686a08b396c` (package version 0.8.1).

The upstream MIT/Apache-2.0 license texts and source copyright notices are retained here. This directory contains the library sources; upstream live-account integration tests are not included.

Local changes:

- Pin the internal Grammers dependencies to the same original Git revision, preserving the existing session/auth/TL family rather than mixing registry versions.
- Require hickory-resolver 0.26.1 or later in the compatible 0.26 line (application lockfile: 0.26.3), propagate fallible resolver construction/lookup errors, and adapt the IP iterator API.
- Add local SOCKS5 authentication, unauthenticated payload round-trip and invalid-URL unit tests. They use synthetic loopback peers, without Telegram accounts or session files.
- Apply equivalent Clippy suggestions in sender.rs and sender_pool.rs, plus Rust formatting, so the vendored path crate passes the application's strict lint gate. No client/session API is replaced.

The application patches only this transport crate from the original Git source. Its workspace and Cargo.lock also cover these regression tests. The rest of the Grammers family remains pinned to the original revision.
