//! VPN Optimizer & Proxy Configuration
//!
//! Stores runtime network configuration that all network operations read from.
//! When vpnMode is off, helpers return hardcoded defaults (zero behaviour change).
//! When vpnMode is on, helpers return user-configured values.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use tauri::Manager;

/// Proxy configuration received from the frontend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub proxy_type: String, // "socks5" | "mtproto"
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String, // SOCKS5
}

/// Validate configuration before saving or starting any network traffic.
/// Reject malformed enabled proxies instead of silently connecting directly.
pub(crate) fn validate_enabled_proxy(proxy: &ProxyConfig) -> Result<(), String> {
    if !proxy.enabled {
        return Ok(());
    }
    let host = proxy.host.trim();
    if host.is_empty() || proxy.port == 0 {
        return Err("Enabled proxy requires a host and nonzero port".into());
    }
    if host
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("Proxy host cannot contain whitespace or control characters".into());
    }
    if !matches!(proxy.proxy_type.as_str(), "socks5" | "http" | "https") {
        return Err("Unsupported proxy type".into());
    }
    if proxy.proxy_type == "socks5" && proxy.username.is_empty() && !proxy.password.is_empty() {
        return Err("SOCKS5 proxy password requires a username".into());
    }
    {
        let normalized = host.trim_start_matches('[').trim_end_matches(']');
        // Brackets are accepted only as a pair surrounding a real IPv6 host.
        if (host.starts_with('[') || host.ends_with(']'))
            && (host != format!("[{normalized}]")
                || normalized.parse::<std::net::Ipv6Addr>().is_err())
        {
            return Err("Invalid bracketed IPv6 proxy host".into());
        }
        let mut url = reqwest::Url::parse("socks5://localhost")
            .map_err(|_| "Could not initialize proxy validation".to_string())?;
        let formatted = if normalized.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{normalized}]")
        } else {
            host.to_string()
        };
        url.set_host(Some(&formatted))
            .map_err(|_| "Invalid SOCKS5 proxy host".to_string())?;
    }
    Ok(())
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            proxy_type: "socks5".into(),
            host: String::new(),
            port: 1080,
            username: String::new(),
            password: String::new(),
        }
    }
}

/// VPN optimizer configuration received from the frontend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VpnConfig {
    pub enabled: bool,
    pub timeout_multiplier: u32,    // 1–5
    pub retry_attempts: u32,        // 0–5
    pub retry_base_backoff_ms: u64, // 500–5000
    pub retry_max_backoff_ms: u64,  // 8000–60000
    pub adaptive_polling: bool,
    pub polling_min_sec: u32,      // 10–30
    pub polling_max_sec: u32,      // 45–120
    pub preferred_dc: String,      // "auto" | "dc1"–"dc5"
    pub dc_fallback_attempts: u32, // 1–4
    pub flood_wait_respect: bool,
    pub peer_cache_size: usize,        // 100–2000
    pub bandwidth_limit_up_kbs: u32,   // 0 = unlimited
    pub bandwidth_limit_down_kbs: u32, // 0 = unlimited
    pub chunk_size_kb: u32,            // 128, 256, 512
    pub keep_alive_interval_sec: u32,  // 0 = disabled, 30–120
    pub auto_detect_vpn: bool,
    pub archive_max_bytes: u64, // 0 = unlimited, max bytes for bulk archive (API)
}

impl Default for VpnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_multiplier: 3,
            retry_attempts: 3,
            retry_base_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            adaptive_polling: true,
            polling_min_sec: 15,
            polling_max_sec: 60,
            preferred_dc: "auto".into(),
            dc_fallback_attempts: 2,
            flood_wait_respect: true,
            peer_cache_size: 500,
            bandwidth_limit_up_kbs: 0,
            bandwidth_limit_down_kbs: 0,
            chunk_size_kb: 512,
            keep_alive_interval_sec: 0,
            auto_detect_vpn: false,
            archive_max_bytes: 256 * 1024 * 1024, // 256 MiB
        }
    }
}

/// Combined network config snapshot (what the frontend receives)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfigSnapshot {
    pub proxy: ProxyConfig,
    pub vpn: VpnConfig,
}

