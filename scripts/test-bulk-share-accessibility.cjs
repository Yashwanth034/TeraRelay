const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');

const dashboard = readFileSync(join(__dirname, '../app/src/components/desktop/DesktopDashboard.tsx'), 'utf8');
const from = dashboard.indexOf('            {/* Bulk Share Results Modal */}');
const to = dashboard.indexOf('\n        </div>\n    );', from);
assert.ok(from >= 0 && to > from, 'Bulk Share Results modal must be located');
const popup = dashboard.slice(from, to);
const tick = String.fromCharCode(96);
const interpolation = '$' + '{file.name}';

function missingA11y(source) {
  const required = [
    ['accessible dialog role', /role="dialog"\s+aria-modal="true"\s+aria-labelledby="tr-bulk-share-links-title"/],
    ['named dialog heading', /<h3 id="tr-bulk-share-links-title"/],
    ['named Close action', /<button type="button" aria-label="Close share links" autoFocus onClick=\{\(\) => setBulkShareLinks\(null\)\}/],
    ['readonly share link names', 'aria-label={' + tick + 'Share link for ' + interpolation + tick + '}'],
    ['named Copy or Copied action', 'aria-label={isCopied ? ' + tick + 'Copied share link for ' + interpolation + tick + ' : ' + tick + 'Copy share link for ' + interpolation + tick + '}'],
    ['named native share action', 'aria-label={' + tick + 'Share link for ' + interpolation + ' with another app' + tick + '}'],
    ['Done closes modal', /<button\s+type="button"\s+onClick=\{\(\) => setBulkShareLinks\(null\)\}\s+className="tr-button/],
  ];
  return required.filter(([, value]) => !(typeof value === 'string' ? source.includes(value) : value.test(source))).map(([label]) => label);
}

test('Bulk Share dialog identifies itself and every action', () => {
  assert.deepEqual(missingA11y(popup), []);
});

test('negative: losing modal semantics fails', () => {
  assert.ok(missingA11y(popup.replace('role="dialog"', '')).includes('accessible dialog role'));
});

test('negative: losing the icon-only Close label fails', () => {
  assert.ok(missingA11y(popup.replace('aria-label="Close share links"', '')).includes('named Close action'));
});

test('negative: removing share-link input label fails', () => {
  const label = 'aria-label={' + tick + 'Share link for ' + interpolation + tick + '}';
  assert.ok(missingA11y(popup.replace(label, '')).includes('readonly share link names'));
});
