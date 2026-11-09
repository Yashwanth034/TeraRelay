use crate::commands::streaming::StreamConfig;
use serde::Serialize;
use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tauri::State;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ExternalVideoPlayer {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
struct PlayerLaunchTarget {
    player: ExternalVideoPlayer,
    executable: PathBuf,
    prefix_args: Vec<String>,
}

impl PlayerLaunchTarget {
    fn direct(id: &'static str, name: &'static str, executable: PathBuf) -> Self {
        Self {
            player: ExternalVideoPlayer {
                id: id.to_string(),
                name: name.to_string(),
            },
            executable,
            prefix_args: Vec::new(),
        }
    }

    fn wrapped(
        id: &'static str,
        name: &'static str,
        executable: PathBuf,
        prefix_args: &[&str],
    ) -> Self {
        Self {
            player: ExternalVideoPlayer {
                id: id.to_string(),
                name: name.to_string(),
            },
            executable,
            prefix_args: prefix_args
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        }
    }

    fn spawn(&self, url: &str) -> Result<(), String> {
        Command::new(&self.executable)
            .args(&self.prefix_args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not start {}: {error}", self.player.name))
    }
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn find_program_in_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for directory in env::split_paths(path) {
        #[cfg(windows)]
        let candidates = {
            let has_extension = Path::new(program).extension().is_some();
            if has_extension {
                vec![directory.join(program)]
            } else {
                let extensions =
                    env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
                extensions
                    .split(';')
                    .map(|extension| directory.join(format!("{program}{extension}")))
                    .collect()
            }
        };

        #[cfg(not(windows))]
        let candidates = vec![directory.join(program)];

        for candidate in candidates {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn find_program(program: &str) -> Option<PathBuf> {
    find_program_in_path(program, env::var_os("PATH").as_deref())
}

fn command_succeeds(executable: &Path, args: &[&str]) -> bool {
    Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn detect_linux_player(id: &'static str, name: &'static str) -> Option<PlayerLaunchTarget> {
    if let Some(executable) = find_program(id) {
        return Some(PlayerLaunchTarget::direct(id, name, executable));
    }

    let flatpak_id = match id {
        "vlc" => "org.videolan.VLC",
        "mpv" => "io.mpv.Mpv",
        _ => return None,
    };

    if let Some(flatpak) = find_program("flatpak") {
        if command_succeeds(&flatpak, &["info", flatpak_id]) {
            return Some(PlayerLaunchTarget::wrapped(
                id,
                name,
                flatpak,
                &["run", flatpak_id],
            ));
        }
    }

    if let Some(snap) = find_program("snap") {
        if command_succeeds(&snap, &["list", id]) {
            return Some(PlayerLaunchTarget::wrapped(id, name, snap, &["run", id]));
        }
    }

    None
}

#[cfg(target_os = "windows")]
fn detect_windows_player(
    id: &'static str,
    name: &'static str,
    standard_locations: &[PathBuf],
) -> Option<PlayerLaunchTarget> {
    if let Some(executable) = find_program(&format!("{id}.exe")) {
        return Some(PlayerLaunchTarget::direct(id, name, executable));
    }

    standard_locations
        .iter()
        .find(|path| is_executable(path))
        .cloned()
        .map(|path| PlayerLaunchTarget::direct(id, name, path))
}

#[cfg(target_os = "windows")]
fn detect_windows_players() -> Vec<PlayerLaunchTarget> {
    let mut players = Vec::new();
    let program_files = env::var_os("ProgramFiles").map(PathBuf::from);
    let program_files_x86 = env::var_os("ProgramFiles(x86)").map(PathBuf::from);

    let vlc_locations: Vec<PathBuf> = [program_files.as_ref(), program_files_x86.as_ref()]
        .into_iter()
        .flatten()
        .map(|root| root.join("VideoLAN").join("VLC").join("vlc.exe"))
        .collect();
    let mpv_locations: Vec<PathBuf> = program_files
        .as_ref()
        .map(|root| root.join("mpv").join("mpv.exe"))
        .into_iter()
        .collect();

    if let Some(player) = detect_windows_player("vlc", "VLC", &vlc_locations) {
        players.push(player);
    }
    if let Some(player) = detect_windows_player("mpv", "MPV", &mpv_locations) {
        players.push(player);
    }
    players
}

#[cfg(target_os = "macos")]
fn detect_macos_player(
    id: &'static str,
    name: &'static str,
    app_bundle: &str,
    executable_name: &str,
) -> Option<PlayerLaunchTarget> {
    if let Some(executable) = find_program(executable_name) {
        return Some(PlayerLaunchTarget::direct(id, name, executable));
    }

    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots
        .into_iter()
        .map(|root| {
            root.join(app_bundle)
                .join("Contents")
                .join("MacOS")
                .join(executable_name)
        })
        .find(|path| is_executable(path))
        .map(|path| PlayerLaunchTarget::direct(id, name, path))
}

#[cfg(target_os = "macos")]
fn detect_macos_players() -> Vec<PlayerLaunchTarget> {
    [
        detect_macos_player("vlc", "VLC", "VLC.app", "VLC"),
        detect_macos_player("mpv", "MPV", "mpv.app", "mpv"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn detect_installed_players() -> Vec<PlayerLaunchTarget> {
    #[cfg(target_os = "linux")]
    {
        return [
            detect_linux_player("vlc", "VLC"),
            detect_linux_player("mpv", "MPV"),
        ]
        .into_iter()
        .flatten()
        .collect();
    }

    #[cfg(target_os = "windows")]
    {
        return detect_windows_players();
    }

    #[cfg(target_os = "macos")]
    {
        return detect_macos_players();
    }

    #[allow(unreachable_code)]
    Vec::new()
}

fn build_authenticated_stream_url(
    port: u16,
    token: &str,
    folder_id: Option<i64>,
    message_id: i32,
) -> String {
    let folder = folder_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "home".to_string());
    let encoded_token = urlencoding::encode(token);
    format!("http://127.0.0.1:{port}/stream/{folder}/{message_id}?token={encoded_token}")
}

#[tauri::command]
pub fn cmd_list_external_video_players() -> Vec<ExternalVideoPlayer> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        return Vec::new();
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        detect_installed_players()
            .into_iter()
            .map(|target| target.player)
            .collect()
    }
}

#[tauri::command]
pub fn cmd_open_video_in_external_player(
    player_id: String,
    message_id: i32,
    folder_id: Option<i64>,
    stream: State<'_, StreamConfig>,
) -> Result<(), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (player_id, message_id, folder_id, stream);
        return Err("External video players are available only on desktop.".to_string());
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if message_id <= 0 {
            return Err("Invalid video message ID.".to_string());
        }
        if folder_id.is_some_and(|id| id <= 0) {
            return Err("Invalid channel ID.".to_string());
        }

        let target = detect_installed_players()
            .into_iter()
            .find(|target| target.player.id == player_id)
            .ok_or_else(|| {
                "That external video player is not installed or no longer available.".to_string()
            })?;

        // Use the app's authenticated IPv4 loopback stream endpoint directly.
        // This preserves Telegram-backed streaming and HTTP Range seeking; it
        // never exports Telegram credentials or writes a second media copy.
        let url = build_authenticated_stream_url(stream.port, &stream.token, folder_id, message_id);
        target.spawn(&url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_url_uses_ipv4_loopback_auth_and_home_folder() {
        assert_eq!(
            build_authenticated_stream_url(1455, "abc123", None, 42),
            "http://127.0.0.1:1455/stream/home/42?token=abc123"
        );
    }

    #[test]
    fn stream_url_uses_channel_id_and_encodes_token() {
        assert_eq!(
            build_authenticated_stream_url(18765, "a b&c", Some(9001), 77),
            "http://127.0.0.1:18765/stream/9001/77?token=a%20b%26c"
        );
    }

    #[test]
    fn path_lookup_returns_only_executable_candidates() {
        let root = std::env::temp_dir().join(format!(
            "terarelay-external-player-path-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let candidate = root.join(if cfg!(windows) { "vlc.exe" } else { "vlc" });
        std::fs::write(&candidate, b"test executable").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&candidate).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&candidate, permissions).unwrap();
        }

        let found = find_program_in_path(
            if cfg!(windows) { "vlc.exe" } else { "vlc" },
            Some(root.as_os_str()),
        );
        assert_eq!(found, Some(candidate.clone()));

        let not_found = find_program_in_path("mpv", Some(root.as_os_str()));
        assert!(not_found.is_none());

        std::fs::remove_dir_all(root).unwrap();
    }
}
