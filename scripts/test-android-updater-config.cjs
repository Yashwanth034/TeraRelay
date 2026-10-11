#!/usr/bin/env node
const assert = require('assert');
const fs = require('fs');
const path = require('path');
const cp = require('child_process');

const root = path.resolve(__dirname, '..');
const fixture = path.join(root, '.superpowers', 'sdd', '2026-10-10-terarelay-app-updater', 'android-config-fixture');
const manifest = path.join(fixture, 'app', 'src', 'main', 'AndroidManifest.xml');
const pathsXml = path.join(fixture, 'app', 'src', 'main', 'res', 'xml', 'terarelay_update_paths.xml');
const tauriProperties = path.join(fixture, 'app', 'tauri.properties');

fs.rmSync(fixture, { recursive: true, force: true });
fs.mkdirSync(path.dirname(manifest), { recursive: true });
fs.writeFileSync(manifest, `<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
    <uses-permission android:name="android.permission.INTERNET" />
    <application android:label="TeraRelay" android:theme="@style/AppTheme">
        <activity android:name=".MainActivity" android:exported="true" />
    </application>
</manifest>
`);

const script = path.join(root, 'scripts', 'configure-android-updater.mjs');
assert.ok(fs.existsSync(script), 'configure-android-updater.mjs is missing');

function run() {
  return cp.execFileSync(process.execPath, [script, '--project-root', fixture, '--version', '1.2.3'], {
    cwd: root,
    encoding: 'utf8',
  });
}

run();
const firstManifest = fs.readFileSync(manifest, 'utf8');
assert.match(firstManifest, /android\.permission\.REQUEST_INSTALL_PACKAGES/);
assert.match(firstManifest, /androidx\.core\.content\.FileProvider/);
assert.match(firstManifest, /\$\{applicationId\}\.fileprovider/);
assert.match(firstManifest, /android\.support\.FILE_PROVIDER_PATHS/);
assert.ok(fs.existsSync(pathsXml), 'FileProvider paths XML was not created');
const firstPaths = fs.readFileSync(pathsXml, 'utf8');
assert.match(firstPaths, /<cache-path[^>]+path="app-updates\/"/);
assert.ok(fs.existsSync(tauriProperties), 'tauri.properties fallback was not created');
const firstProperties = fs.readFileSync(tauriProperties, 'utf8');
assert.match(firstProperties, /^tauri\.android\.versionName=1\.2\.3$/m);
assert.match(firstProperties, /^tauri\.android\.versionCode=1002003$/m);

run();
const secondManifest = fs.readFileSync(manifest, 'utf8');
const secondPaths = fs.readFileSync(pathsXml, 'utf8');
const secondProperties = fs.readFileSync(tauriProperties, 'utf8');
assert.strictEqual(secondManifest, firstManifest, 'configurator is not idempotent for AndroidManifest.xml');
assert.strictEqual(secondPaths, firstPaths, 'configurator is not idempotent for paths XML');
assert.strictEqual(secondProperties, firstProperties, 'configurator is not idempotent for tauri.properties');
assert.strictEqual((secondManifest.match(/REQUEST_INSTALL_PACKAGES/g) || []).length, 1);
assert.strictEqual((secondManifest.match(/androidx\.core\.content\.FileProvider/g) || []).length, 1);

console.log('ANDROID-UPDATER-CONFIG-PASS');
