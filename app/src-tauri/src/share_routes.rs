use crate::commands::utils::resolve_peer;
use crate::commands::TelegramState;
use crate::db::DbConnection;
use actix_web::{cookie::Cookie, get, post, web, HttpRequest, HttpResponse, Responder};
use grammers_client::types::Media;
use rand::Rng;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

const SHARE_AUTH_TTL_SECONDS: i64 = 30 * 60;
const MAX_SHARE_AUTH_SESSIONS: usize = 4096;
const SHARE_VERIFY_WINDOW_SECONDS: i64 = 60;
const SHARE_VERIFY_MAX_ATTEMPTS: u32 = 8;
const MAX_SHARE_VERIFY_WINDOWS: usize = 4096;

struct ShareVerifyWindow {
    started_at: i64,
    attempts: u32,
}

static SHARE_VERIFY_WINDOWS: OnceLock<Mutex<HashMap<String, ShareVerifyWindow>>> = OnceLock::new();

fn share_verify_windows() -> &'static Mutex<HashMap<String, ShareVerifyWindow>> {
    SHARE_VERIFY_WINDOWS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ShareAuthSession {
    token: String,
    password_hash: String,
    expires_at: i64,
}

static SHARE_AUTH_SESSIONS: OnceLock<Mutex<HashMap<String, ShareAuthSession>>> = OnceLock::new();

fn share_auth_sessions() -> &'static Mutex<HashMap<String, ShareAuthSession>> {
    SHARE_AUTH_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reserve a per-share attempt before executing expensive bcrypt verification.
/// An immediate synchronous counter prevents simultaneous requests bypassing
/// the limit while bcrypt runs. Errors fail closed rather than skipping it.
fn admit_share_verification(token: &str, now: i64) -> Result<Option<i64>, String> {
    let mut windows = share_verify_windows()
        .lock()
        .map_err(|_| "Share verification rate limit is unavailable".to_string())?;
    Ok(admit_share_verification_in(&mut windows, token, now))
}

fn admit_share_verification_in(
    windows: &mut HashMap<String, ShareVerifyWindow>,
    token: &str,
    now: i64,
) -> Option<i64> {
    windows.retain(|_, window| {
        now >= window.started_at
            && now.saturating_sub(window.started_at) < SHARE_VERIFY_WINDOW_SECONDS
    });

    if let Some(window) = windows.get_mut(token) {
        if window.attempts >= SHARE_VERIFY_MAX_ATTEMPTS {
            return Some(SHARE_VERIFY_WINDOW_SECONDS - now.saturating_sub(window.started_at));
        }
        window.attempts += 1;
        return None;
    }

    if windows.len() >= MAX_SHARE_VERIFY_WINDOWS {
        // Bound memory even when many independent share links are probed.
        if let Some(oldest) = windows
            .iter()
            .min_by_key(|(_, window)| window.started_at)
            .map(|(key, _)| key.clone())
        {
            windows.remove(&oldest);
        }
    }
    windows.insert(
        token.to_owned(),
        ShareVerifyWindow {
            started_at: now,
            attempts: 1,
        },
    );
    None
}

#[derive(Clone)]
struct SharedLinkRow {
    _id: String,
    folder_id: Option<i64>,
    message_id: i32,
    file_name: String,
    _file_size: i64,
    password_hash: Option<String>,
    _password_salt: Option<String>,
    expires_at: Option<i64>,
    revoked: bool,
}

#[derive(Deserialize)]
struct VerifyForm {
    password: String,
}

