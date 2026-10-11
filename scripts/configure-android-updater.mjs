#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';

function argument(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : null;
}

function appVersion() {
  const explicit = argument('--version');
  const value = explicit ?? JSON.parse(fs.readFileSync(path.resolve(process.cwd(), 'package.json'), 'utf8')).version;
  const match = /^(?:v)?(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/.exec(String(value).trim());
  if (!match) throw new Error(`Invalid Android app version: ${value}`);
  const major = Number(match[1]);
  const minor = Number(match[2]);
  const patch = Number(match[3]);
  const versionCode = major * 1_000_000 + minor * 1_000 + patch;
  if (!Number.isSafeInteger(versionCode) || versionCode <= 0 || versionCode > 2_100_000_000) {
    throw new Error(`Android versionCode is outside the supported range: ${versionCode}`);
  }
  const suffix = match[4] ? `-${match[4]}` : '';
  const build = match[5] ? `+${match[5]}` : '';
  return { versionName: `${major}.${minor}.${patch}${suffix}${build}`, versionCode };
}

const projectRoot = path.resolve(
  process.cwd(),
  argument('--project-root') ?? 'src-tauri/gen/android',
);
const manifestPath = path.join(projectRoot, 'app', 'src', 'main', 'AndroidManifest.xml');
const pathsXmlPath = path.join(
  projectRoot,
  'app',
  'src',
  'main',
  'res',
  'xml',
  'terarelay_update_paths.xml',
);
const tauriPropertiesPath = path.join(projectRoot, 'app', 'tauri.properties');

if (!fs.existsSync(manifestPath)) {
  console.log(`Android updater config skipped: ${manifestPath} does not exist`);
  process.exit(0);
}

let manifest = fs.readFileSync(manifestPath, 'utf8');
const permission = '    <uses-permission android:name="android.permission.REQUEST_INSTALL_PACKAGES" />';
const provider = `        <provider
            android:name="androidx.core.content.FileProvider"
            android:authorities="\${applicationId}.fileprovider"
            android:exported="false"
            android:grantUriPermissions="true">
            <meta-data
                android:name="android.support.FILE_PROVIDER_PATHS"
                android:resource="@xml/terarelay_update_paths" />
        </provider>`;

if (!manifest.includes('android.permission.REQUEST_INSTALL_PACKAGES')) {
  const applicationIndex = manifest.indexOf('    <application');
  if (applicationIndex < 0) {
    throw new Error('AndroidManifest.xml does not contain an <application> element');
  }
  manifest = `${manifest.slice(0, applicationIndex)}${permission}\n${manifest.slice(applicationIndex)}`;
}

if (!manifest.includes('androidx.core.content.FileProvider')) {
  const closeApplication = manifest.indexOf('    </application>');
  if (closeApplication < 0) {
    throw new Error('AndroidManifest.xml does not contain a closing </application> element');
  }
  manifest = `${manifest.slice(0, closeApplication)}${provider}\n${manifest.slice(closeApplication)}`;
}

fs.writeFileSync(manifestPath, manifest);
fs.mkdirSync(path.dirname(pathsXmlPath), { recursive: true });
const pathsXml = `<?xml version="1.0" encoding="utf-8"?>
<paths xmlns:android="http://schemas.android.com/apk/res/android">
    <cache-path name="terarelay_updates" path="app-updates/" />
</paths>
`;
if (!fs.existsSync(pathsXmlPath) || fs.readFileSync(pathsXmlPath, 'utf8') !== pathsXml) {
  fs.writeFileSync(pathsXmlPath, pathsXml);
}

const { versionName, versionCode } = appVersion();
const tauriProperties = `tauri.android.versionName=${versionName}\ntauri.android.versionCode=${versionCode}\n`;
if (!fs.existsSync(tauriPropertiesPath) || fs.readFileSync(tauriPropertiesPath, 'utf8') !== tauriProperties) {
  fs.writeFileSync(tauriPropertiesPath, tauriProperties);
}

console.log(`Android updater config ready: ${manifestPath}`);
