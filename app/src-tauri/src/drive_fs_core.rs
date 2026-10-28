use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

pub const MAX_PROVIDER_IDENTITY_BYTES: usize = 512;
const PROVIDER_IDENTITY_MAGIC: &[u8; 4] = b"TRD1";
const MAX_RECURSIVE_REMOVE_DEPTH: usize = 512;
const MAX_RECURSIVE_REMOVE_ITEMS: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriveNodeId {
    Root,
    Directory(String),
    File(String),
    Pending(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveNodeKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveNodeMetadata {
    pub id: DriveNodeId,
    pub parent: DriveNodeId,
    pub name: String,
    pub kind: DriveNodeKind,
    pub size: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Opaque immutable backing-content identity for native provider versioning.
    /// Remote files use their logical/storage object id; pending writes use the
    /// unique pending id so same-size rapid replacements never share a version.
    pub content_version: String,
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveProjectionRecord {
    pub id: DriveNodeId,
    pub path: Vec<String>,
    pub kind: DriveNodeKind,
    pub content_version: String,
    pub size: u64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveLocalProjectionRecord {
    pub id: DriveNodeId,
    pub path: Vec<String>,
    pub locally_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOfflineProjectionAction {
    Rename { id: DriveNodeId, path: Vec<String> },
    Delete { id: DriveNodeId },
    Conflict { id: DriveNodeId },
}

/// Compare the last successfully projected remote namespace with the current
/// remote namespace and the current local placeholder state. Namespace changes
/// are replayed only when the previous projection proves they happened locally.
/// Dirty content also participates in conflict detection so reconnect never
/// silently overwrites a concurrent remote content change.
pub fn plan_offline_projection_recovery(
    baseline: Option<&[DriveProjectionRecord]>,
    remote: &[DriveProjectionRecord],
    local: &[DriveLocalProjectionRecord],
) -> Vec<DriveOfflineProjectionAction> {
    let remote_by_id: HashMap<&DriveNodeId, &DriveProjectionRecord> =
        remote.iter().map(|record| (&record.id, record)).collect();
    let local_by_id: HashMap<&DriveNodeId, &DriveLocalProjectionRecord> =
        local.iter().map(|record| (&record.id, record)).collect();
    let Some(baseline) = baseline else {
        return local
            .iter()
            .filter(|local_record| {
                local_record.locally_dirty
                    || !remote_by_id
                        .get(&local_record.id)
                        .is_some_and(|remote_record| remote_record.path == local_record.path)
            })
            .map(|local_record| DriveOfflineProjectionAction::Conflict {
                id: local_record.id.clone(),
            })
            .collect();
    };

    let baseline_ids: std::collections::HashSet<&DriveNodeId> =
        baseline.iter().map(|record| &record.id).collect();
    let mut actions = Vec::new();
    for previous in baseline {
        let remote_record = remote_by_id.get(&previous.id).copied();
        let local_record = local_by_id.get(&previous.id).copied();

        let Some(remote_record) = remote_record else {
            match local_record {
                None => {}
                Some(local_record)
                    if local_record.path == previous.path && !local_record.locally_dirty => {}
                Some(_) => actions.push(DriveOfflineProjectionAction::Conflict {
                    id: previous.id.clone(),
                }),
            }
            continue;
        };

        if remote_record.kind != previous.kind {
            actions.push(DriveOfflineProjectionAction::Conflict {
                id: previous.id.clone(),
            });
            continue;
        }

        let remote_path_changed = remote_record.path != previous.path;
        let remote_content_changed = previous.kind == DriveNodeKind::File
            && (remote_record.content_version != previous.content_version
                || remote_record.size != previous.size);

        let Some(local_record) = local_record else {
            if remote_path_changed || remote_content_changed {
                actions.push(DriveOfflineProjectionAction::Conflict {
                    id: previous.id.clone(),
                });
            } else {
                actions.push(DriveOfflineProjectionAction::Delete {
                    id: previous.id.clone(),
                });
            }
            continue;
        };

        let local_path_changed = local_record.path != previous.path;
        let paths_converged = local_record.path == remote_record.path;

        if remote_content_changed && local_record.locally_dirty {
            actions.push(DriveOfflineProjectionAction::Conflict {
                id: previous.id.clone(),
            });
            continue;
        }
        if remote_path_changed && local_path_changed && !paths_converged {
            actions.push(DriveOfflineProjectionAction::Conflict {
                id: previous.id.clone(),
            });
            continue;
        }
        if remote_path_changed && !local_path_changed {
            if local_record.locally_dirty {
                actions.push(DriveOfflineProjectionAction::Conflict {
                    id: previous.id.clone(),
                });
            }
            continue;
        }
        if !remote_path_changed && local_path_changed {
            actions.push(DriveOfflineProjectionAction::Rename {
                id: previous.id.clone(),
                path: local_record.path.clone(),
            });
        }
    }

    for local_record in local {
        if baseline_ids.contains(&local_record.id) {
            continue;
        }
        let safe = !local_record.locally_dirty
            && remote_by_id
                .get(&local_record.id)
                .is_some_and(|remote_record| remote_record.path == local_record.path);
        if !safe {
            actions.push(DriveOfflineProjectionAction::Conflict {
                id: local_record.id.clone(),
            });
        }
    }
    actions
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveVirtualCapacity {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveRecoverySummary {
    pub recovered: u32,
    pub remaining: u32,
    pub failures: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveFsError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidInput(String),
    AccessDenied(String),
    Locked(String),
    NoSpace(String),
    Cancelled,
    Unavailable(String),
    Internal(String),
}

impl fmt::Display for DriveFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "Drive item was not found"),
            Self::AlreadyExists => write!(f, "Drive item already exists"),
            Self::NotDirectory => write!(f, "Drive item is not a directory"),
            Self::IsDirectory => write!(f, "Drive item is a directory"),
            Self::DirectoryNotEmpty => write!(f, "Drive directory is not empty"),
            Self::InvalidInput(message)
            | Self::AccessDenied(message)
            | Self::Locked(message)
            | Self::NoSpace(message)
            | Self::Unavailable(message)
            | Self::Internal(message) => f.write_str(message),
            Self::Cancelled => write!(f, "Drive operation was cancelled"),
        }
    }
}

impl std::error::Error for DriveFsError {}

pub trait DriveFsBackend: Send + Sync {
    fn revision(&self) -> Result<u64, DriveFsError>;
    fn metadata(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError>;
    fn lookup(&self, parent: &DriveNodeId, name: &str) -> Result<DriveNodeMetadata, DriveFsError>;
    fn list_directory(&self, parent: &DriveNodeId) -> Result<Vec<DriveNodeMetadata>, DriveFsError>;
    fn read(&self, id: &DriveNodeId, offset: u64, length: u64) -> Result<Vec<u8>, DriveFsError>;
    fn create_file(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError>;
    fn create_directory(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError>;
    fn begin_write(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError>;
    fn write(&self, id: &DriveNodeId, offset: u64, data: &[u8]) -> Result<u64, DriveFsError>;
    fn resize(&self, id: &DriveNodeId, size: u64) -> Result<(), DriveFsError>;
    fn flush(&self, id: &DriveNodeId) -> Result<(), DriveFsError>;
    fn close_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError>;
    fn abort_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError>;
    fn rename(
        &self,
        id: &DriveNodeId,
        new_parent: &DriveNodeId,
        new_name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError>;
    fn remove_file(&self, id: &DriveNodeId) -> Result<(), DriveFsError>;
    fn remove_directory(&self, id: &DriveNodeId) -> Result<(), DriveFsError>;
    fn recover_pending(&self) -> Result<DriveRecoverySummary, DriveFsError>;
    fn capacity(&self) -> DriveVirtualCapacity;
}

#[derive(Clone)]
pub struct DriveFsCore {
    backend: Arc<dyn DriveFsBackend>,
}

impl DriveFsCore {
    pub fn new(backend: Arc<dyn DriveFsBackend>) -> Self {
        Self { backend }
    }

    pub fn revision(&self) -> Result<u64, DriveFsError> {
        self.backend.revision()
    }

    pub fn metadata(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
        let item = self.backend.metadata(id)?;
        match id {
            DriveNodeId::Root => {
                if item.id != DriveNodeId::Root
                    || item.parent != DriveNodeId::Root
                    || item.kind != DriveNodeKind::Directory
                    || item.pending
                {
                    return Err(DriveFsError::InvalidInput(
                        "Drive root metadata has an invalid identity or kind".to_string(),
                    ));
                }
            }
            DriveNodeId::Directory(_) | DriveNodeId::File(_) => {
                if item.id != *id {
                    return Err(DriveFsError::InvalidInput(
                        "Drive metadata returned another node identity".to_string(),
                    ));
                }
                validate_nonroot_metadata(&item)?;
            }
            DriveNodeId::Pending(pending_id) => {
                // A staging lookup intentionally exposes the logical File ID
                // (or its base entry), while content_version is the staging
                // identity. Do not require the returned File ID to equal the
                // requested Pending ID.
                if !item.pending
                    || !matches!(item.id, DriveNodeId::File(_))
                    || item.content_version != *pending_id
                {
                    return Err(DriveFsError::InvalidInput(
                        "Drive pending metadata has a mismatched staging identity".to_string(),
                    ));
                }
                validate_nonroot_metadata(&item)?;
            }
        }
        Ok(item)
    }

    pub fn lookup(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        require_directory_parent(parent)?;
        let child = self.backend.lookup(parent, name)?;
        validate_child_metadata(parent, &child)?;
        // The backend may preserve on-disk capitalization or Unicode form,
        // but it must not resolve one requested path component to another.
        if crate::commands::drive_metadata::portable_name_key(name)
            != crate::commands::drive_metadata::portable_name_key(&child.name)
        {
            return Err(DriveFsError::InvalidInput(
                "Drive lookup returned a different child name".to_string(),
            ));
        }
        Ok(child)
    }

    pub fn list_directory(
        &self,
        parent: &DriveNodeId,
    ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
        require_directory_parent(parent)?;
        let children = self.backend.list_directory(parent)?;
        // Reject a corrupt backend namespace as a whole instead of giving
        // Windows/FUSE a partial listing containing other parents, invalid
        // identities, duplicate inodes, or cross-platform name collisions.
        // Enumeration is read-only: never normalize or rename stored entries.
        let mut seen = HashSet::with_capacity(children.len());
        let mut names = HashSet::with_capacity(children.len());
        for child in &children {
            validate_child_metadata(parent, child)?;
            if !seen.insert(&child.id) {
                return Err(DriveFsError::InvalidInput(
                    "Drive directory lists a duplicate child identity".to_string(),
                ));
            }
            if !names.insert(crate::commands::drive_metadata::portable_name_key(
                &child.name,
            )) {
                return Err(DriveFsError::InvalidInput(
                    "Drive directory has a portable name collision".to_string(),
                ));
            }
        }
        Ok(children)
    }

    pub fn read(
        &self,
        id: &DriveNodeId,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        require_file_node(id)?;
        let _ = offset
            .checked_add(length)
            .ok_or_else(|| DriveFsError::InvalidInput("Drive read range overflow".to_string()))?;
        let bytes = self.backend.read(id, offset, length)?;
        let returned = u64::try_from(bytes.len()).map_err(|_| {
            DriveFsError::InvalidInput("Drive read result exceeds addressable range".to_string())
        })?;
        if returned > length {
            return Err(DriveFsError::InvalidInput(
                "Drive backend returned more bytes than requested".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub fn read_versioned(
        &self,
        id: &DriveNodeId,
        expected_content_version: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        self.read_versioned_internal(id, expected_content_version, None, offset, length)
    }

    /// Pin every chunk of a Windows hydration to the same original pending
    /// metadata. Pending staging IDs are stable even when file bytes change
    /// between distinct chunk reads, so a content-version string alone is
    /// insufficient to prevent a mixed-state placeholder.
    pub fn read_snapshot_versioned(
        &self,
        id: &DriveNodeId,
        initial: &DriveNodeMetadata,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        if initial.id != *id || initial.kind != DriveNodeKind::File {
            return Err(DriveFsError::InvalidInput(
                "Drive hydration snapshot has the wrong identity or kind".to_string(),
            ));
        }
        self.read_versioned_internal(id, &initial.content_version, Some(initial), offset, length)
    }

    fn read_versioned_internal(
        &self,
        id: &DriveNodeId,
        expected_content_version: &str,
        initial: Option<&DriveNodeMetadata>,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        require_file_node(id)?;
        let before = self.metadata(id)?;
        if let Some(snapshot) = initial {
            Self::check_pending_hydration_snapshot(snapshot, &before)?;
        }
        if before.content_version != expected_content_version {
            return Err(DriveFsError::Unavailable(
                "Drive content version changed during read".to_string(),
            ));
        }
        let bytes = self.read(id, offset, length)?;
        let after = self.metadata(id)?;
        if let Some(snapshot) = initial {
            Self::check_pending_hydration_snapshot(snapshot, &after)?;
        }
        if after.content_version != expected_content_version {
            return Err(DriveFsError::Unavailable(
                "Drive content version changed during read".to_string(),
            ));
        }
        // A writable pending file keeps its staging ID as content_version.
        // Another writer can resize or change those bytes without changing
        // that ID. Refuse a potentially mixed-version provider hydration.
        if (before.pending || after.pending)
            && (before.pending != after.pending
                || before.size != after.size
                || before.updated_at_ms != after.updated_at_ms)
        {
            return Err(DriveFsError::Unavailable(
                "Drive pending content changed during read".to_string(),
            ));
        }
        Ok(bytes)
    }

    fn check_pending_hydration_snapshot(
        initial: &DriveNodeMetadata,
        current: &DriveNodeMetadata,
    ) -> Result<(), DriveFsError> {
        if (initial.pending || current.pending)
            && (initial.pending != current.pending
                || initial.size != current.size
                || initial.updated_at_ms != current.updated_at_ms)
        {
            return Err(DriveFsError::Unavailable(
                "Drive pending content changed since hydration began".to_string(),
            ));
        }
        Ok(())
    }

    pub fn create_file(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        require_directory_parent(parent)?;
        let created = self.backend.create_file(parent, name)?;
        validate_created_metadata(parent, name, DriveNodeKind::File, &created)?;
        Ok(created)
    }

    pub fn create_directory(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        require_directory_parent(parent)?;
        let created = self.backend.create_directory(parent, name)?;
        validate_created_metadata(parent, name, DriveNodeKind::Directory, &created)?;
        Ok(created)
    }

    pub fn begin_write(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
        if matches!(id, DriveNodeId::Root | DriveNodeId::Directory(_)) {
            return Err(DriveFsError::IsDirectory);
        }
        let opened = self.backend.begin_write(id)?;
        validate_nonroot_metadata(&opened)?;
        let valid = opened.kind == DriveNodeKind::File
            && opened.pending
            && !opened.content_version.is_empty()
            && match id {
                DriveNodeId::File(_) => opened.id == *id,
                DriveNodeId::Pending(stage) => {
                    matches!(&opened.id, DriveNodeId::File(_)) && opened.content_version == *stage
                }
                DriveNodeId::Root | DriveNodeId::Directory(_) => false,
            };
        if !valid {
            return Err(DriveFsError::InvalidInput(
                "Drive begin-write returned an unrelated staging identity".to_string(),
            ));
        }
        Ok(opened)
    }

    pub fn write(&self, id: &DriveNodeId, offset: u64, data: &[u8]) -> Result<u64, DriveFsError> {
        require_file_node(id)?;
        let data_len = u64::try_from(data.len())
            .map_err(|_| DriveFsError::InvalidInput("Drive write is too large".to_string()))?;
        let _ = offset
            .checked_add(data_len)
            .ok_or_else(|| DriveFsError::InvalidInput("Drive write range overflow".to_string()))?;
        let written = self.backend.write(id, offset, data)?;
        if written > data_len {
            return Err(DriveFsError::InvalidInput(
                "Drive backend reported more written bytes than supplied".to_string(),
            ));
        }
        Ok(written)
    }

    pub fn resize(&self, id: &DriveNodeId, size: u64) -> Result<(), DriveFsError> {
        require_file_node(id)?;
        self.backend.resize(id, size)
    }

    pub fn flush(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        require_file_node(id)?;
        self.backend.flush(id)
    }

    pub fn close_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        require_file_node(id)?;
        self.backend.close_write(id)
    }

    pub fn abort_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        require_file_node(id)?;
        self.backend.abort_write(id)
    }

    pub fn rename(
        &self,
        id: &DriveNodeId,
        new_parent: &DriveNodeId,
        new_name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(new_name)?;
        require_directory_parent(new_parent)?;
        match id {
            DriveNodeId::Root => {
                return Err(DriveFsError::AccessDenied(
                    "Cannot rename the Drive root".into(),
                ));
            }
            DriveNodeId::Directory(value)
            | DriveNodeId::File(value)
            | DriveNodeId::Pending(value)
                if value.trim().is_empty() =>
            {
                return Err(DriveFsError::InvalidInput(
                    "Drive rename source ID is invalid".into(),
                ));
            }
            _ => {}
        }
        let renamed = self.backend.rename(id, new_parent, new_name)?;
        validate_renamed_metadata(id, new_parent, new_name, &renamed)?;
        Ok(renamed)
    }

    pub fn remove_file(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        if matches!(id, DriveNodeId::Root) {
            return Err(DriveFsError::AccessDenied(
                "Cannot remove the Drive root".to_string(),
            ));
        }
        require_file_node(id)?;
        self.backend.remove_file(id)
    }

    pub fn remove_directory(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        match id {
            DriveNodeId::Root => {
                return Err(DriveFsError::AccessDenied(
                    "Cannot remove the Drive root".to_string(),
                ));
            }
            DriveNodeId::File(_) | DriveNodeId::Pending(_) => {
                return Err(DriveFsError::NotDirectory);
            }
            DriveNodeId::Directory(_) => {}
        }
        self.backend.remove_directory(id)
    }

    pub fn remove_recursive(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        if matches!(id, DriveNodeId::Root) {
            return Err(DriveFsError::AccessDenied(
                "Cannot recursively remove the Drive root".to_string(),
            ));
        }

        // Validate the entire tree before deleting any entries. Previously a
        // late listing error, cycle, or depth/item limit could leave a partially
        // deleted directory, even though the operation was rejected.
        let mut seen = HashSet::new();
        let mut postorder = Vec::new();
        self.plan_recursive_remove(id, 0, &mut seen, &mut postorder)?;
        for (node_id, kind) in postorder {
            match kind {
                DriveNodeKind::File => self.remove_file(&node_id)?,
                DriveNodeKind::Directory => self.remove_directory(&node_id)?,
            }
        }
        Ok(())
    }

    fn plan_recursive_remove(
        &self,
        id: &DriveNodeId,
        depth: usize,
        seen: &mut HashSet<DriveNodeId>,
        postorder: &mut Vec<(DriveNodeId, DriveNodeKind)>,
    ) -> Result<(), DriveFsError> {
        if depth > MAX_RECURSIVE_REMOVE_DEPTH {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal hierarchy is too deep".to_string(),
            ));
        }
        if matches!(id, DriveNodeId::Root) {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal contains the root".to_string(),
            ));
        }
        if !seen.insert(id.clone()) {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal contains a cycle or duplicate entry".to_string(),
            ));
        }
        if seen.len() > MAX_RECURSIVE_REMOVE_ITEMS {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal exceeds the safe item limit".to_string(),
            ));
        }

        let metadata = self.metadata(id)?;
        if metadata.id != *id {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal metadata identity mismatch".to_string(),
            ));
        }
        if metadata.kind == DriveNodeKind::Directory {
            for child in self.list_directory(id)? {
                if child.parent != *id || matches!(child.id, DriveNodeId::Root) {
                    return Err(DriveFsError::InvalidInput(
                        "Drive recursive removal child has an invalid parent or identity"
                            .to_string(),
                    ));
                }
                self.plan_recursive_remove(&child.id, depth + 1, seen, postorder)?;
            }
        }
        postorder.push((id.clone(), metadata.kind));
        Ok(())
    }

    pub fn recover_pending(&self) -> Result<DriveRecoverySummary, DriveFsError> {
        self.backend.recover_pending()
    }

    pub fn capacity(&self) -> DriveVirtualCapacity {
        self.backend.capacity()
    }
}

