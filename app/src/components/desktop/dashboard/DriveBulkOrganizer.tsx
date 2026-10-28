import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { FolderInput, Layers3, Loader2, RefreshCw, ShieldCheck } from 'lucide-react';
import { validateDriveSyncResult } from '../../../deviceContinuity';
import {
  buildOrganizerRequest, buildOrganizerCsvRequest, buildOrganizerRulesRequest,
  readOrganizerSnapshot, type OrganizerOptions, type OrganizerRule, type OrganizerSnapshot,
} from '../../../driveBulkOrganizer';

interface PreviewResult {
  revision: number;
  ready: boolean;
  problems: string[];
  count: number;
}
interface AppliedResult { changed: number; revision: number; dirty: boolean }

const initialOptions: OrganizerOptions = {
  find: '', replace: '', prefix: '', suffix: '', destinationId: 'same',
};
const initialRule: OrganizerRule = {
  extension: '.pdf', nameContains: '', sourceFolderPath: '',
  destinationId: 'same', prefix: '', suffix: '',
};
const fieldClass = 'rounded-md border border-telegram-border bg-telegram-bg px-2 py-1.5 text-xs text-telegram-text';
const buttonClass = 'rounded-lg bg-telegram-hover px-3 py-1.5 text-xs text-telegram-text hover:bg-telegram-border/60 disabled:opacity-50';

