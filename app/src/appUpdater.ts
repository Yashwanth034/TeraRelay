export const DESKTOP_UPDATE_ENDPOINT = 'https://github.com/Yashwanth034/TeraRelay/releases/latest/download/latest.json';
export const MOBILE_UPDATE_ENDPOINT = 'https://github.com/Yashwanth034/TeraRelay/releases/latest/download/mobile-latest.json';

const ANDROID_ASSET_HOSTS = new Set([
  'github.com',
  'objects.githubusercontent.com',
  'release-assets.githubusercontent.com',
]);

const IOS_STORE_HOSTS = new Set([
  'apps.apple.com',
  'testflight.apple.com',
]);

export interface MobileUpdateAsset {
  url: string;
  sha256: string;
}

export interface MobileUpdateManifest {
  version: string;
  notes: string | null;
  publishedAt: string | null;
  android: MobileUpdateAsset | null;
  iosStoreUrl: string | null;
}

export interface AppUpdateRelease {
  version: string;
  currentVersion: string;
  notes: string | null;
  publishedAt: string | null;
  platform: 'desktop' | 'android' | 'ios';
  android?: MobileUpdateAsset | null;
  iosStoreUrl?: string | null;
}

export type AppUpdateStatus =
  | 'idle'
  | 'checking'
  | 'available'
  | 'up-to-date'
  | 'downloading'
  | 'ready-to-install'
  | 'installing'
  | 'permission-required'
  | 'error';

interface ParsedVersion {
  major: number;
  minor: number;
  patch: number;
  prerelease: Array<number | string>;
}

function parseVersion(value: string): ParsedVersion {
  if (typeof value !== 'string') {
    throw new Error('Invalid version: expected a string');
  }

  const trimmed = value.trim().replace(/^v/i, '');
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/.exec(trimmed);
  if (!match) {
    throw new Error(`Invalid semantic version: ${value}`);
  }

  const prerelease = match[4]
    ? match[4].split('.').map((part) => (/^\d+$/.test(part) ? Number(part) : part))
    : [];

  return {
    major: Number(match[1]),
    minor: Number(match[2]),
    patch: Number(match[3]),
    prerelease,
  };
}

function comparePrerelease(left: ParsedVersion['prerelease'], right: ParsedVersion['prerelease']): number {
  if (left.length === 0 && right.length === 0) return 0;
  if (left.length === 0) return 1;
  if (right.length === 0) return -1;

  const max = Math.max(left.length, right.length);
  for (let index = 0; index < max; index += 1) {
    const a = left[index];
    const b = right[index];
    if (a === undefined) return -1;
    if (b === undefined) return 1;
    if (a === b) continue;

    if (typeof a === 'number' && typeof b === 'number') return a < b ? -1 : 1;
    if (typeof a === 'number') return -1;
    if (typeof b === 'number') return 1;
    return a < b ? -1 : 1;
  }

  return 0;
}

export function normalizeVersion(value: string): string {
  const parsed = parseVersion(value);
  const base = `${parsed.major}.${parsed.minor}.${parsed.patch}`;
  return parsed.prerelease.length > 0 ? `${base}-${parsed.prerelease.join('.')}` : base;
}

export function compareVersions(left: string, right: string): number {
  const a = parseVersion(left);
  const b = parseVersion(right);

  for (const key of ['major', 'minor', 'patch'] as const) {
    if (a[key] !== b[key]) return a[key] < b[key] ? -1 : 1;
  }

  return comparePrerelease(a.prerelease, b.prerelease);
}

export function isNewerVersion(candidate: string, current: string): boolean {
  return compareVersions(candidate, current) > 0;
}

export function mobileManifestTargetsPlatform(
  manifest: { android?: MobileUpdateAsset | null; iosStoreUrl?: string | null },
  platform: 'android' | 'ios',
): boolean {
  return platform === 'android' ? Boolean(manifest.android) : Boolean(manifest.iosStoreUrl);
}

function requireObject(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`Invalid ${label}: expected an object`);
  }
  return value as Record<string, unknown>;
}

function optionalString(value: unknown, label: string): string | null {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value !== 'string') throw new Error(`Invalid ${label}: expected a string`);
  return value;
}

function validateHttpsUrl(value: unknown, label: string, allowedHosts: Set<string>): string {
  if (typeof value !== 'string' || value.trim() === '') {
    throw new Error(`Invalid ${label}: URL is required`);
  }

  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    throw new Error(`Invalid ${label}: malformed URL`);
  }

  if (parsed.protocol !== 'https:') {
    throw new Error(`Invalid ${label}: HTTPS is required`);
  }
  if (!allowedHosts.has(parsed.hostname.toLowerCase())) {
    throw new Error(`Invalid ${label} host: ${parsed.hostname}`);
  }
  return parsed.toString();
}

export function normalizeMobileUpdateManifest(value: unknown): MobileUpdateManifest {
  const input = requireObject(value, 'mobile update manifest');
  if (typeof input.version !== 'string') {
    throw new Error('Invalid mobile update manifest version');
  }

  const version = normalizeVersion(input.version);
  const notes = optionalString(input.notes, 'release notes');
  const publishedAt = optionalString(input.publishedAt, 'publishedAt');
  if (publishedAt && Number.isNaN(Date.parse(publishedAt))) {
    throw new Error('Invalid publishedAt timestamp');
  }

  let android: MobileUpdateAsset | null = null;
  if (input.android !== undefined && input.android !== null) {
    const asset = requireObject(input.android, 'Android update asset');
    const url = validateHttpsUrl(asset.url, 'Android APK URL', ANDROID_ASSET_HOSTS);
    if (typeof asset.sha256 !== 'string' || !/^[a-fA-F0-9]{64}$/.test(asset.sha256)) {
      throw new Error('Invalid Android APK SHA-256');
    }
    android = { url, sha256: asset.sha256.toLowerCase() };
  }

  let iosStoreUrl: string | null = null;
  if (input.iosStoreUrl !== undefined && input.iosStoreUrl !== null && input.iosStoreUrl !== '') {
    iosStoreUrl = validateHttpsUrl(input.iosStoreUrl, 'iOS store URL', IOS_STORE_HOSTS);
  }

  if (!android && !iosStoreUrl) {
    throw new Error('Mobile update manifest has no Android or iOS update target');
  }

  return { version, notes, publishedAt, android, iosStoreUrl };
}
