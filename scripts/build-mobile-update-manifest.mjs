#!/usr/bin/env node
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

function arg(name, required = false) {
  const index = process.argv.indexOf(name);
  const value = index >= 0 ? process.argv[index + 1] : null;
  if (required && (!value || value.startsWith('--'))) {
    throw new Error(`Missing required argument ${name}`);
  }
  return value;
}

function normalizeVersion(value) {
  const normalized = String(value || '').trim().replace(/^v/i, '');
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?$/.test(normalized)) {
    throw new Error(`Invalid semantic version: ${value}`);
  }
  return normalized;
}

function validateAssetUrl(value) {
  const url = new URL(value);
  if (url.protocol !== 'https:') throw new Error('APK URL must use HTTPS');
  const trusted = new Set(['github.com', 'objects.githubusercontent.com', 'release-assets.githubusercontent.com']);
  if (!trusted.has(url.hostname.toLowerCase())) throw new Error(`Untrusted APK host: ${url.hostname}`);
  if (!url.pathname.toLowerCase().endsWith('.apk')) throw new Error('APK URL must end in .apk');
  return url.toString();
}

const version = normalizeVersion(arg('--version', true));
const apk = path.resolve(arg('--apk', true));
const apkUrl = validateAssetUrl(arg('--apk-url', true));
const output = path.resolve(arg('--output', true));
const notes = arg('--notes') || null;
const publishedAt = arg('--published-at') || new Date().toISOString();
const iosStoreUrl = arg('--ios-store-url') || null;

if (!fs.existsSync(apk) || !fs.statSync(apk).isFile()) throw new Error(`APK does not exist: ${apk}`);
if (iosStoreUrl) {
  const parsed = new URL(iosStoreUrl);
  if (parsed.protocol !== 'https:' || !['apps.apple.com', 'testflight.apple.com'].includes(parsed.hostname.toLowerCase())) {
    throw new Error('iOS store URL must use an Apple App Store or TestFlight HTTPS host');
  }
}
const published = new Date(publishedAt);
if (Number.isNaN(published.valueOf())) throw new Error(`Invalid --published-at value: ${publishedAt}`);

const sha256 = crypto.createHash('sha256').update(fs.readFileSync(apk)).digest('hex');
const manifest = {
  version,
  notes,
  publishedAt: published.toISOString(),
  android: { url: apkUrl, sha256 },
  iosStoreUrl,
};

fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Mobile update manifest written: ${output}`);