/// Thread-safe global state managed via Tauri's state system
pub struct NetworkConfig {
    pub proxy: RwLock<ProxyConfig>,
    pub vpn: RwLock<VpnConfig>,
    pub bridge_handle: std::sync::Mutex<Option<(u16, tokio::task::JoinHandle<()>)>>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkConfig {
    pub fn new() -> Self {
        Self {
            proxy: RwLock::new(ProxyConfig::default()),
            vpn: RwLock::new(VpnConfig::default()),
            bridge_handle: std::sync::Mutex::new(None),
        }
    }

    pub fn new_with_config(config: NetworkConfigSnapshot) -> Self {
        Self {
            proxy: RwLock::new(config.proxy),
            vpn: RwLock::new(config.vpn),
            bridge_handle: std::sync::Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> NetworkConfigSnapshot {
        NetworkConfigSnapshot {
            proxy: self.proxy.read().unwrap().clone(),
            vpn: self.vpn.read().unwrap().clone(),
        }
    }

    /// Return no proxy *only when explicitly disabled*. An enabled but
    /// malformed/unavailable proxy must never silently downgrade Telegram
    /// traffic, downloads or imports to a direct network connection.
    pub fn effective_proxy_url_checked(&self) -> Result<Option<String>, String> {
        let proxy = self
            .proxy
            .read()
            .map_err(|_| "Proxy configuration lock is unavailable".to_string())?;
        validate_enabled_proxy(&proxy)?;
        if !proxy.enabled {
            return Ok(None);
        }
        match proxy.proxy_type.as_str() {
            "socks5" => {
                let mut url = reqwest::Url::parse("socks5://localhost")
                    .map_err(|_| "SOCKS5 URL setup failed".to_string())?;
                // URL host setters require brackets around IPv6 literals;
                // accept either bracketed or bare IPv6 from the UI.
                let host = proxy.host.trim();
                let host = if host.parse::<std::net::Ipv6Addr>().is_ok() {
                    format!("[{host}]")
                } else {
                    host.to_string()
                };
                url.set_host(Some(&host))
                    .map_err(|_| "Invalid SOCKS5 proxy host".to_string())?;
                url.set_port(Some(proxy.port))
                    .map_err(|_| "Invalid SOCKS5 proxy port".to_string())?;
                if !proxy.username.is_empty() {
                    url.set_username(&proxy.username)
                        .map_err(|_| "Invalid SOCKS5 proxy username".to_string())?;
                    url.set_password(Some(&proxy.password))
                        .map_err(|_| "Invalid SOCKS5 proxy password".to_string())?;
                }
                Ok(Some(url.to_string()))
            }
            "http" | "https" => {
                let guard = self
                    .bridge_handle
                    .lock()
                    .map_err(|_| "Proxy bridge lock is unavailable".to_string())?;
                match &*guard {
                    Some((port, _)) if *port != 0 => Ok(Some(format!("socks5://127.0.0.1:{port}"))),
                    _ => Err("Configured proxy bridge is not running".into()),
                }
            }
            _ => Err("Unsupported proxy type".into()),
        }
    }

    pub async fn start_http_bridge(&self) -> Result<(), String> {
        self.stop_http_bridge();

        let (enabled, proxy_type, host, port, scheme, user, pass) = {
            let proxy = self.proxy.read().unwrap();
            (
                proxy.enabled,
                proxy.proxy_type.clone(),
                proxy.host.clone(),
                proxy.port,
                proxy.proxy_type.clone(),
                proxy.username.clone(),
                proxy.password.clone(),
            )
        };

        if !enabled || host.is_empty() || (proxy_type != "http" && proxy_type != "https") {
            return Ok(());
        }

        match crate::socks5_bridge::start_bridge(host, port, scheme, user, pass).await {
            Ok((local_port, handle)) => {
                let mut guard = self.bridge_handle.lock().unwrap();
                *guard = Some((local_port, handle));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn stop_http_bridge(&self) {
        let mut guard = self.bridge_handle.lock().unwrap();
        if let Some((_, handle)) = guard.take() {
            handle.abort();
            log::info!("SOCKS5 bridge stopped.");
        }
    }

    // ── Helpers that return effective values ────────────────

    /// Network connect timeout in seconds. Default 5s, multiplied when VPN mode on.
    pub fn connect_timeout_secs(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            5 * vpn.timeout_multiplier as u64
        } else {
            5
        }
    }

    /// Network read/write timeout in seconds. Default 10s, multiplied when VPN mode on.
    pub fn rw_timeout_secs(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            10 * vpn.timeout_multiplier as u64
        } else {
            10
        }
    }

    /// Transient retries apply on ordinary networks as well as VPN connections.
    pub fn retry_attempts(&self) -> u32 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.retry_attempts
        } else {
            3
        }
    }

    /// Base backoff duration in milliseconds for retries.
    pub fn retry_base_backoff_ms(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.retry_base_backoff_ms
        } else {
            1000
        }
    }

    /// Max backoff duration in milliseconds for retries.
    pub fn retry_max_backoff_ms(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.retry_max_backoff_ms
        } else {
            30000
        }
    }

