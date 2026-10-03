#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

tracked="$(git ls-files)"

if printf '%s\n' "$tracked" | grep -Ei '(^|/)(\.env($|\.)|.*\.session$|shares\.db$|.*\.(pem|p8|p12|jks|keystore|mobileprovision|provisionprofile)$|keystore\.properties$)' >/dev/null; then
  echo "Sensitive runtime/signing file is tracked by Git"
  printf '%s\n' "$tracked" | grep -Ei '(^|/)(\.env($|\.)|.*\.session$|shares\.db$|.*\.(pem|p8|p12|jks|keystore|mobileprovision|provisionprofile)$|keystore\.properties$)' || true
  exit 1
fi

if printf '%s\n' "$tracked" | grep -Ei '(^|/)(\.repotunnel-tmp|node_modules|dist|target|gen)(/|$)|tdlib-(one-login|premium-check|native-actual).*\.bin$' >/dev/null; then
  echo "Generated/private/QA content is tracked by Git"
  printf '%s\n' "$tracked" | grep -Ei '(^|/)(\.repotunnel-tmp|node_modules|dist|target|gen)(/|$)|tdlib-(one-login|premium-check|native-actual).*\.bin$' || true
  exit 1
fi

hygiene_tmp="${TMPDIR:-/tmp}/terarelay-source-hygiene.$$"
if git grep -nEI '(/home/[^/[:space:]]+/(Downloads|Projects)/|/Users/[^/[:space:]]+/(Downloads|Projects)/|workspace-[0-9a-f]{6,}|\.repotunnel-tmp/)' -- . ':!scripts/check-source.sh' ':!.gitignore' >"$hygiene_tmp" 2>/dev/null; then
  echo "Private machine/workspace/QA reference found in tracked source"
  cat "$hygiene_tmp"
  rm -f "$hygiene_tmp"
  exit 1
fi
rm -f "$hygiene_tmp"

if find app/src app/src-tauri/src -type f \( -name '*.orig' -o -name '*~' \) -print -quit | grep -q .; then
  echo "Temporary editor/migration file found in source tree"
  exit 1
fi

git diff --check

echo "TeraRelay source hygiene checks passed."
