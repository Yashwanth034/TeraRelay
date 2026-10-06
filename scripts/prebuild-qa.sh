#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== TeraRelay prebuild QA =="

./scripts/test-transfer-ui.sh
node scripts/test-focused-ui.cjs
node scripts/test-playback-seeking.cjs
node scripts/test-transfer-recovery.cjs
python3 scripts/test-upload-progress.py
python3 scripts/test-download-progress.py
python3 scripts/test-download-timeout.py
python3 scripts/test-upload-stress.py
python3 scripts/test-download-stress.py
./scripts/test-media-playback.sh

python3 -m py_compile app/src-tauri/src/tdlib_fast_worker.py

(
  cd app
  npm run build
  node check-i18n.cjs
)

node scripts/test-native-media.cjs

(
  cd app/src-tauri
  cargo fmt --all -- --check
  cargo test --lib -- --nocapture
)

# Scope diff whitespace validation to product/test source so unrelated local
# workflow edits do not make this deterministic prebuild gate unusable.
git diff --check -- app scripts

echo "PASS TeraRelay prebuild QA"