pub fn provider_range_covers(
    cover_offset: u64,
    cover_length: u64,
    target_offset: u64,
    target_length: u64,
) -> Result<bool, DriveFsError> {
    let cover_end = cover_offset.checked_add(cover_length).ok_or_else(|| {
        DriveFsError::InvalidInput("Drive provider cover range overflow".to_string())
    })?;
    let target_end = target_offset.checked_add(target_length).ok_or_else(|| {
        DriveFsError::InvalidInput("Drive provider target range overflow".to_string())
    })?;
    Ok(cover_offset <= target_offset && cover_end >= target_end)
}

pub fn windows_volume_target_path(
    volume_dos_name: &str,
    target_path: &str,
) -> Result<String, DriveFsError> {
    let volume = volume_dos_name.trim_end_matches(['\\', '/']);
    let volume_bytes = volume.as_bytes();
    if volume_bytes.len() != 2 || !volume_bytes[0].is_ascii_alphabetic() || volume_bytes[1] != b':'
    {
        return Err(DriveFsError::InvalidInput(
            "Windows callback volume name is invalid".to_string(),
        ));
    }
    if target_path.is_empty() || target_path.contains('\0') {
        return Err(DriveFsError::InvalidInput(
            "Windows callback target path is invalid".to_string(),
        ));
    }

    let normalized = target_path.replace('/', "\\");
    // UNC paths and Win32 device namespaces are not relative to this Drive
    // volume. Prefixing "C:" to them must never turn them into rename targets.
    if normalized.starts_with("\\\\") {
        return Err(DriveFsError::InvalidInput(
            "Windows callback target cannot use a UNC or device path".to_string(),
        ));
    }
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        if !bytes[0].eq_ignore_ascii_case(&volume_bytes[0]) {
            return Err(DriveFsError::AccessDenied(
                "Windows callback target is on another volume".to_string(),
            ));
        }
        // "C:relative.txt" is drive-current-directory relative, not C:\\rooted.
        if bytes.get(2) != Some(&b'\\') {
            return Err(DriveFsError::InvalidInput(
                "Windows callback target is drive-relative, not absolute".to_string(),
            ));
        }
        return Ok(normalized);
    }
    if !normalized.starts_with('\\') {
        return Err(DriveFsError::InvalidInput(
            "Windows callback target is not rooted on its volume".to_string(),
        ));
    }
    Ok(format!("{volume}{normalized}"))
}