/// Verify a password against a bcrypt hash.
fn verify_password(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

/// Issue an unpredictable bearer token and enforce its TTL on the server.
/// Browser Max-Age alone cannot prevent replay of a copied expired cookie.
fn issue_share_auth_session(token: &str, password_hash: &str, now: i64) -> Result<String, String> {
    let expires_at = now
        .checked_add(SHARE_AUTH_TTL_SECONDS)
        .ok_or_else(|| "Share authentication expiry is out of range".to_string())?;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let session_key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut sessions = share_auth_sessions()
        .lock()
        .map_err(|_| "Share authentication store is unavailable".to_string())?;
    sessions.retain(|_, session| session.expires_at > now);
    if sessions.len() >= MAX_SHARE_AUTH_SESSIONS {
        // Bound memory even if many valid share links are verified.
        let oldest = sessions
            .iter()
            .min_by_key(|(_, session)| session.expires_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(
        session_key.clone(),
        ShareAuthSession {
            token: token.to_owned(),
            password_hash: password_hash.to_owned(),
            expires_at,
        },
    );
    Ok(session_key)
}

fn share_auth_session_valid(token: &str, password_hash: &str, key: &str, now: i64) -> bool {
    let Ok(sessions) = share_auth_sessions().lock() else {
        return false;
    };
    sessions.get(key).is_some_and(|session| {
        session.expires_at > now && session.token == token && session.password_hash == password_hash
    })
}

/// Encode untrusted names and token text before inserting them into HTML.
fn escape_share_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn share_has_expired(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|expiry| expiry <= now)
}

/// Share downloads may contain sensitive data even when the link is not
/// password protected. Previously retrieved content must not remain cacheable
/// after a share has expired or been revoked.
fn share_download_headers() -> Vec<(&'static str, String)> {
    vec![
        ("Cache-Control", "no-store".to_string()),
        ("X-Content-Type-Options", "nosniff".to_string()),
        ("Referrer-Policy", "no-referrer".to_string()),
    ]
}

fn get_share_by_token(db: &DbConnection, token: &str) -> Result<Option<SharedLinkRow>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, folder_id, message_id, file_name, file_size, password_hash, password_salt, expires_at, revoked
             FROM shared_links WHERE id = ?"
        )
        .map_err(|e| e.to_string())?;

    stmt.bind((1, token)).map_err(|e| e.to_string())?;

    if let sqlite::State::Row = stmt.next().map_err(|e| e.to_string())? {
        let id = stmt.read::<String, _>("id").map_err(|e| e.to_string())?;
        let folder_id = match stmt
            .read::<sqlite::Value, _>("folder_id")
            .map_err(|e| format!("Invalid share folder ID: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::Integer(id) => Some(id),
            _ => return Err("Invalid share folder ID type".to_string()),
        };
        let message_id = match stmt
            .read::<sqlite::Value, _>("message_id")
            .map_err(|e| format!("Invalid share message ID: {e}"))?
        {
            sqlite::Value::Integer(id) => {
                i32::try_from(id).map_err(|_| "Invalid share message ID range".to_string())?
            }
            _ => return Err("Invalid share message ID type".to_string()),
        };
        let file_name = stmt
            .read::<String, _>("file_name")
            .map_err(|e| e.to_string())?;
        let file_size = stmt
            .read::<i64, _>("file_size")
            .map_err(|e| e.to_string())?;
        // Never silently convert corrupt protection/expiry metadata into None:
        // None explicitly means an unprotected or non-expiring share.
        let password_hash = match stmt
            .read::<sqlite::Value, _>("password_hash")
            .map_err(|e| format!("Invalid share password metadata: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::String(hash) => Some(hash),
            _ => return Err("Invalid share password metadata type".to_string()),
        };
        let _password_salt = match stmt
            .read::<sqlite::Value, _>("password_salt")
            .map_err(|e| format!("Invalid share password salt metadata: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::String(salt) => Some(salt),
            _ => return Err("Invalid share password salt metadata type".to_string()),
        };
        let expires_at = match stmt
            .read::<sqlite::Value, _>("expires_at")
            .map_err(|e| format!("Invalid share expiration metadata: {e}"))?
        {
            sqlite::Value::Null => None,
            sqlite::Value::Integer(expires_at) => Some(expires_at),
            _ => return Err("Invalid share expiration metadata type".to_string()),
        };
        let revoked = match stmt
            .read::<sqlite::Value, _>("revoked")
            .map_err(|e| format!("Invalid share revocation state: {e}"))?
        {
            sqlite::Value::Integer(0) => false,
            sqlite::Value::Integer(1) => true,
            _ => return Err("Invalid share revocation state type/value".to_string()),
        };

        Ok(Some(SharedLinkRow {
            _id: id,
            folder_id,
            message_id,
            file_name,
            _file_size: file_size,
            password_hash,
            _password_salt,
            expires_at,
            revoked,
        }))
    } else {
        Ok(None)
    }
}

/// Renders the password entry form for protected share links.
///
/// The local Actix HTTP page is separate from the Tauri WebView. Its own
/// restrictive CSP permits only the page's inline CSS, not inline scripts.
/// File names, tokens and error text must all be HTML-escaped regardless of
/// the loopback binding: they are untrusted values embedded into HTML.
fn render_password_form(file_name: &str, token: &str, error: Option<&str>) -> HttpResponse {
    let error_html = match error {
        Some(err) => format!("<div class=\"error\">{}</div>", escape_share_html(err)),
        None => String::new(),
    };
    let safe_file_name = escape_share_html(file_name);
    let safe_token = escape_share_html(token);

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Password Protected File - TeraRelay</title>
    <style>
        body {{
            background-color: #182533;
            color: #ffffff;
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            align-items: center;
            justify-content: center;
            height: 100vh;
            margin: 0;
        }}
        .container {{
            background: #202b36;
            padding: 2rem;
            border-radius: 12px;
            box-shadow: 0 8px 24px rgba(0, 0, 0, 0.2);
            border: 1px solid #2f3e4e;
            width: 100%;
            max-width: 400px;
            text-align: center;
        }}
        h2 {{
            margin-top: 0;
            color: #40a7e3;
        }}
        p {{
            font-size: 14px;
            color: #7f91a4;
            margin-bottom: 20px;
        }}
        input[type="password"] {{
            width: 100%;
            padding: 12px;
            border-radius: 6px;
            border: 1px solid #2f3e4e;
            background: #182533;
            color: white;
            box-sizing: border-box;
            margin-bottom: 15px;
            font-size: 16px;
        }}
        input[type="password"]:focus {{
            outline: none;
            border-color: #40a7e3;
        }}
        button {{
            width: 100%;
            padding: 12px;
            border-radius: 6px;
            border: none;
            background: #40a7e3;
            color: white;
            font-weight: bold;
            cursor: pointer;
            font-size: 16px;
            transition: background 0.2s;
        }}
        button:hover {{
            background: #3598d1;
        }}
        .error {{
            color: #ff5e5e;
            font-size: 14px;
            margin-bottom: 15px;
        }}
    </style>
</head>
<body>
    <div class="container">
        <h2>Enter Password</h2>
        <p>This share link is password-protected.<br>File: <strong>{}</strong></p>
        {}
        <form method="POST" action="/d/{}/verify">
            <input type="password" name="password" placeholder="Password" autofocus required>
            <button type="submit">Verify & Download</button>
        </form>
    </div>
</body>
</html>"#,
        safe_file_name, error_html, safe_token
    );

    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .insert_header(("X-Content-Type-Options", "nosniff"))
        .insert_header(("Referrer-Policy", "no-referrer"))
        .insert_header((
            "Content-Security-Policy",
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ))
        .content_type("text/html; charset=utf-8")
        .body(html)
}

#[get("/d/{token}")]
async fn get_shared_file(
    req: HttpRequest,
    path: web::Path<String>,
    db_conn: web::Data<DbConnection>,
    tg_state: web::Data<Arc<TelegramState>>,
) -> impl Responder {
    let token = path.into_inner();

    let row = match get_share_by_token(&db_conn, &token) {
        Ok(Some(r)) => r,
        Ok(None) => return HttpResponse::NotFound().body("Shared link not found"),
        Err(e) => {
            log::error!("DB error resolving token {}: {}", token, e);
            return HttpResponse::InternalServerError().body("Internal server error");
        }
    };

    // Check validation (revocation and expiration)
    if row.revoked {
        return HttpResponse::NotFound().body("This shared link has been revoked");
    }

    if share_has_expired(row.expires_at, chrono::Utc::now().timestamp()) {
        return HttpResponse::Gone().body("This shared link has expired");
    }

    // Check password protection
    if let Some(hash) = &row.password_hash {
        let mut authenticated = false;
        if let Some(cookie) = req.cookie(&format!("share_auth_{}", token)) {
            if share_auth_session_valid(
                &token,
                hash,
                cookie.value(),
                chrono::Utc::now().timestamp(),
            ) {
                authenticated = true;
            }
        }

        if !authenticated {
            return render_password_form(&row.file_name, &token, None);
        }
    }

    // Retrieve and stream the file from Telegram
    let client_opt = { tg_state.client.lock().await.clone() };
    let client = match client_opt {
        Some(c) => c,
        None => return HttpResponse::ServiceUnavailable().body("Telegram client is not connected"),
    };

    let peer = match resolve_peer(&client, row.folder_id, &tg_state.peer_cache).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("Failed to resolve peer for share: {}", e);
            return HttpResponse::InternalServerError().body("Failed to locate folder");
        }
    };

    match client.get_messages_by_id(peer, &[row.message_id]).await {
        Ok(messages) => {
            if let Some(Some(msg)) = messages.first() {
                if let Some(media) = msg.media() {
                    let mime = match &media {
                        Media::Document(d) => d
                            .mime_type()
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                        Media::Photo(_) => "image/jpeg".to_string(),
                        _ => "application/octet-stream".to_string(),
                    };
                    let filename = &row.file_name;

                    return crate::server::build_media_response(
                        &client,
                        &media,
                        &req,
                        &mime,
                        Some(filename),
                        crate::server::StreamingExtras {
                            extra_headers: share_download_headers(),
                            log_label: "Share download",
                        },
                    );
                }
            }
            HttpResponse::NotFound().body("Message or media not found in Telegram")
        }
        Err(e) => {
            log::error!("Failed to fetch shared message {}: {}", row.message_id, e);
            HttpResponse::InternalServerError().body(format!("Failed to retrieve file: {}", e))
        }
    }
}

