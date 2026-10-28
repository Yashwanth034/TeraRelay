use crate::vpn_optimizer::NetworkConfig;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;
use tauri::State;

/// Test whether Telegram MTProto traffic can pass through the configured proxy.
/// Unlike cmd_is_network_available (which only TCP-pings the proxy host:port),
/// this creates a temporary grammers session and attempts a real API call.
/// Returns true only if the Telegram API responds successfully through the proxy.
#[derive(Debug, serde::Serialize)]
pub struct ProxyStatus {
    pub reachable: bool,
    pub latency_ms: i64,
}

#[tauri::command]
pub async fn cmd_get_proxy_status(
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<ProxyStatus, String> {
    let proxy = net_config.proxy.read().map_err(|e| e.to_string())?.clone();
    if !proxy.enabled {
        return Ok(ProxyStatus {
            reachable: false,
            latency_ms: -1,
        });
    }
    crate::vpn_optimizer::validate_enabled_proxy(&proxy)?;
    // Tuple-based ToSocketAddrs accepts unbracketed IPv6 literals on every
    // platform, unlike formatting host:port as an ambiguous string.
    let endpoint = (
        proxy
            .host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        proxy.port,
    );
    tokio::task::spawn_blocking(move || {
        let timeout = Duration::from_secs(3);
        let start = std::time::Instant::now();

        let addrs = match std::net::ToSocketAddrs::to_socket_addrs(&endpoint) {
            Ok(iter) => iter.collect::<Vec<_>>(),
            Err(_) => {
                return Ok(ProxyStatus {
                    reachable: false,
                    latency_ms: -1,
                })
            }
        };

        for addr in addrs {
            if TcpStream::connect_timeout(&addr, timeout).is_ok() {
                let latency = start.elapsed().as_millis() as i64;
                return Ok(ProxyStatus {
                    reachable: true,
                    latency_ms: latency,
                });
            }
        }

        Ok(ProxyStatus {
            reachable: false,
            latency_ms: -1,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Owns a fresh, test-only authentication session directory. Dropping it
/// removes only this attempt's random path, including SQLite -wal/-shm files.
/// Never reuse or delete the user's real Telegram session or a shared test DB.
struct OwnedProxyTestDirectory(PathBuf);

impl OwnedProxyTestDirectory {
    fn create() -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "terarelay-proxy-check-{:032x}",
            rand::random::<u128>()
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&path).map_err(|_| {
                "Could not create a private proxy-test session directory".to_string()
            })?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir(&path)
            .map_err(|_| "Could not create a private proxy-test session directory".to_string())?;
        Ok(Self(path))
    }
}

impl Drop for OwnedProxyTestDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            log::warn!("Could not clean up a private proxy-test session: {error}");
        }
    }
}

/// Test whether Telegram MTProto traffic can pass through the configured proxy.
/// Unlike cmd_is_network_available (which only TCP-pings the proxy host:port),
/// this creates a temporary grammers session and attempts a real API call.
/// Returns true only if the Telegram API responds successfully through the proxy.
#[tauri::command]
pub async fn cmd_test_proxy_traffic(
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<bool, String> {
    let proxy_url = match net_config.effective_proxy_url_checked()? {
        Some(url) => url,
        None => return Ok(false),
    };

    // Keep this diagnostic on the same fail-closed OS-resolved SOCKS5 path
    // as actual Telegram login. Never log the URL: it can carry credentials.
    let proxy_url = crate::commands::auth::resolve_telegram_proxy_url(proxy_url).await?;
    log::info!("Testing configured Telegram proxy traffic");

    let result: Result<bool, String> = tokio::task::spawn_blocking(move || {
        // Each attempt owns a new restricted directory, never the shared
        // historical test.session path (which could contain another session).
        let owned_dir = OwnedProxyTestDirectory::create()?;
        let session_path = owned_dir.0.join("test.session");
        let session_path_str = session_path.to_string_lossy().to_string();
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        rt.block_on(async {
            // Open a fresh in-memory-ish SQLite session
            let session = grammers_session::storages::SqliteSession::open(&session_path_str)
                .map_err(|e| format!("Failed to open test session: {}", e))?;
            let session = std::sync::Arc::new(session);

            // Build connection params with proxy
            let conn_params = grammers_mtsender::ConnectionParams {
                proxy_url: Some(proxy_url),
                ..Default::default()
            };

            let pool = grammers_mtsender::SenderPool::with_configuration(
                session.clone(),
                // Use a hardcoded test app ID (telegram.org test credentials won't work,
                // but with a real session the API ID doesn't matter for get_me).
                // We just need the transport to work. Use API ID 0 as a sentinel —
                // grammers will still establish the TCP+TLS tunnel.
                // NOTE: grammers requires a non-zero API ID for connection params.
                // We use a minimal valid value; the session will still try to connect.
                12345,
                conn_params,
            );

            let client = grammers_client::Client::new(&pool);

            // Spawn the runner briefly
            let grammers_mtsender::SenderPool { runner, .. } = pool;
            let runner_handle = tokio::spawn(async move {
                runner.run().await;
            });

            // Try get_me() with a timeout
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), client.get_me()).await;

            // Abort runner regardless of outcome
            runner_handle.abort();

            // The owned private directory is removed by RAII on all return
            // paths, including session-open failure, timeout and API failure.
            match result {
                Ok(Ok(_me)) => Ok(true),
                Ok(Err(e)) => {
                    log::warn!("Proxy traffic test failed (API error): {}", e);
                    Ok(false)
                }
                Err(_timeout) => {
                    log::warn!("Proxy traffic test timed out after 10s");
                    Ok(false)
                }
            }
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    result
}

/// Telegram DC addresses for connectivity checks and fallback
const DC_ADDRESSES: &[&str] = &[
    "149.154.167.50:443", // DC2
    "149.154.175.53:443", // DC1
    "149.154.167.51:443", // DC3
    "149.154.167.91:443", // DC4
    "91.108.56.130:443",  // DC5
];

/// Network availability check that respects VPN optimizer settings.
///
/// - Uses the configured timeout multiplier when VPN mode is on
/// - When proxy is active, checks proxy reachability instead
/// - Tries multiple DCs when VPN fallback is enabled
#[tauri::command]
pub async fn cmd_is_network_available(
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<bool, String> {
    let timeout_secs = net_config.connect_timeout_secs();
    let is_proxy = net_config.is_proxy_active();
    let proxy_addr = net_config.proxy_addr();
    let dc_attempts = {
        let vpn = net_config.vpn.read().map_err(|e| e.to_string())?;
        if vpn.enabled {
            vpn.dc_fallback_attempts as usize
        } else {
            1
        }
    };

    tokio::task::spawn_blocking(move || {
        let timeout = Duration::from_secs(timeout_secs);

        // If proxy is active, check proxy reachability
        if is_proxy {
            if let Some(addr) = &proxy_addr {
                if let Ok(sock_addr) = addr.parse() {
                    return match TcpStream::connect_timeout(&sock_addr, timeout) {
                        Ok(_) => Ok(true),
                        Err(_) => Ok(false),
                    };
                }
            }
            return Ok(false);
        }

        // Try DCs (up to dc_attempts when VPN mode is on)
        let attempts = dc_attempts.min(DC_ADDRESSES.len());
        for dc in &DC_ADDRESSES[..attempts] {
            if let Ok(addr) = dc.parse() {
                if TcpStream::connect_timeout(&addr, timeout).is_ok() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Measure TCP connect latency to the best Telegram DC.
/// Returns latency in milliseconds, or -1 if unreachable.
#[tauri::command]
pub async fn cmd_check_latency(
    net_config: State<'_, std::sync::Arc<NetworkConfig>>,
) -> Result<i64, String> {
    let timeout_secs = net_config.connect_timeout_secs();
    let is_proxy = net_config.is_proxy_active();
    let proxy_addr = net_config.proxy_addr();

    tokio::task::spawn_blocking(move || {
        let timeout = Duration::from_secs(timeout_secs);

        // Target: proxy if active, else DC2
        let target: String = if is_proxy {
            proxy_addr.unwrap_or_else(|| DC_ADDRESSES[0].to_string())
        } else {
            DC_ADDRESSES[0].to_string()
        };

        let addr = match target.parse() {
            Ok(a) => a,
            Err(_) => return Ok(-1i64),
        };

        let start = std::time::Instant::now();
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => {
                let ms = start.elapsed().as_millis() as i64;
                Ok(ms)
            }
            Err(_) => Ok(-1i64),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Detect VPN network interfaces on the system.
/// Returns true if common VPN interfaces (tun, utun, wg, ppp, tap) are found.
#[tauri::command]
pub async fn cmd_detect_vpn() -> Result<bool, String> {
    tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "macos")]
        {
            // macOS: check for utun/tun/wg/ppp/tap/ipsec interfaces via ifconfig
            match std::process::Command::new("ifconfig").arg("-l").output() {
                Ok(output) => {
                    let ifaces = String::from_utf8_lossy(&output.stdout);
                    let vpn_prefixes = ["utun", "tun", "wg", "ppp", "tap", "ipsec"];
                    let found = ifaces
                        .split_whitespace()
                        .any(|iface| vpn_prefixes.iter().any(|prefix| iface.starts_with(prefix)));
                    Ok(found)
                }
                Err(_) => Ok(false),
            }
        }

        #[cfg(target_os = "linux")]
        {
            // Linux: inspect /sys/class/net to find interface names without executing shell commands
            if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
                let vpn_prefixes = ["tun", "tap", "wg", "ppp", "utun", "ipsec"];
                let mut found = false;
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if vpn_prefixes.iter().any(|prefix| name.starts_with(prefix)) {
                            found = true;
                            break;
                        }
                    }
                }
                Ok(found)
            } else {
                Ok(false)
            }
        }

        #[cfg(target_os = "windows")]
        {
            // Windows: run ipconfig and check output for common VPN adapter keywords
            match std::process::Command::new("ipconfig").output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
                    let vpn_keywords = [
                        "tap-windows",
                        "tunnel",
                        "wireguard",
                        "openvpn",
                        "fortinet",
                        "cisco",
                        "tailscale",
                        "zerotier",
                        "ipsec",
                    ];
                    let found = vpn_keywords.iter().any(|kw| stdout.contains(kw));
                    Ok(found)
                }
                Err(_) => Ok(false),
            }
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            // Fallback for other systems
            Ok(false)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod proxy_test_isolation_tests {
    use super::*;

    #[test]
    fn proxy_diagnostics_use_distinct_owned_session_directories_and_clean_up() {
        let first = OwnedProxyTestDirectory::create().unwrap();
        let second = OwnedProxyTestDirectory::create().unwrap();
        assert_ne!(first.0, second.0);
        assert!(first.0.is_dir());
        assert!(second.0.is_dir());
        let first_path = first.0.clone();
        let second_path = second.0.clone();
        let marker = first.0.join("test.session");
        std::fs::write(&marker, b"synthetic-ephemeral-test").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&first.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&second.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        drop(first);
        assert!(
            !first_path.exists(),
            "cleanup must include the owned SQLite fixture"
        );
        assert!(
            second_path.is_dir(),
            "cleanup must not touch a concurrent attempt"
        );
        drop(second);
        assert!(!second_path.exists());
    }
}
