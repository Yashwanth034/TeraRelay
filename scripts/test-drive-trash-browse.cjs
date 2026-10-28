'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
const helper = path.join(root, 'app/src/driveTrashBrowse.ts');
const js = ts.transpileModule(fs.readFileSync(helper, 'utf8'), { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 } }).outputText;
const moduleExports = {};
vm.runInNewContext(js, { exports: moduleExports }, { filename: helper });
const filter = moduleExports.filterDriveTrashEntries;
const filterAvailability = moduleExports.filterDriveTrashAvailability;
const filterAge = moduleExports.filterDriveTrashByAge;
const sort = moduleExports.sortDriveTrashEntries;
const normalize = moduleExports.normalizeDriveTrashEntries;
assert.throws(() => normalize(null), /not a list/, 'null is not a valid empty trash response');
assert.throws(() => normalize({ entries: [] }), /not a list/,
  'object responses must fail instead of silently erasing the last known list');
const validRow = Object.freeze({
  kind: 'file', id: 'same', name: 'keep.txt', deleted_at: 123456789,
  restorable: true, original_directory_id: null, size: 200,
});
const distinctKind = Object.freeze({
  kind: 'folder', id: 'same', name: 'keep-folder', deleted_at: 123456788,
  restorable: false, size: null,
});
const damaged = Object.freeze([
  validRow,
  Object.freeze({ ...validRow, name: 'duplicate-name.txt' }),
  distinctKind,
  Object.freeze({ ...validRow, id: 'wrong-kind', kind: 'unknown' }),
  Object.freeze({ ...validRow, id: '', name: 'empty-id' }),
  Object.freeze({ ...validRow, id: 'bad-nul\0' }),
  Object.freeze({ ...validRow, id: 'invalid-date', deleted_at: Number.NaN }),
  Object.freeze({ ...validRow, id: 'infinite-date', deleted_at: Infinity }),
  Object.freeze({ ...validRow, id: 'negative-date', deleted_at: -1 }),
  Object.freeze({ ...validRow, id: 'timestamp-string', deleted_at: '1234' }),
  Object.freeze({ ...validRow, id: 'maybe-restorable', restorable: 'true' }),
  Object.freeze({ ...validRow, id: 'blank-name', name: '   ' }),
  null,
]);
const sanitized = normalize(damaged);
assert.equal(sanitized.skipped, damaged.length - 2, 'bad identities and duplicates are counted');
assert.deepEqual(Array.from(sanitized.entries, entry => [entry.kind, entry.id].join(':')),
  ['file:same', 'folder:same'], 'first distinct type-qualified identities keep original order');
assert.equal(sanitized.entries[0].restorable, true);
assert.equal(sanitized.entries[1].restorable, false, 'normalizer never elevates restore eligibility');
assert.equal(sanitized.entries[1].original_directory_id, null,
  'missing optional native fields are sanitized without dropping valid rows');
assert.equal(damaged.length, 13, 'normalization must not mutate backend response');
const clean = normalize([validRow, distinctKind]);
assert.equal(clean.skipped, 0);
assert.equal(normalize([]).skipped, 0, 'genuinely empty deleted list is valid');
assert.equal(normalize([]).entries.length, 0);
const badAux = normalize([{
  ...validRow, id: 'safe', size: -1, original_directory_id: { bogus: 3 },
}]);
assert.equal(badAux.entries[0].size, null, 'corrupt optional size is not trusted');
assert.equal(badAux.entries[0].original_directory_id, null);
assert.equal(badAux.entries.length, 1, 'auxiliary metadata must not hide otherwise valid identities');

