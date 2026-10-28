import { validateDriveSnapshot } from './deviceContinuity';
import { normalizeDriveFinderSnapshot } from './driveLocalFinder';

export interface OrganizerFile {
  id: string;
  name: string;
  path: string;
  directoryId: string | null;
}
export interface OrganizerFolder {
  id: string;
  path: string;
}
export interface OrganizerSnapshot {
  revision: number;
  files: OrganizerFile[];
  folders: OrganizerFolder[];
  skipped: number;
}
export interface OrganizerOptions {
  find: string;
  replace: string;
  prefix: string;
  suffix: string;
  destinationId: string | null | 'same';
}
export interface OrganizerChange {
  entry_id: string;
  expected_name: string;
  expected_directory_id: string | null;
  new_name: string;
  new_directory_id: string | null;
}
export interface OrganizerRequest {
  revision: number;
  changes: OrganizerChange[];
}

const obj = (value: unknown): Record<string, unknown> | null =>
  !!value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : null;

/** Cross-check identities against the existing fail-closed Finder path parser. */
export function readOrganizerSnapshot(value: unknown): OrganizerSnapshot {
  const native = validateDriveSnapshot(value);
  const finder = normalizeDriveFinderSnapshot(value);
  const indexed = new Map(finder.entries.map(entry => [entry.kind + ':' + entry.id, entry]));
  const files: OrganizerFile[] = [];
  const seen = new Set<string>();
  for (const raw of native.files) {
    const r = obj(raw);
    const id = r?.entry_id;
    if (typeof id !== 'string' || seen.has(id)) continue;
    const entry = indexed.get('file:' + id);
    const directoryId = r?.directory_id;
    if (!entry || entry.kind !== 'file' || r?.display_name !== entry.name
        || !(directoryId === null || typeof directoryId === 'string')
        || (r?.inode_id !== null && r?.inode_id !== undefined)) continue;
    files.push({ id, name: entry.name, path: entry.path, directoryId });
    seen.add(id);
  }
  const folders: OrganizerFolder[] = finder.entries.filter(entry => entry.kind === 'folder')
    .map(entry => ({ id: entry.id, path: entry.path }));
  return { revision: native.revision, files, folders, skipped: finder.skipped + finder.truncated };
}

export function targetOrganizerName(name: string, options: OrganizerOptions): string {
  const changed = options.find ? name.split(options.find).join(options.replace) : name;
  const dot = changed.lastIndexOf('.');
  const stem = dot > 0 ? changed.slice(0, dot) : changed;
  const ext = dot > 0 ? changed.slice(dot) : '';
  return options.prefix + stem + options.suffix + ext;
}

