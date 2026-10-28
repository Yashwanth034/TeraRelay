# TeraRelay Grammers transport compatibility patch

Source: `grammers-mtsender` from https://github.com/Lonami/grammers at commit `d07f96f6bee22a3a850a1f3067834686a08b396c` (package version 0.8.1).

The upstream MIT/Apache-2.0 license texts and source copyright notices are retained here. This directory contains the library sources; upstream live-account integration tests are not included.

Local changes:

- Pin the internal Grammers dependencies to the same original Git revision, preserving the existing session/auth/TL family rather than mixing registry versions.
- Require hickory-resolver 0.26.1 or later in the compatible 0.26 line (application lockfile: 0.26.3), propagate fallible resolver construction/lookup errors, and adapt the IP iterator API.
- Add local SOCKS5 authentication, unauthenticated payload round-trip and invalid-URL unit tests. They use synthetic loopback peers, without Telegram accounts or session files.
- Apply equivalent Clippy suggestions in sender.rs and sender_pool.rs, plus Rust formatting, so the vendored path crate passes the application's strict lint gate. No client/session API is replaced.

The application patches only this transport crate from the original Git source. Its workspace and Cargo.lock also cover these regression tests. The rest of the Grammers family remains pinned to the original revision.

## iOS resolver compatibility

Hickory 0.26.3 is optional only on non-iOS targets. Its Apple system-config path references macOS-only SCDynamicStore APIs, so it must not be linked into iOS. On iOS, proxy domain names use `tokio::net::lookup_host`, backed by the OS resolver. Numeric proxy addresses, destination addresses, SOCKS authentication, and error propagation are preserved. Resolution failures do not bypass the proxy or select public DNS servers.

Regression checks: run the local SOCKS unit tests with `proxy` enabled; on an iOS simulator also run `socks5_loopback_with_os_resolved_hostname` (password authentication and payload echo via a localhost proxy). From the application manifest, inspect `cargo tree --locked --target aarch64-apple-ios -e normal` and the `aarch64-apple-ios-sim` equivalent: neither graph should include `hickory-resolver` or `system-configuration`. A non-iOS target such as `aarch64-apple-darwin` should retain Hickory 0.26.3. Finally build/link the iOS application in CI; `cargo check` alone does not exercise the missing-symbol failure.
