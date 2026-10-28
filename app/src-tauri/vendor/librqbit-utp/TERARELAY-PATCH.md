# TeraRelay dependency patch

Source: published `librqbit-utp` 0.7.0, retrieved from
https://static.crates.io/crates/librqbit-utp/librqbit-utp-0.7.0.crate

Archive SHA-256: `4f3bfdc73944bc76cab24d5690a98816770040a654c449edf5ff2b9ba22626aa`

Upstream VCS revision: `c26f57b2debbe35ed0ace1ad419de529f7a5bf95`.
Copyright and Apache-2.0 licensing are retained in LICENSE; the complete license
text is provided in LICENSE-APACHE.

Local changes:
- Require ringbuf `0.5.2` (Cargo's compatible range `>=0.5.2, <0.6.0`) in both
  manifests, allowing ringbuf 0.5.3.
- Add buffer wrap, growth, maximum-capacity, and truncation regression tests in
  `src/stream_tx.rs`. Production behavior and crate features are retained; the
  ringbuf APIs used by this crate remain compatible.
- Apply equivalent Clippy conditional rewrites in `src/socket.rs`,
  `src/stream_dispatch.rs` and `src/utils.rs`, preserving the strict lint gate.
- Omit the upstream Cargo.lock, which describes its standalone development tree
  with ringbuf 0.4.7. The application owns dependency resolution.
- Omit upstream CI metadata and gitignore; retain all library, test, binary
  fixture, documentation, and original package provenance files.
- Upstream demo source is retained for reference but its example targets are
  not built. Remove demo-only Prometheus/CLI development dependencies because
  the exporter pulls vulnerable ringbuf 0.4 through metrics-util. The uTP library,
  transport features and unit/loopback tests are retained.
- Omit interoperability-only tests against the external libutp-rs2 reference
  library and that development dependency: it also requires vulnerable ringbuf
  0.4. Retain the native uTP IPv4/IPv6/dual-stack loopback tests and all production
  transport functionality.
- Use application workspace profiles rather than ignored standalone profiles.
- Apply Rust formatting to the vendored sources.
- Remove the legacy `std::f64` module import in `src/congestion/cubic.rs` so
  `f64::INFINITY` resolves to the primitive type's associated constant. This
  preserves the value while avoiding the deprecated module constant rejected
  by current stable Rust's strict Clippy gate.

The application integrates this directory with a `[patch.crates-io]` entry for
`librqbit-utp`.