const items = Object.freeze([
  Object.freeze({ id: 'new', name: 'Report.PDF', kind: 'file', restorable: true }),
  Object.freeze({ id: 'folder', name: 'Monthly Reports', kind: 'folder', restorable: true }),
  Object.freeze({ id: 'link', name: 'report shortcut', kind: 'symlink', restorable: false }),
  Object.freeze({ id: 'old', name: 'notes.txt', kind: 'file', restorable: false }),
]);
const ids = (q,k) => Array.from(filter(items,q,k), it => it.id);
assert.deepEqual(ids('', 'all'), ['new','folder','link','old']);
assert.deepEqual(ids(' REPORT ', 'all'), ['new','folder','link']);
assert.deepEqual(ids('report', 'file'), ['new']);
assert.deepEqual(ids('', 'folder'), ['folder']);
assert.deepEqual(ids('', 'symlink'), ['link']);
assert.deepEqual(ids('missing', 'all'), []);
assert.equal(items[2].restorable, false);
const availabilityIds = criterion => Array.from(filterAvailability(items, criterion), it => it.id);
assert.deepEqual(availabilityIds('all'), ['new','folder','link','old']);
assert.deepEqual(availabilityIds('restorable'), ['new','folder'], 'ready files and folders retained');
assert.deepEqual(availabilityIds('unavailable'), ['link','old'], 'unavailable files and symlinks retained');
assert.deepEqual(Array.from(filterAvailability(filter(items,'report','all'), 'restorable'), it => it.id),
  ['new','folder'], 'availability composes with name search');
assert.deepEqual(Array.from(filterAvailability(filter(items,'report','symlink'), 'unavailable'), it => it.id),
  ['link'], 'availability composes with kind');
assert.deepEqual(Array.from(filterAvailability(filter(items,'missing','all'), 'restorable'), it => it.id),
  [], 'empty search remains empty after availability filter');
assert.equal(items[0].restorable, true, 'availability filter must not mutate native restore eligibility');

// Native tombstones use Unix milliseconds. Inclusive cutoff, future clock
// skew, and malformed timestamps must be deterministic and never mutate data.
const day = 24 * 60 * 60 * 1000;
const now = Date.UTC(2026, 9, 25, 12);
const timedItems = Object.freeze([
  Object.freeze({ id: 'now', deleted_at: now, restorable: true, name: 'Today', kind: 'file' }),
  Object.freeze({ id: 'day-edge', deleted_at: now - day, restorable: false, name: 'Yesterday', kind: 'file' }),
  Object.freeze({ id: 'day-out', deleted_at: now - day - 1, restorable: true, name: 'Old one', kind: 'folder' }),
  Object.freeze({ id: 'week-edge', deleted_at: now - 7 * day, restorable: false, name: 'Week edge', kind: 'symlink' }),
  Object.freeze({ id: 'week-out', deleted_at: now - 7 * day - 1, restorable: true, name: 'Older week', kind: 'file' }),
  Object.freeze({ id: 'month-edge', deleted_at: now - 30 * day, restorable: true, name: 'Month edge', kind: 'file' }),
  Object.freeze({ id: 'month-out', deleted_at: now - 30 * day - 1, restorable: true, name: 'Long ago', kind: 'file' }),
  Object.freeze({ id: 'future', deleted_at: now + 1, restorable: true, name: 'Clock ahead', kind: 'folder' }),
  Object.freeze({ id: 'invalid', deleted_at: Number.NaN, restorable: true, name: 'Bad tombstone', kind: 'file' }),
]);
const ageIds = range => Array.from(filterAge(timedItems, range, now), entry => entry.id);
assert.deepEqual(ageIds('24h'), ['now', 'day-edge'], 'last 24h includes boundary exactly');
assert.deepEqual(ageIds('7d'), ['now', 'day-edge', 'day-out', 'week-edge'],
  'last seven days include cutoff, excluding older and future entries');
assert.deepEqual(ageIds('30d'), ['now', 'day-edge', 'day-out', 'week-edge', 'week-out', 'month-edge'],
  'last thirty days include cutoff and omit future/invalid dates');
assert.equal(ageIds('all').length, timedItems.length, 'any time keeps all rows, even malformed dates');
const combinedTimes = sort(
  filterAge(filterAvailability(filter(timedItems, 'edge', 'all'), 'unavailable'), '30d', now),
  'name',
);
assert.deepEqual(Array.from(combinedTimes, entry => entry.id), ['week-edge'],
  'time window composes with search, availability and name sort');
