const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');

const settings = fs.readFileSync(
  path.join(__dirname, '../app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8'
);
const start = settings.indexOf('    const trapDialogTab = (event: React.KeyboardEvent<HTMLDivElement>) => {');
const end = settings.indexOf('\n    };', start);
assert.ok(start >= 0 && end > start, 'Actual SettingsModal Tab handler must be present');
const handler = settings.slice(start, end + '\n    };'.length);
const source = 'function makeTrap(dialogRef, document) {\n' + handler +
  '\n return trapDialogTab;\n}\nexports.makeTrap = makeTrap;';
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
});
const compiledExports = {};
vm.runInNewContext(compiled.outputText, { exports: compiledExports }, { timeout: 1000 });

function fixture({ initial = 'first', visible = [true, true, true] } = {}) {
  const document = { activeElement: null };
  function control(name, shown = true) {
    return {
      name,
      focus() { document.activeElement = this; },
      getClientRects() { return shown ? [{}] : []; },
    };
  }
  const buttons = [
    control('first', visible[0]),
    control('middle', visible[1]),
    control('last', visible[2]),
  ];
  const outside = control('outside');
  const root = {
    contains(element) { return buttons.includes(element); },
    querySelectorAll(selector) {
      assert.ok(selector.includes('button:not([disabled])'), 'disabled buttons must be excluded');
      return buttons;
    },
    focus() { document.activeElement = this; },
  };
  const elements = { first: buttons[0], middle: buttons[1], last: buttons[2], outside, root };
  document.activeElement = elements[initial];
  const trap = compiledExports.makeTrap({ current: root }, document);
  const press = (key, shift = false) => {
    let prevented = 0;
    trap({ key, shiftKey: shift, preventDefault() { prevented++; } });
    return { prevented, focus: document.activeElement };
  };
  return { press, elements, document };
}

test('Shift+Tab at first control wraps to last control', () => {
  const f = fixture();
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.last);
});

test('Tab at last control wraps to first control', () => {
  const f = fixture({ initial: 'last' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.first);
});

test('ordinary Tab from middle leaves native browser navigation intact', () => {
  const f = fixture({ initial: 'middle' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.elements.middle);
});

test('ordinary reverse Tab from middle leaves native browser navigation intact', () => {
  const f = fixture({ initial: 'middle' });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.elements.middle);
});

test('focus accidentally outside dialog is brought back to first on Tab', () => {
  const f = fixture({ initial: 'outside' });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.first);
});

test('focus accidentally outside dialog is brought back to last on Shift+Tab', () => {
  const f = fixture({ initial: 'outside' });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.last);
});

test('hidden controls are not used as wrap targets', () => {
  const f = fixture({ visible: [true, true, false] });
  const result = f.press('Tab', true);
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.middle);
});

test('empty dialog focuses its root instead of escaping', () => {
  const f = fixture({ visible: [false, false, false] });
  const result = f.press('Tab');
  assert.equal(result.prevented, 1);
  assert.equal(result.focus, f.elements.root);
});

test('non-Tab keys are never consumed by the focus trap', () => {
  const f = fixture();
  const result = f.press('Enter');
  assert.equal(result.prevented, 0);
  assert.equal(result.focus, f.elements.first);
});
