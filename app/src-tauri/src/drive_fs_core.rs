use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
        self.backend.metadata(id)
    }

    pub fn lookup(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        self.backend.lookup(parent, name)
    }

    pub fn list_directory(
        &self,
        parent: &DriveNodeId,
    ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
        self.backend.list_directory(parent)
    }

    pub fn read(
        &self,
        id: &DriveNodeId,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        let _ = offset
            .checked_add(length)
            .ok_or_else(|| DriveFsError::InvalidInput("Drive read range overflow".to_string()))?;
        self.backend.read(id, offset, length)
    }

    pub fn read_versioned(
        &self,
        id: &DriveNodeId,
        expected_content_version: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, DriveFsError> {
        let before = self.metadata(id)?;
        if before.content_version != expected_content_version {
            return Err(DriveFsError::Unavailable(
                "Drive content version changed during read".to_string(),
            ));
        }
        let bytes = self.read(id, offset, length)?;
        let after = self.metadata(id)?;
        if after.content_version != expected_content_version {
            return Err(DriveFsError::Unavailable(
                "Drive content version changed during read".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub fn create_file(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        self.backend.create_file(parent, name)
    }

    pub fn create_directory(
        &self,
        parent: &DriveNodeId,
        name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(name)?;
        self.backend.create_directory(parent, name)
    }

    pub fn begin_write(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
        self.backend.begin_write(id)
    }

    pub fn write(&self, id: &DriveNodeId, offset: u64, data: &[u8]) -> Result<u64, DriveFsError> {
        let data_len = u64::try_from(data.len())
            .map_err(|_| DriveFsError::InvalidInput("Drive write is too large".to_string()))?;
        let _ = offset
            .checked_add(data_len)
            .ok_or_else(|| DriveFsError::InvalidInput("Drive write range overflow".to_string()))?;
        self.backend.write(id, offset, data)
    }

    pub fn resize(&self, id: &DriveNodeId, size: u64) -> Result<(), DriveFsError> {
        self.backend.resize(id, size)
    }

    pub fn flush(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        self.backend.flush(id)
    }

    pub fn close_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        self.backend.close_write(id)
    }

    pub fn abort_write(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        self.backend.abort_write(id)
    }

    pub fn rename(
        &self,
        id: &DriveNodeId,
        new_parent: &DriveNodeId,
        new_name: &str,
    ) -> Result<DriveNodeMetadata, DriveFsError> {
        validate_component(new_name)?;
        self.backend.rename(id, new_parent, new_name)
    }

    pub fn remove_file(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        self.backend.remove_file(id)
    }

    pub fn remove_directory(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        self.backend.remove_directory(id)
    }

    pub fn remove_recursive(&self, id: &DriveNodeId) -> Result<(), DriveFsError> {
        if matches!(id, DriveNodeId::Root) {
            return Err(DriveFsError::AccessDenied(
                "Cannot recursively remove the Drive root".to_string(),
            ));
        }
        let mut visited = 0usize;
        self.remove_recursive_inner(id, 0, &mut visited)
    }

    fn remove_recursive_inner(
        &self,
        id: &DriveNodeId,
        depth: usize,
        visited: &mut usize,
    ) -> Result<(), DriveFsError> {
        if depth > MAX_RECURSIVE_REMOVE_DEPTH {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal hierarchy is too deep".to_string(),
            ));
        }
        *visited = visited.checked_add(1).ok_or_else(|| {
            DriveFsError::InvalidInput("Drive recursive removal item count overflow".to_string())
        })?;
        if *visited > MAX_RECURSIVE_REMOVE_ITEMS {
            return Err(DriveFsError::InvalidInput(
                "Drive recursive removal exceeds the safe item limit".to_string(),
            ));
        }

        let metadata = self.metadata(id)?;
        match metadata.kind {
            DriveNodeKind::File => self.remove_file(id),
            DriveNodeKind::Directory => {
                for child in self.list_directory(id)? {
                    self.remove_recursive_inner(&child.id, depth + 1, visited)?;
                }
                self.remove_directory(id)
            }
        }
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
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        if !bytes[0].eq_ignore_ascii_case(&volume_bytes[0]) {
            return Err(DriveFsError::AccessDenied(
                "Windows callback target is on another volume".to_string(),
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

pub fn encode_provider_identity(id: &DriveNodeId) -> Result<Vec<u8>, DriveFsError> {
    let (kind, value): (u8, &[u8]) = match id {
        DriveNodeId::Root => (0, &[]),
        DriveNodeId::Directory(value) => (1, value.as_bytes()),
        DriveNodeId::File(value) => (2, value.as_bytes()),
        DriveNodeId::Pending(value) => (3, value.as_bytes()),
    };
    if kind != 0 && value.is_empty() {
        return Err(DriveFsError::InvalidInput(
            "Drive provider identity cannot have an empty id".to_string(),
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
        1 if !value.is_empty() => Ok(DriveNodeId::Directory(value.to_string())),
        2 if !value.is_empty() => Ok(DriveNodeId::File(value.to_string())),
        3 if !value.is_empty() => Ok(DriveNodeId::Pending(value.to_string())),
        _ => Err(DriveFsError::InvalidInput(
            "Drive provider identity kind is invalid".to_string(),
        )),
    }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct VersionFlipBackend {
        version: AtomicUsize,
    }

    impl VersionFlipBackend {
        fn metadata_value(&self) -> DriveNodeMetadata {
            DriveNodeMetadata {
                id: DriveNodeId::File("entry".to_string()),
                parent: DriveNodeId::Root,
                name: "file.bin".to_string(),
                kind: DriveNodeKind::File,
                size: 4,
                created_at_ms: 0,
                updated_at_ms: 0,
                content_version: format!("v{}", self.version.load(Ordering::SeqCst)),
                pending: false,
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
            self.version.store(2, Ordering::SeqCst);
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

    struct RecursiveRemoveBackend {
        removed: Mutex<Vec<String>>,
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
    }

    impl DriveFsBackend for RecursiveRemoveBackend {
        fn revision(&self) -> Result<u64, DriveFsError> {
            Ok(0)
        }
        fn metadata(&self, id: &DriveNodeId) -> Result<DriveNodeMetadata, DriveFsError> {
            Self::item(id)
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
            parent: &DriveNodeId,
        ) -> Result<Vec<DriveNodeMetadata>, DriveFsError> {
            let ids: &[DriveNodeId] = match parent {
                DriveNodeId::Directory(value) if value == "dir-a" => &[
                    DriveNodeId::File("file-a".to_string()),
                    DriveNodeId::Directory("dir-b".to_string()),
                ],
                DriveNodeId::Directory(value) if value == "dir-b" => {
                    &[DriveNodeId::File("file-b".to_string())]
                }
                _ => &[],
            };
            ids.iter().map(Self::item).collect()
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
    fn versioned_read_rejects_content_that_changes_during_the_read() {
        let core = DriveFsCore::new(Arc::new(VersionFlipBackend {
            version: AtomicUsize::new(1),
        }));
        let error = core
            .read_versioned(&DriveNodeId::File("entry".to_string()), "v1", 0, 4)
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("content version changed during read"));
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