pub fn align_provider_read_range(
    offset: u64,
    length: u64,
    file_size: u64,
    alignment: u64,
) -> Result<(u64, u64), DriveFsError> {
    if alignment == 0 {
        return Err(DriveFsError::InvalidInput(
            "Drive provider read alignment cannot be zero".to_string(),
        ));
    }
    if offset > file_size {
        return Err(DriveFsError::InvalidInput(
            "Drive provider read starts beyond end of file".to_string(),
        ));
    }
    if length == 0 {
        return Ok((offset, 0));
    }
    if offset == file_size {
        return Err(DriveFsError::InvalidInput(
            "Drive provider read starts at end of file".to_string(),
        ));
    }
    let requested_end = offset.checked_add(length).ok_or_else(|| {
        DriveFsError::InvalidInput("Drive provider read range overflow".to_string())
    })?;
    let required_end = requested_end.min(file_size);
    let aligned_start = offset - (offset % alignment);
    let aligned_end = if required_end == file_size {
        file_size
    } else {
        let remainder = required_end % alignment;
        if remainder == 0 {
            required_end
        } else {
            required_end
                .checked_add(alignment - remainder)
                .ok_or_else(|| {
                    DriveFsError::InvalidInput("Drive provider aligned range overflow".to_string())
                })?
                .min(file_size)
        }
    };
    let aligned_length = aligned_end.checked_sub(aligned_start).ok_or_else(|| {
        DriveFsError::InvalidInput("Drive provider aligned range is invalid".to_string())
    })?;
    Ok((aligned_start, aligned_length))
}

fn valid_provider_identity_value(value: &[u8]) -> bool {
    std::str::from_utf8(value).is_ok_and(|id| !id.trim().is_empty() && !id.contains('\0'))
}

pub fn encode_provider_identity(id: &DriveNodeId) -> Result<Vec<u8>, DriveFsError> {
    let (kind, value): (u8, &[u8]) = match id {
        DriveNodeId::Root => (0, &[]),
        DriveNodeId::Directory(value) => (1, value.as_bytes()),
        DriveNodeId::File(value) => (2, value.as_bytes()),
        DriveNodeId::Pending(value) => (3, value.as_bytes()),
    };
    if kind != 0 && !valid_provider_identity_value(value) {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity contains an invalid node id".to_string(),
        ));
    }
    let len = u16::try_from(value.len()).map_err(|_| {
        DriveFsError::InvalidInput("Drive provider identity is too large".to_string())
    })?;
    let total = 4usize
        .checked_add(1)
        .and_then(|size| size.checked_add(2))
        .and_then(|size| size.checked_add(value.len()))
        .ok_or_else(|| {
            DriveFsError::InvalidInput("Drive provider identity overflow".to_string())
        })?;
    if total > MAX_PROVIDER_IDENTITY_BYTES {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity exceeds the maximum size".to_string(),
        ));
    }
    let mut encoded = Vec::with_capacity(total);
    encoded.extend_from_slice(PROVIDER_IDENTITY_MAGIC);
    encoded.push(kind);
    encoded.extend_from_slice(&len.to_le_bytes());
    encoded.extend_from_slice(value);
    Ok(encoded)
}

pub fn decode_provider_identity(bytes: &[u8]) -> Result<DriveNodeId, DriveFsError> {
    if bytes.len() > MAX_PROVIDER_IDENTITY_BYTES {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity exceeds the maximum size".to_string(),
        ));
    }
    if bytes.len() < 7 || &bytes[..4] != PROVIDER_IDENTITY_MAGIC {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity has an invalid header".to_string(),
        ));
    }
    let kind = bytes[4];
    let len = u16::from_le_bytes([bytes[5], bytes[6]]) as usize;
    if bytes.len() != 7usize.saturating_add(len) {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity length is invalid".to_string(),
        ));
    }
    let value = std::str::from_utf8(&bytes[7..]).map_err(|_| {
        DriveFsError::InvalidInput("Drive provider identity is not UTF-8".to_string())
    })?;
    match kind {
        0 if value.is_empty() => Ok(DriveNodeId::Root),
        1 if valid_provider_identity_value(value.as_bytes()) => {
            Ok(DriveNodeId::Directory(value.to_string()))
        }
        2 if valid_provider_identity_value(value.as_bytes()) => {
            Ok(DriveNodeId::File(value.to_string()))
        }
        3 if valid_provider_identity_value(value.as_bytes()) => {
            Ok(DriveNodeId::Pending(value.to_string()))
        }
        _ => Err(DriveFsError::InvalidInput(
            "Drive provider identity kind is invalid".to_string(),
        )),
    }
}

fn require_directory_parent(parent: &DriveNodeId) -> Result<(), DriveFsError> {
    match parent {
        DriveNodeId::Root => Ok(()),
        DriveNodeId::Directory(value) if !value.trim().is_empty() => Ok(()),
        DriveNodeId::Directory(_) => Err(DriveFsError::InvalidInput(
            "Drive parent directory ID is invalid".into(),
        )),
        DriveNodeId::File(_) | DriveNodeId::Pending(_) => Err(DriveFsError::NotDirectory),
    }
}

fn require_file_node(id: &DriveNodeId) -> Result<(), DriveFsError> {
    match id {
        DriveNodeId::Root | DriveNodeId::Directory(_) => Err(DriveFsError::IsDirectory),
        DriveNodeId::File(_) | DriveNodeId::Pending(_) => Ok(()),
    }
}

fn validate_renamed_metadata(
    source: &DriveNodeId,
    parent: &DriveNodeId,
    requested_name: &str,
    item: &DriveNodeMetadata,
) -> Result<(), DriveFsError> {
    validate_child_metadata(parent, item)?;
    if crate::commands::drive_metadata::portable_name_key(requested_name)
        != crate::commands::drive_metadata::portable_name_key(&item.name)
    {
        return Err(DriveFsError::InvalidInput(
            "Drive rename returned a different child name".to_string(),
        ));
    }
    let identity_ok = match source {
        DriveNodeId::Root => false,
        DriveNodeId::Directory(_) => {
            item.id == *source && item.kind == DriveNodeKind::Directory && !item.pending
        }
        DriveNodeId::File(_) => item.id == *source && item.kind == DriveNodeKind::File,
        DriveNodeId::Pending(staging_id) => {
            item.pending
                && item.kind == DriveNodeKind::File
                && matches!(&item.id, DriveNodeId::File(_))
                && item.content_version == *staging_id
        }
    };
    if !identity_ok {
        return Err(DriveFsError::InvalidInput(
            "Drive rename returned an unrelated node identity".to_string(),
        ));
    }
    Ok(())
}

fn validate_created_metadata(
    parent: &DriveNodeId,
    requested_name: &str,
    expected_kind: DriveNodeKind,
    item: &DriveNodeMetadata,
) -> Result<(), DriveFsError> {
    validate_child_metadata(parent, item)?;
    if item.kind != expected_kind || (expected_kind == DriveNodeKind::Directory && item.pending) {
        return Err(DriveFsError::InvalidInput(
            "Drive creation returned the wrong node kind".to_string(),
        ));
    }
    if crate::commands::drive_metadata::portable_name_key(requested_name)
        != crate::commands::drive_metadata::portable_name_key(&item.name)
    {
        return Err(DriveFsError::InvalidInput(
            "Drive creation returned a different child name".to_string(),
        ));
    }
    Ok(())
}

fn validate_nonroot_metadata(item: &DriveNodeMetadata) -> Result<(), DriveFsError> {
    match &item.parent {
        DriveNodeId::Root => {}
        DriveNodeId::Directory(value) if !value.is_empty() && item.parent != item.id => {}
        _ => {
            return Err(DriveFsError::InvalidInput(
                "Drive metadata has an invalid parent identity".to_string(),
            ));
        }
    }
    if item.pending && item.kind != DriveNodeKind::File {
        return Err(DriveFsError::InvalidInput(
            "Drive directory metadata cannot be a pending file".to_string(),
        ));
    }
    validate_child_metadata(&item.parent, item)
}

fn validate_child_metadata(
    expected_parent: &DriveNodeId,
    child: &DriveNodeMetadata,
) -> Result<(), DriveFsError> {
    if child.parent != *expected_parent {
        return Err(DriveFsError::InvalidInput(
            "Drive child metadata has an invalid parent (belongs to another parent)".to_string(),
        ));
    }
    if !matches!(
        (&child.id, child.kind),
        (DriveNodeId::Directory(_), DriveNodeKind::Directory)
            | (
                DriveNodeId::File(_) | DriveNodeId::Pending(_),
                DriveNodeKind::File
            )
    ) {
        return Err(DriveFsError::InvalidInput(
            "Drive child metadata has a mismatched node identity or kind".to_string(),
        ));
    }
    // A typed File/Directory/Pending variant still needs a real opaque ID.
    // Empty IDs are not encodable as provider identities and must never be
    // projected as filesystem children.
    if matches!(
        &child.id,
        DriveNodeId::Directory(value) | DriveNodeId::File(value) | DriveNodeId::Pending(value)
            if value.is_empty()
    ) {
        return Err(DriveFsError::InvalidInput(
            "Drive child metadata has an empty node identity".to_string(),
        ));
    }
    // A backend row may be corrupt even if the local creation API normally
    // enforces portable names. Never hand Windows Explorer/FUSE an entry
    // rejected by the shared Drive namespace rules.
    crate::commands::drive_metadata::validate_drive_name(&child.name)
        .map(|_| ())
        .map_err(|_| DriveFsError::InvalidInput("Drive item name is invalid".to_string()))
}