export function DriveBulkOrganizer({ active }: { active: boolean }) {
  const [open, setOpen] = useState(false);
  const [snapshot, setSnapshot] = useState<OrganizerSnapshot | null>(null);
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [options, setOptions] = useState<OrganizerOptions>(initialOptions);
  const [search, setSearch] = useState('');
  const [csvMode, setCsvMode] = useState(false);
  const [csvText, setCsvText] = useState('source_path,new_path\n');
  const [rulesMode, setRulesMode] = useState(false);
  const [rules, setRules] = useState<OrganizerRule[]>([{ ...initialRule }]);
  const [preview, setPreview] = useState<{ key: string; result: PreviewResult } | null>(null);
  const [busy, setBusy] = useState<'load' | 'preview' | 'apply' | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const generation = useRef(0);
  const operationBusy = useRef(false);

  useEffect(() => {
    if (active) return;
    // Closing Settings must not erase the outcome of an already submitted
    // mutation. Let its IPC, sync and acknowledgement finish safely.
    if (busy === 'apply') {
      setOpen(false);
      return;
    }
    generation.current++;
    setOpen(false);
    setSnapshot(null);
    setSelectedIds([]);
    setPreview(null);
    // Retain the last mutation outcome across Settings close/reopen until the
    // next user-initiated plan or apply. Never leave a preview armed.
    setBusy(null);
  }, [active, busy]);

  const matching = useMemo(() => {
    const needle = search.trim().toLocaleLowerCase();
    return snapshot?.files.filter(file => !needle || file.path.toLocaleLowerCase().includes(needle)) ?? [];
  }, [snapshot, search]);
  const shown = matching.slice(0, 80);

  const planning = useMemo(() => {
    if (!snapshot) return { request: null as ReturnType<typeof buildOrganizerRequest> | null, problem: '' };
    try {
      const request = rulesMode ? buildOrganizerRulesRequest(snapshot, rules)
        : csvMode ? buildOrganizerCsvRequest(snapshot, csvText)
        : selectedIds.length ? buildOrganizerRequest(snapshot, selectedIds, options) : null;
      return { request, problem: '' };
    } catch (cause) {
      return { request: null, problem: String(cause) };
    }
  }, [snapshot, selectedIds, options, csvMode, csvText, rulesMode, rules]);
  const request = planning.request;
  const requestKey = request ? JSON.stringify(request) : null;
  const previewReady = !!preview && !!requestKey && preview.key === requestKey && preview.result.ready;

  const updateRule = (index: number, patch: Partial<OrganizerRule>) => {
    setRules(current => current.map((rule, i) => i === index ? { ...rule, ...patch } : rule));
    setPreview(null);
    setNotice(null);
    setError(null);
  };
  const updateOptions = (patch: Partial<OrganizerOptions>) => {
    setOptions(current => ({ ...current, ...patch }));
    setPreview(null);
    setNotice(null);
  };
  const load = async () => {
    if (operationBusy.current || !active) return;
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('load');
    setError(null);
    setPreview(null);
    try {
      const result = readOrganizerSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
      if (generation.current !== id) return;
      setSnapshot(result);
      setSelectedIds([]);
    } catch (cause) {
      if (generation.current === id) setError('Cannot safely read Drive metadata: ' + String(cause));
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const check = async () => {
    if (!active || operationBusy.current) return;
    if (!request || !requestKey) {
      setError(planning.problem || 'Choose 1–100 files with valid filenames and a destination.');
      return;
    }
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('preview');
    setError(null);
    setPreview(null);
    try {
      const result = await invoke<PreviewResult>('cmd_drive_bulk_preview', { request });
      if (generation.current !== id) return;
      if (!Number.isSafeInteger(result.revision) || result.revision !== request.revision
          || !Array.isArray(result.problems) || typeof result.ready !== 'boolean'
          || result.count !== request.changes.length) {
        throw new Error('Native preview returned an invalid or stale result.');
      }
      setPreview({ key: requestKey, result });
    } catch (cause) {
      if (generation.current === id) setError('Preview failed: ' + String(cause));
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const apply = async () => {
    if (!active || operationBusy.current || !previewReady || !request || !requestKey) return;
    const confirmed = preview?.key === JSON.stringify(request) && preview.result.ready;
    if (!confirmed) return;
    operationBusy.current = true;
    const id = ++generation.current;
    setBusy('apply');
    setError(null);
    setNotice(null);
    setPreview(null);
    try {
      let outcome: AppliedResult;
      try {
        outcome = await invoke<AppliedResult>('cmd_drive_bulk_apply', { request });
        if (!Number.isSafeInteger(outcome.changed) || outcome.changed !== request.changes.length
            || !Number.isSafeInteger(outcome.revision) || outcome.revision <= request.revision
            || typeof outcome.dirty !== 'boolean') {
          throw new Error('Native operation returned an invalid result.');
        }
      } catch (cause) {
        // IPC might have committed before a lost response. Never resend without
        // refreshing/re-previewing, even if the error looks like a timeout.
        if (generation.current === id) {
          setError('Apply result is uncertain: ' + String(cause) + '. Refresh and inspect the files before creating another preview.');
        }
        return;
      }
      if (generation.current !== id) return;
      setSelectedIds([]);
      setNotice(`${outcome.changed} files reorganized locally. Checking cloud sync…`);
      // Apply has succeeded. Later sync/refresh failures must never present
      // themselves as failed/rolled-back file organization.
      let syncStatus = 'Cloud synchronization is not yet confirmed; retry Drive sync when connected.';
      try {
        const sync = validateDriveSyncResult(await invoke<unknown>('cmd_drive_sync_metadata'));
        if (!sync.dirty && sync.source !== 'qa-local') {
          syncStatus = 'Drive metadata sync returned a clean result.';
        }
      } catch {
        // Local transaction remains committed when network/cloud sync fails.
      }
      let refreshStatus = '';
      try {
        const fresh = readOrganizerSnapshot(await invoke<unknown>('cmd_drive_get_snapshot'));
        if (generation.current === id) setSnapshot(fresh);
      } catch {
        refreshStatus = ' Local file list could not be refreshed; use Refresh before further changes.';
      }
      if (generation.current === id) {
        setNotice(`${outcome.changed} files reorganized locally in one atomic transaction. ${syncStatus}${refreshStatus}`);
      }
    } finally {
      operationBusy.current = false;
      if (generation.current === id) setBusy(null);
    }
  };

  const toggle = (id: string) => {
    setSelectedIds(current => current.includes(id) ? current.filter(x => x !== id)
      : current.length < 100 ? [...current, id] : current);
    setPreview(null);
  };
  const plannedPaths = request?.changes.map(change => {
    const original = snapshot?.files.find(file => file.id === change.entry_id);
    const folder = change.new_directory_id === null ? ''
      : snapshot?.folders.find(item => item.id === change.new_directory_id)?.path ?? '(missing destination)';
    return { id: change.entry_id, from: original?.path ?? change.expected_name, to: folder + '/' + change.new_name };
  }) ?? [];

  return (
    <section aria-label="Drive Bulk Organizer" className="tr-setting-row space-y-3 p-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex min-w-0 items-center gap-2.5">
          <FolderInput className="h-4 w-4 shrink-0 text-telegram-subtext" aria-hidden="true" />
          <div>
            <p className="text-sm font-medium text-telegram-text">TeraRelay Drive · Bulk Organizer</p>
            <p className="text-xs text-telegram-subtext">Move and rename up to 100 existing files as one reversible-on-error local metadata transaction. Preview every path first.</p>
          </div>
        </div>
        <button type="button" className={buttonClass} aria-expanded={open} aria-label="Open Drive Bulk Organizer"
          disabled={busy === 'apply'}
          onClick={() => { if (operationBusy.current) return; if (open) { generation.current++; setOpen(false); setPreview(null); } else { setOpen(true); void load(); } }}>
          {open ? 'Hide organizer' : 'Organize files'}
        </button>
      </div>
      {open && (
        <div className="space-y-3 border-t border-telegram-border pt-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p role="status" className="text-xs text-telegram-subtext">
              {snapshot ? `${snapshot.files.length} local completed files · ${rulesMode ? (request?.changes.length ?? 0) + ' rule matches' : csvMode ? (request?.changes.length ?? 0) + ' CSV mappings' : selectedIds.length + ' selected'}${snapshot.skipped ? ` · ${snapshot.skipped} unverified/limited items excluded` : ''}` : 'Reading local Drive inventory…'}
            </p>
            <button type="button" className={buttonClass} disabled={busy !== null}
              onClick={() => void load()}><RefreshCw className="mr-1 inline h-3 w-3" aria-hidden="true" />Refresh</button>
          </div>
          {snapshot && (
            <>
              <label className="flex items-center gap-2 text-xs text-telegram-text">
                <input type="checkbox" checked={csvMode} disabled={busy !== null}
                  onChange={event => { setCsvMode(event.target.checked); if (event.target.checked) setRulesMode(false); setPreview(null); setNotice(null); setError(null); }}/>
                Map each file to a different folder or name using exact CSV paths
              </label>
              <label className="flex items-center gap-2 text-xs text-telegram-text">
                <input type="checkbox" checked={rulesMode} disabled={busy !== null}
                  onChange={event => { setRulesMode(event.target.checked); if (event.target.checked) setCsvMode(false); setPreview(null); setNotice(null); setError(null); }}/>
                Route completed files using multiple matching rules
              </label>
              {rulesMode && <div className="space-y-3">
                <p className="text-xs text-telegram-subtext">
                  One-time organization, not a background automation. The first matching rule wins.
                  Choose filters and destinations; verify all proposed changes before applying one atomic batch.
                  Maximum 20 rules and 100 files. Only existing Drive folders can be destinations.
                </p>
                {rules.map((rule, index) => <div key={index}
                  className="rounded-md border border-telegram-border p-2 space-y-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-xs font-semibold text-telegram-text">Rule {index + 1}</span>
                    <div className="flex gap-1">
                      <button type="button" className={buttonClass} disabled={busy !== null || index === 0}
                        onClick={() => { setRules(current => { const next = [...current]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; return next; }); setPreview(null); }}>
                        Move up
                      </button>
                      <button type="button" className={buttonClass} disabled={busy !== null || rules.length <= 1}
                        onClick={() => { setRules(current => current.filter((_, i) => i !== index)); setPreview(null); }}>
                        Remove
                      </button>
                    </div>
                  </div>
                  <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                    <label className="text-xs text-telegram-subtext space-y-1">File extension (optional)
                      <input className={fieldClass + ' w-full'} value={rule.extension}
                        disabled={busy !== null} placeholder=".pdf"
                        onChange={event => updateRule(index, {extension: event.target.value})}/>
                    </label>
                    <label className="text-xs text-telegram-subtext space-y-1">Filename contains (optional)
                      <input className={fieldClass + ' w-full'} value={rule.nameContains}
                        disabled={busy !== null} placeholder="invoice"
                        onChange={event => updateRule(index, {nameContains: event.target.value})}/>
                    </label>
                    <label className="text-xs text-telegram-subtext space-y-1">Search files under
                      <select className={fieldClass + ' w-full'} value={rule.sourceFolderPath}
                        disabled={busy !== null}
                        onChange={event => updateRule(index, {sourceFolderPath: event.target.value})}>
                        <option value="">Any folder (including root)</option>
                        <option value="/">Drive root only</option>
                        {snapshot.folders.map(folder =>
                          <option key={folder.id} value={folder.path}>{folder.path} and subfolders</option>)}
                      </select>
                    </label>
                    <label className="text-xs text-telegram-subtext space-y-1">Move matching files into
                      <select className={fieldClass + ' w-full'} value={rule.destinationId ?? '__root__'}
                        disabled={busy !== null}
                        onChange={event => updateRule(index, {destinationId: event.target.value === '__root__'
                          ? null : event.target.value})}>
                        <option value="same">Keep existing folder</option>
                        <option value="__root__">Drive root</option>
                        {snapshot.folders.map(folder =>
                          <option key={folder.id} value={folder.id}>{folder.path}</option>)}
                      </select>
                    </label>
                    <label className="text-xs text-telegram-subtext space-y-1">Optional filename prefix
                      <input className={fieldClass + ' w-full'} disabled={busy !== null}
                        value={rule.prefix} onChange={event => updateRule(index, {prefix: event.target.value})}/>
                    </label>
                    <label className="text-xs text-telegram-subtext space-y-1">Optional filename suffix
                      <input className={fieldClass + ' w-full'} disabled={busy !== null}
                        value={rule.suffix} onChange={event => updateRule(index, {suffix: event.target.value})}/>
                    </label>
                  </div>
                </div>)}
                <button type="button" className={buttonClass} disabled={busy !== null || rules.length >= 20}
                  onClick={() => { setRules(current => [...current, { ...initialRule, extension: '' }]); setPreview(null); }}>
                  Add another rule
                </button>
                {planning.problem && <p role="alert" className="text-xs text-red-400">{planning.problem}</p>}
              </div>}
              {csvMode && <div className="space-y-2">
                <label className="block space-y-1 text-xs text-telegram-subtext">
                  File mappings: source_path,new_path (1–100 rows; target folders must already exist)
                  <textarea className={fieldClass + ' w-full font-mono'} rows={8} spellCheck={false}
                    aria-label="Exact Drive file move and rename CSV mappings" value={csvText} disabled={busy !== null}
                    onChange={event => { setCsvText(event.target.value); setPreview(null); setNotice(null); setError(null); }}
                    placeholder={'source_path,new_path\n/Work/old.txt,/Archive/new.txt\n/Docs/costs.csv,/Reports/costs.csv'}/>
                </label>
                <p className="text-xs text-telegram-subtext">
                  Paste complete Drive file paths. Quote paths containing commas and double embedded quotes.
                  Each target folder must exist. The same native atomic preview, collision checks and undo history apply.
                  Nothing is changed until you explicitly preview and apply.
                </p>
                {planning.problem && <p role="alert" className="text-xs text-red-400">{planning.problem}</p>}
              </div>}
              {!csvMode && !rulesMode && <>
              <div className="flex flex-wrap items-center gap-2">
                <input type="search" aria-label="Filter files to organize" value={search}
                  onChange={event => setSearch(event.target.value)} placeholder="Find file paths…"
                  className={fieldClass + ' min-w-[180px] flex-1'} />
                <button type="button" className={buttonClass} disabled={busy !== null || matching.length === 0}
                  onClick={() => { setSelectedIds(matching.slice(0, 100).map(file => file.id)); setPreview(null); }}>
                  Select first {Math.min(matching.length, 100)}
                </button>
                <button type="button" className={buttonClass} disabled={busy === 'apply'} onClick={() => { setSelectedIds([]); setPreview(null); }}>
                  Clear
                </button>
              </div>
              <div className="max-h-44 space-y-1 overflow-auto rounded-md border border-telegram-border p-2" aria-label="Drive files available for bulk organization">
                {shown.map(file => (
                  <label key={file.id} className="flex cursor-pointer items-start gap-2 rounded p-1 text-xs text-telegram-text hover:bg-telegram-hover/50">
                    <input type="checkbox" disabled={busy === 'apply'} checked={selectedIds.includes(file.id)} onChange={() => toggle(file.id)}
                      className="mt-0.5" />
                    <span className="min-w-0 break-all">{file.path}</span>
                  </label>
                ))}
                {matching.length > 80 && <p className="text-xs text-telegram-subtext">Showing 80 of {matching.length} matching files; Select first applies to up to 100.</p>}
                {matching.length === 0 && <p className="text-xs text-telegram-subtext">No matching completed files in the local Drive index.</p>}
              </div>
              <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                <label className="space-y-1 text-xs text-telegram-subtext">Find text (literal)
                  <input className={fieldClass + ' w-full'} value={options.find}
                    onChange={event => updateOptions({ find: event.target.value })} />
                </label>
                <label className="space-y-1 text-xs text-telegram-subtext">Replace with
                  <input className={fieldClass + ' w-full'} value={options.replace}
                    onChange={event => updateOptions({ replace: event.target.value })} />
                </label>
                <label className="space-y-1 text-xs text-telegram-subtext">Filename prefix
                  <input className={fieldClass + ' w-full'} value={options.prefix}
                    onChange={event => updateOptions({ prefix: event.target.value })} />
                </label>
                <label className="space-y-1 text-xs text-telegram-subtext">Filename suffix (before extension)
                  <input className={fieldClass + ' w-full'} value={options.suffix}
                    onChange={event => updateOptions({ suffix: event.target.value })} />
                </label>
              </div>
              <label className="block space-y-1 text-xs text-telegram-subtext">Move selected files into
                <select aria-label="Bulk organizer destination folder" className={fieldClass + ' w-full'}
                  value={options.destinationId ?? '__root__'}
                  onChange={event => updateOptions({ destinationId: event.target.value === '__root__' ? null : event.target.value })}>
                  <option value="same">Keep each file's folder</option>
                  <option value="__root__">/ (Drive root)</option>
                  {snapshot.folders.map(folder => <option value={folder.id} key={folder.id}>{folder.path}</option>)}
                </select>
              </label>
              </>}
              <div className="flex flex-wrap items-center gap-2">
                <button type="button" className={buttonClass + ' font-semibold'} disabled={busy !== null || !request}
                  onClick={() => void check()}>
                  {busy === 'preview' && <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" />}
                  Preview {request?.changes.length ?? 0} file changes
                </button>
                {previewReady && (
                  <button type="button" className="rounded-lg bg-telegram-primary px-3 py-1.5 text-xs font-semibold text-white hover:opacity-90 disabled:opacity-50"
                    disabled={busy !== null} onClick={() => void apply()}>
                    <ShieldCheck className="mr-1 inline h-3 w-3" aria-hidden="true" />Apply exactly {request?.changes.length ?? 0} changes
                  </button>
                )}
              </div>
              {preview && preview.key === requestKey && (
                <div className="space-y-2 rounded-lg border border-telegram-border p-2">
                  <p className="flex items-center gap-1 text-xs font-semibold text-telegram-text">
                    <Layers3 className="h-3 w-3" />{preview.result.ready ? 'Ready to apply — no conflicts detected' : 'Preview blocked — nothing changed'}
                  </p>
                  {preview.result.problems.map((problem, index) => (
                    <p role="alert" key={index} className="text-xs text-red-400">{problem}</p>
                  ))}
                  {preview.result.ready && (
                    <ol className="max-h-48 space-y-2 overflow-auto">
                      {plannedPaths.map(path => (
                        <li key={path.id} className="text-xs text-telegram-subtext">
                          <span className="block break-all line-through">{path.from}</span>
                          <span className="block break-all font-medium text-telegram-text">→ {path.to}</span>
                        </li>
                      ))}
                    </ol>
                  )}
                </div>
              )}
            </>
          )}
          {busy === 'apply' && <p role="status" className="text-xs text-telegram-subtext">Applying and verifying Drive metadata…</p>}
          {error && <p role="alert" className="rounded-md border border-red-500/30 p-2 text-xs text-red-400">{error}</p>}
          {notice && <p role="status" className="rounded-md border border-telegram-border p-2 text-xs text-telegram-text">{notice}</p>}
          <p className="text-[11px] text-telegram-subtext">Only completed files are eligible. Existing destinations are never overwritten; a stale revision or one failed operation cancels the whole batch. Cloud sync requires an active authorized connection.</p>
        </div>
      )}
    </section>
  );
}
