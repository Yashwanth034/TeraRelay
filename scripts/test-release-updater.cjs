#!/usr/bin/env node
const assert = require('assert');
const fs = require('fs');
const os = require('os');
const path = require('path');
const cp = require('child_process');

const root = path.resolve(__dirname, '..');
const workflow = fs.readFileSync(path.join(root, '.github/workflows/release.yml'), 'utf8');
const ciWorkflow = fs.readFileSync(path.join(root, '.github/workflows/ci.yml'), 'utf8');
const generator = path.join(root, 'scripts/build-mobile-update-manifest.mjs');

assert.ok(fs.existsSync(generator), 'mobile update manifest generator is missing');
assert.match(workflow, /TAURI_SIGNING_PRIVATE_KEY:\s*\$\{\{ secrets\.TAURI_SIGNING_PRIVATE_KEY \}\}/);
assert.match(workflow, /uploadUpdaterJson:\s*true/);
assert.match(workflow, /uploadUpdaterSignatures:\s*true/);
assert.match(workflow, /max-parallel:\s*1/, 'desktop updater JSON publication must be serialized to avoid latest.json races');
assert.match(workflow, /release-capabilities:/, 'release credential capability preflight job is missing');
assert.match(workflow, /fromJSON\(needs\.release-capabilities\.outputs\.desktop_matrix\)/, 'desktop matrix is not derived from release capabilities');
assert.match(workflow, /windows_ready/, 'Windows signing readiness is not computed');
assert.match(workflow, /macos_ready/, 'macOS signing readiness is not computed');
assert.match(workflow, /ios_ready/, 'iOS distribution readiness is not computed');
assert.doesNotMatch(workflow, /if os\.environ\["WINDOWS_READY"\] == "true"/, 'Windows must always be present in the desktop release matrix');
assert.doesNotMatch(workflow, /if os\.environ\["MACOS_READY"\] == "true"/, 'macOS must always be present in the desktop release matrix');
assert.match(workflow, /ios-simulator:/, 'iOS simulator release validation must always run even without App Store credentials');
assert.match(workflow, /if:\s*needs\.release-capabilities\.outputs\.ios_ready\s*==\s*'true'/, 'iOS job is not gated by complete real distribution credentials');
assert.match(workflow, /needs:\s*desktop/, 'Android publishing must wait for all desktop updater artifacts');
assert.match(workflow, /mobile-latest\.json/);
assert.match(workflow, /tauri signer sign[^\n]*mobile-latest\.json|mobile-latest\.json[^\n]*signer sign/s, 'mobile manifest is not cryptographically signed');
assert.match(workflow, /mobile-latest\.json\.sig/, 'mobile manifest signature is not uploaded');
assert.match(workflow, /IOS_UPDATE_URL/, 'iOS store/TestFlight update URL is not plumbed into the mobile manifest');
assert.match(workflow, /TeraRelay-\$version-android-arm64\.apk/);
assert.match(workflow, /aapt[^\n]*dump badging|dump badging[^\n]*aapt/s, 'Android APK package/version metadata is not verified');
assert.match(workflow, /io\.terarelay\.desktop/, 'Android APK application ID is not pinned in release verification');
assert.doesNotMatch(workflow, /APPLE_SIGNING_IDENTITY:\s*["']-["']/, 'production macOS release still uses ad-hoc signing');
assert.match(workflow, /APPLE_CERTIFICATE/, 'macOS Developer ID certificate is not provisioned in CI');
assert.match(workflow, /APPLE_(ID|API_ISSUER)/, 'macOS notarization credentials are not wired');
assert.match(workflow, /MACOS_APP_PROVISION_PROFILE/, 'macOS host App Group provisioning profile is not required');
assert.match(workflow, /MACOS_FILE_PROVIDER_PROVISION_PROFILE/, 'macOS File Provider provisioning profile is not required');
assert.match(workflow, /TeraRelayFileProvider\.appex/, 'macOS release does not embed the File Provider extension');
assert.match(workflow, /MACOS-FILE-PROVIDER-PROFILES-PASS/, 'macOS release does not validate File Provider provisioning entitlements');
assert.match(workflow, /MACOS-FILE-PROVIDER-SIGNATURE-PASS/, 'macOS release does not verify the final File Provider signature');
assert.match(workflow, /cargo check --manifest-path app\/src-tauri\/Cargo\.toml --target/, 'native Windows/macOS Drive providers are not compile-gated on release runners');
assert.match(workflow, /build-macos-file-provider\.sh/, 'macOS File Provider Swift build is not run in release CI');
assert.match(workflow, /\"swift_triple\":\s*\"arm64-apple-macosx12\.0\"/, 'Apple Silicon File Provider Swift target is not pinned');
assert.match(workflow, /\"swift_triple\":\s*\"x86_64-apple-macosx12\.0\"/, 'Intel File Provider Swift target is not pinned');
assert.match(workflow, /lipo -verify_arch/, 'macOS File Provider architecture is not verified after bundling');
assert.match(workflow, /gh release upload/);
assert.match(workflow, /ios-release:/, 'real iOS release job is missing');
assert.match(workflow, /IOS_CERTIFICATE/, 'iOS distribution certificate is not wired');
assert.match(workflow, /IOS_MOBILE_PROVISION/, 'iOS provisioning profile is not wired');
assert.match(workflow, /--export-method app-store-connect/, 'iOS is not built for App Store Connect');
assert.match(workflow, /xcrun altool --upload-app --type ios/, 'iOS IPA is not uploaded to App Store Connect/TestFlight');
assert.match(workflow, /APPLE_API_KEY_ID/, 'App Store Connect API key ID is missing');
assert.match(workflow, /APPLE_API_ISSUER/, 'App Store Connect issuer is missing');
assert.match(workflow, /GITHUB_RUN_NUMBER/, 'iOS release does not stamp a unique App Store build number');
assert.match(workflow, /GITHUB_RUN_ATTEMPT/, 'iOS release build number is not retry-safe');
assert.match(workflow, /CFBundleIdentifier/, 'iOS IPA bundle identifier is not verified before upload');
assert.match(workflow, /CFBundleShortVersionString/, 'iOS IPA marketing version is not verified before upload');
assert.match(workflow, /CFBundleVersion/, 'iOS IPA build number is not verified before upload');
assert.match(workflow, /WINDOWS_CERTIFICATE/, 'Windows code-signing certificate is not wired');
assert.match(workflow, /WINDOWS_CERTIFICATE_PASSWORD/, 'Windows code-signing certificate password is not wired');
assert.match(workflow, /certificateThumbprint/, 'Windows Tauri signing config is not generated from the imported certificate');
assert.match(workflow, /tauri\.windows-signing\.conf\.json/, 'Windows release does not consume its runtime signing config');
assert.match(workflow, /GITHUB_REF_NAME/, 'release workflow does not validate the vX.Y.Z tag against package version');
assert.match(workflow, /android build --apk --aab --target aarch64 --ci/, 'Android release build must use Tauri boolean APK/AAB flags without values');
assert.doesNotMatch(workflow, /--apk\s+true|--aab\s+true/, 'release workflow still passes obsolete boolean values to Tauri Android build flags');
assert.match(ciWorkflow, /android build --apk --target aarch64 --ci/, 'Android CI build must use the Tauri APK boolean flag without a value');
assert.doesNotMatch(ciWorkflow, /--apk\s+true|--aab\s+true/, 'CI workflow still passes obsolete boolean values to Tauri Android build flags');

const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'terarelay-update-manifest-'));
try {
  const apk = path.join(dir, 'TeraRelay-1.2.3-android-arm64.apk');
  const output = path.join(dir, 'mobile-latest.json');
  fs.writeFileSync(apk, Buffer.from('signed-apk-fixture'));
  cp.execFileSync(process.execPath, [generator,
    '--version', '1.2.3',
    '--apk', apk,
    '--apk-url', 'https://github.com/Yashwanth034/TeraRelay/releases/download/v1.2.3/TeraRelay-1.2.3-android-arm64.apk',
    '--notes', 'Updater QA release',
    '--published-at', '2026-10-10T12:00:00Z',
    '--output', output,
  ], { cwd: root, stdio: 'pipe' });
  const manifest = JSON.parse(fs.readFileSync(output, 'utf8'));
  assert.strictEqual(manifest.version, '1.2.3');
  assert.strictEqual(manifest.notes, 'Updater QA release');
  assert.strictEqual(manifest.publishedAt, '2026-10-10T12:00:00.000Z');
  assert.strictEqual(manifest.android.url, 'https://github.com/Yashwanth034/TeraRelay/releases/download/v1.2.3/TeraRelay-1.2.3-android-arm64.apk');
  assert.match(manifest.android.sha256, /^[a-f0-9]{64}$/);
  assert.strictEqual(manifest.iosStoreUrl, null);

  assert.throws(() => cp.execFileSync(process.execPath, [generator,
    '--version', '../bad', '--apk', apk,
    '--apk-url', 'https://github.com/Yashwanth034/TeraRelay/releases/download/v1.2.3/x.apk',
    '--output', output,
  ], { cwd: root, stdio: 'pipe' }));
} finally {
  fs.rmSync(dir, { recursive: true, force: true });
}

console.log('RELEASE-UPDATER-CONTRACT-PASS');
