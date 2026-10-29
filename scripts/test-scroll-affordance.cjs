const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');

function setup() {
  let next = 0;
  const frames = new Map(), timers = new Map(), values = new Map(), listeners = new Map(), observers = [];
  const element = {
    clientHeight: 200, scrollHeight: 1000, scrollTop: 0, firstElementChild: null, lastElementChild: null,
    dataset: {},
    style: { setProperty: (key, value) => values.set(key, value), removeProperty: key => values.delete(key) },
    addEventListener: (type, fn, options) => listeners.set(type, { fn, options }),
    removeEventListener: (type, fn) => { if (listeners.get(type)?.fn === fn) listeners.delete(type); },
  };
  class ResizeObserver {
    constructor(fn) { this.fn = fn; observers.push(this); }
    observe() {}
    disconnect() { this.disconnected = true; }
  }
  const module = { exports: {} };
  vm.runInNewContext(ts.transpileModule(fs.readFileSync('app/src/hooks/useScrollAffordance.ts', 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText, { module, exports: module.exports, require: () => ({ useEffect() {} }), ResizeObserver,
    requestAnimationFrame: fn => { const id = ++next; frames.set(id, fn); return id; },
    cancelAnimationFrame: id => frames.delete(id),
    window: {
      setTimeout: fn => { const id = ++next; timers.set(id, fn); return id; },
      clearTimeout: id => timers.delete(id),
    },
  });
  const flush = () => { const pending = [...frames.values()]; frames.clear(); pending.forEach(fn => fn()); };
  return { element, values, listeners, observers, frames, timers, flush,
    attach: () => module.exports.attachScrollAffordance(element) };
}
test('native scroll edges follow overflow and clamp overscroll', () => {
  const h = setup(), cleanup = h.attach();
  assert.equal(h.values.get('--tr-scroll-top'), '0px');
  assert.equal(h.values.get('--tr-scroll-bottom'), '16px');
  assert.equal(h.listeners.get('scroll').options.passive, true);
  h.element.scrollTop = 800; h.listeners.get('scroll').fn(); h.flush();
  assert.equal(h.values.get('--tr-scroll-top'), '16px');
  assert.equal(h.values.get('--tr-scroll-bottom'), '0px');
  h.element.scrollTop = -20; h.listeners.get('scroll').fn(); h.flush();
  assert.equal(h.values.get('--tr-scroll-top'), '0px');
  h.element.scrollHeight = 150;
  h.observers[0].fn(); h.flush();
  assert.equal(h.values.get('--tr-scroll-bottom'), '0px');
  cleanup();
});
test('scroll events coalesce without React state and cleanup releases every resource', () => {
  const h = setup(), cleanup = h.attach();
  for (let i = 0; i < 20; i++) h.listeners.get('scroll').fn();
  assert.equal(h.frames.size, 1);
  assert.equal(h.timers.size, 1);
  assert.equal(h.element.dataset.scrollActive, 'true');
  h.flush();
  const hide = [...h.timers.values()][0]; h.timers.clear(); hide();
  assert.equal(h.element.dataset.scrollActive, undefined);
  h.listeners.get('scroll').fn();
  cleanup();
  assert.equal(h.frames.size, 0);
  assert.equal(h.timers.size, 0);
  assert.equal(h.listeners.size, 0);
  assert.equal(h.values.size, 0);
  assert.ok(h.observers.every(o => o.disconnected));
});
