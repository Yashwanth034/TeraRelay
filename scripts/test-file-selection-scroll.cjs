const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('../app/node_modules/typescript');

// No DOM test runner is installed. Run the real components and handlers against
// a small lifecycle/geometry adapter; UI integration is verified separately.
function mount(name, imports = {}) {
  const slots = [], observers = [];
  let cursor = 0, pending = [];
  let scrollTop = 0;
  const scroller = {
    scrollHeight: 1000, clientHeight: 200,
    get scrollTop() { return scrollTop; },
    set scrollTop(value) { scrollTop = Math.max(0, Math.min(value, this.scrollHeight - this.clientHeight)); },
  };
  const element = (type, props, key) => ({ type, props: props || {}, key });
  const changed = (a, b) => !a || !b || a.length !== b.length || a.some((v, i) => v !== b[i]);
  const react = {
    useState(initial) {
      const i = cursor++;
      if (!(i in slots)) slots[i] = typeof initial === 'function' ? initial() : initial;
      return [slots[i], next => { slots[i] = typeof next === 'function' ? next(slots[i]) : next; }];
    },
    useRef(initial) {
      const i = cursor++;
      if (!(i in slots)) slots[i] = { current: initial };
      return slots[i];
    },
    useMemo(factory, deps) {
      const i = cursor++;
      if (!slots[i] || changed(slots[i].deps, deps)) slots[i] = { deps, value: factory() };
      return slots[i].value;
    },
    useCallback(fn, deps) { return react.useMemo(() => fn, deps); },
    useEffect(fn, deps) {
      const i = cursor++;
      if (!slots[i] || changed(slots[i].deps, deps)) {
        const previous = slots[i];
        slots[i] = { deps, cleanup: previous?.cleanup };
        pending.push(() => { slots[i].cleanup?.(); slots[i].cleanup = fn(); });
      }
    },
    useLayoutEffect(fn, deps) { react.useEffect(fn, deps); },
  };
  const primitive = new Proxy({}, { get: (_, key) => props => element(key, props) });
  const source = fs.readFileSync(path.join(__dirname, '../app/src/components/desktop/dashboard', name + '.tsx'), 'utf8');
  const module = { exports: {} };
  const window = { innerWidth: 1200, innerHeight: 800, addEventListener() {}, removeEventListener() {} };
  class ResizeObserver {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe() {}
    disconnect() { this.disconnected = true; }
  }
  const dependencies = {
    react, 'react/jsx-runtime': { jsx: element, jsxs: element, Fragment: 'Fragment' },
    'lucide-react': primitive, 'framer-motion': { motion: primitive },
    'react-i18next': { useTranslation: () => ({ t: key => key === 'files.select_all' ? 'Select All' : key }) },
    '@tauri-apps/api/core': { invoke: async () => ({ members: [] }) },
    '../../../context/ThemeContext': { useTheme: () => ({ theme: 'dark', toggleTheme() {} }) },
    '../../../context/SettingsContext': { useSettings: () => ({ settings: { language: 'en' } }) },
    '../../../utils': { displayFileName: value => value, formatBytes: String, isMediaFile: () => false, isPdfFile: () => false },
    '../../../filePresentation': { getPremiumFileTitle: file => file.name, getPremiumFileMeta: () => 'File' },
    '../../../hooks/useVideoMetadata': { useVideoMetadata: () => ({ data: null, isLoading: false }) },
    '../../../hooks/useCachedVariants': { useCachedVariants: () => ({ data: [] }) },
    sonner: { toast: { success() {}, error() {} } }, ...imports,
  };
  vm.runInNewContext(ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022 },
  }).outputText, { module, exports: module.exports, require: key => dependencies[key] || primitive, window, ResizeObserver, Date, Set, console, getComputedStyle: () => ({ paddingLeft: '0', paddingRight: '0' }) });
  return { scroller, observers, render(props) {
    cursor = 0; pending = [];
    const tree = module.exports[name](props);
    walk(tree, node => { if (node.props.ref) node.props.ref.current =
      node.props.className?.includes('tr-channel-scroll') ? scroller : {
        getBoundingClientRect: () => ({ width: 218, height: 300 }), clientWidth: 800,
      }; });
    pending.forEach(fn => fn());
    return tree;
  } };
}
function walk(tree, visit) {
  if (Array.isArray(tree)) return tree.forEach(node => walk(node, visit));
  if (!tree || typeof tree !== 'object') return;
  visit(tree); walk(tree.props?.children, visit);
}
function nodes(tree, predicate) { const found = []; walk(tree, node => { if (predicate(node)) found.push(node); }); return found; }
function byClass(tree, name) { return nodes(tree, node => node.props.className?.split(' ').includes(name)); }
function textOf(tree) {
  if (Array.isArray(tree)) return tree.map(textOf).join('');
  return tree && typeof tree === 'object' ? textOf(tree.props?.children) : String(tree || '');
}
const file = { id: 1, name: 'a.pdf', type: 'file', size: 10, created_at: '2026-01-01T00:00:00Z' };
const props = {
  files: [file], loading: false, error: null, selectedIds: [], activeFolderId: 10,
  onFileClick() {}, onToggleSelection() {}, onDownload() {}, onDownloadAll() {}, onPreview() {},
  onDelete() {}, onRename() {}, onFileMove() {}, onManualUpload() {}, onFolderUpload() {},
};
test('channel has no empty selection affordance before selection starts', () => {
  assert.equal(byClass(mount('ChannelFeed').render(props), 'tr-file-select').length, 0);
});
test('selection mode allows toggling another channel file', () => {
  const toggled = [];
  const tree = mount('ChannelFeed').render({ ...props, selectedIds: [2], onToggleSelection: id => toggled.push(id) });
  const buttons = byClass(tree, 'tr-file-select');
  assert.equal(buttons.length, 1); buttons[0].props.onClick({ stopPropagation() {} });
  assert.deepEqual(toggled, [1]);
});
test('context menu Select invokes selection and closes the menu', () => {
  const calls = [];
  const tree = mount('ContextMenu').render({ x: 0, y: 0, file, onDownload() {}, onPreview() {},
    onSelect: () => calls.push('select'), onClose: () => calls.push('close') });
  const action = nodes(tree, node => node.type === 'button' && textOf(node) === 'Select')[0];
  assert.ok(action, 'Select action should be present'); action.props.onClick();
  assert.deepEqual(calls, ['select', 'close']);
});
test('context menu offers Deselect for the selected file', () => {
  const tree = mount('ContextMenu').render({ x: 0, y: 0, file, selected: true, onSelect() {}, onClose() {}, onDownload() {}, onPreview() {} });
  assert.ok(nodes(tree, node => node.type === 'button' && textOf(node) === 'Deselect').length);
});
test('grid card has no initial selection circle and exposes a three-dot menu', () => {
  let opened = 0;
  const tree = mount('FileCard').render({ file, isSelected: false, selectionMode: false, onDelete() {}, onDownload() {}, onContextMenu: () => opened++ });
  assert.equal(byClass(tree, 'tr-library-select').length, 0);
  const action = nodes(tree, node => node.props['aria-label'] === 'More actions')[0];
  assert.ok(action, 'three-dot actions button should be available');
  action.props.onClick({ stopPropagation() {} }); assert.equal(opened, 1);
});
test('Select All appears only while selection is active and runs its handler', () => {
  let selectedAll = 0;
  const harness = mount('TopBar');
  const topProps = { currentFolderName: 'Channel', selectedIds: [], onSelectAll: () => selectedAll++ };
  assert.equal(nodes(harness.render(topProps), node => textOf(node) === 'Select All').length, 0);
  const action = nodes(harness.render({ ...topProps, selectedIds: [1] }), node => textOf(node) === 'Select All' && node.props.onClick)[0];
  assert.ok(action, 'selection toolbar should expose Select All'); action.props.onClick(); assert.equal(selectedAll, 1);
});
test('channel opens at newest after loading finishes', () => {
  const harness = mount('ChannelFeed'); harness.render({ ...props, loading: true }); harness.render(props);
  assert.equal(harness.scroller.scrollTop, 800);
});
test('new files follow the bottom while the reader is at the bottom', () => {
  const harness = mount('ChannelFeed'); harness.render(props); harness.scroller.scrollHeight = 1300;
  harness.render({ ...props, files: [file, { ...file, id: 2 }] }); assert.equal(harness.scroller.scrollTop, 1100);
});
test('upward scrolling is preserved across new files and content resizing', () => {
  const harness = mount('ChannelFeed'), tree = harness.render(props);
  harness.scroller.scrollTop = 100; byClass(tree, 'tr-channel-scroll')[0].props.onScroll?.({ currentTarget: harness.scroller });
  harness.scroller.scrollHeight = 1300; harness.render({ ...props, files: [file, { ...file, id: 2 }] });
  harness.observers.filter(o => !o.disconnected).forEach(o => o.callback()); assert.equal(harness.scroller.scrollTop, 100);
});
test('a different channel starts at newest after the previous one was scrolled upward', () => {
  const harness = mount('ChannelFeed'), tree = harness.render(props);
  harness.scroller.scrollTop = 100; byClass(tree, 'tr-channel-scroll')[0].props.onScroll?.({ currentTarget: harness.scroller });
  harness.render({ ...props, activeFolderId: 20 }); assert.equal(harness.scroller.scrollTop, 800);
});
test('asynchronous stack/content growth stays at the bottom on initial open', () => {
  const harness = mount('ChannelFeed'); harness.render(props); harness.scroller.scrollHeight = 1500;
  harness.observers.filter(o => !o.disconnected).forEach(o => o.callback()); assert.equal(harness.scroller.scrollTop, 1300);
});

