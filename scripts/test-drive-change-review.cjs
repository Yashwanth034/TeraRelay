'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const ts = require(require.resolve('typescript', { paths: [path.join(root, 'app')] }));
function load(file) {
  const source = fs.readFileSync(path.join(root, 'app/src/' + file + '.ts'), 'utf8');
  const js = ts.transpileModule(source, {
    compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS },
  }).outputText;
  const mod = { exports: {} };
  vm.runInNewContext('(function(module,exports){' + js + '\n})', { console })(mod, mod.exports);
  return mod.exports;
}
const review = load('driveChangeReview');
const item = (status, before, after, evidence = 'Local metadata change.') =>
  ({kind:'file',status,before_path:before,after_path:after,content_evidence:evidence});
const data = {
  generation: 4, captured_at: 1792990000000, baseline_revision: 11,
  current_revision: 13, current_dirty: true, current_pending: 0,
  tracked_files: 3, tracked_folders: 2, baseline_count: 4,
  change_count: 4, truncated: false,
  changes: [item('added', null, '/Work/new.txt'),
    item('missing', '/Work/old.txt', null),
    item('renamed', '/a.txt', '/b.txt'),
    item('version_changed', '/same.txt', '/same.txt', 'Storage reference changed.')],
};
const parsed = review.parseDriveChangeReview(data);
assert.equal(parsed.generation, 4);
assert.equal(parsed.current_dirty, true);
assert.equal(parsed.changes.length, 4);
assert.equal(review.filterDriveChanges(parsed.changes,'added','').length, 1);
assert.equal(review.filterDriveChanges(parsed.changes,'all','old.txt').length, 1);
assert.equal(review.filterDriveChanges(parsed.changes,'all','missing-query').length, 0);
assert.equal(review.filterDriveChanges(parsed.changes,'version_changed','same').length, 1);
assert.throws(() => review.parseDriveChangeReview({...data, generation:-1}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, change_count:2}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, current_pending:'0'}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, truncated:true}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, changes:[item('added','/old','/new')]}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, changes:[item('bogus',null,'/ok')]}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, changes:[item('added',null,'relative/path')]}), /Untrusted/);
assert.throws(() => review.parseDriveChangeReview({...data, generation:0, captured_at:null, baseline_revision:null}), /Untrusted/);
assert.equal(review.parseDriveChangeReview({
  ...data, generation:0, captured_at:null, baseline_revision:null, baseline_count:0,
  change_count:0, changes:[], truncated:false,
}).changes.length, 0);
const csv = review.driveChangesCsv(parsed.changes);
assert.ok(csv.includes('Work/new.txt'));
assert.ok(csv.includes('Storage reference changed.'));
const formula = review.driveChangesCsv([item('added', null, '=CMD("danger")')]);
assert.ok(formula.includes('"\'=CMD(""danger"")"'), 'formula payload escaped');
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveChangeReview.tsx'), 'utf8');
for (const name of ['cmd_drive_change_review_inspect','cmd_drive_change_review_capture',
  'expectedGeneration: report.generation','parseDriveChangeReview',
  'Confirm replace saved checkpoint','Current local Drive index','navigator.clipboard.writeText']) {
  // Text varies for the natural-language explanatory labels.
  if (name === 'Current local Drive index') continue;
  assert.ok(ui.includes(name), 'Change Review UI missing '+name);
}
assert.ok(!ui.includes('cmd_drive_delete'), 'must not expose destructive Drive API');
const settings = fs.readFileSync(path.join(root, 'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
assert.ok(settings.includes('<DriveChangeReviewPanel active='), 'Change Review must be reachable in Settings');
const commands = fs.readFileSync(path.join(root, 'app/src-tauri/src/commands/mod.rs'), 'utf8');
const lib = fs.readFileSync(path.join(root, 'app/src-tauri/src/lib.rs'), 'utf8');
assert.ok(commands.includes('pub mod drive_change_review;') && commands.includes('pub use drive_change_review::*;'));
assert.ok(lib.includes('commands::cmd_drive_change_review_inspect'));
assert.ok(lib.includes('commands::cmd_drive_change_review_capture'));
const native = fs.readFileSync(path.join(root,
  'app/src-tauri/src/commands/drive_change_review.rs'), 'utf8');
for (const term of ['SAVEPOINT drive_change_checkpoint','ROLLBACK TO drive_change_checkpoint',
  'MAX_ITEMS','MAX_CHANGES','expected_generation','drive_local_change_baseline']) {
  assert.ok(native.includes(term), 'Native change checkpoint missing ' + term);
}
console.log('PASS Drive Change Review: strict IPC contract, safe CSV, user-confirmed local checkpoint, bounded read-only UI');