#[post("/d/{token}/verify")]
async fn verify_shared_file_password(
    path: web::Path<String>,
    form: web::Form<VerifyForm>,
    db_conn: web::Data<DbConnection>,
) -> impl Responder {
    let token = path.into_inner();

    let row = match get_share_by_token(&db_conn, &token) {
        Ok(Some(r)) => r,
        Ok(None) => return HttpResponse::NotFound().body("Shared link not found"),
        Err(e) => {
            log::error!("DB error resolving token {}: {}", token, e);
            return HttpResponse::InternalServerError().body("Internal server error");
        }
    };

    if row.revoked {
        return HttpResponse::NotFound().body("This shared link has been revoked");
    }

    // The verification endpoint must enforce the same expiry as downloads.
    if share_has_expired(row.expires_at, chrono::Utc::now().timestamp()) {
        return HttpResponse::Gone().body("This shared link has expired");
    }

    let hash = match &row.password_hash {
        Some(h) => h,
        None => return HttpResponse::BadRequest().body("No password required for this link"),
    };

    match admit_share_verification(&token, chrono::Utc::now().timestamp()) {
        Ok(Some(retry_after)) => {
            return HttpResponse::TooManyRequests()
                .insert_header(("Retry-After", retry_after.to_string()))
                .body("Too many verification attempts. Please retry later.");
        }
        Ok(None) => {}
        Err(err) => {
            log::error!("Unable to enforce share verification rate limit: {err}");
            return HttpResponse::InternalServerError().body("Internal server error");
        }
    }

    if verify_password(&form.password, hash) {
        // Issue a random per-verification session. Its expiry is checked
        // server-side, not merely by the browser's cookie Max-Age.
        // The loopback-only plain HTTP server cannot use Secure cookies.
        let val = match issue_share_auth_session(&token, hash, chrono::Utc::now().timestamp()) {
            Ok(val) => val,
            Err(err) => {
                log::error!("Unable to issue share authentication session: {err}");
                return HttpResponse::InternalServerError().body("Internal server error");
            }
        };
        let cookie = Cookie::build(format!("share_auth_{}", token), val)
            .path(format!("/d/{}", token))
            .http_only(true)
            .same_site(actix_web::cookie::SameSite::Strict)
            .max_age(actix_web::cookie::time::Duration::seconds(
                SHARE_AUTH_TTL_SECONDS,
            ))
            .finish();

        HttpResponse::Found()
            .insert_header(("Location", format!("/d/{}", token)))
            .cookie(cookie)
            .finish()
    } else {
        render_password_form(
            &row.file_name,
            &token,
            Some("Incorrect password. Please try again."),
        )
    }
}

