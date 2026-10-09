use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "status", content = "data")]
pub enum AuthState {
    LoggedOut,
    AwaitingCode {
        phone: String,
        phone_code_hash: String,
    },
    AwaitingPassword {
        phone: String,
    },
    LoggedIn,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AuthResult {
    pub success: bool,
    pub next_step: Option<String>, // "code", "password", "dashboard"
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileMetadata {
    pub id: i64,
    pub folder_id: Option<i64>,
    pub name: String,
    pub size: u64, // Updated to u64
    pub mime_type: Option<String>,
    pub file_ext: Option<String>, // Added field
    pub created_at: String,
    pub icon_type: String,
    /// True when this entry aggregates multiple ".tgdpart" messages (file > 2GB).
    #[serde(default)]
    pub is_split: bool,
    /// Stable TeraRelay logical-file identity. Present for manifest-backed
    /// channel files and used by higher-level metadata such as version stacks.
    #[serde(default)]
    pub logical_file_id: Option<String>,
    /// Optional version-stack identity. When present, this visible entry is the
    /// stack's current primary file.
    #[serde(default)]
    pub stack_id: Option<String>,
    /// User-facing stack name. The physical primary filename remains in `name`
    /// so preview/download/type detection continue to use the real file.
    #[serde(default)]
    pub stack_name: Option<String>,
    /// Number of currently available versions represented by this entry.
    #[serde(default)]
    pub stack_version_count: u32,
    /// Optional label attached to the primary version (for example "4K HDR").
    #[serde(default)]
    pub stack_label: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FolderMetadata {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    /// Telegram public username (e.g. "mychannel"). None if private.
    pub username: Option<String>,
    /// Whether the channel is public (has a username set).
    pub is_public: bool,
    // Local-first grouping & ordering metadata
    pub group_id: Option<i32>,
    pub display_order: i32,
    /// TeraRelay logical-channel role. "owner" can manage/invite/upload;
    /// "member" is view/download-only in V1. None is legacy/unscoped data.
    #[serde(default)]
    pub role: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FolderGroup {
    pub id: i32,
    pub name: String,
    pub color_hex: String,
    pub display_order: i32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Drive {
    pub chat_id: i64,
    pub name: String,
    pub icon: Option<String>,
}
