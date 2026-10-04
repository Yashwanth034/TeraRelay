# Third-party software notices

TeraRelay is built with third-party open-source software. Those projects remain subject to their own licenses and copyrights.

This document is a release-maintenance summary. The exact resolved versions are pinned by `app/package-lock.json` and `app/src-tauri/Cargo.lock`.

## Major direct components

| Component | Role | License family |
| --- | --- | --- |
| Tauri and official Tauri plugins | application framework and native integrations | MIT / Apache-2.0 |
| React / React DOM | user interface | MIT |
| Grammers | Telegram MTProto client | MIT / Apache-2.0 |
| Actix Web ecosystem | local HTTP services | MIT / Apache-2.0 |
| Tokio | async runtime | MIT |
| reqwest / rustls | networking and TLS | MIT / Apache-2.0 family |
| hls.js | HLS playback | Apache-2.0 |
| mp4box | MP4 processing | BSD-3-Clause |
| mp4parse | MP4 parsing | MPL-2.0 |
| Lucide React | icons | ISC |
| qrcode.react | QR rendering | ISC |
| i18next / react-i18next | localization | MIT |
| Framer Motion | UI animation | MIT |
| SQLite Rust bindings | local application database | MIT / Apache-2.0 |
| sevenz-rust2 | archive handling | Apache-2.0 |
| zip / rar crates | archive handling | permissive open-source licenses |

The resolved JavaScript dependency graph also contains packages under permissive licenses including MIT, Apache-2.0, ISC, BSD, 0BSD and CC-BY-4.0, plus MPL-2.0 components. The resolved Rust dependency graph contains MIT/Apache/BSD/ISC/MPL/BSL/CDLA/Unicode and other permissive-or-weak-copyleft license expressions.

## Runtime-downloaded Linux components

The Linux fast-transfer bootstrap can obtain system packages at runtime rather than vendoring their binaries in this repository:

- TDLib — Boost Software License 1.0
- SQLCipher — BSD-3-Clause-style community license

Their upstream license terms and package documentation continue to apply.

## Release requirement

Before publishing binary artifacts, maintainers should regenerate/review the resolved dependency inventory, retain required copyright/license notices, and include any notices required by the exact versions distributed.

TeraRelay's original source code is governed by the proprietary [LICENSE](LICENSE) at the repository root. That license does not alter or restrict the separate licenses and notices that apply to third-party software.