export function buildOrganizerRequest(
  snapshot: OrganizerSnapshot, selected: readonly string[], options: OrganizerOptions,
): OrganizerRequest {
  if (selected.length < 1 || selected.length > 100 || new Set(selected).size !== selected.length) {
    throw new Error('Select 1–100 distinct completed files.');
  }
  if (options.destinationId !== 'same' && options.destinationId !== null
      && !snapshot.folders.some(folder => folder.id === options.destinationId)) {
    throw new Error('Destination folder is not in this Drive snapshot.');
  }
  const files = new Map(snapshot.files.map(file => [file.id, file]));
  return {
    revision: snapshot.revision,
    changes: selected.map(id => {
      const file = files.get(id);
      if (!file) throw new Error('Selected file no longer appears in this Drive snapshot.');
      const newName = targetOrganizerName(file.name, options);
      if (!newName || newName.trim() !== newName || newName === '.' || newName === '..'
          || newName.endsWith('.') || /[<>:"/\\|?*\u0000-\u001f\u007f]/.test(newName)) {
        throw new Error('A proposed filename is not portable across operating systems.');
      }
      return {
        entry_id: file.id, expected_name: file.name,
        expected_directory_id: file.directoryId,
        new_name: newName,
        new_directory_id: options.destinationId === 'same' ? file.directoryId : options.destinationId,
      };
    }),
  };
}


/**
 * Exact per-file CSV targets reuse the native bulk organizer's reviewed
 * all-or-none preview/apply transaction. No destination folders are created.
 * CSV paths resolve against ONE validated current Drive snapshot.
 */
export function buildOrganizerCsvRequest(snapshot: OrganizerSnapshot, csv: string): OrganizerRequest {
  if (typeof csv !== 'string' || new TextEncoder().encode(csv).length > 128 * 1024) {
    throw Error('File CSV exceeds the 128 KiB safety limit.');
  }
  const input = csv.replace(/^\uFEFF/, '');
  const rows: string[][] = [];
  let fields: string[] = [];
  let field = '';
  let quoted = false;
  let quoteClosed = false;
  let fieldStart = true;
  const finishField = () => {
    fields.push(field);
    field = '';
    quoteClosed = false;
    fieldStart = true;
    if (fields.length > 2) throw Error('File CSV must have exactly two columns.');
  };
  const finishRow = () => {
    finishField();
    if (fields.length !== 2) throw Error('File CSV must have exactly two columns.');
    rows.push(fields);
    if (rows.length > 101) throw Error('File CSV accepts at most 100 file mappings.');
    fields = [];
  };
  for (let i = 0; i < input.length; i++) {
    const ch = input[i];
    if (quoted) {
      if (ch === '"') {
        if (input[i + 1] === '"') { field += '"'; i++; }
        else { quoted = false; quoteClosed = true; }
      } else {
        field += ch;
      }
      continue;
    }
    if (ch === '"' && fieldStart) { quoted = true; fieldStart = false; continue; }
    if (ch === '"' || (quoteClosed && ch !== ',' && ch !== '\r' && ch !== '\n')) {
      throw Error('Malformed file CSV quoting.');
    }
    if (ch === ',') { finishField(); continue; }
    if (ch === '\r' || ch === '\n') {
      finishRow();
      if (ch === '\r' && input[i + 1] === '\n') i++;
      continue;
    }
    field += ch;
    fieldStart = false;
  }
  if (quoted) throw Error('Unterminated quoted file CSV field.');
  if (field.length || fields.length || !fieldStart || quoteClosed) finishRow();
  if (rows[0]?.[0] !== 'source_path' || rows[0]?.[1] !== 'new_path') {
    throw Error('File CSV must begin with source_path,new_path.');
  }
  if (rows.length < 2) throw Error('File CSV must include 1–100 mapping rows.');

  const byPath = new Map<string, OrganizerFile>();
  const ambiguousSource = new Set<string>();
  const byFolder = new Map<string, OrganizerFolder>();
  const ambiguousParent = new Set<string>();
  for (const file of snapshot.files) {
    if (byPath.has(file.path)) ambiguousSource.add(file.path);
    byPath.set(file.path, file);
  }
  for (const folder of snapshot.folders) {
    if (byFolder.has(folder.path)) ambiguousParent.add(folder.path);
    byFolder.set(folder.path, folder);
  }
  const seenSource = new Set<string>();
  const seenTarget = new Set<string>();
  const changes = rows.slice(1).map(([from, to], index): OrganizerChange => {
    const row = index + 2;
    if (!from.startsWith('/') || !to.startsWith('/')
        || from.endsWith('/') || to.endsWith('/')
        || from.includes('//') || to.includes('//')) {
      throw Error('File CSV row ' + row + ': use full absolute Drive file paths.');
    }
    if (ambiguousSource.has(from)) throw Error('File CSV row ' + row + ': ambiguous source file path.');
    const source = byPath.get(from);
    if (!source) throw Error('File CSV row ' + row + ': source file not found in this snapshot.');
    if (seenSource.has(source.id)) throw Error('File CSV row ' + row + ': duplicate source file.');
    seenSource.add(source.id);
    const split = to.lastIndexOf('/');
    const parentPath = split === 0 ? '/' : to.slice(0, split);
    const targetName = to.slice(split + 1);
    if (!targetName || new TextEncoder().encode(targetName).length > 255
        || targetName.trim() !== targetName || targetName === '.' || targetName === '..'
        || targetName.endsWith('.') || /[<>:"/\\|?*\u0000-\u001f\u007f]/.test(targetName)
        || /^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(targetName)) {
      throw Error('File CSV row ' + row + ': unsafe or nonportable destination filename.');
    }
    if (ambiguousParent.has(parentPath)) {
      throw Error('File CSV row ' + row + ': ambiguous destination folder.');
    }
    const folder = parentPath === '/' ? null : byFolder.get(parentPath);
    if (folder === undefined) {
      throw Error('File CSV row ' + row + ': destination parent must already exist.');
    }
    const destinationId = folder?.id ?? null;
    if (source.name === targetName && source.directoryId === destinationId) {
      throw Error('File CSV row ' + row + ': no change requested.');
    }
    const portable = to.normalize('NFC').toLocaleLowerCase();
    if (seenTarget.has(portable)) throw Error('File CSV row ' + row + ': duplicate target path.');
    seenTarget.add(portable);
    return {
      entry_id: source.id,
      expected_name: source.name,
      expected_directory_id: source.directoryId,
      new_name: targetName,
      new_directory_id: destinationId,
    };
  });
  return { revision: snapshot.revision, changes };
}


/** One-time, first-match routing rules. Nothing runs automatically or changes the
 * Drive until native collision/revision preview and explicit user approval.
 */
export interface OrganizerRule {
  extension: string;
  nameContains: string;
  sourceFolderPath: string; // '' any, '/' root only, '/Folder' that folder and its descendants
  destinationId: string | null | 'same';
  prefix: string;
  suffix: string;
}
function safeRoutingName(name: string): boolean {
  return name.length > 0 && new TextEncoder().encode(name).length <= 255
    && name.trim() === name && name !== '.' && name !== '..'
    && !name.endsWith('.') && !/[<>:"/\\|?*\u0000-\u001f\u007f]/.test(name)
    && !/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)/i.test(name);
}
const portableRoutingName = (name: string) => name.normalize('NFC').toLocaleLowerCase();

export function buildOrganizerRulesRequest(
  snapshot: OrganizerSnapshot, rules: readonly OrganizerRule[],
): OrganizerRequest {
  if (!Array.isArray(rules) || rules.length < 1 || rules.length > 20) {
    throw Error('Provide 1–20 organization rules.');
  }
  const folderIdToPath = new Map(snapshot.folders.map(f => [f.id, f.path]));
  const pathSet = new Set(snapshot.folders.map(f => f.path));
  if (folderIdToPath.size !== snapshot.folders.length) {
    throw Error('Drive inventory has ambiguous folder identities. Refresh.');
  }
  for (const [i, rule] of rules.entries()) {
    const label = 'Rule ' + (i + 1);
    if (!rule || typeof rule.extension !== 'string' || typeof rule.nameContains !== 'string'
      || typeof rule.sourceFolderPath !== 'string' || typeof rule.prefix !== 'string'
      || typeof rule.suffix !== 'string'
      || rule.nameContains.length > 100 || rule.prefix.length > 100 || rule.suffix.length > 100
      || rule.sourceFolderPath.length > 4096) {
      throw Error(label + ': invalid or oversized rule field.');
    }
    if (rule.extension && !/^\.[A-Za-z0-9]{1,20}$/.test(rule.extension)) {
      throw Error(label + ': extension should be like .pdf or .jpg.');
    }
    if (!rule.extension && !rule.nameContains && !rule.sourceFolderPath) {
      throw Error(label + ': enter an extension, name filter or source folder.');
    }
    if (rule.sourceFolderPath !== '' && rule.sourceFolderPath !== '/'
      && !pathSet.has(rule.sourceFolderPath)) {
      throw Error(label + ': source folder is not in the latest snapshot.');
    }
    if (rule.destinationId !== 'same' && rule.destinationId !== null
      && !folderIdToPath.has(rule.destinationId)) {
      throw Error(label + ': destination folder is not in the latest snapshot.');
    }
    if (!safeRoutingName('example' + rule.prefix + rule.suffix)) {
      throw Error(label + ': filename decoration contains unsafe characters.');
    }
  }

  const changes: OrganizerChange[] = [];
  const destinations = new Set<string>();
  const seenIds = new Set<string>();
  for (const file of snapshot.files) {
    if (seenIds.has(file.id)) throw Error('Ambiguous completed file identity. Refresh Drive.');
    seenIds.add(file.id);
    const slash = file.path.lastIndexOf('/');
    const parentPath = slash === 0 ? '/' : file.path.slice(0, slash);
    for (const rule of rules) {
      if (rule.extension && !portableRoutingName(file.name)
        .endsWith(rule.extension.toLocaleLowerCase())) continue;
      if (rule.nameContains && !portableRoutingName(file.name)
        .includes(portableRoutingName(rule.nameContains))) continue;
      if (rule.sourceFolderPath && rule.sourceFolderPath !== parentPath
        && (rule.sourceFolderPath === '/' || !parentPath.startsWith(rule.sourceFolderPath + '/'))) continue;

      const newName = targetOrganizerName(file.name, {
        find: '', replace: '', prefix: rule.prefix, suffix: rule.suffix,
        destinationId: rule.destinationId,
      });
      const newParent = rule.destinationId === 'same' ? file.directoryId : rule.destinationId;
      // First match is authoritative, including if it requests no change:
      // later broad rules must not unexpectedly move that file.
      if (newParent === file.directoryId && newName === file.name) break;
      if (!safeRoutingName(newName)) {
        throw Error('A routing rule produced an unsafe filename for ' + file.path);
      }
      const key = String(newParent) + '\0' + portableRoutingName(newName);
      if (destinations.has(key)) {
        throw Error('Two rules would produce the same target path. Narrow the match filters.');
      }
      destinations.add(key);
      changes.push({
        entry_id: file.id, expected_name: file.name,
        expected_directory_id: file.directoryId,
        new_name: newName, new_directory_id: newParent,
      });
      if (changes.length > 100) {
        throw Error('Rules match more than 100 files. Narrow the filters before previewing.');
      }
      break;
    }
  }
  if (!changes.length) throw Error('Rules would not change any completed files.');
  return { revision: snapshot.revision, changes };
}