pub fn configure_share_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(get_shared_file)
        .service(verify_shared_file_password);
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as web_test;
    use actix_web::{body::to_bytes, http::StatusCode, App};
    use std::sync::Mutex;

    #[actix_web::test]
    async fn password_form_escapes_untrusted_values_and_disables_active_content() {
        let response = render_password_form(
            "<img src=x onerror=alert(1)> & \"' ",
            "test\" onmouseover=\"alert(1)",
            Some("<script>alert(1)</script>"),
        );
        let headers = response.headers();
        assert_eq!(
            headers.get("Cache-Control").unwrap().to_str().unwrap(),
            "no-store"
        );
        assert_eq!(
            headers
                .get("X-Content-Type-Options")
                .unwrap()
                .to_str()
                .unwrap(),
            "nosniff"
        );
        assert_eq!(
            headers.get("Referrer-Policy").unwrap().to_str().unwrap(),
            "no-referrer"
        );
        let csp = headers
            .get("Content-Security-Policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("form-action 'self'"));
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!csp.contains("script-src 'unsafe-inline'"));

        let bytes = to_bytes(response.into_body()).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("&lt;img src=x onerror=alert(1)&gt; &amp; &quot;&#39;"));
        assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(body.contains("action=\"/d/test&quot; onmouseover=&quot;alert(1)/verify\""));
        assert!(!body.contains("<img src=x"));
        assert!(!body.contains("<script>alert(1)</script>"));
        assert!(!body.contains("onmouseover=\"alert(1)"));
    }

    #[test]
    fn malformed_share_protection_metadata_never_downgrades_to_none() {
        // SQLite is dynamically typed even when columns declare TEXT/INTEGER.
        // Use incompatible BLOB/TEXT values that must never become None or 0.
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY, folder_id INTEGER, message_id INTEGER,
                file_name TEXT, file_size INTEGER, password_hash TEXT,
                password_salt TEXT, expires_at INTEGER, revoked INTEGER
             );
             INSERT INTO shared_links VALUES
                ('qa-metadata', NULL, 1, 'safe.txt', 42, 'hash', NULL, 100, 0);",
        )
        .unwrap();
        let db: DbConnection = Arc::new(Mutex::new(conn));
        let good = get_share_by_token(&db, "qa-metadata").unwrap().unwrap();
        assert_eq!(good.password_hash.as_deref(), Some("hash"));
        assert_eq!(good.expires_at, Some(100));
        assert_eq!(good.folder_id, None);

        for (column, update) in [
            (
                "password_hash",
                "UPDATE shared_links SET password_hash = X'FF'",
            ),
            (
                "password_salt",
                "UPDATE shared_links SET password_salt = X'FF'",
            ),
            (
                "expires_at",
                "UPDATE shared_links SET expires_at = 'invalid-hours'",
            ),
            ("folder_id", "UPDATE shared_links SET folder_id = X'FF'"),
            (
                "message_id",
                "UPDATE shared_links SET message_id = 2147483648",
            ),
            (
                "revoked",
                "UPDATE shared_links SET revoked = 'invalid-revocation'",
            ),
            (
                "message_id_type",
                "UPDATE shared_links SET message_id = X'FF'",
            ),
        ] {
            {
                let locked = db.lock().unwrap();
                locked
                    .execute(
                        "UPDATE shared_links SET folder_id = NULL, message_id = 1,
                         password_hash = 'hash', password_salt = NULL,
                         expires_at = 100, revoked = 0",
                    )
                    .unwrap();
                locked.execute(update).unwrap();
            }
            assert!(
                get_share_by_token(&db, "qa-metadata").is_err(),
                "Corrupt {column} must never be interpreted as no protection"
            );
        }
    }

    #[actix_web::test]
    async fn corrupt_password_metadata_returns_server_error_not_cookie() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY, folder_id INTEGER, message_id INTEGER,
                file_name TEXT, file_size INTEGER, password_hash TEXT,
                password_salt TEXT, expires_at INTEGER, revoked INTEGER
             );
             INSERT INTO shared_links VALUES
                ('qa-corrupt', NULL, 1, 'private.txt', 42, X'FF', NULL, NULL, 0);",
        )
        .unwrap();
        let db: DbConnection = Arc::new(Mutex::new(conn));
        let app = web_test::init_service(
            App::new()
                .app_data(web::Data::new(db))
                .configure(configure_share_routes),
        )
        .await;
        let request = web_test::TestRequest::post()
            .uri("/d/qa-corrupt/verify")
            .set_form([("password", "qa-only-password")])
            .to_request();
        let response = web_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(response.headers().get("Set-Cookie").is_none());
    }

    #[test]
    fn share_download_headers_prevent_cached_protected_content() {
        let headers = share_download_headers();
        assert_eq!(headers.len(), 3);
        for (key, expected) in [
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
        ] {
            assert_eq!(
                headers
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, val)| val.as_str()),
                Some(expected)
            );
        }
    }

    #[test]
    fn share_verify_attempts_limit_retries_and_recover_at_window_boundary() {
        let mut windows = HashMap::new();
        for _ in 0..SHARE_VERIFY_MAX_ATTEMPTS {
            assert_eq!(
                admit_share_verification_in(&mut windows, "qa-attempt-window-a", 1_000),
                None
            );
        }
        assert_eq!(
            admit_share_verification_in(&mut windows, "qa-attempt-window-a", 1_000),
            Some(SHARE_VERIFY_WINDOW_SECONDS)
        );
        assert_eq!(
            admit_share_verification_in(&mut windows, "qa-attempt-window-a", 1_059),
            Some(1)
        );
        assert_eq!(
            admit_share_verification_in(&mut windows, "qa-attempt-window-b", 1_059),
            None,
            "one share's guesses must not consume another share's allowance"
        );
        assert_eq!(
            admit_share_verification_in(&mut windows, "qa-attempt-window-a", 1_060),
            None,
            "the exact sixty-second boundary must admit a new request"
        );
        assert_eq!(
            admit_share_verification_in(&mut windows, "qa-attempt-window-a", 1_000),
            None,
            "a backwards system clock must not permanently lock a share"
        );
    }

    #[test]
    fn share_verify_attempt_windows_have_bounded_memory() {
        let mut windows = HashMap::new();
        for i in 0..=MAX_SHARE_VERIFY_WINDOWS {
            let token = format!("qa-bounded-share-{i}");
            assert_eq!(
                admit_share_verification_in(&mut windows, &token, 1_000),
                None
            );
        }
        assert_eq!(windows.len(), MAX_SHARE_VERIFY_WINDOWS);
    }

    #[actix_web::test]
    async fn share_verify_http_rate_limit_rejects_ninth_attempt_without_cookie() {
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY, folder_id INTEGER, message_id INTEGER,
                file_name TEXT, file_size INTEGER, password_hash TEXT,
                password_salt TEXT, expires_at INTEGER, revoked INTEGER
             );
             INSERT INTO shared_links VALUES
                ('qa-throttle', NULL, 1, 'safe.txt', 42,
                 'invalid-synthetic-bcrypt-hash', NULL, NULL, 0);",
        )
        .unwrap();
        let db: DbConnection = Arc::new(Mutex::new(conn));
        let app = web_test::init_service(
            App::new()
                .app_data(web::Data::new(db))
                .configure(configure_share_routes),
        )
        .await;

        for attempt in 1..=SHARE_VERIFY_MAX_ATTEMPTS + 1 {
            let request = web_test::TestRequest::post()
                .uri("/d/qa-throttle/verify")
                .set_form([("password", "qa-invalid-attempt")])
                .to_request();
            let response = web_test::call_service(&app, request).await;
            assert!(response.headers().get("Set-Cookie").is_none());
            if attempt <= SHARE_VERIFY_MAX_ATTEMPTS {
                assert_eq!(response.status(), StatusCode::OK);
            } else {
                assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                let seconds: i64 = response
                    .headers()
                    .get("Retry-After")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!((1..=SHARE_VERIFY_WINDOW_SECONDS).contains(&seconds));
            }
        }
    }

    #[test]
    fn share_auth_cookie_is_random_scoped_and_expires_on_server() {
        let now = 1_000_i64;
        let first = issue_share_auth_session("qa-link-a", "qa-hash-a", now).unwrap();
        let second = issue_share_auth_session("qa-link-a", "qa-hash-a", now).unwrap();
        assert_ne!(
            first, second,
            "each login must receive a unique bearer token"
        );
        assert_eq!(first.len(), 64, "32 random bytes, hex encoded");
        assert!(share_auth_session_valid(
            "qa-link-a",
            "qa-hash-a",
            &first,
            now
        ));
        assert!(share_auth_session_valid(
            "qa-link-a",
            "qa-hash-a",
            &first,
            now + SHARE_AUTH_TTL_SECONDS - 1,
        ));
        assert!(!share_auth_session_valid(
            "qa-link-a",
            "qa-hash-a",
            &first,
            now + SHARE_AUTH_TTL_SECONDS,
        ));
        assert!(!share_auth_session_valid(
            "qa-link-b",
            "qa-hash-a",
            &first,
            now
        ));
        assert!(!share_auth_session_valid(
            "qa-link-a",
            "qa-hash-b",
            &first,
            now
        ));
        assert!(!share_auth_session_valid(
            "qa-link-a",
            "qa-hash-a",
            "forged",
            now
        ));
        assert!(issue_share_auth_session("qa-link-a", "qa-hash-a", i64::MAX).is_err());
    }

    #[actix_web::test]
    async fn verification_issues_http_only_cookie_with_server_enforced_ttl() {
        let qa_phrase = ["synthetic", "only", "share"].join("-");
        let hash = bcrypt::hash(&qa_phrase, 4).unwrap();
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY, folder_id INTEGER, message_id INTEGER,
                file_name TEXT, file_size INTEGER, password_hash TEXT,
                password_salt TEXT, expires_at INTEGER, revoked INTEGER
            );",
        )
        .unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO shared_links
                    (id, folder_id, message_id, file_name, file_size,
                     password_hash, password_salt, expires_at, revoked)
                 VALUES ('qa-session', NULL, 1, 'private.txt', 42, ?, NULL, NULL, 0)",
            )
            .unwrap();
        insert.bind((1, hash.as_str())).unwrap();
        insert.next().unwrap();
        drop(insert);
        let db: DbConnection = Arc::new(Mutex::new(conn));
        let app = web_test::init_service(
            App::new()
                .app_data(web::Data::new(db))
                .configure(configure_share_routes),
        )
        .await;

        let mut keys = Vec::new();
        for _ in 0..2 {
            let request = web_test::TestRequest::post()
                .uri("/d/qa-session/verify")
                .set_form([("password", qa_phrase.as_str())])
                .to_request();
            let response = web_test::call_service(&app, request).await;
            assert_eq!(response.status(), StatusCode::FOUND);
            let cookie = response
                .response()
                .cookies()
                .next()
                .expect("successful verification must issue a cookie");
            assert_eq!(cookie.name(), "share_auth_qa-session");
            assert!(cookie.http_only().unwrap_or(false));
            assert_eq!(
                cookie.same_site(),
                Some(actix_web::cookie::SameSite::Strict)
            );
            assert_eq!(
                cookie.max_age().unwrap().whole_seconds(),
                SHARE_AUTH_TTL_SECONDS
            );
            let key = cookie.value().to_owned();
            assert!(share_auth_session_valid(
                "qa-session",
                &hash,
                &key,
                chrono::Utc::now().timestamp()
            ));
            keys.push(key);
        }
        assert_ne!(keys[0], keys[1], "separate logins must not share a cookie");
    }

    #[test]
    fn expiry_is_enforced_at_the_exact_boundary_and_on_both_paths() {
        assert!(!share_has_expired(None, 500));
        assert!(!share_has_expired(Some(501), 500));
        assert!(share_has_expired(Some(500), 500));
        assert!(share_has_expired(Some(499), 500));
    }

    #[actix_web::test]
    async fn password_verification_rejects_expired_share_without_setting_cookie() {
        // Entirely in memory; no Telegram connection, client, or shared profile.
        let conn = sqlite::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE shared_links (
                id TEXT PRIMARY KEY,
                folder_id INTEGER,
                message_id INTEGER NOT NULL,
                file_name TEXT NOT NULL,
                file_size INTEGER NOT NULL,
                password_hash TEXT,
                password_salt TEXT,
                expires_at INTEGER,
                revoked INTEGER NOT NULL
            );
            INSERT INTO shared_links
                (id, folder_id, message_id, file_name, file_size,
                 password_hash, password_salt, expires_at, revoked)
            VALUES ('qa-expired', NULL, 1, 'private.txt', 42, 'qa-hash', NULL, 1, 0);",
        )
        .unwrap();
        let db: DbConnection = Arc::new(Mutex::new(conn));
        let app = web_test::init_service(
            App::new()
                .app_data(web::Data::new(db))
                .configure(configure_share_routes),
        )
        .await;
        let request = web_test::TestRequest::post()
            .uri("/d/qa-expired/verify")
            .set_form([("password", "synthetic-password")])
            .to_request();
        let response = web_test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::GONE);
        assert!(response.headers().get("Set-Cookie").is_none());
    }
}