    /// Whether to automatically sleep on FLOOD_WAIT errors.
    pub fn should_respect_flood_wait(&self) -> bool {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.flood_wait_respect
        } else {
            true
        }
    }

    /// Peer cache size. Default 500.
    pub fn peer_cache_size(&self) -> usize {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.peer_cache_size
        } else {
            500
        }
    }

    /// Whether proxy is active and has a valid host.
    pub fn is_proxy_active(&self) -> bool {
        let proxy = self.proxy.read().unwrap();
        proxy.enabled && !proxy.host.is_empty()
    }

    /// Get proxy address as "host:port" if active.
    pub fn proxy_addr(&self) -> Option<String> {
        let proxy = self.proxy.read().unwrap();
        if proxy.enabled && !proxy.host.is_empty() {
            Some(format!("{}:{}", proxy.host, proxy.port))
        } else {
            None
        }
    }

    /// Upload bandwidth limit in bytes/sec. 0 = unlimited.
    pub fn upload_limit_bytes_per_sec(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled && vpn.bandwidth_limit_up_kbs > 0 {
            vpn.bandwidth_limit_up_kbs as u64 * 1024
        } else {
            0 // unlimited
        }
    }

    /// Download bandwidth limit in bytes/sec. 0 = unlimited.
    pub fn download_limit_bytes_per_sec(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled && vpn.bandwidth_limit_down_kbs > 0 {
            vpn.bandwidth_limit_down_kbs as u64 * 1024
        } else {
            0 // unlimited
        }
    }

    /// Chunk size in bytes for transfers.
    pub fn chunk_size_bytes(&self) -> usize {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            (vpn.chunk_size_kb as usize) * 1024
        } else {
            512 * 1024 // default 512KB
        }
    }

    /// Keep-alive ping interval in seconds. 0 = disabled.
    pub fn keep_alive_interval_sec(&self) -> u32 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.keep_alive_interval_sec
        } else {
            0
        }
    }

    /// Maximum total uncompressed bytes for a single bulk archive (API).
    /// 0 = unlimited.
    pub fn archive_max_bytes(&self) -> u64 {
        let vpn = self.vpn.read().unwrap();
        if vpn.enabled {
            vpn.archive_max_bytes // 0 = unlimited when VPN is on
        } else {
            256 * 1024 * 1024 // default 256 MiB when VPN off
        }
    }
}

/// Compute exponential backoff with jitter for a given attempt.
/// Returns duration in milliseconds.
pub fn backoff_ms(attempt: u32, base_ms: u64, max_ms: u64) -> u64 {
    let exp = base_ms.saturating_mul(1u64 << attempt.min(10));
    let capped = exp.min(max_ms);
    // Add ~25% jitter
    let jitter = (capped as f64 * 0.25 * rand::random::<f64>()) as u64;
    capped + jitter
}

fn settings_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("network_settings.json"))
}

pub fn load_network_config(app: &tauri::AppHandle) -> NetworkConfigSnapshot {
    let path = match settings_path(app) {
        Ok(p) => p,
        Err(_) => {
            return NetworkConfigSnapshot {
                proxy: ProxyConfig::default(),
                vpn: VpnConfig::default(),
            }
        }
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|_| NetworkConfigSnapshot {
            proxy: ProxyConfig::default(),
            vpn: VpnConfig::default(),
        }),
        Err(_) => NetworkConfigSnapshot {
            proxy: ProxyConfig::default(),
            vpn: VpnConfig::default(),
        },
    }
}

