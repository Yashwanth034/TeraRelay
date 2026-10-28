//! Local Drive Smart Views: stored metadata rules re-evaluated against the
//! authoritative visible Drive snapshot on every preview. No cloud changes.
use crate::commands::drive_metadata::{drive_snapshot, new_drive_id, DriveSnapshot};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tauri::State;

const MAX_VIEWS: usize = 50;
const MAX_FILES: usize = 25_000;
const MAX_RESULTS: usize = 200;
const MAX_RULE_JSON: usize = 2_048;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SmartRule {
    pub name_contains: Option<String>,
    pub extension: Option<String>,
    pub category: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub updated_after: Option<i64>,
    #[serde(default)]
    pub updated_within_days: Option<u16>,
    pub folder_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SmartView {
    pub id: String,
    pub name: String,
    pub revision: i64,
    pub rule: SmartRule,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SmartMatch {
    pub entry_id: String,
    pub name: String,
    pub path: String,
    pub size: u64,
    pub updated_at: i64,
    pub category: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SmartPreview {
    pub view_id: String,
    pub revision: i64,
    pub snapshot_revision: i64,
    pub total_matches: usize,
    pub truncated: bool,
    pub matches: Vec<SmartMatch>,
    pub local_only: bool,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
// A saved search title is limited to 100 bytes; existing Drive filenames
// follow the wider native portable-name limit and must not make previews fail.
fn valid_drive_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
}
fn check_view_id(id: &str) -> Result<(), String> {
    if id.len() != 38
        || !id.starts_with("smart-")
        || !id[6..].bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("Invalid saved Smart View identity; refresh your saved views.".into());
    }
    Ok(())
}
fn check_rule(rule: &SmartRule) -> Result<(), String> {
    let mut active = 0usize;
    if let Some(name) = &rule.name_contains {
        if !valid_name(name) {
            return Err("Invalid Smart View filename filter.".into());
        }
        active += 1;
    }
    if let Some(ext) = &rule.extension {
        if ext.is_empty()
            || ext.len() > 16
            || !ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || ext.to_ascii_lowercase() != *ext
        {
            return Err(
                "Extension must be 1–16 lowercase letters, digits, or hyphens (without a dot)."
                    .into(),
            );
        }
        active += 1;
    }
    if let Some(category) = &rule.category {
        if !matches!(
            category.as_str(),
            "image" | "video" | "audio" | "document" | "archive" | "other"
        ) {
            return Err("Invalid Smart View file category.".into());
        }
        active += 1;
    }
    for size in [rule.min_size, rule.max_size].into_iter().flatten() {
        if size > MAX_SAFE_INTEGER {
            return Err("Smart View size exceeds safe numeric range.".into());
        }
    }
    if rule.min_size.is_some() {
        active += 1;
    }
    if rule.max_size.is_some() {
        active += 1;
    }
    if let (Some(min), Some(max)) = (rule.min_size, rule.max_size) {
        if min > max {
            return Err("Minimum file size exceeds maximum.".into());
        }
    }
    if let Some(epoch) = rule.updated_after {
        if !(1..=MAX_SAFE_INTEGER as i64).contains(&epoch) {
            return Err("Invalid Smart View update timestamp.".into());
        }
        active += 1;
    }
    if let Some(days) = rule.updated_within_days {
        if !(1..=3650).contains(&days) || rule.updated_after.is_some() {
            return Err("Choose either a fixed update timestamp or a rolling day window.".into());
        }
        active += 1;
    }
    if let Some(folder) = &rule.folder_id {
        if !valid_id(folder) {
            return Err("Invalid Smart View folder identity.".into());
        }
        active += 1;
    }
    if active == 0 {
        return Err("Choose at least one Smart View rule.".into());
    }
    Ok(())
}

fn schema(conn: &sqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS drive_local_smart_views (
          id TEXT PRIMARY KEY NOT NULL,
          name TEXT NOT NULL COLLATE NOCASE UNIQUE,
          revision INTEGER NOT NULL CHECK(revision>=1),
          rules_json TEXT NOT NULL
        );",
    )
    .map_err(|e| e.to_string())
}
fn parse_view(id: String, name: String, revision: i64, json: String) -> Result<SmartView, String> {
    check_view_id(&id)?;
    if !valid_name(&name) || revision < 1 || json.len() > MAX_RULE_JSON {
        return Err("Invalid saved Smart View metadata.".into());
    }
    let rule: SmartRule =
        serde_json::from_str(&json).map_err(|_| "Corrupt stored Smart View rules.")?;
    check_rule(&rule)?;
    Ok(SmartView {
        id,
        name,
        revision,
        rule,
    })
}
fn list(conn: &sqlite::Connection) -> Result<Vec<SmartView>, String> {
    schema(conn)?;
    let mut q=conn.prepare("SELECT id,name,revision,rules_json FROM drive_local_smart_views ORDER BY name COLLATE NOCASE,id LIMIT ?")
        .map_err(|e:sqlite::Error|e.to_string())?;
    q.bind((1, (MAX_VIEWS + 1) as i64))
        .map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    while q.next().map_err(|e| e.to_string())? == sqlite::State::Row {
        if rows.len() == MAX_VIEWS {
            return Err("Saved Smart Views exceed the 50-view limit.".into());
        }
        rows.push(parse_view(
            q.read::<String, _>(0).map_err(|e| e.to_string())?,
            q.read::<String, _>(1).map_err(|e| e.to_string())?,
            q.read::<i64, _>(2).map_err(|e| e.to_string())?,
            q.read::<String, _>(3).map_err(|e| e.to_string())?,
        )?);
    }
    Ok(rows)
}
fn save(
    conn: &sqlite::Connection,
    id: Option<&str>,
    name: &str,
    revision: Option<i64>,
    rule: SmartRule,
) -> Result<SmartView, String> {
    if !valid_name(name) {
        return Err("Smart View name must be 1–100 clean characters.".into());
    }
    check_rule(&rule)?;
    let json = serde_json::to_string(&rule).map_err(|e| e.to_string())?;
    if json.len() > MAX_RULE_JSON {
        return Err("Smart View rules too long.".into());
    }
    schema(conn)?;
    conn.execute("SAVEPOINT smart_view_save")
        .map_err(|e| e.to_string())?;
    let result = (|| {
        let (id, next_revision) = if let Some(id) = id {
            check_view_id(id)?;
            let previous = revision
                .filter(|v| *v >= 1 && *v < i64::MAX)
                .ok_or("Expected revision required to change an existing Smart View.")?;
            let mut q = conn
                .prepare("SELECT revision FROM drive_local_smart_views WHERE id=?")
                .map_err(|e: sqlite::Error| e.to_string())?;
            q.bind((1, id)).map_err(|e| e.to_string())?;
            if q.next().map_err(|e| e.to_string())? != sqlite::State::Row
                || q.read::<i64, _>(0).map_err(|e| e.to_string())? != previous
            {
                return Err(
                    "Smart View changed since it was opened. Refresh before editing.".into(),
                );
            }
            (id.to_string(), previous + 1)
        } else {
            if revision.is_some() {
                return Err("New Smart View cannot specify a revision.".into());
            }
            if list(conn)?.len() >= MAX_VIEWS {
                return Err("At most 50 Smart Views can be saved.".into());
            }
            (format!("smart-{}", new_drive_id()), 1)
        };
        if revision.is_some() {
            let mut q = conn
                .prepare(
                    "UPDATE drive_local_smart_views SET name=?,revision=?,rules_json=? WHERE id=?",
                )
                .map_err(|e: sqlite::Error| e.to_string())?;
            q.bind((1, name)).map_err(|e| e.to_string())?;
            q.bind((2, next_revision)).map_err(|e| e.to_string())?;
            q.bind((3, json.as_str())).map_err(|e| e.to_string())?;
            q.bind((4, id.as_str())).map_err(|e| e.to_string())?;
            q.next().map_err(|e| e.to_string())?;
        } else {
            let mut q=conn.prepare("INSERT INTO drive_local_smart_views(id,name,revision,rules_json) VALUES(?,?,1,?)")
                .map_err(|e:sqlite::Error|e.to_string())?;
            q.bind((1, id.as_str())).map_err(|e| e.to_string())?;
            q.bind((2, name)).map_err(|e| e.to_string())?;
            q.bind((3, json.as_str())).map_err(|e| e.to_string())?;
            q.next().map_err(|e| e.to_string())?;
        }
        Ok(SmartView {
            id,
            name: name.to_string(),
            revision: next_revision,
            rule,
        })
    })();
    if result.is_ok() {
        if let Err(error) = conn.execute("RELEASE smart_view_save") {
            let _ = conn.execute("ROLLBACK TO smart_view_save; RELEASE smart_view_save");
            return Err(error.to_string());
        }
    } else {
        let _ = conn.execute("ROLLBACK TO smart_view_save; RELEASE smart_view_save");
    }
    result
}
fn delete(conn: &sqlite::Connection, id: &str, expected_revision: i64) -> Result<(), String> {
    check_view_id(id)?;
    if expected_revision < 1 {
        return Err("Expected revision is required to delete a Smart View.".into());
    }
    schema(conn)?;
    let mut q = conn
        .prepare("DELETE FROM drive_local_smart_views WHERE id=? AND revision=?")
        .map_err(|e: sqlite::Error| e.to_string())?;
    q.bind((1, id)).map_err(|e| e.to_string())?;
    q.bind((2, expected_revision)).map_err(|e| e.to_string())?;
    q.next().map_err(|e| e.to_string())?;
    let mut changed = conn
        .prepare("SELECT changes()")
        .map_err(|e: sqlite::Error| e.to_string())?;
    if changed.next().map_err(|e| e.to_string())? != sqlite::State::Row
        || changed.read::<i64, _>(0).map_err(|e| e.to_string())? != 1
    {
        return Err("Saved Smart View changed or was removed; refresh first.".into());
    }
    Ok(())
}
fn category(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "heic" => "image",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" => "video",
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" => "audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "csv" | "odt"
        | "rtf" => "document",
        "zip" | "tar" | "gz" | "7z" | "rar" | "bz2" => "archive",
        _ => "other",
    }
}
fn folder_path(
    id: &str,
    parents: &HashMap<String, (String, Option<String>)>,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<String, String> {
    if depth > 100 || !visited.insert(id.to_string()) {
        return Err("Smart View cannot resolve a cyclic or deep Drive folder tree.".into());
    }
    let (name, parent) = parents
        .get(id)
        .ok_or("Smart View folder is missing from current Drive.")?;
    let first = if let Some(parent) = parent {
        folder_path(parent, parents, visited, depth + 1)?
    } else {
        String::new()
    };
    visited.remove(id);
    Ok(format!("{first}/{name}"))
}
fn preview(view: &SmartView, snapshot: &DriveSnapshot) -> Result<SmartPreview, String> {
    if snapshot.files.len() > MAX_FILES {
        return Err("Smart View cannot inspect more than 25,000 visible files.".into());
    }
    let mut folders = HashMap::new();
    for f in snapshot
        .directories
        .iter()
        .filter(|f| f.trashed_at.is_none())
    {
        if !valid_id(&f.id)
            || !valid_drive_name(&f.name)
            || !f.parent_id.as_deref().is_none_or(valid_id)
            || folders
                .insert(f.id.clone(), (f.name.clone(), f.parent_id.clone()))
                .is_some()
        {
            return Err("Ambiguous Smart View Drive folder metadata.".into());
        }
    }
    let prefix = if let Some(ref id) = view.rule.folder_id {
        folder_path(id, &folders, &mut HashSet::new(), 0)?
    } else {
        String::new()
    };
    // Relative windows are calculated at preview time, not frozen when the
    // saved view is created. A seven-day rule remains seven days tomorrow.
    let rolling_cutoff = view.rule.updated_within_days.map(|days| {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(i64::from(days) * 86_400_000)
    });
    let mut matches = Vec::new();
    let mut total = 0usize;
    let mut seen = HashSet::new();
    for file in &snapshot.files {
        if !valid_id(&file.entry_id)
            || !valid_drive_name(&file.display_name)
            || !file.directory_id.as_deref().is_none_or(valid_id)
            || !seen.insert(file.entry_id.as_str())
        {
            return Err("Ambiguous Smart View file metadata.".into());
        }
        if file.total_size > MAX_SAFE_INTEGER {
            return Err("Unsafe Smart View file size.".into());
        }
        let rule = &view.rule;
        if rule
            .name_contains
            .as_ref()
            .is_some_and(|q| !file.display_name.to_lowercase().contains(&q.to_lowercase()))
            || rule.extension.as_ref().is_some_and(|q| {
                file.display_name
                    .rsplit_once('.')
                    .is_none_or(|(_, ext)| !ext.eq_ignore_ascii_case(q))
            })
            || rule
                .category
                .as_ref()
                .is_some_and(|q| category(&file.display_name) != q)
            || rule.min_size.is_some_and(|v| file.total_size < v)
            || rule.max_size.is_some_and(|v| file.total_size > v)
            || rule.updated_after.is_some_and(|v| file.updated_at < v)
            || rolling_cutoff.is_some_and(|v| file.updated_at < v)
        {
            continue;
        }
        let parent = if let Some(ref id) = file.directory_id {
            folder_path(id, &folders, &mut HashSet::new(), 0)?
        } else {
            String::new()
        };
        if rule.folder_id.is_some()
            && parent != prefix
            && !parent.starts_with(&format!("{prefix}/"))
        {
            continue;
        }
        total += 1;
        if matches.len() < MAX_RESULTS {
            matches.push(SmartMatch {
                entry_id: file.entry_id.clone(),
                name: file.display_name.clone(),
                path: format!("{parent}/{}", file.display_name),
                size: file.total_size,
                updated_at: file.updated_at,
                category: category(&file.display_name).to_string(),
            });
        }
    }
    matches.sort_by(|a, b| {
        a.path
            .to_lowercase()
            .cmp(&b.path.to_lowercase())
            .then(a.entry_id.cmp(&b.entry_id))
    });
    Ok(SmartPreview {
        view_id: view.id.clone(),
        revision: view.revision,
        snapshot_revision: snapshot.revision,
        total_matches: total,
        truncated: total > MAX_RESULTS,
        matches,
        local_only: true,
    })
}

#[tauri::command]
pub fn cmd_drive_smart_views_list(db: State<'_, DbConnection>) -> Result<Vec<SmartView>, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    list(&conn)
}
#[tauri::command]
pub fn cmd_drive_smart_views_save(
    db: State<'_, DbConnection>,
    id: Option<String>,
    name: String,
    revision: Option<i64>,
    rule: SmartRule,
) -> Result<SmartView, String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    save(&conn, id.as_deref(), &name, revision, rule)
}
#[tauri::command]
pub fn cmd_drive_smart_views_delete(
    db: State<'_, DbConnection>,
    id: String,
    revision: i64,
) -> Result<(), String> {
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    delete(&conn, &id, revision)
}
#[tauri::command]
pub fn cmd_drive_smart_views_preview(
    db: State<'_, DbConnection>,
    id: String,
) -> Result<SmartPreview, String> {
    check_view_id(&id)?;
    let conn = db
        .lock()
        .map_err(|_| "Drive database unavailable".to_string())?;
    let view = list(&conn)?
        .into_iter()
        .find(|v| v.id == id)
        .ok_or("Saved Smart View no longer exists; refresh the list.")?;
    let snapshot = drive_snapshot(&conn)?;
    preview(&view, &snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rule() -> SmartRule {
        SmartRule {
            name_contains: Some("report".into()),
            extension: Some("pdf".into()),
            category: Some("document".into()),
            min_size: Some(10),
            max_size: Some(1000),
            updated_after: Some(100),
            updated_within_days: None,
            folder_id: None,
        }
    }
    fn snapshot() -> DriveSnapshot {
        use crate::commands::drive_metadata::{DriveDirectoryRecord, DriveFileRecord};
        DriveSnapshot {
            revision: 4,
            dirty: false,
            directories: vec![
                DriveDirectoryRecord {
                    id: "root".into(),
                    parent_id: None,
                    name: "Work".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
                DriveDirectoryRecord {
                    id: "child".into(),
                    parent_id: Some("root".into()),
                    name: "Reports".into(),
                    backing_logical_channel_id: None,
                    created_at: 1,
                    updated_at: 1,
                    trashed_at: None,
                },
            ],
            files: vec![
                DriveFileRecord {
                    entry_id: "a".into(),
                    file_id: "fa".into(),
                    inode_id: None,
                    directory_id: Some("child".into()),
                    display_name: "Annual Report.PDF".into(),
                    total_size: 500,
                    mime_type: None,
                    first_message_id: 1,
                    backing_channel_id: 1,
                    logical_channel_id: "c".into(),
                    created_at: 1,
                    updated_at: 250,
                    encryption_version: 0,
                    plaintext_size: None,
                    crypto_id: None,
                    content_fingerprint: None,
                    storage_version: 1,
                },
                DriveFileRecord {
                    entry_id: "b".into(),
                    file_id: "fb".into(),
                    inode_id: None,
                    directory_id: Some("child".into()),
                    display_name: "Old Report.pdf".into(),
                    total_size: 5,
                    mime_type: None,
                    first_message_id: 1,
                    backing_channel_id: 1,
                    logical_channel_id: "c".into(),
                    created_at: 1,
                    updated_at: 250,
                    encryption_version: 0,
                    plaintext_size: None,
                    crypto_id: None,
                    content_fingerprint: None,
                    storage_version: 1,
                },
            ],
            pending: vec![],
            symlinks: vec![],
        }
    }
    #[test]
    fn live_view_changes_when_drive_metadata_changes() {
        let mut rule = rule();
        rule.folder_id = Some("root".into());
        let saved = SmartView {
            id: format!("smart-{:032x}", 1),
            name: "Latest reports".into(),
            revision: 1,
            rule,
        };
        let mut state = snapshot();
        let a = preview(&saved, &state).unwrap();
        assert_eq!(a.total_matches, 1);
        assert_eq!(a.matches[0].path, "/Work/Reports/Annual Report.PDF");
        state.files[0].updated_at = 50;
        assert_eq!(preview(&saved, &state).unwrap().total_matches, 0);
        assert_eq!(a.snapshot_revision, 4);
        let mut wrong = saved.clone();
        wrong.rule.folder_id = Some("missing".into());
        assert!(preview(&wrong, &state).is_err());
    }
    #[test]
    fn saved_rules_survive_reopen_and_optimistic_edits_reject_stale() {
        let path = std::env::temp_dir().join(format!("smart-view-{}.sqlite", new_drive_id()));
        let conn = sqlite::open(&path).unwrap();
        let saved = save(&conn, None, "Review", None, rule()).unwrap();
        let id = saved.id.clone();
        assert_eq!(list(&conn).unwrap().len(), 1);
        let updated = save(&conn, Some(&id), "Review PDFs", Some(1), rule()).unwrap();
        assert_eq!(updated.revision, 2);
        assert!(save(&conn, Some(&id), "Stale", Some(1), rule()).is_err());
        drop(conn);
        let conn = sqlite::open(&path).unwrap();
        assert_eq!(list(&conn).unwrap()[0], updated);
        assert!(delete(&conn, &id, 1).is_err());
        delete(&conn, &id, 2).unwrap();
        assert!(list(&conn).unwrap().is_empty());
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn corrupt_rules_and_invalid_filters_fail_closed_without_overwrite() {
        let conn = sqlite::open(":memory:").unwrap();
        let saved = save(&conn, None, "A", None, rule()).unwrap();
        let bad = SmartRule {
            name_contains: None,
            extension: Some("../pdf".into()),
            category: None,
            min_size: None,
            max_size: None,
            updated_after: None,
            updated_within_days: None,
            folder_id: None,
        };
        assert!(save(&conn, Some(&saved.id), "A", Some(1), bad).is_err());
        assert_eq!(list(&conn).unwrap()[0].revision, 1);
        conn.execute("UPDATE drive_local_smart_views SET rules_json='{\"extension\":\"../path\"}'")
            .unwrap();
        assert!(list(&conn).is_err());
        assert!(
            save(&conn, None, "Another", None, rule()).is_err(),
            "Corrupt stored data must not be silently replaced with a new view"
        );
    }
    #[test]
    fn rolling_date_filter_stays_relative_to_preview_time() {
        let mut criteria = rule();
        criteria.updated_after = None;
        criteria.updated_within_days = Some(7);
        let view = SmartView {
            id: format!("smart-{:032x}", 5),
            name: "Last seven days".into(),
            revision: 1,
            rule: criteria,
        };
        let mut state = snapshot();
        state.files[0].updated_at = chrono::Utc::now().timestamp_millis();
        assert_eq!(preview(&view, &state).unwrap().total_matches, 1);
        state.files[0].updated_at -= 8 * 86_400_000;
        assert_eq!(preview(&view, &state).unwrap().total_matches, 0);
        let mut invalid = view.rule.clone();
        invalid.updated_after = Some(1);
        assert!(
            check_rule(&invalid).is_err(),
            "rolling and fixed time windows are exclusive"
        );
        invalid.updated_after = None;
        invalid.updated_within_days = Some(0);
        assert!(check_rule(&invalid).is_err(), "zero-day window is invalid");
    }

    #[test]
    fn portable_long_drive_names_are_not_confused_with_short_view_titles() {
        let view = SmartView {
            id: format!("smart-{:032x}", 6),
            name: "Documents".into(),
            revision: 1,
            rule: rule(),
        };
        let mut state = snapshot();
        let name = format!("{}report.pdf", "a".repeat(150));
        assert!(name.len() > 100 && name.len() < 255);
        state.files[0].display_name = name;
        assert_eq!(preview(&view, &state).unwrap().total_matches, 1);
        assert!(!valid_name(&state.files[0].display_name));
        assert!(valid_drive_name(&state.files[0].display_name));
    }

    #[test]
    fn view_rejects_cycle_ambiguous_identity_and_size_overflow() {
        let view = SmartView {
            id: format!("smart-{:032x}", 3),
            name: "A".into(),
            revision: 1,
            rule: rule(),
        };
        let mut state = snapshot();
        state.directories[0].parent_id = Some("child".into());
        assert!(preview(&view, &state).is_err());
        state = snapshot();
        state.files[1].entry_id = "a".into();
        assert!(preview(&view, &state).is_err());
        state = snapshot();
        state.files[0].total_size = MAX_SAFE_INTEGER + 1;
        assert!(preview(&view, &state).is_err());
    }
}
