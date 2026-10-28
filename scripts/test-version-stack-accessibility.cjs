const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');

const source = fs.readFileSync(
  path.join(__dirname, '../app/src/components/desktop/dashboard/VersionStackModal.tsx'), 'utf8'
);
const marker = '    const trapVersionStackTab = (event: React.KeyboardEvent<HTMLDivElement>) => {';
const start = source.indexOf(marker);
const end = source.indexOf('\n    };', start);
assert.ok(start >= 0 && end > start, 'Actual VersionStackModal keyboard handler required');
const handler = source.slice(start, end + '\n    };'.length);
const transpiled = ts.transpileModule(
  'function makeTrap(dialogRef, document) {\n' + handler +
  '\nreturn trapVersionStackTab;\n}\nexports.makeTrap = makeTrap;',
  { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }
);
const moduleExports = {};
vm.runInNewContext(transpiled.outputText, { exports: moduleExports }, { timeout: 1500 });

function fixture({ current = 'first', visibility = [true, true, true] } = {}) {
  const document = { activeElement: null };
  const nodes = ['first', 'middle', 'last'].map((name, i) => ({
    name,
    focus() { document.activeElement = this; },
    getClientRects() { return visibility[i] ? [{}] : []; },
  }));
  const outside = { name: 'outside', focus() { document.activeElement = this; } };
  const root = {
    name: 'root',
    focus() { document.activeElement = this; },
    contains(node) { return nodes.includes(node); },
    querySelectorAll(selector) {
      assert.match(selector, /button:not\(\[disabled\]\)/);
      return nodes;
    },
  };
  const index = { first: nodes[0], middle: nodes[1], last: nodes[2], outside, root };
  document.activeElement = index[current];
  const trap = moduleExports.makeTrap({ current: root }, document);
  return {
    nodes: index,
    press(key, shiftKey = false) {
      let blocked = 0;
      trap({ key, shiftKey, preventDefault() { blocked++; } });
      return { blocked, focus: document.activeElement };
    },
  };
}

test('Version Stack dialog exposes a named modal and an actionable Close button', () => {
  assert.match(source, /role="dialog"[\s\S]*?aria-modal="true"[\s\S]*?aria-labelledby="tr-version-stack-title"/);
  assert.match(source, /id="tr-version-stack-title"/);
  assert.match(source, /ref=\{closeButtonRef\}/);
  assert.match(source, /aria-label="Close"/);
  assert.match(source, /onKeyDown=\{trapVersionStackTab\}/);
});

test('Version Stack focuses Close and restores a connected file-row opener', () => {
  assert.match(source, /requestAnimationFrame\(\(\) => closeButtonRef\.current\?\.focus/);
  assert.match(source, /active !== document\.body/);
  assert.match(source, /fileActionsLabel = `More actions for \$\{file\.stack_name \|\| file\.name\}`/);
  assert.match(source, /returnTarget = previousFocus\?\.isConnected \? previousFocus : fileActions/);
  assert.match(source, /requestAnimationFrame\(\(\) => \{[\s\S]*?returnTarget\.focus/);
});

test('Shift+Tab wraps from first to last', () => {
  const f = fixture();
  assert.deepEqual(f.press('Tab', true), { blocked: 1, focus: f.nodes.last });
});

test('Tab wraps from last to first', () => {
  const f = fixture({ current: 'last' });
  assert.deepEqual(f.press('Tab'), { blocked: 1, focus: f.nodes.first });
});

test('Tab and Shift+Tab from middle use native navigation', () => {
  const f = fixture({ current: 'middle' });
  assert.equal(f.press('Tab').blocked, 0);
  assert.equal(f.press('Tab', true).blocked, 0);
});

test('escaped focus is returned to a dialog boundary', () => {
  const f = fixture({ current: 'outside' });
  assert.deepEqual(f.press('Tab'), { blocked: 1, focus: f.nodes.first });
  assert.deepEqual(f.press('Tab', true), { blocked: 1, focus: f.nodes.last });
});

test('hidden controls are excluded from boundary selection', () => {
  const f = fixture({ visibility: [true, true, false] });
  assert.deepEqual(f.press('Tab', true), { blocked: 1, focus: f.nodes.middle });
});

test('no visible controls focuses the dialog root', () => {
  const f = fixture({ visibility: [false, false, false] });
  assert.deepEqual(f.press('Tab'), { blocked: 1, focus: f.nodes.root });
});

test('non-Tab key does not alter focus', () => {
  const f = fixture();
  assert.deepEqual(f.press('Enter'), { blocked: 0, focus: f.nodes.first });
});