assert.equal(timedItems[0].id, 'now', 'age filter never changes source order');
const sortable = Object.freeze([
  Object.freeze({ id: 'z', name: 'Zeta', deleted_at: 300, kind: 'file' }),
  Object.freeze({ id: 'a2', name: 'alpha', deleted_at: 200, kind: 'file' }),
  Object.freeze({ id: 'a1', name: 'Alpha', deleted_at: 200, kind: 'folder' }),
  Object.freeze({ id: 'b', name: 'beta', deleted_at: 100, kind: 'symlink' }),
]);
const sortedIds = order => Array.from(sort(sortable, order), item => item.id);
assert.deepEqual(sortedIds('newest'), ['z', 'a2', 'a1', 'b'], 'default keeps backend order');
assert.deepEqual(sortedIds('oldest'), ['b', 'a2', 'a1', 'z'], 'oldest stable for timestamp ties');
assert.deepEqual(sortedIds('name'), ['a2', 'a1', 'b', 'z'], 'name ordering is case-insensitive and stable');
assert.deepEqual(Array.from(sort(filter(sortable, 'alp', 'file'), 'oldest'), e => e.id), ['a2'], 'filter and sort compose');
assert.equal(sortable[0].id, 'z', 'sorting must not mutate the backend list');

// Restore triage: size ordering uses trustworthy native FILE byte sizes only.
// Unknown sizes, folders and symlinks are last; ties preserve native order.
const sizes = Object.freeze([
  Object.freeze({ id: 'unknown', name: 'Unknown', kind: 'file', size: null, deleted_at: 8 }),
  Object.freeze({ id: 'zero', name: 'Empty', kind: 'file', size: 0, deleted_at: 7 }),
  Object.freeze({ id: 'big', name: 'Video', kind: 'file', size: 8 * 1024 * 1024, deleted_at: 6 }),
  Object.freeze({ id: 'folder-size', name: 'Folder', kind: 'folder', size: 4 * 1024 * 1024, deleted_at: 5 }),
  Object.freeze({ id: 'small', name: 'Photo', kind: 'file', size: 2048, deleted_at: 4 }),
  Object.freeze({ id: 'same', name: 'Photo2', kind: 'file', size: 2048, deleted_at: 3 }),
  Object.freeze({ id: 'bad-size', name: 'Corrupt', kind: 'file', size: -1, deleted_at: 2 }),
  Object.freeze({ id: 'link', name: 'Shortcut', kind: 'symlink', size: 100, deleted_at: 1 }),
]);
assert.deepEqual(Array.from(sort(sizes, 'largest'), it => it.id),
  ['big', 'small', 'same', 'zero', 'unknown', 'folder-size', 'bad-size', 'link']);
assert.deepEqual(Array.from(sort(sizes, 'smallest'), it => it.id),
  ['zero', 'small', 'same', 'big', 'unknown', 'folder-size', 'bad-size', 'link']);
assert.deepEqual(Array.from(sort(sizes, 'newest'), it => it.id),
  Array.from(sizes, it => it.id), 'default order is never altered by size sorting');
assert.equal(sizes[0].size, null, 'size sorting must never mutate provider rows');
const formatBytes = moduleExports.formatDriveTrashBytes;
const summarizeSizes = moduleExports.summarizeSelectedDriveTrashSizes;
assert.equal(formatBytes(null), 'Size unknown');
assert.equal(formatBytes(-1), 'Size unknown');
assert.equal(formatBytes(Number.NaN), 'Size unknown');
assert.equal(formatBytes(0), '0 B');
assert.equal(formatBytes(1023), '1023 B');
assert.equal(formatBytes(1024), '1.0 KiB');
assert.equal(formatBytes(2048), '2.0 KiB');
assert.equal(formatBytes(1024 ** 3), '1.0 GiB');
const selected = summarizeSizes(sizes);
assert.equal(selected.knownBytes, BigInt(8 * 1024 * 1024 + 4096));
assert.equal(selected.knownFiles, 4, 'zero-byte files still count as known');
assert.equal(selected.unknownFiles, 2, 'unknown and corrupt file sizes must not be fabricated');
const extremelyLarge = summarizeSizes([
  { kind: 'file', size: Number.MAX_SAFE_INTEGER },
  { kind: 'file', size: Number.MAX_SAFE_INTEGER },
  { kind: 'folder', size: Number.MAX_SAFE_INTEGER },
]);
assert.equal(extremelyLarge.knownBytes, 2n * BigInt(Number.MAX_SAFE_INTEGER),
  'sum beyond Number safe precision must retain exact integer bytes');
assert.equal(extremelyLarge.knownFiles, 2);
assert.equal(extremelyLarge.unknownFiles, 0);
assert.match(formatBytes(extremelyLarge.knownBytes), /PiB|EiB/);

