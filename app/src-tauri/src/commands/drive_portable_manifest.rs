//! Portable, opt-in, metadata-only Drive inventory manifest.
//! Not a file backup. No file bytes, Telegram traffic, or Drive metadata writes.
use crate::commands::drive_metadata::portable_name_key;
use crate::commands::drive_time_capsule::{
    capture_snapshot, compare, validate_capsule, Capsule, CapsuleComparison, CapsuleInfo,
};
use crate::db::DbConnection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};
use tauri::State;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortableManifest {
    format: String,
    version: u32,
    captured_at: i64,
    inventory_sha256: String,
    envelope_sha256: String,
    inventory: Capsule,
}

#[derive(Debug, Serialize)]
pub struct PortableExportReceipt {
    pub destination_path: String,
    pub captured_at: i64,
    pub item_count: usize,
    pub revision: i64,
    pub dirty: bool,
    pub checksum: String,
}

#[derive(Debug, Serialize)]
pub struct PortableComparison {
    pub captured_at: i64,
    pub checksum: String,
    pub metadata_only: bool,
    pub comparison: CapsuleComparison,
}

fn digest(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn validate_graph(c: &Capsule) -> Result<(), String> {
    validate_capsule(c)?;
    if c.entries.len() > MAX_ENTRIES {
        return Err("Portable inventory exceeds 5000 entries".into());
    }
    let mut folders = HashMap::new();
    for e in &c.entries {
        if e.kind == "folder" && folders.insert(e.id.as_str(), e).is_some() {
            return Err("Duplicate folder identity in portable manifest".into());
        }
    }
    let mut paths: HashMap<String, &crate::commands::drive_time_capsule::Entry> = HashMap::new();
    for e in &c.entries {
        let mut names = vec![e.name.as_str()];
        let mut parent = e.parent.as_deref();
        let mut ancestors = HashSet::new();
        while let Some(id) = parent {
            if !ancestors.insert(id) || ancestors.len() > 512 {
                return Err("Manifest contains a parent cycle or excessive folder depth".into());
            }
            let d = folders
                .get(id)
                .ok_or("Manifest contains an orphaned parent")?;
            names.push(d.name.as_str());
            parent = d.parent.as_deref();
        }
        names.reverse();
        if e.path != format!("/{}", names.join("/")) {
            return Err("Manifest path does not match its parent hierarchy".into());
        }
        let key = e
            .path
            .split('/')
            .map(portable_name_key)
            .collect::<Vec<_>>()
            .join("/");
        if let Some(previous) = paths.insert(key, e) {
            // An unfinished copy-on-write upload intentionally overlays its completed base.
            // Keep both metadata records instead of misreporting a valid pending edit as
            // an irrecoverably colliding namespace. Every other duplicate is rejected.
            let pair = if e.kind == "pending" && previous.kind == "file" {
                Some((e, previous))
            } else if e.kind == "file" && previous.kind == "pending" {
                Some((previous, e))
            } else {
                None
            };
            match pair {
                Some((pending, base))
                    if pending.fingerprint == base.id && pending.path == base.path => {}
                _ => return Err("Manifest has colliding portable paths".into()),
            }
        }
        let backing_valid = match e.kind.as_str() {
            "folder" | "link" => e.backing == "metadata" && e.size == 0 && e.fingerprint.is_empty(),
            "file" => matches!(
                e.backing.as_str(),
                "legacy" | "v2" | "legacy_unavailable" | "v2_unavailable"
            ),
            "pending" => matches!(e.backing.as_str(), "open" | "closed"),
            _ => false,
        };
        if !backing_valid {
            return Err("Manifest kind and backing state disagree".into());
        }
    }
    Ok(())
}

fn signed_digest(m: &PortableManifest) -> Result<String, String> {
    let signed = serde_json::to_vec(&(
        &m.format,
        m.version,
        m.captured_at,
        &m.inventory_sha256,
        &m.inventory,
    ))
    .map_err(|_| "Could not hash manifest envelope")?;
    Ok(digest(&signed))
}

fn encode(capsule: Capsule, at: i64) -> Result<(String, PortableManifest), String> {
    validate_graph(&capsule)?;
    if at <= 0 {
        return Err("Invalid manifest timestamp".into());
    }
    let body = serde_json::to_vec(&capsule).map_err(|_| "Could not serialize Drive inventory")?;
    let manifest = PortableManifest {
        format: "terarelay-drive-portable-inventory".into(),
        version: 1,
        captured_at: at,
        inventory_sha256: digest(&body),
        envelope_sha256: String::new(),
        inventory: capsule,
    };
    let mut manifest = manifest;
    manifest.envelope_sha256 = signed_digest(&manifest)?;
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|_| "Could not encode portable manifest")?;
    if json.len() > MAX_BYTES {
        return Err("Portable metadata export exceeds 4 MiB; nothing was written".into());
    }
    Ok((json, manifest))
}

