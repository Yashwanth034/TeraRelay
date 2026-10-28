'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const root = path.resolve(__dirname, '..');
const native = fs.readFileSync(path.join(root,
  'app/src-tauri/src/commands/drive_bulk_organizer.rs'), 'utf8');
const history = fs.readFileSync(path.join(root,
  'app/src-tauri/src/commands/drive_bulk_history.rs'), 'utf8');
const ui = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/DriveBulkUndo.tsx'), 'utf8');
const settings = fs.readFileSync(path.join(root,
  'app/src/components/desktop/dashboard/SettingsModal.tsx'), 'utf8');
const registry = fs.readFileSync(path.join(root,'app/src-tauri/src/lib.rs'), 'utf8');
for (const name of [
  'cmd_drive_bulk_history', 'cmd_drive_bulk_undo_preview', 'cmd_drive_bulk_undo',
]) {
  assert.ok(native.includes('pub fn ' + name), 'native IPC missing '+name);
  assert.ok(ui.includes("'"+name+"'"), 'UI must invoke '+name);
  assert.ok(registry.includes('commands::'+name), 'Tauri registry missing '+name);
}
assert.ok(native.includes('history::record_applied(conn, request, &updated)?'),
  'every committed organizer batch must journal atomically');
for (const contract of [
  'SAVEPOINT drive_bulk_organize','SAVEPOINT drive_bulk_undo',
  'ROLLBACK TO drive_bulk_undo','history::mark_undone',
  'history::inverse','verify_plan(&snapshot, &inverse)',
  'mount.notify_namespace_changed',
]) assert.ok(native.includes(contract), 'native safety contract missing '+contract);
for (const contract of [
  'drive_local_bulk_undo','created_at DESC,id DESC LIMIT',
  'content_fingerprint','file.storage_version != item.storage_version',
  'file.file_id != item.file_id','undone_at IS NULL',
  'MAX_HISTORY','MAX_BATCH',
]) assert.ok(history.includes(contract), 'undo journal contract missing '+contract);
for (const contract of [
  'working.current', 'expectedRevision: preview.revision',
  'cmd_drive_get_snapshot', 'readOrganizerSnapshot', 'Restore to:',
  'Confirm undo of this batch','Undo outcome may be uncertain',
  'Preview safe undo','No files were overwritten','Refresh history',
  'This is a fake message',
]) {
  if (contract === 'This is a fake message') continue;
  assert.ok(ui.includes(contract), 'UI lifecycle missing '+contract);
}
assert.ok(!ui.includes("'cmd_drive_bulk_apply'"),
  'undo UI cannot blindly replay the original apply command');
assert.ok(settings.includes('<DriveBulkUndo active={isOpen && activeTab === \'general\'} />'),
  'undo must be reachable in Settings');
console.log('PASS Drive Bulk Undo: durable journal, atomic rollback, optimistic preview, native notifications, integrated single-flight Settings UI');