// Copy inventory is an explicit read-only, bounded export of the user's
// currently filtered and sorted display, not an internal identity dump.
const inventory = moduleExports.formatDriveTrashInventory;
const hazardous = Object.freeze([
  Object.freeze({ ...validRow, name: '=cmd|\tDROP\nprivate\rfile\u2028name', id: 'SECRET_NATIVE_ID', deleted_at: now, size: 2048 }),
  Object.freeze({ ...validRow, name: '\u0001+SUM(1,2)', id: 'another-secret', deleted_at: now - day, restorable: false, size: null }),
  Object.freeze({ ...distinctKind, id: 'folder-secret', name: 'Private folder', deleted_at: now, kind: 'folder' }),
]);
const copy = inventory(hazardous);
assert.equal(copy.exported, 3);
assert.equal(copy.omitted, 0);
assert.equal(copy.text.split('\n').length, 6, 'one safe line per inventory entry');
assert.match(copy.text, /2\.0 KiB/);
assert.match(copy.text, /Size unknown/);
assert.match(copy.text, /Not available/);
assert.match(copy.text, /2026-10-25T12:00:00\.000Z/);
assert.doesNotMatch(copy.text, /SECRET_NATIVE_ID|another-secret|folder-secret/,
  'never expose native identities in support copy');
assert.doesNotMatch(copy.text, /\r|\t|\u0001|\u2028/,
  'dangerous line and table separators are escaped');
assert.match(copy.text, /• =cmd/, 'spreadsheet formula cannot lead a row');
assert.match(copy.text, /• \+SUM/, 'second formula-like filename cannot lead a row');
const longList = Array.from({ length: 635 }, (_, i) => Object.freeze({
  ...validRow, id: 'row-' + i, name: 'Row ' + i, deleted_at: now - i,
}));
const bounded = inventory(longList);
assert.equal(bounded.exported, 500);
assert.equal(bounded.omitted, 135);
assert.match(bounded.text, /135 additional matches omitted/);
assert.doesNotMatch(bounded.text, /Row 634/);
assert.match(bounded.text, /Row 499/);
assert.equal(inventory([]).exported, 0);
const selectedFiltered = sort(filter(longList, 'Row 3', 'file'), 'name');
const filteredExport = inventory(selectedFiltered, 50);
assert.deepEqual(filteredExport.exported, Math.min(selectedFiltered.length, 50),
  'export uses the supplied filtered, sorted subset rather than all native rows');
assert.equal(longList.length, 635, 'export must not mutate native entries');