pub fn save_network_config(
    app: &tauri::AppHandle,
    config: &NetworkConfigSnapshot,
) -> Result<(), String> {
    let path = settings_path(app)?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod transfer_policy_tests {
    use super::*;

    #[test]
    fn network_config_default_preserves_new_initialization() {
        let from_default = NetworkConfig::default();
        let from_new = NetworkConfig::new();
        assert_eq!(from_default.retry_attempts(), from_new.retry_attempts());
        assert_eq!(
            from_default.should_respect_flood_wait(),
            from_new.should_respect_flood_wait()
        );
        assert!(from_default.bridge_handle.lock().unwrap().is_none());
    }

    #[test]
    fn proxy_urls_support_ipv4_ipv6_dns_and_encoded_credentials() {
        let config = NetworkConfig::new();
        {
            let mut proxy = config.proxy.write().unwrap();
            proxy.enabled = true;
            proxy.proxy_type = "socks5".into();
            proxy.host = "::1".into();
            proxy.port = 1080;
            proxy.username = ["synthetic", "user"].join("-");
            proxy.password = ["not", "a", "credential"].join("-");
        }
        let result = config.effective_proxy_url_checked().unwrap().unwrap();
        let url = reqwest::Url::parse(&result).unwrap();
        assert_eq!(url.host_str(), Some("[::1]"));
        assert_eq!(url.port(), Some(1080));
        assert_eq!(url.username(), "synthetic-user");
        assert_eq!(url.password(), Some("not-a-credential"));
        assert_eq!(url.scheme(), "socks5");

        config.proxy.write().unwrap().host = "[2001:db8::abcd]".into();
        let url = config.effective_proxy_url_checked().unwrap().unwrap();
        assert_eq!(
            reqwest::Url::parse(&url).unwrap().host_str(),
            Some("[2001:db8::abcd]")
        );

        config.proxy.write().unwrap().host = "localhost".into();
        let url = config.effective_proxy_url_checked().unwrap().unwrap();
        assert_eq!(
            reqwest::Url::parse(&url).unwrap().host_str(),
            Some("localhost")
        );

        config.proxy.write().unwrap().host = "192.0.2.55".into();
        let url = config.effective_proxy_url_checked().unwrap().unwrap();
        assert_eq!(
            reqwest::Url::parse(&url).unwrap().host_str(),
            Some("192.0.2.55")
        );
    }

    #[test]
    fn enabled_proxy_configuration_fails_closed_instead_of_direct_fallback() {
        let config = NetworkConfig::new();
        assert!(config.effective_proxy_url_checked().unwrap().is_none());
        let invalid = [
            ("", 1080, "socks5"),
            ("example.test", 0, "socks5"),
            ("bad host", 1080, "socks5"),
            ("bad\nname", 1080, "socks5"),
            ("[::1", 1080, "socks5"),
            ("example.test", 1080, "unknown"),
            ("example.test", 1080, "http"),
        ];
        for (host, port, kind) in invalid {
            let mut proxy = config.proxy.write().unwrap();
            proxy.enabled = true;
            proxy.host = host.into();
            proxy.port = port;
            proxy.proxy_type = kind.into();
            proxy.username.clear();
            proxy.password.clear();
            drop(proxy);
            assert!(
                config.effective_proxy_url_checked().is_err(),
                "enabled {kind:?} proxy {host:?} must not silently go direct"
            );
        }
        {
            let mut proxy = config.proxy.write().unwrap();
            proxy.enabled = true;
            proxy.proxy_type = "socks5".into();
            proxy.host = "proxy.example".into();
            proxy.port = 1080;
            proxy.username.clear();
            proxy.password = ["synthetic", "password"].join("-");
        }
        assert!(config.effective_proxy_url_checked().is_err());
    }

    #[test]
    fn ordinary_network_transfers_retry_and_respect_telegram_waits() {
        let config = NetworkConfig::new();
        assert_eq!(config.retry_attempts(), 3);
        assert!(config.should_respect_flood_wait());
        config.vpn.write().unwrap().enabled = true;
        config.vpn.write().unwrap().retry_attempts = 1;
        config.vpn.write().unwrap().flood_wait_respect = false;
        assert_eq!(config.retry_attempts(), 1);
        assert!(!config.should_respect_flood_wait());
    }
}