fn decode(bytes: &[u8]) -> Result<PortableManifest, String> {
    if bytes.len() > MAX_BYTES {
        return Err("Portable manifest exceeds 4 MiB".into());
    }
    let m: PortableManifest = serde_json::from_slice(bytes)
        .map_err(|_| "Manifest is truncated, malformed or has an unsupported schema")?;
    if m.format != "terarelay-drive-portable-inventory"
        || m.version != 1
        || m.captured_at <= 0
        || m.inventory_sha256.len() != 64
        || m.envelope_sha256.len() != 64
        || !m
            .inventory_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || !m
            .envelope_sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err("Manifest format, version, time or checksum is invalid".into());
    }
    if signed_digest(&m)? != m.envelope_sha256 {
        return Err("Manifest envelope SHA-256 verification failed".into());
    }
    validate_graph(&m.inventory)?;
    let canonical = serde_json::to_vec(&m.inventory)
        .map_err(|_| "Could not serialize imported Drive metadata")?;
    if digest(&canonical) != m.inventory_sha256 {
        return Err("Manifest SHA-256 verification failed".into());
    }
    Ok(m)
}

fn checked_path(path: &str) -> Result<&Path, String> {
    if path.len() > 4096 || !path.ends_with(".json") {
        return Err("Choose an absolute .json manifest destination".into());
    }
    let p = Path::new(path);
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Manifest path must be absolute without traversal".into());
    }
    Ok(p)
}

fn safe_parent(p: &Path) -> Result<(), String> {
    // Reject symlinked directory components, not just the target file.
    let parent = p
        .parent()
        .ok_or("Manifest destination has no parent directory")?;
    let mut current = std::path::PathBuf::new();
    for component in parent.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        let md = fs::symlink_metadata(&current)
            .map_err(|_| "Manifest parent directory is unavailable")?;
        if md.file_type().is_symlink() || !md.is_dir() {
            return Err("Manifest destination parent must contain only actual directories".into());
        }
    }
    Ok(())
}

fn write_new(destination: &str, contents: &[u8]) -> Result<(), String> {
    let p = checked_path(destination)?;
    safe_parent(p)?;
    // Never expose an incomplete JSON at the final path. First write+fsync a
    // uniquely named temporary file beside the destination on the same FS.
    let staging = p.with_file_name(format!(
        ".terarelay-portable-manifest-{}.tmp",
        rand::random::<u128>()
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&staging)
        .map_err(|_| "Could not create exclusive temporary manifest file")?;
    let write_result = file.write_all(contents).and_then(|()| file.sync_all());
    drop(file);
    if let Err(err) = write_result {
        let _ = fs::remove_file(&staging);
        return Err(format!("Manifest staging was incomplete: {err}"));
    }
    // hard_link is a create-only atomic publication: it fails rather than
    // replacing an existing file, unlike fs::rename on common platforms.
    let publish_result = fs::hard_link(&staging, p);
    let _ = fs::remove_file(&staging);
    publish_result.map_err(|_| {
        "Manifest destination exists or atomic no-clobber publication failed; no file was overwritten".to_string()
    })?;
    #[cfg(unix)]
    {
        // Do not remove an already published manifest on fsync failure: tell
        // the caller to inspect it, rather than claim the outcome rolled back.
        let directory = fs::File::open(p.parent().ok_or("Missing manifest directory")?)
            .map_err(|_| "Manifest published but durability confirmation unavailable")?;
        directory
            .sync_all()
            .map_err(|_| "Manifest published but directory durability is uncertain")?;
    }
    Ok(())
}

fn read_checked(path: &str) -> Result<Vec<u8>, String> {
    let p = checked_path(path)?;
    safe_parent(p)?;
    let md = fs::symlink_metadata(p).map_err(|_| "Manifest file not found")?;
    if !md.is_file() || md.len() > MAX_BYTES as u64 {
        return Err("Manifest must be a regular file no larger than 4 MiB".into());
    }
    let f = fs::File::open(p).map_err(|_| "Cannot open selected manifest")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = f.metadata().map_err(|_| "Cannot stat selected manifest")?;
        if opened.dev() != md.dev() || opened.ino() != md.ino() {
            return Err("Selected manifest changed while being opened".into());
        }
    }
    let mut bytes = Vec::new();
    f.take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read selected manifest")?;
    if bytes.len() > MAX_BYTES {
        return Err("Manifest is too large".into());
    }
    Ok(bytes)
}