fn validate_component(name: &str) -> Result<(), DriveFsError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(DriveFsError::InvalidInput(
            "Drive item name is invalid".to_string(),
        ));
    }
    // Validate at the native provider boundary before create or rename can
    // touch SQLite, staging files, or the remote namespace. Shared rules keep
    // Linux and Windows filenames compatible.
    crate::commands::drive_metadata::validate_drive_name(name)
        .map(|_| ())
        .map_err(|_| DriveFsError::InvalidInput("Drive item name is invalid".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct VersionFlipBackend {
        version: AtomicUsize,
        flip_on_read: bool,
        stable_content_version: bool,
        pending: bool,
        change_size: bool,
        change_modified_time: bool,
    }

    impl VersionFlipBackend {
        fn metadata_value(&self) -> DriveNodeMetadata {
            let version = self.version.load(Ordering::SeqCst);
            DriveNodeMetadata {
                id: DriveNodeId::File("entry".to_string()),
                parent: DriveNodeId::Root,
                name: "file.bin".to_string(),
                kind: DriveNodeKind::File,
                size: if self.change_size && version > 1 {
                    5
                } else {
                    4
                },
                created_at_ms: 0,
                updated_at_ms: if self.change_modified_time && version > 1 {
                    10
                } else {
                    0
                },
                content_version: if self.stable_content_version {
                    "v1".into()
                } else {
                    format!("v{version}")
                },
                pending: self.pending,
            }
        }

        fn unsupported<T>() -> Result<T, DriveFsError> {
            Err(DriveFsError::Internal(
                "unused test backend operation".to_string(),
            ))
        }
    }

    impl DriveFsBackend for VersionFlipBackend {
        fn revision(&self) -> Result<u64, DriveFsError> {
            Ok(0)
        }
        fn metadata(&self, _id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            Ok(self.metadata_value())
        }
        fn lookup(
            &self,
            _parent: &DriveNodeId,
            _name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::unsupported()
        }
        fn list_directory(
            &self,
            _parent: &DriveNodeId,
        ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
            Self::unsupported()
        }
        fn read(
            &self,
            _id: &DriveNodeId,
            _offset: u64,
            _length: u64,
        ) -> Result<Vec<u8>, DriveFsError> {
            if self.flip_on_read {
                self.version.store(2, Ordering::SeqCst);
            }
            Ok(vec![1, 2, 3, 4])
        }
        fn create_file(
            &self,
            _parent: &DriveNodeId,
            _name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::unsupported()
        }
        fn create_directory(
            &self,
            _parent: &DriveNodeId,
            _name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::unsupported()
        }
        fn begin_write(&self, _id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::unsupported()
        }
        fn write(
            &self,
            _id: &DriveNodeId,
            _offset: u64,
            _data: &[u8],
        ) -> Result<u64, DriveFsError> {
            Self::unsupported()
        }
        fn resize(&self, _id: &DriveNodeId, _size: u64) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn flush(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn close_write(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn abort_write(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn rename(
            &self,
            _id: &DriveNodeId,
            _new_parent: &DriveNodeId,
            _new_name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::unsupported()
        }
        fn remove_file(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn remove_directory(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn recover_pending(&self) -> Result<DriveRecoverySummary, DriveFsError> {
            Self::unsupported()
        }
        fn capacity(&self) -> DriveVirtualCapacity {
            DriveVirtualCapacity {
                total_bytes: 0,
                available_bytes: 0,
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RecursiveRemoveFixture {
        Normal,
        ListingError,
        Cycle,
        WrongParent,
        InvalidChildName,
        ReservedChildName,
        TrailingDotChildName,
        OverlongChildName,
        RootChildIdentity,
        WrongChildKind,
        EmptyFileIdentity,
        EmptyDirectoryIdentity,
        EmptyPendingIdentity,
        DuplicateChildIdentity,
        CaseFoldedChildNameCollision,
        UnicodeChildNameCollision,
        WrongLookupName,
        CaseVariantLookupName,
        ShortWrite,
        ExactWrite,
        OverlongWrite,
        MetadataWrongId,
        MetadataWrongKind,
        MetadataInvalidParent,
        MetadataInvalidName,
        MetadataSelfParent,
        MetadataInvalidRoot,
        CreatedValid,
        CreatedWrongParent,
        CreatedWrongName,
        CreatedWrongKind,
        CreatedEmptyId,
        CreatedPendingDirectory,
        RenamedValid,
        RenamedWrongParent,
        RenamedWrongName,
        RenamedWrongId,
        RenamedWrongKind,
        RenamedWrongStage,
        RenamedPendingNotPending,
        RenamedDirectoryPending,
        BeginWriteValid,
        BeginWriteWrongId,
        BeginWriteWrongStage,
        BeginWriteNotPending,
        BeginWriteInvalidName,
        BeginWriteInvalidParent,
        BeginWriteWrongKind,
        TooDeep,
    }

    struct RecursiveRemoveBackend {
        removed: Mutex<Vec<String>>,
        fixture: RecursiveRemoveFixture,
    }

    impl RecursiveRemoveBackend {
        fn item(id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            let (parent, name, kind) = match id {
                DriveNodeId::Directory(value) if value == "dir-a" => {
                    (DriveNodeId::Root, "A", DriveNodeKind::Directory)
                }
                DriveNodeId::Directory(value) if value == "dir-b" => (
                    DriveNodeId::Directory("dir-a".to_string()),
                    "B",
                    DriveNodeKind::Directory,
                ),
                DriveNodeId::File(value) if value == "file-a" => (
                    DriveNodeId::Directory("dir-a".to_string()),
                    "a.txt",
                    DriveNodeKind::File,
                ),
                DriveNodeId::File(value) if value == "file-b" => (
                    DriveNodeId::Directory("dir-b".to_string()),
                    "b.txt",
                    DriveNodeKind::File,
                ),
                DriveNodeId::Directory(value) if value.starts_with("deep-") => {
                    let depth: usize = value["deep-".len()..]
                        .parse()
                        .map_err(|_| DriveFsError::NotFound)?;
                    let parent = if depth == 0 {
                        DriveNodeId::Directory("dir-a".to_string())
                    } else {
                        DriveNodeId::Directory(format!("deep-{}", depth - 1))
                    };
                    (parent, "deep", DriveNodeKind::Directory)
                }
                _ => return Err(DriveFsError::NotFound),
            };
            Ok(DriveNodeMetadata {
                id: id.clone(),
                parent,
                name: name.to_string(),
                kind,
                size: if kind == DriveNodeKind::File { 1 } else { 0 },
                created_at_ms: 0,
                updated_at_ms: 0,
                content_version: format!("{id:?}"),
                pending: false,
            })
        }

        fn unsupported<T>() -> Result<T, DriveFsError> {
            Err(DriveFsError::Internal(
                "unused recursive-remove test operation".to_string(),
            ))
        }

        fn created(
            &self,
            parent: &DriveNodeId,
            name: &str,
            kind: DriveNodeKind,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            if !matches!(
                self.fixture,
                RecursiveRemoveFixture::CreatedValid
                    | RecursiveRemoveFixture::CreatedWrongParent
                    | RecursiveRemoveFixture::CreatedWrongName
                    | RecursiveRemoveFixture::CreatedWrongKind
                    | RecursiveRemoveFixture::CreatedEmptyId
                    | RecursiveRemoveFixture::CreatedPendingDirectory
            ) {
                return Self::unsupported();
            }
            let mut result = DriveNodeMetadata {
                id: if kind == DriveNodeKind::File {
                    DriveNodeId::File("created-file".to_string())
                } else {
                    DriveNodeId::Directory("created-directory".to_string())
                },
                parent: parent.clone(),
                name: name.to_string(),
                kind,
                size: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
                content_version: "created-staging".to_string(),
                pending: kind == DriveNodeKind::File,
            };
            match self.fixture {
                RecursiveRemoveFixture::CreatedWrongParent => {
                    result.parent = DriveNodeId::File("not-a-parent".to_string());
                }
                RecursiveRemoveFixture::CreatedWrongName => {
                    result.name = "unrelated.txt".to_string();
                }
                RecursiveRemoveFixture::CreatedWrongKind => {
                    result.kind = if kind == DriveNodeKind::File {
                        DriveNodeKind::Directory
                    } else {
                        DriveNodeKind::File
                    };
                }
                RecursiveRemoveFixture::CreatedEmptyId => {
                    result.id = if kind == DriveNodeKind::File {
                        DriveNodeId::File(String::new())
                    } else {
                        DriveNodeId::Directory(String::new())
                    };
                }
                RecursiveRemoveFixture::CreatedPendingDirectory => {
                    result.pending = true;
                }
                _ => {}
            }
            Ok(result)
        }

        fn opened_for_write(
            &self,
            source: &DriveNodeId,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            if !matches!(
                self.fixture,
                RecursiveRemoveFixture::BeginWriteValid
                    | RecursiveRemoveFixture::BeginWriteWrongId
                    | RecursiveRemoveFixture::BeginWriteWrongStage
                    | RecursiveRemoveFixture::BeginWriteNotPending
                    | RecursiveRemoveFixture::BeginWriteInvalidName
                    | RecursiveRemoveFixture::BeginWriteInvalidParent
                    | RecursiveRemoveFixture::BeginWriteWrongKind
            ) {
                return Self::unsupported();
            }
            let mut entry = DriveNodeMetadata {
                id: match source {
                    DriveNodeId::File(_) => source.clone(),
                    DriveNodeId::Pending(_) => DriveNodeId::File("visible-entry".into()),
                    DriveNodeId::Root | DriveNodeId::Directory(_) => {
                        return Err(DriveFsError::IsDirectory)
                    }
                },
                parent: DriveNodeId::Root,
                name: "writable.txt".to_string(),
                kind: DriveNodeKind::File,
                size: 3,
                created_at_ms: 1,
                updated_at_ms: 2,
                content_version: match source {
                    DriveNodeId::Pending(stage) => stage.clone(),
                    _ => "new-staging-id".to_string(),
                },
                pending: true,
            };
            match self.fixture {
                RecursiveRemoveFixture::BeginWriteWrongId => {
                    entry.id = DriveNodeId::File("other-visible-file".into());
                }
                RecursiveRemoveFixture::BeginWriteWrongStage => {
                    entry.content_version = "other-staging-file".to_string();
                }
                RecursiveRemoveFixture::BeginWriteNotPending => {
                    entry.pending = false;
                }
                RecursiveRemoveFixture::BeginWriteInvalidName => {
                    entry.name = "CON.txt".to_string();
                }
                RecursiveRemoveFixture::BeginWriteInvalidParent => {
                    entry.parent = DriveNodeId::File("not-a-parent".into());
                }
                RecursiveRemoveFixture::BeginWriteWrongKind => {
                    entry.kind = DriveNodeKind::Directory;
                }
                _ => {}
            }
            Ok(entry)
        }

        fn renamed(
            &self,
            source: &DriveNodeId,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            if !matches!(
                self.fixture,
                RecursiveRemoveFixture::RenamedValid
                    | RecursiveRemoveFixture::RenamedWrongParent
                    | RecursiveRemoveFixture::RenamedWrongName
                    | RecursiveRemoveFixture::RenamedWrongId
                    | RecursiveRemoveFixture::RenamedWrongKind
                    | RecursiveRemoveFixture::RenamedWrongStage
                    | RecursiveRemoveFixture::RenamedPendingNotPending
                    | RecursiveRemoveFixture::RenamedDirectoryPending
            ) {
                return Self::unsupported();
            }
            let (id, kind, pending, content_version) = match source {
                DriveNodeId::Root => {
                    return Err(DriveFsError::AccessDenied("Cannot rename root".into()))
                }
                DriveNodeId::Directory(_) => (
                    source.clone(),
                    DriveNodeKind::Directory,
                    false,
                    "directory-version".to_string(),
                ),
                DriveNodeId::File(_) => (
                    source.clone(),
                    DriveNodeKind::File,
                    false,
                    "remote-version".to_string(),
                ),
                DriveNodeId::Pending(staging_id) => (
                    DriveNodeId::File("visible-entry".into()),
                    DriveNodeKind::File,
                    true,
                    staging_id.clone(),
                ),
            };
            let mut result = DriveNodeMetadata {
                id,
                parent: parent.clone(),
                name: name.to_string(),
                kind,
                size: 0,
                created_at_ms: 1,
                updated_at_ms: 2,
                content_version,
                pending,
            };
            match self.fixture {
                RecursiveRemoveFixture::RenamedWrongParent => {
                    result.parent = DriveNodeId::Root;
                }
                RecursiveRemoveFixture::RenamedWrongName => {
                    result.name = "another-valid.txt".to_string();
                }
                RecursiveRemoveFixture::RenamedWrongId => {
                    result.id = if kind == DriveNodeKind::Directory {
                        DriveNodeId::Directory("another-directory".into())
                    } else {
                        DriveNodeId::File("another-file".into())
                    };
                }
                RecursiveRemoveFixture::RenamedWrongKind => {
                    result.kind = if kind == DriveNodeKind::File {
                        DriveNodeKind::Directory
                    } else {
                        DriveNodeKind::File
                    };
                }
                RecursiveRemoveFixture::RenamedWrongStage => {
                    result.content_version = "unrelated-staging-id".to_string();
                }
                RecursiveRemoveFixture::RenamedPendingNotPending => {
                    result.pending = false;
                }
                RecursiveRemoveFixture::RenamedDirectoryPending => {
                    result.pending = true;
                }
                _ => {}
            }
            Ok(result)
        }
    }

    impl DriveFsBackend for RecursiveRemoveBackend {
        fn revision(&self) -> Result<u64, DriveFsError> {
            Ok(0)
        }
        fn metadata(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            let mut item = if *id == DriveNodeId::Root {
                DriveNodeMetadata {
                    id: DriveNodeId::Root,
                    parent: DriveNodeId::Root,
                    name: "TeraRelay Drive".to_string(),
                    kind: DriveNodeKind::Directory,
                    size: 0,
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    content_version: "root".to_string(),
                    pending: false,
                }
            } else {
                Self::item(id)?
            };
            match self.fixture {
                RecursiveRemoveFixture::MetadataWrongId => {
                    item.id = DriveNodeId::File("other-file".to_string());
                }
                RecursiveRemoveFixture::MetadataWrongKind => {
                    item.kind = DriveNodeKind::Directory;
                }
                RecursiveRemoveFixture::MetadataInvalidParent => {
                    item.parent = DriveNodeId::File("not-a-directory".to_string());
                }
                RecursiveRemoveFixture::MetadataInvalidName => {
                    item.name = "CON.txt".to_string();
                }
                RecursiveRemoveFixture::MetadataSelfParent => {
                    item.parent = item.id.clone();
                }
                RecursiveRemoveFixture::MetadataInvalidRoot => {
                    item.pending = true;
                }
                _ => {}
            }
            Ok(item)
        }
        fn lookup(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            if *parent != DriveNodeId::Directory("dir-a".to_string()) || name != "a.txt" {
                return Err(DriveFsError::NotFound);
            }
            let mut item = Self::item(&DriveNodeId::File("file-a".to_string()))?;
            if self.fixture == RecursiveRemoveFixture::WrongParent {
                item.parent = DriveNodeId::Root;
            }
            match self.fixture {
                RecursiveRemoveFixture::InvalidChildName => {
                    item.name = "bad/name".to_string();
                }
                RecursiveRemoveFixture::ReservedChildName => {
                    item.name = "CON.txt".to_string();
                }
                RecursiveRemoveFixture::TrailingDotChildName => {
                    item.name = "bad.".to_string();
                }
                RecursiveRemoveFixture::OverlongChildName => {
                    item.name = "a".repeat(256);
                }
                RecursiveRemoveFixture::WrongLookupName => {
                    item.name = "different.txt".to_string();
                }
                RecursiveRemoveFixture::CaseVariantLookupName => {
                    item.name = "A.TXT".to_string();
                }
                _ => {}
            }
            if self.fixture == RecursiveRemoveFixture::RootChildIdentity {
                item.id = DriveNodeId::Root;
            }
            if self.fixture == RecursiveRemoveFixture::WrongChildKind {
                item.kind = DriveNodeKind::Directory;
            }
            match self.fixture {
                RecursiveRemoveFixture::EmptyFileIdentity => {
                    item.id = DriveNodeId::File(String::new());
                }
                RecursiveRemoveFixture::EmptyDirectoryIdentity => {
                    item.id = DriveNodeId::Directory(String::new());
                    item.kind = DriveNodeKind::Directory;
                }
                RecursiveRemoveFixture::EmptyPendingIdentity => {
                    item.id = DriveNodeId::Pending(String::new());
                }
                _ => {}
            }
            Ok(item)
        }
        fn list_directory(
            &self,
            parent: &DriveNodeId,
        ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
            if self.fixture == RecursiveRemoveFixture::ListingError
                && *parent == DriveNodeId::Directory("dir-b".to_string())
            {
                return Err(DriveFsError::Unavailable(
                    "synthetic nested directory listing failure".to_string(),
                ));
            }
            let ids = match parent {
                DriveNodeId::Directory(value) if value == "dir-a" => vec![
                    DriveNodeId::File("file-a".to_string()),
                    if self.fixture == RecursiveRemoveFixture::TooDeep {
                        DriveNodeId::Directory("deep-0".to_string())
                    } else {
                        DriveNodeId::Directory("dir-b".to_string())
                    },
                ],
                DriveNodeId::Directory(value) if value == "dir-b" => {
                    vec![if self.fixture == RecursiveRemoveFixture::Cycle {
                        DriveNodeId::Directory("dir-a".to_string())
                    } else {
                        DriveNodeId::File("file-b".to_string())
                    }]
                }
                DriveNodeId::Directory(value) if value.starts_with("deep-") => {
                    let depth: usize = value["deep-".len()..]
                        .parse()
                        .map_err(|_| DriveFsError::NotFound)?;
                    if depth <= MAX_RECURSIVE_REMOVE_DEPTH {
                        vec![DriveNodeId::Directory(format!("deep-{}", depth + 1))]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            };
            let mut items = ids.iter().map(Self::item).collect::<Result<Vec<_>, _>>()?;
            if self.fixture == RecursiveRemoveFixture::DuplicateChildIdentity
                && *parent == DriveNodeId::Directory("dir-a".to_string())
                && items.len() > 1
            {
                items[1].id = items[0].id.clone();
                items[1].kind = items[0].kind;
            }
            if *parent == DriveNodeId::Directory("dir-a".to_string()) && items.len() > 1 {
                match self.fixture {
                    RecursiveRemoveFixture::CaseFoldedChildNameCollision => {
                        items[1].name = "A.TXT".to_string();
                    }
                    RecursiveRemoveFixture::UnicodeChildNameCollision => {
                        items[0].name = "Café".to_string();
                        items[1].name = "Cafe\u{301}".to_string();
                    }
                    _ => {}
                }
            }
            if let Some(first) = items.first_mut() {
                if self.fixture == RecursiveRemoveFixture::Cycle
                    && *parent == DriveNodeId::Directory("dir-b".to_string())
                {
                    first.parent = parent.clone();
                }
                if self.fixture == RecursiveRemoveFixture::WrongParent
                    && *parent == DriveNodeId::Directory("dir-b".to_string())
                {
                    first.parent = DriveNodeId::Root;
                }
                if *parent == DriveNodeId::Directory("dir-a".to_string()) {
                    match self.fixture {
                        RecursiveRemoveFixture::InvalidChildName => {
                            first.name = "bad/name".to_string();
                        }
                        RecursiveRemoveFixture::ReservedChildName => {
                            first.name = "CON.txt".to_string();
                        }
                        RecursiveRemoveFixture::TrailingDotChildName => {
                            first.name = "bad.".to_string();
                        }
                        RecursiveRemoveFixture::OverlongChildName => {
                            first.name = "a".repeat(256);
                        }
                        RecursiveRemoveFixture::RootChildIdentity => {
                            first.id = DriveNodeId::Root;
                        }
                        RecursiveRemoveFixture::WrongChildKind => {
                            first.kind = DriveNodeKind::Directory;
                        }
                        RecursiveRemoveFixture::EmptyFileIdentity => {
                            first.id = DriveNodeId::File(String::new());
                        }
                        RecursiveRemoveFixture::EmptyDirectoryIdentity => {
                            first.id = DriveNodeId::Directory(String::new());
                            first.kind = DriveNodeKind::Directory;
                        }
                        RecursiveRemoveFixture::EmptyPendingIdentity => {
                            first.id = DriveNodeId::Pending(String::new());
                        }
                        _ => {}
                    }
                }
            }
            Ok(items)
        }
        fn read(
            &self,
            _id: &DriveNodeId,
            _offset: u64,
            _length: u64,
        ) -> Result<Vec<u8>, DriveFsError> {
            Self::unsupported()
        }
        fn create_file(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            self.created(parent, name, DriveNodeKind::File)
        }
        fn create_directory(
            &self,
            parent: &DriveNodeId,
            name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            self.created(parent, name, DriveNodeKind::Directory)
        }
        fn begin_write(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            self.opened_for_write(id)
        }
        fn write(&self, _id: &DriveNodeId, _offset: u64, data: &[u8]) -> Result<u64, DriveFsError> {
            let supplied = u64::try_from(data.len()).unwrap();
            match self.fixture {
                RecursiveRemoveFixture::ShortWrite => Ok(supplied.saturating_sub(1)),
                RecursiveRemoveFixture::ExactWrite => Ok(supplied),
                RecursiveRemoveFixture::OverlongWrite => Ok(supplied.saturating_add(1)),
                _ => Self::unsupported(),
            }
        }
        fn resize(&self, _id: &DriveNodeId, _size: u64) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn flush(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn close_write(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn abort_write(&self, _id: &DriveNodeId) -> Result<(), DriveFsError> {
            Self::unsupported()
        }
        fn rename(
            &self,
            id: &DriveNodeId,
            new_parent: &DriveNodeId,
            new_name: &str,
        ) -> Result<DriveNodeMetadata, DriveFsError> {
            self.renamed(id, new_parent, new_name)
        }
        fn remove_file(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            self.removed.lock().unwrap().push(format!("file:{id:?}"));
            Ok(())
        }
        fn remove_directory(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
            self.removed.lock().unwrap().push(format!("dir:{id:?}"));
            Ok(())
        }
        fn recover_pending(&self) -> Result<DriveRecoverySummary, DriveFsError> {
            Self::unsupported()
        }
        fn capacity(&self) -> DriveVirtualCapacity {
            DriveVirtualCapacity {
                total_bytes: 0,
                available_bytes: 0,
            }
        }
    }

    #[test]
    fn offline_projection_recovery_applies_only_provable_local_rename_or_delete() {
        let file = DriveNodeId::File("file-a".to_string());
        let folder = DriveNodeId::Directory("dir-a".to_string());
        let baseline = vec![
            DriveProjectionRecord {
                id: file.clone(),
                path: vec!["Old.txt".to_string()],
                kind: DriveNodeKind::File,
                content_version: "v1".to_string(),
                size: 10,
                updated_at_ms: 1,
            },
            DriveProjectionRecord {
                id: folder.clone(),
                path: vec!["Folder".to_string()],
                kind: DriveNodeKind::Directory,
                content_version: String::new(),
                size: 0,
                updated_at_ms: 1,
            },
        ];
        let remote = baseline.clone();
        let local = vec![DriveLocalProjectionRecord {
            id: file.clone(),
            path: vec!["Renamed.txt".to_string()],
            locally_dirty: false,
        }];

        assert_eq!(
            plan_offline_projection_recovery(Some(&baseline), &remote, &local),
            vec![
                DriveOfflineProjectionAction::Rename {
                    id: file,
                    path: vec!["Renamed.txt".to_string()],
                },
                DriveOfflineProjectionAction::Delete { id: folder },
            ]
        );
    }

    #[test]
    fn offline_projection_recovery_keeps_remote_only_changes_and_detects_divergence() {
        let id = DriveNodeId::File("file-a".to_string());
        let baseline = vec![DriveProjectionRecord {
            id: id.clone(),
            path: vec!["Old.txt".to_string()],
            kind: DriveNodeKind::File,
            content_version: "v1".to_string(),
            size: 10,
            updated_at_ms: 1,
        }];
        let remote = vec![DriveProjectionRecord {
            id: id.clone(),
            path: vec!["Remote.txt".to_string()],
            content_version: "v2".to_string(),
            updated_at_ms: 2,
            ..baseline[0].clone()
        }];

        assert!(plan_offline_projection_recovery(
            Some(&baseline),
            &remote,
            &[DriveLocalProjectionRecord {
                id: id.clone(),
                path: vec!["Old.txt".to_string()],
                locally_dirty: false,
            }],
        )
        .is_empty());

        assert_eq!(
            plan_offline_projection_recovery(
                Some(&baseline),
                &remote,
                &[DriveLocalProjectionRecord {
                    id: id.clone(),
                    path: vec!["Local.txt".to_string()],
                    locally_dirty: false,
                }],
            ),
            vec![DriveOfflineProjectionAction::Conflict { id: id.clone() }]
        );

        assert!(plan_offline_projection_recovery(
            Some(&baseline),
            &remote,
            &[DriveLocalProjectionRecord {
                id,
                path: vec!["Remote.txt".to_string()],
                locally_dirty: false,
            }],
        )
        .is_empty());
    }

    #[test]
    fn offline_projection_recovery_rejects_concurrent_remote_content_and_local_dirty_data() {
        let id = DriveNodeId::File("file-a".to_string());
        let baseline = vec![DriveProjectionRecord {
            id: id.clone(),
            path: vec!["File.txt".to_string()],
            kind: DriveNodeKind::File,
            content_version: "v1".to_string(),
            size: 10,
            updated_at_ms: 1,
        }];
        let remote = vec![DriveProjectionRecord {
            content_version: "v2".to_string(),
            size: 11,
            updated_at_ms: 2,
            ..baseline[0].clone()
        }];
        let local = vec![DriveLocalProjectionRecord {
            id: id.clone(),
            path: baseline[0].path.clone(),
            locally_dirty: true,
        }];
        assert_eq!(
            plan_offline_projection_recovery(Some(&baseline), &remote, &local),
            vec![DriveOfflineProjectionAction::Conflict { id }]
        );
    }

    #[test]
    fn offline_projection_recovery_rejects_uncheckpointed_placeholder_move_after_crash_window() {
        let old = DriveNodeId::File("old".to_string());
        let unknown = DriveNodeId::File("new".to_string());
        let baseline = vec![DriveProjectionRecord {
            id: old,
            path: vec!["Old.txt".to_string()],
            kind: DriveNodeKind::File,
            content_version: "old-v1".to_string(),
            size: 1,
            updated_at_ms: 1,
        }];
        let remote = vec![
            baseline[0].clone(),
            DriveProjectionRecord {
                id: unknown.clone(),
                path: vec!["New.txt".to_string()],
                kind: DriveNodeKind::File,
                content_version: "new-v1".to_string(),
                size: 1,
                updated_at_ms: 1,
            },
        ];
        let local = vec![
            DriveLocalProjectionRecord {
                id: baseline[0].id.clone(),
                path: baseline[0].path.clone(),
                locally_dirty: false,
            },
            DriveLocalProjectionRecord {
                id: unknown.clone(),
                path: vec!["Moved-New.txt".to_string()],
                locally_dirty: false,
            },
        ];
        assert_eq!(
            plan_offline_projection_recovery(Some(&baseline), &remote, &local),
            vec![DriveOfflineProjectionAction::Conflict { id: unknown }]
        );
    }

    #[test]
    fn offline_projection_recovery_fails_closed_for_unknown_initial_placeholder_move() {
        let id = DriveNodeId::File("file-a".to_string());
        let remote = vec![DriveProjectionRecord {
            id: id.clone(),
            path: vec!["Remote.txt".to_string()],
            kind: DriveNodeKind::File,
            content_version: "v1".to_string(),
            size: 10,
            updated_at_ms: 1,
        }];
        assert_eq!(
            plan_offline_projection_recovery(
                None,
                &remote,
                &[DriveLocalProjectionRecord {
                    id: id.clone(),
                    path: vec!["Unknown.txt".to_string()],
                    locally_dirty: false,
                }],
            ),
            vec![DriveOfflineProjectionAction::Conflict { id }]
        );
    }

    #[test]
    fn recursive_remove_deletes_children_before_parent_and_rejects_root() {
        let backend = Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        });
        let core = DriveFsCore::new(backend.clone());
        core.remove_recursive(&DriveNodeId::Directory("dir-a".to_string()))
            .unwrap();
        assert_eq!(
            backend.removed.lock().unwrap().as_slice(),
            [
                "file:File(\"file-a\")",
                "file:File(\"file-b\")",
                "dir:Directory(\"dir-b\")",
                "dir:Directory(\"dir-a\")",
            ]
        );
        assert!(core.remove_recursive(&DriveNodeId::Root).is_err());
    }

    #[test]
    fn recursive_remove_preflight_rejects_late_failures_without_deleting_any_children() {
        let root = DriveNodeId::Directory("dir-a".to_string());
        for (fixture, expected_error) in [
            (RecursiveRemoveFixture::ListingError, "listing failure"),
            (RecursiveRemoveFixture::Cycle, "cycle or duplicate"),
            (RecursiveRemoveFixture::WrongParent, "invalid parent"),
            (RecursiveRemoveFixture::TooDeep, "hierarchy is too deep"),
        ] {
            let backend = Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            });
            let core = DriveFsCore::new(backend.clone());
            let error = core.remove_recursive(&root).unwrap_err();
            assert!(
                error.to_string().contains(expected_error),
                "unexpected preflight error: {error}"
            );
            assert!(
                backend.removed.lock().unwrap().is_empty(),
                "preflight failure must not partially delete earlier siblings"
            );
        }
    }

    #[test]
    fn native_lookup_and_readdir_reject_malformed_child_metadata() {
        let parent = DriveNodeId::Directory("dir-a".to_string());
        let valid = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        }));
        assert_eq!(
            valid.lookup(&parent, "a.txt").unwrap().parent,
            parent,
            "valid metadata stays accessible"
        );
        assert_eq!(valid.list_directory(&parent).unwrap().len(), 2);

        for (fixture, reason) in [
            (RecursiveRemoveFixture::WrongParent, "another parent"),
            (
                RecursiveRemoveFixture::InvalidChildName,
                "item name is invalid",
            ),
            (
                RecursiveRemoveFixture::ReservedChildName,
                "item name is invalid",
            ),
            (
                RecursiveRemoveFixture::TrailingDotChildName,
                "item name is invalid",
            ),
            (
                RecursiveRemoveFixture::OverlongChildName,
                "item name is invalid",
            ),
            (
                RecursiveRemoveFixture::RootChildIdentity,
                "node identity or kind",
            ),
            (
                RecursiveRemoveFixture::WrongChildKind,
                "node identity or kind",
            ),
            (
                RecursiveRemoveFixture::EmptyFileIdentity,
                "empty node identity",
            ),
            (
                RecursiveRemoveFixture::EmptyDirectoryIdentity,
                "empty node identity",
            ),
            (
                RecursiveRemoveFixture::EmptyPendingIdentity,
                "empty node identity",
            ),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            let lookup_error = core.lookup(&parent, "a.txt").unwrap_err();
            assert!(
                lookup_error.to_string().contains(reason),
                "invalid lookup {reason} was not rejected: {lookup_error}"
            );
            let listing_parent = if fixture == RecursiveRemoveFixture::WrongParent {
                DriveNodeId::Directory("dir-b".to_string())
            } else {
                parent.clone()
            };
            let listing_error = core.list_directory(&listing_parent).unwrap_err();
            assert!(
                listing_error.to_string().contains(reason),
                "invalid listing {reason} was not rejected: {listing_error}"
            );
        }

        assert!(valid.lookup(&parent, "bad/name").is_err());
    }

    #[test]
    fn native_create_and_rename_reject_unportable_names_before_backend_calls() {
        let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        }));
        let parent = DriveNodeId::Root;
        let file = DriveNodeId::File("file-a".to_string());
        for invalid in [
            "CON.txt".to_string(),
            "LPT1".to_string(),
            "trailing.".to_string(),
            "trailing ".to_string(),
            "name|with-pipe".to_string(),
            "name\nwith-newline".to_string(),
            "a".repeat(256),
        ] {
            for result in [
                core.create_file(&parent, &invalid),
                core.create_directory(&parent, &invalid),
                core.rename(&file, &parent, &invalid),
                core.lookup(&parent, &invalid),
            ] {
                assert!(
                    matches!(result, Err(DriveFsError::InvalidInput(_))),
                    "unportable native name {invalid:?} reached backend: {result:?}"
                );
            }
        }

        // With a valid portable component the same unused test backend is
        // invoked, proving the precheck does not reject ordinary operations.
        for result in [
            core.create_file(&parent, "report.txt"),
            core.create_directory(&parent, "Work"),
            core.rename(&file, &parent, "renamed.txt"),
        ] {
            assert!(
                matches!(result, Err(DriveFsError::Internal(_))),
                "valid name was rejected before backend: {result:?}"
            );
        }
    }

    #[test]
    fn native_begin_write_verifies_staging_metadata_and_original_file() {
        let file = DriveNodeId::File("file-a".into());
        let stage = DriveNodeId::Pending("staging-123".into());
        let valid = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::BeginWriteValid,
        }));
        let opened = valid.begin_write(&file).unwrap();
        assert_eq!(opened.id, file);
        assert!(opened.pending);
        assert_eq!(opened.content_version, "new-staging-id");
        let pending = valid.begin_write(&stage).unwrap();
        assert_eq!(pending.id, DriveNodeId::File("visible-entry".into()));
        assert!(pending.pending);
        assert_eq!(pending.content_version, "staging-123");
        assert!(matches!(
            valid.begin_write(&DriveNodeId::Root),
            Err(DriveFsError::IsDirectory)
        ));
        assert!(matches!(
            valid.begin_write(&DriveNodeId::Directory("folder".into())),
            Err(DriveFsError::IsDirectory)
        ));

        for (source, fixture, reason) in [
            (
                &file,
                RecursiveRemoveFixture::BeginWriteWrongId,
                "unrelated staging identity",
            ),
            (
                &stage,
                RecursiveRemoveFixture::BeginWriteWrongStage,
                "unrelated staging identity",
            ),
            (
                &file,
                RecursiveRemoveFixture::BeginWriteNotPending,
                "unrelated staging identity",
            ),
            (
                &stage,
                RecursiveRemoveFixture::BeginWriteNotPending,
                "unrelated staging identity",
            ),
            (
                &file,
                RecursiveRemoveFixture::BeginWriteInvalidName,
                "item name is invalid",
            ),
            (
                &file,
                RecursiveRemoveFixture::BeginWriteInvalidParent,
                "invalid parent identity",
            ),
            (
                &file,
                RecursiveRemoveFixture::BeginWriteWrongKind,
                "cannot be a pending file",
            ),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            let error = core.begin_write(source).unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "corrupt staging metadata escaped verification for {source:?}: {error}"
            );
        }
    }

    #[test]
    fn native_rename_verifies_backend_identity_and_destination() {
        let parent = DriveNodeId::Directory("dir-a".to_string());
        let file = DriveNodeId::File("file-a".to_string());
        let directory = DriveNodeId::Directory("dir-b".to_string());
        let staging = DriveNodeId::Pending("staging-123".to_string());
        let valid = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::RenamedValid,
        }));
        assert_eq!(
            valid.rename(&file, &parent, "new-file.txt").unwrap().id,
            file
        );
        let renamed_directory = valid.rename(&directory, &parent, "New Folder").unwrap();
        assert_eq!(renamed_directory.id, directory);
        assert_eq!(renamed_directory.kind, DriveNodeKind::Directory);
        let pending = valid.rename(&staging, &parent, "new-pending.txt").unwrap();
        assert_eq!(pending.id, DriveNodeId::File("visible-entry".into()));
        assert!(pending.pending);
        assert_eq!(pending.content_version, "staging-123");

        for (source, fixture, reason) in [
            (
                &file,
                RecursiveRemoveFixture::RenamedWrongParent,
                "invalid parent",
            ),
            (
                &file,
                RecursiveRemoveFixture::RenamedWrongName,
                "different child name",
            ),
            (
                &file,
                RecursiveRemoveFixture::RenamedWrongId,
                "unrelated node identity",
            ),
            (
                &directory,
                RecursiveRemoveFixture::RenamedWrongId,
                "unrelated node identity",
            ),
            (
                &directory,
                RecursiveRemoveFixture::RenamedWrongKind,
                "identity or kind",
            ),
            (
                &directory,
                RecursiveRemoveFixture::RenamedDirectoryPending,
                "unrelated node identity",
            ),
            (
                &staging,
                RecursiveRemoveFixture::RenamedWrongStage,
                "unrelated node identity",
            ),
            (
                &staging,
                RecursiveRemoveFixture::RenamedPendingNotPending,
                "unrelated node identity",
            ),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            let error = core.rename(source, &parent, "new-file.txt").unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "corrupt rename metadata was accepted for {source:?}: {error}"
            );
        }
    }

    #[test]
    fn native_create_and_rename_reject_invalid_destinations_before_backend_dispatch() {
        let backend = Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: false,
            stable_content_version: true,
            pending: false,
            change_size: false,
            change_modified_time: false,
        });
        let core = DriveFsCore::new(backend);
        let source = DriveNodeId::File("source".into());
        for parent in [
            DriveNodeId::File("other-file".into()),
            DriveNodeId::Pending("other-stage".into()),
            DriveNodeId::Directory("  ".into()),
        ] {
            let expected = if matches!(parent, DriveNodeId::Directory(_)) {
                DriveFsError::InvalidInput("Drive parent directory ID is invalid".into())
            } else {
                DriveFsError::NotDirectory
            };
            assert_eq!(core.create_file(&parent, "new.txt"), Err(expected.clone()));
            assert_eq!(
                core.create_directory(&parent, "new-dir"),
                Err(expected.clone())
            );
            assert_eq!(core.rename(&source, &parent, "renamed.txt"), Err(expected));
        }
        assert_eq!(
            core.rename(&DriveNodeId::Root, &DriveNodeId::Root, "renamed.txt"),
            Err(DriveFsError::AccessDenied(
                "Cannot rename the Drive root".into()
            ))
        );
        for source in [
            DriveNodeId::Directory("".into()),
            DriveNodeId::File("   ".into()),
            DriveNodeId::Pending("\t".into()),
        ] {
            assert_eq!(
                core.rename(&source, &DriveNodeId::Root, "renamed.txt"),
                Err(DriveFsError::InvalidInput(
                    "Drive rename source ID is invalid".into()
                ))
            );
        }
        // The synthetic backend rejects all actual mutations with Internal.
        // Only structurally valid requests are allowed to reach that sentinel.
        for parent in [DriveNodeId::Root, DriveNodeId::Directory("folder".into())] {
            for result in [
                core.create_file(&parent, "new.txt"),
                core.create_directory(&parent, "new-dir"),
                core.rename(&source, &parent, "renamed.txt"),
            ] {
                assert!(matches!(result, Err(DriveFsError::Internal(_))));
            }
        }
        // A normal pending overlay rename still keeps the visible File identity.
        let valid = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::RenamedValid,
        }));
        let renamed = valid
            .rename(
                &DriveNodeId::Pending("stage".into()),
                &DriveNodeId::Root,
                "new.txt",
            )
            .unwrap();
        assert_eq!(renamed.id, DriveNodeId::File("visible-entry".into()));
        assert_eq!(renamed.content_version, "stage");
    }

    #[test]
    fn native_create_validates_backend_file_and_folder_identity() {
        let parent = DriveNodeId::Root;
        let valid = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::CreatedValid,
        }));
        let file = valid.create_file(&parent, "report.txt").unwrap();
        assert_eq!(file.parent, parent);
        assert_eq!(file.name, "report.txt");
        assert_eq!(file.kind, DriveNodeKind::File);
        assert!(file.pending, "normal staging-file creation remains valid");
        let directory = valid.create_directory(&parent, "Work").unwrap();
        assert_eq!(directory.parent, parent);
        assert_eq!(directory.name, "Work");
        assert_eq!(directory.kind, DriveNodeKind::Directory);
        assert!(!directory.pending);

        for (fixture, reason) in [
            (RecursiveRemoveFixture::CreatedWrongParent, "invalid parent"),
            (
                RecursiveRemoveFixture::CreatedWrongName,
                "different child name",
            ),
            (RecursiveRemoveFixture::CreatedWrongKind, "identity or kind"),
            (
                RecursiveRemoveFixture::CreatedEmptyId,
                "empty node identity",
            ),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            for result in [
                core.create_file(&parent, "report.txt"),
                core.create_directory(&parent, "Work"),
            ] {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains(reason),
                    "corrupt created metadata escaped {reason} validation: {error}"
                );
            }
        }
        let pending_directory = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::CreatedPendingDirectory,
        }));
        assert!(pending_directory.create_file(&parent, "report.txt").is_ok());
        assert!(pending_directory
            .create_directory(&parent, "Work")
            .unwrap_err()
            .to_string()
            .contains("wrong node kind"));
    }

    #[test]
    fn native_getattr_rejects_corrupt_backend_identity_and_parent() {
        let normal = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        }));
        let directory = DriveNodeId::Directory("dir-a".to_string());
        let file = DriveNodeId::File("file-a".to_string());
        assert_eq!(
            normal.metadata(&DriveNodeId::Root).unwrap().kind,
            DriveNodeKind::Directory
        );
        assert_eq!(normal.metadata(&directory).unwrap().id, directory);
        assert_eq!(normal.metadata(&file).unwrap().id, file);

        for (fixture, reason) in [
            (
                RecursiveRemoveFixture::MetadataWrongId,
                "another node identity",
            ),
            (
                RecursiveRemoveFixture::MetadataWrongKind,
                "identity or kind",
            ),
            (
                RecursiveRemoveFixture::MetadataInvalidParent,
                "invalid parent identity",
            ),
            (
                RecursiveRemoveFixture::MetadataInvalidName,
                "item name is invalid",
            ),
            (
                RecursiveRemoveFixture::MetadataSelfParent,
                "invalid parent identity",
            ),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            let error = core.metadata(&file).unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "invalid native getattr {reason} escaped validation: {error}"
            );
        }
        let corrupt_root = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::MetadataInvalidRoot,
        }));
        assert!(corrupt_root.metadata(&DriveNodeId::Root).is_err());

        // Native staging lookups intentionally resolve the visible File ID,
        // not the Pending ID. The pending content version must still match.
        let pending = DriveFsCore::new(Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: false,
            stable_content_version: true,
            pending: true,
            change_size: false,
            change_modified_time: false,
        }));
        let stage = pending
            .metadata(&DriveNodeId::Pending("v1".to_string()))
            .unwrap();
        assert_eq!(stage.id, DriveNodeId::File("entry".to_string()));
        assert!(stage.pending);
        assert!(pending
            .metadata(&DriveNodeId::Pending("wrong-stage".to_string()))
            .is_err());
        assert!(pending
            .metadata(&DriveNodeId::File("wrong-file".to_string()))
            .is_err());
    }

    #[test]
    fn native_read_and_directory_kind_guards_reject_invalid_provider_calls() {
        let backend = Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        });
        let core = DriveFsCore::new(backend);
        let root = DriveNodeId::Root;
        let directory = DriveNodeId::Directory("dir-a".into());
        let file = DriveNodeId::File("file-a".into());
        let pending = DriveNodeId::Pending("stage-a".into());

        for invalid_parent in [&file, &pending] {
            assert_eq!(
                core.lookup(invalid_parent, "a.txt"),
                Err(DriveFsError::NotDirectory)
            );
            assert_eq!(
                core.list_directory(invalid_parent),
                Err(DriveFsError::NotDirectory)
            );
        }
        let missing_parent = DriveNodeId::Directory("   ".into());
        assert!(matches!(
            core.lookup(&missing_parent, "a.txt"),
            Err(DriveFsError::InvalidInput(_))
        ));
        assert!(matches!(
            core.list_directory(&missing_parent),
            Err(DriveFsError::InvalidInput(_))
        ));
        for invalid_file in [&root, &directory] {
            assert_eq!(
                core.read(invalid_file, 0, 4),
                Err(DriveFsError::IsDirectory)
            );
            assert_eq!(
                core.read_versioned(invalid_file, "v1", 0, 4),
                Err(DriveFsError::IsDirectory)
            );
        }

        // Valid directory traversal is still delegated to the backend.
        assert_eq!(core.list_directory(&root).unwrap(), vec![]);
        assert_eq!(core.lookup(&directory, "a.txt").unwrap().id, file);

        // Both visible files and staging IDs remain legitimate read targets.
        let valid = DriveFsCore::new(Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: false,
            stable_content_version: true,
            pending: true,
            change_size: false,
            change_modified_time: false,
        }));
        for id in [
            DriveNodeId::File("entry".into()),
            DriveNodeId::Pending("v1".into()),
        ] {
            assert_eq!(valid.read(&id, 0, 4).unwrap(), vec![1, 2, 3, 4]);
            assert_eq!(
                valid.read_versioned(&id, "v1", 0, 4).unwrap(),
                vec![1, 2, 3, 4]
            );
        }
    }

    #[test]
    fn native_file_mutation_kind_guards_prevent_invalid_backend_calls() {
        let backend = Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::ExactWrite,
        });
        let core = DriveFsCore::new(backend.clone());
        let root = DriveNodeId::Root;
        let directory = DriveNodeId::Directory("dir-a".to_string());
        let file = DriveNodeId::File("file-a".to_string());
        let pending = DriveNodeId::Pending("pending-a".to_string());

        for invalid in [&root, &directory] {
            assert_eq!(
                core.write(invalid, 0, b"data"),
                Err(DriveFsError::IsDirectory)
            );
            assert_eq!(core.resize(invalid, 3), Err(DriveFsError::IsDirectory));
            assert_eq!(core.flush(invalid), Err(DriveFsError::IsDirectory));
            assert_eq!(core.close_write(invalid), Err(DriveFsError::IsDirectory));
            assert_eq!(core.abort_write(invalid), Err(DriveFsError::IsDirectory));
        }
        assert!(matches!(
            core.remove_file(&root),
            Err(DriveFsError::AccessDenied(_))
        ));
        assert_eq!(core.remove_file(&directory), Err(DriveFsError::IsDirectory));
        assert!(matches!(
            core.remove_directory(&root),
            Err(DriveFsError::AccessDenied(_))
        ));
        for invalid in [&file, &pending] {
            assert_eq!(
                core.remove_directory(invalid),
                Err(DriveFsError::NotDirectory)
            );
        }
        assert!(
            backend.removed.lock().unwrap().is_empty(),
            "invalid filesystem mutations must not reach the provider"
        );

        for valid in [&file, &pending] {
            assert_eq!(core.write(valid, 0, b"data"), Ok(4));
            for result in [
                core.resize(valid, 3),
                core.flush(valid),
                core.close_write(valid),
                core.abort_write(valid),
            ] {
                assert!(matches!(result, Err(DriveFsError::Internal(_))));
            }
            core.remove_file(valid).unwrap();
        }
        core.remove_directory(&directory).unwrap();
        assert_eq!(
            *backend.removed.lock().unwrap(),
            vec![
                format!("file:{file:?}"),
                format!("file:{pending:?}"),
                format!("dir:{directory:?}")
            ]
        );
    }

    #[test]
    fn native_write_rejects_impossible_backend_byte_counts() {
        let id = DriveNodeId::Pending("stage-a".to_string());
        let payload = [1_u8, 2, 3, 4];
        for (fixture, expected_written) in [
            (RecursiveRemoveFixture::ShortWrite, 3_u64),
            (RecursiveRemoveFixture::ExactWrite, 4_u64),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            assert_eq!(
                core.write(&id, 0, &payload).unwrap(),
                expected_written,
                "legal partial/full backend writes must remain allowed"
            );
        }
        let corrupt = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::OverlongWrite,
        }));
        for payload in [&payload[..], &payload[..0]] {
            let error = corrupt.write(&id, 0, payload).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("more written bytes than supplied"),
                "overlong native write was reported as success: {error}"
            );
        }
        assert!(corrupt.write(&id, u64::MAX, &payload).is_err());
    }

    #[test]
    fn native_lookup_requires_the_requested_portable_name() {
        let parent = DriveNodeId::Directory("dir-a".to_string());
        let wrong = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::WrongLookupName,
        }));
        let error = wrong.lookup(&parent, "a.txt").unwrap_err();
        assert!(
            error.to_string().contains("different child name"),
            "wrong but otherwise valid lookup target escaped verification: {error}"
        );
        // Preserve ordinary case-insensitive lookup with the stored original
        // display spelling, as expected by Windows Explorer.
        let valid_case = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::CaseVariantLookupName,
        }));
        assert_eq!(valid_case.lookup(&parent, "a.txt").unwrap().name, "A.TXT");
    }

    #[test]
    fn native_readdir_rejects_duplicate_provider_identities() {
        let parent = DriveNodeId::Directory("dir-a".to_string());
        let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::DuplicateChildIdentity,
        }));
        // Each child is individually valid and has a distinct display name,
        // but duplicate provider IDs would map two entries to the same inode.
        let error = core.list_directory(&parent).unwrap_err();
        assert!(
            error.to_string().contains("duplicate child identity"),
            "corrupt listing must fail closed: {error}"
        );
        assert_eq!(core.lookup(&parent, "a.txt").unwrap().name, "a.txt");
        assert_eq!(
            DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture: RecursiveRemoveFixture::Normal,
            }))
            .list_directory(&parent)
            .unwrap()
            .len(),
            2
        );
    }

    #[test]
    fn native_readdir_rejects_portable_name_collisions() {
        let parent = DriveNodeId::Directory("dir-a".to_string());
        for (fixture, description) in [
            (RecursiveRemoveFixture::CaseFoldedChildNameCollision, "case"),
            (RecursiveRemoveFixture::UnicodeChildNameCollision, "Unicode"),
        ] {
            let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
                removed: Mutex::new(Vec::new()),
                fixture,
            }));
            let error = core.list_directory(&parent).unwrap_err();
            assert!(
                error.to_string().contains("portable name collision"),
                "{description} collision escaped native folder validation: {error}"
            );
        }

        let core = DriveFsCore::new(Arc::new(RecursiveRemoveBackend {
            removed: Mutex::new(Vec::new()),
            fixture: RecursiveRemoveFixture::Normal,
        }));
        let normal = core.list_directory(&parent).unwrap();
        assert_eq!(normal.len(), 2);
        assert_eq!(normal[0].name, "a.txt");
        assert_eq!(normal[1].name, "B");
    }

    #[test]
    fn core_read_rejects_backend_results_larger_than_requested_ranges() {
        let id = DriveNodeId::File("entry".to_string());
        let backend = Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: false,
            stable_content_version: true,
            pending: false,
            change_size: false,
            change_modified_time: false,
        });
        let snapshot = backend.metadata_value();
        let core = DriveFsCore::new(backend);
        for length in [0_u64, 1, 3] {
            let error = core.read(&id, 0, length).unwrap_err();
            assert!(
                error.to_string().contains("more bytes than requested"),
                "overlong {length}-byte read was accepted: {error}"
            );
            let error = core.read_versioned(&id, "v1", 0, length).unwrap_err();
            assert!(error.to_string().contains("more bytes than requested"));
            let error = core
                .read_snapshot_versioned(&id, &snapshot, 0, length)
                .unwrap_err();
            assert!(error.to_string().contains("more bytes than requested"));
        }
        assert_eq!(core.read(&id, 0, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(core.read(&id, 0, 5).unwrap(), vec![1, 2, 3, 4]);
        assert!(core.read(&id, u64::MAX, 1).is_err());
    }

    #[test]
    fn versioned_read_rejects_content_that_changes_during_the_read() {
        let core = DriveFsCore::new(Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: true,
            stable_content_version: false,
            pending: false,
            change_size: false,
            change_modified_time: false,
        }));
        let error = core
            .read_versioned(&DriveNodeId::File("entry".to_string()), "v1", 0, 4)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("content version changed during read"));
    }

    #[test]
    fn versioned_hydration_rejects_pending_edits_even_with_stable_staging_id() {
        let id = DriveNodeId::File("entry".to_string());
        for (change_size, change_modified_time) in [(true, false), (false, true)] {
            let core = DriveFsCore::new(Arc::new(VersionFlipBackend {
                version: AtomicUsize::new(1),
                flip_on_read: true,
                stable_content_version: true,
                pending: true,
                change_size,
                change_modified_time,
            }));
            let error = core.read_versioned(&id, "v1", 0, 4).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("pending content changed during read"),
                "pending resize={change_size}, update={change_modified_time}: {error}"
            );
        }

        // A stable pending file is still hydrateable; reject only real
        // metadata changes, not every write-in-progress staging identity.
        let unchanged = DriveFsCore::new(Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: true,
            stable_content_version: true,
            pending: true,
            change_size: false,
            change_modified_time: false,
        }));
        assert_eq!(
            unchanged.read_versioned(&id, "v1", 0, 4).unwrap(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn windows_hydration_snapshot_catches_pending_mutation_between_chunks() {
        let id = DriveNodeId::File("entry".to_string());
        for (change_size, change_modified_time) in [(true, false), (false, true)] {
            let backend = Arc::new(VersionFlipBackend {
                version: AtomicUsize::new(1),
                flip_on_read: false,
                stable_content_version: true,
                pending: true,
                change_size,
                change_modified_time,
            });
            let pinned = backend.metadata_value();
            let core = DriveFsCore::new(backend.clone());
            assert_eq!(
                core.read_snapshot_versioned(&id, &pinned, 0, 4).unwrap(),
                vec![1, 2, 3, 4],
                "unchanged first chunk must hydrate"
            );
            // This change happens strictly BETWEEN two successful read calls.
            // A per-chunk staging-ID-only check cannot detect it.
            backend.version.store(2, Ordering::SeqCst);
            assert_eq!(
                core.read_versioned(&id, "v1", 0, 4).unwrap(),
                vec![1, 2, 3, 4],
                "old version-only path cannot see between-chunk pending edits"
            );
            let error = core
                .read_snapshot_versioned(&id, &pinned, 4, 4)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("pending content changed since hydration began"),
                "snapshot must reject changed size={change_size}, time={change_modified_time}: {error}"
            );
        }

        let stable = Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
            flip_on_read: false,
            stable_content_version: true,
            pending: true,
            change_size: false,
            change_modified_time: false,
        });
        let pinned = stable.metadata_value();
        let core = DriveFsCore::new(stable);
        assert_eq!(
            core.read_snapshot_versioned(&id, &pinned, 0, 4)
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            core.read_snapshot_versioned(&id, &pinned, 4, 4)
                .unwrap()
                .len(),
            4
        );
        assert!(core
            .read_snapshot_versioned(&DriveNodeId::Root, &pinned, 0, 4)
            .is_err());
    }

    #[test]
    fn provider_identity_round_trips_all_node_kinds() {
        let identities = [
            DriveNodeId::Root,
            DriveNodeId::Directory("dir-123".to_string()),
            DriveNodeId::File("entry-456".to_string()),
            DriveNodeId::Pending("pending-789".to_string()),
        ];
        for identity in identities {
            let encoded = encode_provider_identity(&identity).unwrap();
            assert_eq!(decode_provider_identity(&encoded).unwrap(), identity);
        }
    }

    #[test]
    fn provider_identity_rejects_whitespace_and_nul_opaque_ids_in_both_directions() {
        for payload in ["", " ", "\t\n", "\u{2003}", "valid\0spoofed"] {
            for kind in 1_u8..=3 {
                let id = match kind {
                    1 => DriveNodeId::Directory(payload.to_string()),
                    2 => DriveNodeId::File(payload.to_string()),
                    3 => DriveNodeId::Pending(payload.to_string()),
                    _ => unreachable!(),
                };
                assert!(
                    encode_provider_identity(&id).is_err(),
                    "invalid node ID must not be serialized: {id:?}"
                );
                let mut forged = PROVIDER_IDENTITY_MAGIC.to_vec();
                forged.push(kind);
                forged.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                forged.extend_from_slice(payload.as_bytes());
                assert!(
                    decode_provider_identity(&forged).is_err(),
                    "malformed Windows callback ID must not be accepted: {id:?}"
                );
            }
        }

        // Root has no opaque ID. Legitimate opaque IDs remain round-trippable.
        assert_eq!(
            decode_provider_identity(&encode_provider_identity(&DriveNodeId::Root).unwrap())
                .unwrap(),
            DriveNodeId::Root
        );
        assert_eq!(
            decode_provider_identity(
                &encode_provider_identity(&DriveNodeId::File("legitimate-123".into())).unwrap()
            )
            .unwrap(),
            DriveNodeId::File("legitimate-123".into())
        );
    }

    #[test]
    fn provider_identity_rejects_truncation_invalid_kind_and_oversize() {
        assert!(decode_provider_identity(b"TRD1\x01\x08\x00short").is_err());
        assert!(decode_provider_identity(b"TRD1\x7f\x00\x00").is_err());
        assert!(decode_provider_identity(&vec![0; MAX_PROVIDER_IDENTITY_BYTES + 1]).is_err());
        assert!(encode_provider_identity(&DriveNodeId::File(
            "x".repeat(MAX_PROVIDER_IDENTITY_BYTES)
        ))
        .is_err());
    }

    #[test]
    fn provider_range_coverage_distinguishes_partial_from_full_cancellation() {
        assert!(provider_range_covers(100, 900, 200, 500).unwrap());
        assert!(!provider_range_covers(250, 300, 200, 500).unwrap());
        assert!(!provider_range_covers(100, 400, 200, 500).unwrap());
        assert!(provider_range_covers(200, 500, 200, 500).unwrap());
        assert!(provider_range_covers(u64::MAX - 1, 8, 0, 1).is_err());
    }

    #[test]
    fn windows_volume_relative_rename_targets_become_absolute_and_stay_on_volume() {
        assert_eq!(
            windows_volume_target_path("C:", "\\Users\\Alice\\TeraRelay Drive\\renamed.txt")
                .unwrap(),
            "C:\\Users\\Alice\\TeraRelay Drive\\renamed.txt"
        );
        assert_eq!(
            windows_volume_target_path("c:", "C:/Users/Alice/TeraRelay Drive/renamed.txt").unwrap(),
            "C:\\Users\\Alice\\TeraRelay Drive\\renamed.txt"
        );
        assert!(windows_volume_target_path("C:", "D:\\escape.txt").is_err());
        assert!(windows_volume_target_path("bad", "\\Users\\Alice\\x").is_err());
        assert!(windows_volume_target_path("C:", "relative\\x").is_err());
        // Drive-relative paths are resolved against the process's per-drive
        // current directory, not the supplied volume root.
        assert!(windows_volume_target_path("C:", "C:relative.txt").is_err());
        assert!(windows_volume_target_path("C:", "C:").is_err());
        // Network shares and Win32 device namespaces must never be prefixed
        // with C: and treated as valid same-volume Cloud Files rename targets.
        assert!(windows_volume_target_path("C:", r"\\server\share\item.txt").is_err());
        assert!(windows_volume_target_path("C:", r"\\?\C:\device.txt").is_err());
    }

    #[test]
    fn provider_read_range_alignment_expands_to_blocks_and_respects_eof() {
        assert_eq!(
            align_provider_read_range(123, 100, 10_000, 4096).unwrap(),
            (0, 4096)
        );
        assert_eq!(
            align_provider_read_range(4096, 4096, 10_000, 4096).unwrap(),
            (4096, 4096)
        );
        assert_eq!(
            align_provider_read_range(8192, 4096, 10_000, 4096).unwrap(),
            (8192, 1808)
        );
        assert_eq!(
            align_provider_read_range(0, 0, 10_000, 4096).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn provider_read_range_alignment_rejects_invalid_or_overflowing_ranges() {
        assert!(align_provider_read_range(0, 1, 10, 0).is_err());
        assert!(align_provider_read_range(11, 1, 10, 4096).is_err());
        assert!(align_provider_read_range(u64::MAX - 1, 8, u64::MAX, 4096).is_err());
    }

    #[test]
    fn provider_names_reject_path_traversal_and_separators() {
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "dir/file",
            "dir\\file",
            "bad\0name",
        ] {
            assert!(
                validate_component(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }
        assert!(validate_component("normal file.txt").is_ok());
    }
}