const modalPath = path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx');
const modal = fs.readFileSync(modalPath, 'utf8');
assert.match(modal, /filterDriveTrashEntries\(driveTrash, driveTrashSearch, driveTrashFilter\)/);
assert.match(modal, /aria-label="Search recently deleted Drive items"/);
assert.match(modal, /aria-label="Filter recently deleted Drive items by type"/);
assert.match(modal, /aria-label="Filter recently deleted Drive items by availability"/);
assert.match(modal, /aria-label="Filter recently deleted Drive items by age"/);
assert.match(modal, /filterDriveTrashByAge\(/);
assert.match(modal, /driveTrashAge, driveTrashClock, driveTrashSort/,
  'rolling age filter re-evaluates when the local clock advances');
assert.match(modal, /setDriveTrashAge\(event\.target\.value as DriveTrashAgeRange\)/);
assert.match(modal, /filterDriveTrashAvailability\(/);
assert.match(modal, /setDriveTrashAvailability\(event\.target\.value as DriveTrashAvailability\)/);
assert.match(modal, /aria-label="Copy filtered recently deleted Drive inventory"/);
assert.match(modal, /formatDriveTrashInventory\(visibleDriveTrash\)/,
  'Copy list must use live user filter/sort ordering');
assert.match(modal, /navigator\.clipboard\.writeText\(inventory\.text\)/,
  'Only explicit user action writes the sanitized inventory to clipboard');
assert.match(modal, /driveTrashInventoryCopyInFlightRef\.current/,
  'Copy list should not allow overlapping clipboard operations');
assert.match(modal, /aria-label="Sort recently deleted Drive items"/);
assert.match(modal, /<option value="largest">Largest files first<\/option>/);
assert.match(modal, /<option value="smallest">Smallest files first<\/option>/);
assert.match(modal, /selectedDriveTrashSizes\.knownFiles > 0/);
assert.match(modal, /selectedDriveTrashSizes\.unknownFiles > 0/);
assert.match(modal, /formatDriveTrashBytes\(selectedDriveTrashSizes\.knownBytes\)/);
assert.match(modal, /entry\.kind === 'file' \? ` · \$\{formatDriveTrashBytes\(entry\.size\)\}` : ''/,
  'size shown only for files, not folders and symlinks');
assert.match(modal, /sortDriveTrashEntries\(/);
assert.match(modal, /driveTrashSort.*'newest'/);
assert.match(modal, /Showing \{shownDriveTrash\.length\} of \{visibleDriveTrash\.length\} matching deleted items/);
assert.match(modal, /\{shownDriveTrash\.map\(entry =>/);
assert.match(modal, /aria-label="Show more recently deleted Drive items"/);
assert.match(modal, /aria-label="Reset recently deleted Drive filters"/);
assert.match(modal, /\{hasActiveDriveTrashFilters && \(/);
assert.match(modal, /onClick=\{resetDriveTrashFilters\}/);
assert.match(modal, /setDriveTrashVisibleLimit\(limit => Math\.min\(limit \+ 50, visibleDriveTrash\.length\)\)/);
assert.ok((modal.match(/setDriveTrashVisibleLimit\(50\)/g) || []).length >= 6,
  'search, type, availability, age, sort and closing Settings each reset paging');
assert.match(modal, /\{driveTrashError\s*&&\s*\(/);
assert.match(modal, /No deleted items match the current search, type, availability, or time filters/);
assert.match(modal, /!driveTrashError && driveTrash\.length === 0/);
// Evaluate the actual SettingsModal paging expression, not a duplicated
// test implementation, against a 125-row synthetic trash listing.
const modalAst = ts.createSourceFile(modalPath, modal, ts.ScriptTarget.ES2020, true, ts.ScriptKind.TSX);
const component = modalAst.statements.find(node => ts.isFunctionDeclaration(node)
  && node.name?.text === 'SettingsModal');
assert.ok(component);
const declarations = component.body.statements.filter(ts.isVariableStatement)
  .flatMap(node => Array.from(node.declarationList.declarations));
const getDeclaration = name => declarations.find(node => ts.isIdentifier(node.name)
  && node.name.text === name);
const resetDeclaration = getDeclaration('resetDriveTrashFilters');
const activeDeclaration = getDeclaration('hasActiveDriveTrashFilters');
assert.ok(resetDeclaration?.initializer, 'real Settings reset action exists');
assert.ok(activeDeclaration?.initializer, 'real conditional visibility expression exists');
const resetCode = ts.transpileModule(
  'globalThis.resetDriveFilters = ' + resetDeclaration.initializer.getText(modalAst) + ';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
const resetState = {
  search: 'report',
  kind: 'folder',
  availability: 'unavailable',
  age: '7d',
  sort: 'name',
  limit: 125,
};
const resetContext = {
  setDriveTrashSearch: value => { resetState.search = value; },
  setDriveTrashFilter: value => { resetState.kind = value; },
  setDriveTrashAvailability: value => { resetState.availability = value; },
  setDriveTrashAge: value => { resetState.age = value; },
  setDriveTrashSort: value => { resetState.sort = value; },
  setDriveTrashVisibleLimit: value => { resetState.limit = value; },
};
vm.runInNewContext(resetCode, resetContext);
resetContext.resetDriveFilters();
assert.deepEqual(resetState, {
  search: '', kind: 'all', availability: 'all', age: 'all',
  sort: 'newest', limit: 50,
}, 'single Reset filters action restores all defaults and first page');
const visibilityCode = ts.transpileModule(
  'globalThis.filtersActive = (driveTrashSearch, driveTrashFilter, driveTrashAvailability, driveTrashAge, driveTrashSort) => '
    + activeDeclaration.initializer.getText(modalAst) + ';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
const visibilityContext = {};
vm.runInNewContext(visibilityCode, visibilityContext);
const active = visibilityContext.filtersActive;
assert.equal(active('', 'all', 'all', 'all', 'newest'), false, 'reset control hidden at defaults');
assert.equal(active('   ', 'all', 'all', 'all', 'newest'), false, 'spaces are not an active search');
for (const args of [
  ['doc', 'all', 'all', 'all', 'newest'],
  ['', 'file', 'all', 'all', 'newest'],
  ['', 'all', 'restorable', 'all', 'newest'],
  ['', 'all', 'all', '24h', 'newest'],
  ['', 'all', 'all', 'all', 'oldest'],
]) {
  assert.equal(active(...args), true, 'any altered filter makes Reset visible');
}
// Exercise the actual age-timer React effect in a fake clock environment:
 // open+expanded+age-filtered is the only state that owns a timer.
const ageEffect = component.body.statements.find(node => ts.isExpressionStatement(node)
  && ts.isCallExpression(node.expression)
  && node.expression.expression.getText(modalAst) === 'useEffect'
  && node.expression.arguments[0]?.getText(modalAst).includes('setDriveTrashClock'));
assert.ok(ageEffect, 'active rolling age window must install a lifecycle-scoped timer');
assert.match(ageEffect.expression.arguments[1].getText(modalAst),
  /driveTrashAge/, 'clock effect depends on selected age range');
const ageScript = ts.transpileModule(
  'globalThis.runDriveAgeEffect = ' + ageEffect.expression.arguments[0].getText(modalAst) + ';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
let simulatedNow = 100 * day;
const timers = new Map();
let nextTimerId = 0;
const clockChanges = [];
const clockContext = {
  isOpen: false, driveTrashOpen: false, driveTrashAge: '24h',
  Date: { now: () => simulatedNow },
  setDriveTrashClock: nowMs => clockChanges.push(nowMs),
  window: {
    setInterval: (fn, interval) => {
      assert.equal(interval, 60_000, 'age clock ticks once per minute, not continuously');
      timers.set(++nextTimerId, fn);
      return nextTimerId;
    },
    clearInterval: id => { assert.equal(timers.delete(id), true); },
  },
};
vm.runInNewContext(ageScript, clockContext);
assert.equal(clockContext.runDriveAgeEffect(), undefined);
assert.equal(timers.size, 0, 'closed dialog must not schedule age filter work');
clockContext.isOpen = true;
assert.equal(clockContext.runDriveAgeEffect(), undefined);
assert.equal(timers.size, 0, 'collapsed panel must not schedule age filter work');
clockContext.driveTrashOpen = true;
clockContext.driveTrashAge = 'all';
assert.equal(clockContext.runDriveAgeEffect(), undefined);
assert.equal(timers.size, 0, 'all-time filter must not schedule age filter work');
clockContext.driveTrashAge = '24h';
const cancelAgeTimer = clockContext.runDriveAgeEffect();
assert.equal(clockChanges.at(-1), simulatedNow, 'enable filtered panel refreshes cutoff immediately');
assert.equal(timers.size, 1, 'expanded filtered panel owns one timer');
const lastTick = [...timers.values()][0];
simulatedNow += 120_000;
lastTick();
assert.equal(clockChanges.at(-1), simulatedNow, 'timer advances rolling cutoff without reloading native data');
const edgeItem = Object.freeze({ deleted_at: 100 * day - day + 60_000 });
assert.equal(filterAge([edgeItem], '24h', 100 * day).length, 1);
assert.equal(filterAge([edgeItem], '24h', simulatedNow).length, 0,
  'a row crossing the 24h boundary disappears after local clock tick');
cancelAgeTimer();
assert.equal(timers.size, 0, 'closing/collapsing panel clears interval');

const pageDeclaration = component.body.statements.filter(ts.isVariableStatement)
  .flatMap(node => Array.from(node.declarationList.declarations))
  .find(node => ts.isIdentifier(node.name) && node.name.text === 'shownDriveTrash');
assert.ok(pageDeclaration?.initializer, 'real SettingsModal paging expression must exist');
const pagingJs = ts.transpileModule(
  'globalThis.pageItems = (visibleDriveTrash, driveTrashVisibleLimit) => '
    + pageDeclaration.initializer.getText(modalAst) + ';',
  { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
).outputText;
const pageContext = {};
vm.runInNewContext(pagingJs, pageContext);
const pageItems = pageContext.pageItems;
const largeList = Object.freeze(Array.from({ length: 125 }, (_, n) => Object.freeze({
  id: 'fixture-' + n, name: 'File ' + String(n).padStart(3, '0'),
  kind: 'file', deleted_at: 125 - n,
})));
const idsOnPage = limit => Array.from(pageItems(largeList, limit), row => row.id);
assert.equal(idsOnPage(50).length, 50, 'initial page contains only 50 DOM rows');
assert.equal(idsOnPage(100).length, 100, 'Show more extends the same ordered rows');
assert.equal(idsOnPage(125).length, 125, 'last page exposes the complete list');
assert.equal(new Set(idsOnPage(125)).size, 125, 'incremental list has no duplicates');
assert.equal(largeList[50].id, idsOnPage(100)[50], 'stable paging preserves original order');
const searched = filter(largeList, 'File 10', 'file');
assert.equal(pageItems(searched, 50).length, 10, 'new search within 50 shows all matches');
const alphabetical = sort(largeList, 'name');
assert.equal(pageItems(alphabetical, 50)[0].name, 'File 000');
assert.equal(largeList.length, 125, 'paging never changes backend data');
console.log('PASS Drive Recently Deleted filters, one-click Reset, live rolling age timer, 50-row paging and errors');

// Execute the actual SettingsModal clipboard handler using deferred native UI
// futures: a second click must never overlap and stale closed dialogs cannot toast.
(async () => {
  const declaration = getDeclaration('copyFilteredDriveTrashInventory');
  assert.ok(declaration?.initializer, 'actual Settings copy inventory callback exists');
  const body = ts.transpileModule(
    'globalThis.copyInventory = ' + declaration.initializer.getText(modalAst) + ';',
    { compilerOptions: { module: ts.ModuleKind.None, target: ts.ScriptTarget.ES2020 } },
  ).outputText;
  const makeClipboard = (items, behavior = 'deferred') => {
    const copyCalls = [];
    const notifications = [];
    const states = [];
    let resolveCopy, rejectCopy;
    const pending = new Promise((resolve, reject) => {
      resolveCopy = resolve; rejectCopy = reject;
    });
    const context = {
      driveSettingsOpenRef: { current: true }, driveTrashOpen: true,
      driveTrashSessionRef: { current: 1 },
      driveTrashInventoryCopyInFlightRef: { current: false },
      visibleDriveTrash: items, driveTrashBusy: false, driveTrashError: null,
      formatDriveTrashInventory: inventory,
      setDriveTrashInventoryCopyBusy: value => states.push(value),
      navigator: { clipboard: { writeText: text => {
        copyCalls.push(text);
        return behavior === 'error' ? Promise.reject(new Error('no permission')) : pending;
      } } },
      toast: { success: message => notifications.push(['success', message]),
        error: message => notifications.push(['error', message]) },
    };
    vm.runInNewContext(body, context);
    return { context, copyCalls, notifications, states, resolveCopy, rejectCopy,
      run: () => context.copyInventory() };
  };
  const fresh = makeClipboard(hazardous);
  const first = fresh.run();
  await fresh.run();
  assert.equal(fresh.copyCalls.length, 1, 'rapid clicks must not overwrite clipboard concurrently');
  assert.deepEqual(fresh.states, [true], 'busy state updates before the native clipboard settles');
  assert.equal(fresh.copyCalls[0], copy.text, 'copied report must exactly match sanitized inventory');
  fresh.resolveCopy();
  await first;
  assert.deepEqual(fresh.states, [true, false]);
  assert.equal(fresh.notifications[0][0], 'success');
  const denied = makeClipboard([]);
  await denied.run();
  assert.equal(denied.copyCalls.length, 0, 'no clipboard writes on empty listing');
  const broken = makeClipboard(hazardous, 'error');
  await broken.run();
  assert.equal(broken.notifications[0][0], 'error', 'clipboard errors are actionable');
  assert.deepEqual(broken.states, [true, false]);
  const closed = makeClipboard(hazardous);
  const late = closed.run();
  closed.context.driveSettingsOpenRef.current = false;
  closed.context.driveTrashSessionRef.current++;
  closed.resolveCopy();
  await late;
  assert.equal(closed.notifications.length, 0, 'old Settings session cannot toast after closing');
  assert.deepEqual(closed.states, [true], 'closed dialog is never setState from late clipboard result');
  console.log('PASS Drive Recently Deleted copy: real clipboard handler, busy, error and stale lifecycle');
})().catch(error => { console.error(error); process.exitCode = 1; });