#[tauri::command]
pub fn cmd_drive_portable_manifest_export(
    db: State<'_, DbConnection>,
    destination_path: String,
) -> Result<PortableExportReceipt, String> {
    let p = checked_path(&destination_path)?;
    safe_parent(p)?;
    // Capture one coherent local database snapshot, then release the DB before filesystem I/O.
    let capsule = {
        let conn = db.lock().map_err(|_| "Drive database unavailable")?;
        capture_snapshot(&conn)?
    };
    let (json, m) = encode(capsule, chrono::Utc::now().timestamp_millis())?;
    write_new(&destination_path, json.as_bytes())?;
    Ok(PortableExportReceipt {
        destination_path,
        captured_at: m.captured_at,
        item_count: m.inventory.entries.len(),
        revision: m.inventory.revision,
        dirty: m.inventory.dirty,
        checksum: m.inventory_sha256,
    })
}

#[tauri::command]
pub fn cmd_drive_portable_manifest_compare(
    db: State<'_, DbConnection>,
    source_path: String,
) -> Result<PortableComparison, String> {
    let bytes = read_checked(&source_path)?;
    let old = decode(&bytes)?;
    // A snapshot can change between export and comparison. Do not infer cloud synchronization.
    let current = {
        let conn = db.lock().map_err(|_| "Drive database unavailable")?;
        capture_snapshot(&conn)?
    };
    let old_info = CapsuleInfo {
        seq: 1,
        captured_at: old.captured_at,
        revision: old.inventory.revision,
        dirty: old.inventory.dirty,
        item_count: old.inventory.entries.len(),
        fingerprint: old.inventory_sha256.clone(),
    };
    let current_info = CapsuleInfo {
        seq: 2,
        captured_at: chrono::Utc::now().timestamp_millis(),
        revision: current.revision,
        dirty: current.dirty,
        item_count: current.entries.len(),
        fingerprint: digest(
            &serde_json::to_vec(&current).map_err(|_| "Cannot encode current inventory")?,
        ),
    };
    let changes = compare(old_info, old.inventory, current_info, current);
    Ok(PortableComparison {
        captured_at: old.captured_at,
        checksum: old.inventory_sha256,
        metadata_only: true,
        comparison: changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(
        kind: &str,
        id: &str,
        parent: Option<&str>,
        name: &str,
        path: &str,
    ) -> crate::commands::drive_time_capsule::Entry {
        crate::commands::drive_time_capsule::Entry {
            kind: kind.into(),
            id: id.into(),
            parent: parent.map(str::to_owned),
            name: name.into(),
            path: path.into(),
            backing: if kind == "file" { "v2" } else { "metadata" }.into(),
            size: 0,
            fingerprint: String::new(),
        }
    }
    fn fixture() -> Capsule {
        Capsule {
            version: 1,
            revision: 7,
            dirty: false,
            entries: vec![
                entry("folder", "a", None, "Documents", "/Documents"),
                entry(
                    "file",
                    "b",
                    Some("a"),
                    "report.txt",
                    "/Documents/report.txt",
                ),
            ],
        }
    }
    #[test]
    fn verified_roundtrip_and_checksum_tamper_rejected() {
        let (json, m) = encode(fixture(), 123).unwrap();
        assert_eq!(decode(json.as_bytes()).unwrap().inventory.entries.len(), 2);
        assert_eq!(m.inventory_sha256.len(), 64);
        let tampered = json.replace("report.txt", "report.pdf");
        assert!(decode(tampered.as_bytes()).is_err());
        assert!(decode(
            json.replace("\"captured_at\": 123", "\"captured_at\": 999")
                .as_bytes()
        )
        .is_err());
        assert!(decode(&json.as_bytes()[..json.len() - 2]).is_err());
    }
    #[test]
    fn invalid_parent_cycle_path_and_duplicate_identity_fail_closed() {
        let mut c = fixture();
        c.entries[1].parent = Some("missing".into());
        assert!(encode(c, 123).is_err());
        let mut c = fixture();
        c.entries[0].parent = Some("a".into());
        assert!(encode(c, 123).is_err());
        let mut c = fixture();
        c.entries[1].path = "/Documents/other.txt".into();
        assert!(encode(c, 123).is_err());
        let mut c = fixture();
        c.entries.push(c.entries[1].clone());
        assert!(encode(c, 123).is_err());
        let mut c = fixture();
        c.entries[1].name = "../escape".into();
        assert!(encode(c, 123).is_err());
    }
    #[test]
    fn overlayed_copy_on_write_is_held_as_two_distinct_entries() {
        let mut c = fixture();
        let mut pending = entry(
            "pending",
            "staged",
            Some("a"),
            "report.txt",
            "/Documents/report.txt",
        );
        pending.backing = "closed".into();
        pending.fingerprint = "b".into(); // exact base entry identity
        c.entries.push(pending);
        assert!(encode(c.clone(), 123).is_ok());
        c.entries.last_mut().unwrap().fingerprint = "different".into();
        assert!(encode(c, 123).is_err());
    }
    #[test]
    fn portable_unicode_name_collisions_are_rejected() {
        let mut c = fixture();
        c.entries.push(entry(
            "file",
            "other",
            Some("a"),
            "REPÓRT.txt",
            "/Documents/REPÓRT.txt",
        ));
        c.entries[1].name = "REPÓRT.txt".into();
        c.entries[1].path = "/Documents/REPÓRT.txt".into();
        assert!(encode(c, 123).is_err());
    }
    #[test]
    fn export_failure_does_not_leave_partial_destination() {
        let dir = std::env::temp_dir().join(format!(
            "terarelay-manifest-atomic-{}",
            rand::random::<u128>()
        ));
        fs::create_dir(&dir).unwrap();
        let dest = dir.join("inventory.json");
        let path = dest.to_string_lossy().to_string();
        let (json, _) = encode(fixture(), 123).unwrap();
        write_new(&path, json.as_bytes()).unwrap();
        let original = fs::read(&dest).unwrap();
        assert!(write_new(&path, b"WRONG").is_err());
        assert_eq!(fs::read(&dest).unwrap(), original);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(dest).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn size_schema_and_checksum_validation() {
        let (json, _) = encode(fixture(), 123).unwrap();
        assert!(decode(&vec![b' '; MAX_BYTES + 1]).is_err());
        assert!(decode(json.replace("\"version\": 1", "\"version\": 2").as_bytes()).is_err());
        assert!(decode(
            json.replace(
                "\"inventory_sha256\":",
                "\"unknown\": 1,\n  \"inventory_sha256\":"
            )
            .as_bytes()
        )
        .is_err());
    }
    #[test]
    fn create_only_export_and_import_preserve_original_bytes() {
        let dir = std::env::temp_dir().join(format!(
            "terarelay-manifest-test-{}",
            rand::random::<u128>()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("portable.json");
        let name = path.to_string_lossy().to_string();
        let (json, _) = encode(fixture(), 123).unwrap();
        write_new(&name, json.as_bytes()).unwrap();
        assert!(write_new(&name, b"overwritten").is_err());
        assert_eq!(read_checked(&name).unwrap(), json.as_bytes());
        assert_eq!(
            decode(&read_checked(&name).unwrap())
                .unwrap()
                .inventory
                .revision,
            7
        );
        fs::remove_file(&path).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
    #[test]
    fn destination_traversal_and_symlink_are_rejected() {
        assert!(checked_path("relative.json").is_err());
        assert!(checked_path("/tmp/../escape.json").is_err());
        assert!(checked_path("/tmp/escape.zip").is_err());
        let dir = std::env::temp_dir().join(format!(
            "terarelay-manifest-symlink-{}",
            rand::random::<u128>()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("record.json");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp/missing", &path).unwrap();
            assert!(read_checked(&path.to_string_lossy()).is_err());
            assert!(write_new(&path.to_string_lossy(), b"hi").is_err());
            fs::remove_file(&path).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
    }
}
