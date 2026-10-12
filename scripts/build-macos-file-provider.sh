#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
provider_root="$repo_root/app/src-tauri/macos-file-provider"
build_root="$provider_root/build"
appex_root="$build_root/TeraRelayFileProvider.appex"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS File Provider build skipped: host is not macOS"
  exit 0
fi

command -v swift >/dev/null 2>&1 || { echo "swift is required for the macOS File Provider" >&2; exit 1; }
command -v codesign >/dev/null 2>&1 || { echo "codesign is required for the macOS File Provider" >&2; exit 1; }
command -v plutil >/dev/null 2>&1 || { echo "plutil is required for the macOS File Provider" >&2; exit 1; }

version="$(python3 - "$repo_root/app/src-tauri/Cargo.toml" <<'PY'
import pathlib,re,sys
text=pathlib.Path(sys.argv[1]).read_text()
m=re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
if not m:
    raise SystemExit('Could not read TeraRelay package version')
print(m.group(1))
PY
)"
build_number="${TERARELAY_MACOS_BUILD_NUMBER:-${GITHUB_RUN_NUMBER:-1}.${GITHUB_RUN_ATTEMPT:-1}}"

rm -rf "$appex_root"
mkdir -p "$appex_root/Contents/MacOS" "$build_root"

swift_args=(--package-path "$provider_root" -c release)
if [[ -n "${TERARELAY_MACOS_SWIFT_TRIPLE:-}" ]]; then
  swift_args+=(--triple "$TERARELAY_MACOS_SWIFT_TRIPLE")
fi
swift build "${swift_args[@]}" --product TeraRelayFileProvider
bin_dir="$(swift build "${swift_args[@]}" --show-bin-path)"

install -m 0755 "$bin_dir/TeraRelayFileProvider" "$appex_root/Contents/MacOS/TeraRelayFileProvider"
cp "$provider_root/Info.plist" "$appex_root/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$version" "$appex_root/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$build_number" "$appex_root/Contents/Info.plist"
plutil -lint "$appex_root/Contents/Info.plist" >/dev/null

if [[ -n "${TERARELAY_FILE_PROVIDER_PROFILE_PATH:-}" ]]; then
  [[ -f "$TERARELAY_FILE_PROVIDER_PROFILE_PATH" ]] || {
    echo "TERARELAY_FILE_PROVIDER_PROFILE_PATH does not exist" >&2
    exit 1
  }
  cp "$TERARELAY_FILE_PROVIDER_PROFILE_PATH" "$appex_root/Contents/embedded.provisionprofile"
fi

if [[ -n "${TERARELAY_MAIN_APP_PROFILE_PATH:-}" ]]; then
  [[ -f "$TERARELAY_MAIN_APP_PROFILE_PATH" ]] || {
    echo "TERARELAY_MAIN_APP_PROFILE_PATH does not exist" >&2
    exit 1
  }
  cp "$TERARELAY_MAIN_APP_PROFILE_PATH" "$build_root/TeraRelayMain.provisionprofile"
else
  rm -f "$build_root/TeraRelayMain.provisionprofile"
fi

if [[ -n "${APPLE_SIGNING_IDENTITY:-}" && "${APPLE_SIGNING_IDENTITY}" != "-" ]]; then
  if [[ ! -f "$appex_root/Contents/embedded.provisionprofile" ]]; then
    echo "Production File Provider signing requires TERARELAY_FILE_PROVIDER_PROFILE_PATH" >&2
    exit 1
  fi
  if [[ ! -f "$build_root/TeraRelayMain.provisionprofile" ]]; then
    echo "Production File Provider signing requires TERARELAY_MAIN_APP_PROFILE_PATH" >&2
    exit 1
  fi
  codesign --force --options runtime --timestamp \
    --entitlements "$provider_root/TeraRelayFileProvider.entitlements" \
    --sign "$APPLE_SIGNING_IDENTITY" \
    "$appex_root"
  codesign --verify --strict --verbose=2 "$appex_root"
elif [[ "${APPLE_SIGNING_IDENTITY:-}" == "-" ]]; then
  # Ad-hoc development/CI bundles have no Developer ID provisioning profiles.
  # Production identities above still require both profiles and timestamping.
  codesign --force --entitlements "$provider_root/TeraRelayFileProvider.entitlements" \
    --sign - "$appex_root"
  codesign --verify --strict --verbose=2 "$appex_root"
fi

echo "macOS File Provider bundle ready: $appex_root"
