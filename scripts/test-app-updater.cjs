#!/usr/bin/env node
const assert = require('assert');
const fs = require('fs');
const path = require('path');
const vm = require('vm');
const ts = require(path.join(__dirname, '..', 'app', 'node_modules', 'typescript'));

const root = path.resolve(__dirname, '..');
const read = (relative) => fs.readFileSync(path.join(root, relative), 'utf8');
const exists = (relative) => fs.existsSync(path.join(root, relative));

function test(name, fn) {
  try {
    fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}: ${error.message}`);
    process.exitCode = 1;
  }
}

function loadUpdaterModule() {
  const relative = 'app/src/appUpdater.ts';
  assert.ok(exists(relative), `${relative} does not exist`);
  const source = read(relative);
  const output = ts.transpileModule(source, {
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      esModuleInterop: true,
    },
  }).outputText;
  const module = { exports: {} };
  const context = vm.createContext({ module, exports: module.exports, require, URL, console });
  new vm.Script(output, { filename: relative }).runInContext(context);
  return module.exports;
}

test('semantic versions are compared numerically and ignore a leading v', () => {
  const updater = loadUpdaterModule();
  assert.strictEqual(updater.compareVersions('0.1.10', '0.1.9'), 1);
  assert.strictEqual(updater.compareVersions('v2.0.0', '2.0.0'), 0);
  assert.strictEqual(updater.compareVersions('1.2.0', '1.10.0'), -1);
  assert.strictEqual(updater.isNewerVersion('1.0.1', '1.0.0'), true);
  assert.strictEqual(updater.isNewerVersion('1.0.0', '1.0.0'), false);
  assert.throws(() => updater.compareVersions('not-a-version', '1.0.0'), /version/i);
});

test('mobile update manifest accepts a valid GitHub APK release', () => {
  const updater = loadUpdaterModule();
  const manifest = updater.normalizeMobileUpdateManifest({
    version: 'v0.1.2',
    notes: 'Security and reliability fixes',
    publishedAt: '2026-10-10T12:00:00Z',
    android: {
      url: 'https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay-0.1.2-android-arm64.apk',
      sha256: 'a'.repeat(64),
    },
    iosStoreUrl: 'https://testflight.apple.com/join/example',
  });
  assert.strictEqual(manifest.version, '0.1.2');
  assert.strictEqual(manifest.android.sha256, 'a'.repeat(64));
});

test('mobile update manifest rejects unsafe update sources and hashes', () => {
  const updater = loadUpdaterModule();
  const base = {
    version: '0.1.2',
    android: {
      url: 'https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay.apk',
      sha256: 'b'.repeat(64),
    },
  };
  assert.throws(() => updater.normalizeMobileUpdateManifest({ ...base, android: { ...base.android, url: 'http://github.com/x.apk' } }), /https/i);
  assert.throws(() => updater.normalizeMobileUpdateManifest({ ...base, android: { ...base.android, url: 'https://evil.example/x.apk' } }), /host/i);
  assert.throws(() => updater.normalizeMobileUpdateManifest({ ...base, android: { ...base.android, sha256: 'xyz' } }), /sha-?256/i);
  assert.throws(() => updater.normalizeMobileUpdateManifest({ ...base, iosStoreUrl: 'https://evil.example/app' }), /ios|store|host/i);
});

test('desktop updater is configured for signed GitHub latest.json artifacts', () => {
  const config = JSON.parse(read('app/src-tauri/tauri.conf.json'));
  assert.strictEqual(config.bundle.createUpdaterArtifacts, true, 'createUpdaterArtifacts must be true');
  assert.ok(config.plugins?.updater?.endpoints?.some((value) => value.includes('/releases/latest/download/latest.json')), 'GitHub latest.json updater endpoint missing');
  assert.strictEqual(config.plugins?.updater?.requireSignedVersion, true, 'desktop updater must bind signed artifacts to the announced version');
  assert.ok(typeof config.plugins?.updater?.pubkey === 'string' && config.plugins.updater.pubkey.trim().length > 20, 'updater public key missing');

  const pkg = JSON.parse(read('app/package.json'));
  const packageLock = JSON.parse(read('app/package-lock.json'));
  assert.ok(pkg.dependencies['@tauri-apps/plugin-updater'], 'frontend updater plugin dependency missing');
  assert.ok(pkg.dependencies['@tauri-apps/plugin-process'], 'frontend process plugin dependency missing');
  assert.strictEqual(pkg.devDependencies['@tauri-apps/cli'], '2.12.1', 'Tauri CLI must stay pinned above the Android CI/version and signer-env fixes');
  assert.strictEqual(packageLock.packages['node_modules/@tauri-apps/cli']?.version, '2.12.1', 'package-lock Tauri CLI pin drifted');

  const cargo = read('app/src-tauri/Cargo.toml');
  assert.ok(cargo.includes('tauri-plugin-updater'), 'Rust updater plugin dependency missing');
  assert.ok(cargo.includes('tauri-plugin-process'), 'Rust process plugin dependency missing');

  const lib = read('app/src-tauri/src/lib.rs');
  assert.ok(lib.includes('tauri_plugin_updater'), 'desktop updater plugin is not registered');

  const capability = read('app/src-tauri/capabilities/desktop-updater.json');
  assert.ok(capability.includes('updater:'), 'updater capability missing');
  assert.ok(capability.includes('process:'), 'process capability missing');
});

test('shared update coordinator and prompt are mounted and both About surfaces can check', () => {
  assert.ok(exists('app/src/context/AppUpdateContext.tsx'), 'AppUpdateContext missing');
  assert.ok(exists('app/src/components/shared/AppUpdatePrompt.tsx'), 'AppUpdatePrompt missing');
  const app = read('app/src/App.tsx');
  assert.ok(app.includes('AppUpdateProvider'), 'AppUpdateProvider is not mounted');
  assert.ok(app.includes('AppUpdatePrompt'), 'AppUpdatePrompt is not mounted');

  const context = read('app/src/context/AppUpdateContext.tsx');
  assert.ok(context.includes("import('@tauri-apps/plugin-updater')"), 'desktop updater must be dynamically loaded');
  assert.ok(context.includes('cmd_mobile_check_update'), 'mobile update check is not wired');
  assert.ok(context.includes('app-update-download-progress'), 'Android download progress event is not wired');
  assert.ok(context.includes("cmd_android_install_update', { apkPath, sha256: asset.sha256 }"), 'Android installer does not re-bind the cached APK to the signed manifest hash');
  assert.ok(/cmd_android_install_update[\s\S]{0,260}setStatus\('ready-to-install'\)/.test(context), 'Android installer cancellation would leave the updater stuck in installing state');
  assert.ok(context.includes('startup'), 'startup update check marker missing');

  const desktop = read('app/src/components/desktop/dashboard/SettingsModal.tsx');
  const mobile = read('app/src/components/mobile/MobileDashboard.tsx');
  assert.ok(desktop.includes('useAppUpdate'), 'desktop About has no updater hook');
  assert.ok(mobile.includes('useAppUpdate'), 'mobile About has no updater hook');
  assert.ok(desktop.includes('checkForUpdates'), 'desktop About has no manual update check');
  assert.ok(mobile.includes('checkForUpdates'), 'mobile About has no manual update check');
});

test('all locales expose the updater Settings labels', () => {
  const localeDir = path.join(root, 'app/src/i18n/locales');
  const required = ['check_for_updates', 'checking_for_updates', 'up_to_date', 'update_available_version'];
  for (const name of fs.readdirSync(localeDir).filter((entry) => entry.endsWith('.json'))) {
    const locale = JSON.parse(fs.readFileSync(path.join(localeDir, name), 'utf8'));
    for (const key of required) {
      assert.strictEqual(typeof locale.settings?.[key], 'string', `${name} is missing settings.${key}`);
      assert.ok(locale.settings[key].trim(), `${name} has an empty settings.${key}`);
    }
  }
});

test('Android updater backend and generated native wiring are present', () => {
  assert.ok(exists('app/src-tauri/src/commands/app_update.rs'), 'Android updater backend missing');
  const backend = read('app/src-tauri/src/commands/app_update.rs');
  for (const name of [
    'cmd_mobile_check_update',
    'cmd_android_download_update',
    'cmd_android_can_install_packages',
    'cmd_android_open_install_permission',
    'cmd_android_install_update',
  ]) {
    assert.ok(backend.includes(name), `${name} missing`);
  }
  assert.ok(backend.includes('app-update-download-progress'), 'Android progress event missing');
  assert.ok(backend.includes('FileProvider'), 'Android FileProvider installer path missing');
  assert.ok(backend.includes('getPackageName'), 'Android FileProvider authority is not derived from the runtime package name');
  assert.ok(!backend.includes('const FILE_PROVIDER_AUTHORITY'), 'Android FileProvider authority must not be hard-coded');
  assert.ok(
    !backend.includes('get_string((&jni::objects::JString::from('),
    'Android JNI strings must be bound before get_string so the borrowed JavaStr outlives the call',
  );
  assert.ok(backend.includes('mobile_manifest_targets_platform'), 'backend mobile platform targeting guard missing');

  assert.ok(exists('scripts/configure-android-updater.mjs'), 'Android native configurator missing');
  const config = JSON.parse(read('app/src-tauri/tauri.conf.json'));
  assert.ok(config.build.beforeBuildCommand.includes('configure-android-updater'), 'Android updater configurator not wired into builds');
});

test('release workflow publishes signed desktop updater assets and verified Android manifest', () => {
  const workflow = read('.github/workflows/release.yml');
  assert.ok(workflow.includes('TAURI_SIGNING_PRIVATE_KEY'), 'desktop updater signing private key is not supplied to CI');
  assert.ok(workflow.includes('uploadUpdaterJson: true'), 'tauri-action latest.json upload not enabled');
  assert.ok(workflow.includes('uploadUpdaterSignatures: true'), 'tauri-action signature upload not enabled');
  assert.ok(workflow.includes('mobile-latest.json'), 'mobile latest manifest is not uploaded to the release');
  assert.ok(workflow.includes('mobile-latest.json.sig'), 'signed mobile manifest is not uploaded to the release');
  assert.ok(workflow.includes('gh release upload'), 'Android release assets are not attached to the GitHub Release');
  assert.ok(exists('scripts/build-mobile-update-manifest.mjs'), 'mobile manifest generator missing');
});

test('desktop updater permissions are isolated from mobile capabilities', () => {
  const defaultCapability = JSON.parse(read('app/src-tauri/capabilities/default.json'));
  assert.ok(!defaultCapability.permissions.includes('updater:default'), 'updater permission leaked into all-platform default capability');
  assert.ok(!defaultCapability.permissions.includes('process:default'), 'process permission leaked into all-platform default capability');
  assert.ok(exists('app/src-tauri/capabilities/desktop-updater.json'), 'desktop updater capability missing');
  const desktopCapability = JSON.parse(read('app/src-tauri/capabilities/desktop-updater.json'));
  assert.deepStrictEqual([...desktopCapability.platforms].sort(), ['linux', 'macOS', 'windows'].sort());
  assert.ok(desktopCapability.permissions.includes('updater:default'));
  assert.ok(desktopCapability.permissions.includes('process:default'));
  assert.ok(exists('app/src-tauri/capabilities/ios-updater.json'), 'iOS updater capability missing');
  const iosCapability = JSON.parse(read('app/src-tauri/capabilities/ios-updater.json'));
  assert.deepStrictEqual(iosCapability.platforms, ['iOS']);
  assert.ok(iosCapability.permissions.includes('opener:default'), 'iOS update page cannot be opened');
});

test('mobile releases are only surfaced when the current mobile platform has a target', () => {
  const updater = loadUpdaterModule();
  const androidOnly = updater.normalizeMobileUpdateManifest({
    version: '0.1.2',
    android: {
      url: 'https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay.apk',
      sha256: 'c'.repeat(64),
    },
  });
  assert.strictEqual(updater.mobileManifestTargetsPlatform(androidOnly, 'android'), true);
  assert.strictEqual(updater.mobileManifestTargetsPlatform(androidOnly, 'ios'), false);

  const iosOnly = updater.normalizeMobileUpdateManifest({
    version: '0.1.2',
    iosStoreUrl: 'https://testflight.apple.com/join/example',
  });
  assert.strictEqual(updater.mobileManifestTargetsPlatform(iosOnly, 'android'), false);
  assert.strictEqual(updater.mobileManifestTargetsPlatform(iosOnly, 'ios'), true);
});

test('Android 7 unknown-source permission path is implemented instead of blindly returning true', () => {
  const backend = read('app/src-tauri/src/commands/app_update.rs');
  assert.ok(backend.includes('install_non_market_apps'), 'legacy Android unknown-source setting is not checked');
  assert.ok(backend.includes('android/provider/Settings$Secure'), 'legacy Android Settings.Secure path is missing');
});

process.on('exit', () => {
  if (process.exitCode) {
    console.error('APP-UPDATER-REGRESSION-FAIL');
  } else {
    console.log('APP-UPDATER-REGRESSION-PASS');
  }
});