test('returning to the bottom resumes following new files', () => {
  const harness = mount('ChannelFeed'), tree = harness.render(props);
  const onScroll = byClass(tree, 'tr-channel-scroll')[0].props.onScroll;
  harness.scroller.scrollTop = 100; onScroll({ currentTarget: harness.scroller });
  harness.scroller.scrollTop = 800; onScroll({ currentTarget: harness.scroller });
  harness.scroller.scrollHeight = 1400; harness.render({ ...props, files: [file, { ...file, id: 3 }] });
  assert.equal(harness.scroller.scrollTop, 1200);
});
test('channel three-dot menu routes Select to the file and closes', () => {
  const toggled = [];
  const harness = mount('ChannelFeed'), input = { ...props, onToggleSelection: id => toggled.push(id) };
  const tree = harness.render(input);
  const action = nodes(tree, node => node.props['aria-label'] === 'More actions for a.pdf')[0];
  action.props.onClick({ stopPropagation() {}, currentTarget: { getBoundingClientRect: () => ({ right: 100, bottom: 200 }) } });
  const menu = nodes(harness.render(input), node => node.props.file === file && node.props.onSelect)[0];
  assert.ok(menu); menu.props.onSelect(); menu.props.onClose();
  assert.deepEqual(toggled, [1]);
  assert.equal(nodes(harness.render(input), node => node.props.file === file && node.props.onSelect).length, 0);
});
test('library card selection mode and three-dot Select route to the same file', () => {
  const toggled = [];
  const harness = mount('FileExplorer', { '@tanstack/react-virtual': { useVirtualizer: options => ({
    getTotalSize: () => options.count * 48,
    getVirtualItems: () => Array.from({ length: options.count }, (_, index) => ({ index, start: index * 48 })),
    scrollToOffset() {}, measure() {},
  }) } });
  const input = { ...props, viewMode: 'grid', cardScale: 1, onToggleSelection: id => toggled.push(id) };
  const tree = harness.render(input);
  const card = nodes(tree, node => node.props.file === file && node.props.onContextMenu)[0];
  assert.equal(card.props.selectionMode, false);
  card.props.onContextMenu({ type: 'click', stopPropagation() {}, preventDefault() {}, currentTarget: { getBoundingClientRect: () => ({ right: 100, bottom: 200 }) } });
  const menu = nodes(harness.render(input), node => node.props.file === file && node.props.onSelect)[0];
  assert.ok(menu); menu.props.onSelect(); assert.deepEqual(toggled, [1]);
  const selectedCard = nodes(harness.render({ ...input, selectedIds: [2] }), node => node.props.file === file && node.props.onContextMenu)[0];
  assert.equal(selectedCard.props.selectionMode, true);
});
test('grid card preview remains independent of selection and context actions', () => {
  let previews = 0, selections = 0;
  const tree = mount('FileCard').render({
    file, isSelected: false, selectionMode: true, onDelete() {}, onDownload() {},
    onPreview: () => previews++, onToggleSelection: () => selections++,
  });
  nodes(tree, node => node.props['aria-label'] === 'Preview')[0].props.onClick({ stopPropagation() {} });
  assert.equal(previews, 1); assert.equal(selections, 0);
});
