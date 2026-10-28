const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const settings = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
const topbar = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/TopBar.tsx'), 'utf8');

function missingContracts(markup, header) {
  const checks = new Map([
    ['named Settings dialog', /role="dialog"\s+aria-modal="true"\s+aria-labelledby="tr-settings-dialog-title"/],
    ['Settings heading', /<h2\s+id="tr-settings-dialog-title"/],
    ['named Close control', /aria-label=\{t\('common\.close'\)\}/],
    ['close focus target', /ref=\{closeButtonRef\}/],
    ['dialog focus trap', /onKeyDown=\{trapDialogTab\}/],
    ['focus first on open', /requestAnimationFrame\(\(\) => closeButtonRef\.current\?\.focus/],
    ['restore previous focus on close', /returnFocus\.focus\(\{ preventScroll: true \}\)/],
    ['language select', /<select\s+aria-label=\{t\('settings\.app_language'\)\}/],
    ['proxy select', /<select\s+aria-label=\{t\('common\.proxy_type'\)\}/],
    ['Hide Groups toggle', /aria-label=\{t\('common\.hide_groups'\)\}\s+aria-pressed=\{settings\.hideGroups\}/],
    ['Proxy toggle', /aria-label=\{t\('common\.enable_proxy'\)\}\s+aria-pressed=\{settings\.proxyEnabled\}/],
    ['proxy host', /aria-label=\{t\('common\.host'\)\}\s+value=\{settings\.proxyHost\}/],
    ['proxy port', /aria-label=\{t\('common\.port'\)\}\s+value=\{settings\.proxyPort\}/],
    ['proxy username', /aria-label=\{t\('common\.username'\)\}\s+value=\{settings\.proxyUsername\}/],
    ['proxy password', /aria-label=\{t\('common\.password'\)\}\s+value=\{settings\.proxyPassword\}/],
    ['upload decrement', /updateSetting\('maxConcurrentUploads',[^\n]+\n\s+aria-label=/],
    ['download increment', /updateSetting\('maxConcurrentDownloads',[^\n]+\n\s+aria-label=/],
  ]);
  const missing = [...checks].filter(([, re]) => !re.test(markup)).map(([key]) => key);
  if (!header.includes('channelMode && onChannelInfo ? (') ||
      !header.includes('<div className="tr-topbar__identity min-w-0"')) {
    missing.push('noninteractive workspace identity');
  }
  return missing;
}

test('Settings and TopBar accessibility source contracts are present', () => {
  assert.deepEqual(missingContracts(settings, topbar), []);
});

test('negative: removing dialog semantics is detected', () => {
  const modified = settings.replace('role="dialog"', '');
  assert.ok(missingContracts(modified, topbar).includes('named Settings dialog'));
});

test('negative: removing proxy select label is detected', () => {
  const modified = settings.replace("aria-label={t('common.proxy_type')}", '');
  assert.ok(missingContracts(modified, topbar).includes('proxy select'));
});

test('negative: removing the Settings keyboard trap is detected', () => {
  const modified = settings.replace('onKeyDown={trapDialogTab}', '');
  assert.ok(missingContracts(modified, topbar).includes('dialog focus trap'));
});

test('negative: removing focus restoration is detected', () => {
  const modified = settings.replace('returnFocus.focus({ preventScroll: true })', '');
  assert.ok(missingContracts(modified, topbar).includes('restore previous focus on close'));
});

test('negative: making the workspace title interactive is detected', () => {
  const modified = topbar.replace('<div className="tr-topbar__identity min-w-0"',
    '<button disabled className="tr-topbar__identity min-w-0"');
  assert.ok(missingContracts(settings, modified).includes('noninteractive workspace identity'));
});
