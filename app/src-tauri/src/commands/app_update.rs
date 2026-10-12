use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager};

const MOBILE_UPDATE_ENDPOINT: &str =
    "https://github.com/Yashwanth034/TeraRelay/releases/latest/download/mobile-latest.json";
const MOBILE_UPDATE_SIGNATURE_ENDPOINT: &str =
    "https://github.com/Yashwanth034/TeraRelay/releases/latest/download/mobile-latest.json.sig";
const MOBILE_UPDATE_PUBLIC_KEY: &str = "untrusted comment: minisign public key: AD9037B636C870B0\nRWSwcMg2tjeQrSH51pQIvcw4ZNbFlsPEMpxWGihFAqz7dWh/fBseR6wV\n";
const UPDATE_USER_AGENT: &str = "TeraRelay-Updater";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileUpdateAsset {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MobileUpdateManifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default, rename = "publishedAt")]
    pub published_at: Option<String>,
    #[serde(default)]
    pub android: Option<MobileUpdateAsset>,
    #[serde(default, rename = "iosStoreUrl")]
    pub ios_store_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MobileUpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
    pub android: Option<MobileUpdateAsset>,
    pub ios_store_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct UpdateDownloadProgress {
    downloaded: u64,
    total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<VersionIdentifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionIdentifier {
    Number(u64),
    Text(String),
}

fn parse_version(value: &str) -> Result<ParsedVersion, String> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let value = value.split('+').next().unwrap_or(value);
    let (core, prerelease) = match value.split_once('-') {
        Some((core, prerelease)) if !prerelease.is_empty() => (core, Some(prerelease)),
        Some(_) => return Err("Invalid semantic version prerelease".to_string()),
        None => (value, None),
    };
    let mut parts = core.split('.');
    let parse_number = |part: Option<&str>| -> Result<u64, String> {
        let part =
            part.ok_or_else(|| "Semantic version must contain major.minor.patch".to_string())?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("Invalid semantic version number".to_string());
        }
        part.parse::<u64>()
            .map_err(|_| "Semantic version number is too large".to_string())
    };
    let major = parse_number(parts.next())?;
    let minor = parse_number(parts.next())?;
    let patch = parse_number(parts.next())?;
    if parts.next().is_some() {
        return Err("Semantic version must contain exactly major.minor.patch".to_string());
    }
    let prerelease = prerelease
        .map(|raw| {
            raw.split('.')
                .map(|part| {
                    if part.is_empty()
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    {
                        return Err("Invalid semantic version prerelease".to_string());
                    }
                    if part.bytes().all(|byte| byte.is_ascii_digit()) {
                        if part.len() > 1 && part.starts_with('0') {
                            return Err("Invalid numeric prerelease identifier".to_string());
                        }
                        part.parse::<u64>()
                            .map(VersionIdentifier::Number)
                            .map_err(|_| "Prerelease number is too large".to_string())
                    } else {
                        Ok(VersionIdentifier::Text(part.to_string()))
                    }
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(ParsedVersion {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn compare_prerelease(left: &[VersionIdentifier], right: &[VersionIdentifier]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for index in 0..left.len().max(right.len()) {
        let Some(a) = left.get(index) else {
            return Ordering::Less;
        };
        let Some(b) = right.get(index) else {
            return Ordering::Greater;
        };
        let ordering = match (a, b) {
            (VersionIdentifier::Number(a), VersionIdentifier::Number(b)) => a.cmp(b),
            (VersionIdentifier::Number(_), VersionIdentifier::Text(_)) => Ordering::Less,
            (VersionIdentifier::Text(_), VersionIdentifier::Number(_)) => Ordering::Greater,
            (VersionIdentifier::Text(a), VersionIdentifier::Text(b)) => a.cmp(b),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering, String> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Ok(left
        .major
        .cmp(&right.major)
        .then(left.minor.cmp(&right.minor))
        .then(left.patch.cmp(&right.patch))
        .then_with(|| compare_prerelease(&left.prerelease, &right.prerelease)))
}

fn normalize_version(value: &str) -> Result<String, String> {
    let parsed = parse_version(value)?;
    let mut normalized = format!("{}.{}.{}", parsed.major, parsed.minor, parsed.patch);
    if !parsed.prerelease.is_empty() {
        normalized.push('-');
        normalized.push_str(
            &parsed
                .prerelease
                .iter()
                .map(|part| match part {
                    VersionIdentifier::Number(value) => value.to_string(),
                    VersionIdentifier::Text(value) => value.clone(),
                })
                .collect::<Vec<_>>()
                .join("."),
        );
    }
    Ok(normalized)
}

fn trusted_android_host(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "github.com" | "objects.githubusercontent.com" | "release-assets.githubusercontent.com"
    )
}

fn validate_android_asset(url: &str, sha256: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Android APK URL is invalid".to_string())?;
    if parsed.scheme() != "https" {
        return Err("Android APK URL must use HTTPS".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "Android APK URL is missing a host".to_string())?;
    if !trusted_android_host(host) {
        return Err(format!("Android APK host is not trusted: {host}"));
    }
    if parsed.path().is_empty() || !parsed.path().to_ascii_lowercase().ends_with(".apk") {
        return Err("Android update URL must point to an APK".to_string());
    }
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Android APK SHA-256 is invalid".to_string());
    }
    Ok(())
}

fn validate_ios_store_url(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "iOS store URL is invalid".to_string())?;
    if parsed.scheme() != "https" {
        return Err("iOS store URL must use HTTPS".to_string());
    }
    match parsed
        .host_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "apps.apple.com" | "testflight.apple.com" => Ok(()),
        host => Err(format!("iOS store host is not trusted: {host}")),
    }
}

fn verify_minisign_payload(
    payload: &[u8],
    signature: &str,
    public_key: &str,
) -> Result<(), String> {
    let public_key = minisign_verify::PublicKey::decode(public_key)
        .map_err(|error| format!("Mobile update public key is invalid: {error}"))?;
    let signature = minisign_verify::Signature::decode(signature)
        .map_err(|error| format!("Mobile update signature is invalid: {error}"))?;
    public_key
        .verify(payload, &signature, false)
        .map_err(|_| "Mobile update manifest signature verification failed".to_string())
}

fn mobile_manifest_targets_platform(manifest: &MobileUpdateManifest, platform: &str) -> bool {
    match platform {
        "android" => manifest.android.is_some(),
        "ios" => manifest.ios_store_url.is_some(),
        _ => false,
    }
}

fn parse_mobile_manifest(bytes: &[u8]) -> Result<MobileUpdateManifest, String> {
    let mut manifest: MobileUpdateManifest = serde_json::from_slice(bytes)
        .map_err(|_| "Mobile update manifest is not valid JSON".to_string())?;
    manifest.version = normalize_version(&manifest.version)?;
    if let Some(asset) = manifest.android.as_mut() {
        validate_android_asset(&asset.url, &asset.sha256)?;
        asset.sha256.make_ascii_lowercase();
    }
    if let Some(url) = manifest.ios_store_url.as_deref() {
        validate_ios_store_url(url)?;
    }
    if let Some(published_at) = manifest.published_at.as_deref() {
        chrono::DateTime::parse_from_rfc3339(published_at)
            .map_err(|_| "Mobile update manifest publishedAt is invalid".to_string())?;
    }
    Ok(manifest)
}

fn apk_filename_for_version(version: &str) -> Result<String, String> {
    let normalized = normalize_version(version)?;
    if normalized
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    {
        return Err("Update version is not safe for a filename".to_string());
    }
    Ok(format!("TeraRelay-{normalized}.apk"))
}

fn update_cache_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_cache_dir()
        .map(|path| path.join("app-updates"))
        .map_err(|error| format!("Could not locate TeraRelay update cache: {error}"))
}

fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many update redirects");
        }
        let url = attempt.url();
        if url.scheme() != "https" || !url.host_str().map(trusted_android_host).unwrap_or(false) {
            return attempt.error("update redirect left the trusted HTTPS hosts");
        }
        attempt.follow()
    })
}

fn update_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(UPDATE_USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(15 * 60))
        .redirect(redirect_policy())
        .build()
        .map_err(|error| format!("Could not initialize update client: {error}"))
}

#[tauri::command]
pub async fn cmd_mobile_check_update(
    app_handle: tauri::AppHandle,
) -> Result<Option<MobileUpdateInfo>, String> {
    let client = update_client()?;
    let response = client
        .get(MOBILE_UPDATE_ENDPOINT)
        .send()
        .await
        .map_err(|error| format!("Could not check for updates: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Update server returned HTTP {}", response.status()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Could not read update manifest: {error}"))?;

    let signature_response = client
        .get(MOBILE_UPDATE_SIGNATURE_ENDPOINT)
        .send()
        .await
        .map_err(|error| format!("Could not read update manifest signature: {error}"))?;
    if !signature_response.status().is_success() {
        return Err(format!(
            "Update signature server returned HTTP {}",
            signature_response.status()
        ));
    }
    let signature = signature_response
        .text()
        .await
        .map_err(|error| format!("Could not read update manifest signature: {error}"))?;
    verify_minisign_payload(&bytes, &signature, MOBILE_UPDATE_PUBLIC_KEY)?;

    let manifest = parse_mobile_manifest(&bytes)?;
    let mobile_platform = if cfg!(target_os = "android") {
        Some("android")
    } else if cfg!(target_os = "ios") {
        Some("ios")
    } else {
        None
    };
    if mobile_platform
        .map(|platform| !mobile_manifest_targets_platform(&manifest, platform))
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let current_version = app_handle.package_info().version.to_string();
    if compare_versions(&manifest.version, &current_version)? != Ordering::Greater {
        return Ok(None);
    }
    Ok(Some(MobileUpdateInfo {
        version: manifest.version,
        current_version,
        notes: manifest.notes,
        published_at: manifest.published_at,
        android: manifest.android,
        ios_store_url: manifest.ios_store_url,
    }))
}

#[tauri::command]
pub async fn cmd_android_download_update(
    app_handle: tauri::AppHandle,
    version: String,
    url: String,
    sha256: String,
) -> Result<String, String> {
    validate_android_asset(&url, &sha256)?;
    let filename = apk_filename_for_version(&version)?;
    let update_dir = update_cache_dir(&app_handle)?;
    tokio::fs::create_dir_all(&update_dir)
        .await
        .map_err(|error| format!("Could not create update cache: {error}"))?;
    let final_path = update_dir.join(filename);
    let partial_path = final_path.with_extension("apk.part");
    let _ = tokio::fs::remove_file(&partial_path).await;

    let response = update_client()?
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("Could not download update: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Update download returned HTTP {}",
            response.status()
        ));
    }
    let total = response.content_length().unwrap_or(0);
    let mut file = tokio::fs::File::create(&partial_path)
        .await
        .map_err(|error| format!("Could not create update file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("Update download failed: {error}"))?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|error| format!("Could not write update file: {error}"))?;
        hasher.update(&chunk);
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        let _ = app_handle.emit(
            "app-update-download-progress",
            UpdateDownloadProgress { downloaded, total },
        );
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|error| format!("Could not flush update file: {error}"))?;
    file.sync_all()
        .await
        .map_err(|error| format!("Could not sync update file: {error}"))?;
    drop(file);

    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(&sha256) {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err("Downloaded APK failed SHA-256 verification".to_string());
    }
    if final_path.exists() {
        tokio::fs::remove_file(&final_path)
            .await
            .map_err(|error| format!("Could not replace previous update: {error}"))?;
    }
    tokio::fs::rename(&partial_path, &final_path)
        .await
        .map_err(|error| format!("Could not finalize downloaded update: {error}"))?;
    Ok(final_path.to_string_lossy().to_string())
}

async fn verify_file_sha256(path: &Path, expected_sha256: &str) -> Result<(), String> {
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("Android APK SHA-256 is invalid".to_string());
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("Could not reopen verified update APK: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer)
            .await
            .map_err(|error| format!("Could not recheck update APK: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err("Cached APK failed SHA-256 verification before install".to_string());
    }
    Ok(())
}

fn canonical_verified_apk(app: &tauri::AppHandle, apk_path: &str) -> Result<PathBuf, String> {
    let root = update_cache_dir(app)?
        .canonicalize()
        .map_err(|_| "TeraRelay update cache is unavailable".to_string())?;
    let candidate = Path::new(apk_path)
        .canonicalize()
        .map_err(|_| "Downloaded update APK is unavailable".to_string())?;
    if !candidate.starts_with(&root)
        || candidate.extension().and_then(|value| value.to_str()) != Some("apk")
        || !candidate.is_file()
    {
        return Err(
            "Refusing to install an APK outside TeraRelay's verified update cache".to_string(),
        );
    }
    Ok(candidate)
}

#[cfg(target_os = "android")]
fn android_sdk_int(env: &mut jni::JNIEnv<'_>) -> Result<i32, String> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .map_err(|error| format!("Could not inspect Android version: {error}"))?
        .i()
        .map_err(|error| format!("Could not read Android version: {error}"))
}

#[cfg(target_os = "android")]
fn with_android_context<T>(
    operation: impl FnOnce(&mut jni::JNIEnv<'_>, jni::objects::JObject<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let context = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(context.vm().cast()) }
        .map_err(|error| format!("Android Java VM is unavailable: {error}"))?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|error| format!("Could not attach to Android runtime: {error}"))?;
    let context_object =
        unsafe { jni::objects::JObject::from_raw(context.context().cast::<jni::sys::_jobject>()) };
    operation(&mut env, context_object)
}

#[cfg(target_os = "android")]
fn android_can_install_packages_impl() -> Result<bool, String> {
    with_android_context(|env, context| {
        if android_sdk_int(env)? < 26 {
            let resolver = env
                .call_method(
                    &context,
                    "getContentResolver",
                    "()Landroid/content/ContentResolver;",
                    &[],
                )
                .map_err(|error| format!("Could not access Android content resolver: {error}"))?
                .l()
                .map_err(|error| format!("Android content resolver is unavailable: {error}"))?;
            let setting = env.new_string("install_non_market_apps").map_err(|error| {
                format!("Could not prepare Android unknown-source setting: {error}")
            })?;
            let setting_object = jni::objects::JObject::from(setting);
            let allowed = env
                .call_static_method(
                    "android/provider/Settings$Secure",
                    "getInt",
                    "(Landroid/content/ContentResolver;Ljava/lang/String;I)I",
                    &[
                        jni::objects::JValue::Object(&resolver),
                        jni::objects::JValue::Object(&setting_object),
                        jni::objects::JValue::Int(0),
                    ],
                )
                .map_err(|error| format!("Could not read Android unknown-source setting: {error}"))?
                .i()
                .map_err(|error| {
                    format!("Android unknown-source setting is unavailable: {error}")
                })?;
            return Ok(allowed != 0);
        }
        let manager = env
            .call_method(
                &context,
                "getPackageManager",
                "()Landroid/content/pm/PackageManager;",
                &[],
            )
            .map_err(|error| format!("Could not access Android package manager: {error}"))?
            .l()
            .map_err(|error| format!("Android package manager is unavailable: {error}"))?;
        env.call_method(&manager, "canRequestPackageInstalls", "()Z", &[])
            .map_err(|error| format!("Could not read Android install permission: {error}"))?
            .z()
            .map_err(|error| format!("Android install permission is unavailable: {error}"))
    })
}

#[cfg(target_os = "android")]
fn android_start_activity(
    env: &mut jni::JNIEnv<'_>,
    context: &jni::objects::JObject<'_>,
    intent: &jni::objects::JObject<'_>,
) -> Result<(), String> {
    env.call_method(
        intent,
        "addFlags",
        "(I)Landroid/content/Intent;",
        &[jni::objects::JValue::Int(0x10000000)],
    )
    .map_err(|error| format!("Could not configure Android update activity: {error}"))?;
    env.call_method(
        context,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[jni::objects::JValue::Object(intent)],
    )
    .map_err(|error| format!("Could not open Android update activity: {error}"))?;
    Ok(())
}

#[cfg(target_os = "android")]
fn android_open_install_permission_impl() -> Result<(), String> {
    with_android_context(|env, context| {
        let sdk = android_sdk_int(env)?;
        let action_text = if sdk >= 26 {
            "android.settings.MANAGE_UNKNOWN_APP_SOURCES"
        } else {
            "android.settings.SECURITY_SETTINGS"
        };
        let action = env
            .new_string(action_text)
            .map_err(|error| format!("Could not prepare Android settings action: {error}"))?;
        let action_object = jni::objects::JObject::from(action);
        let intent = if sdk >= 26 {
            let package = env
                .call_method(&context, "getPackageName", "()Ljava/lang/String;", &[])
                .map_err(|error| format!("Could not read Android package name: {error}"))?
                .l()
                .map_err(|error| format!("Android package name is unavailable: {error}"))?;
            let package_jstring = jni::objects::JString::from(package);
            let package_string = env
                .get_string(&package_jstring)
                .map_err(|error| format!("Could not decode Android package name: {error}"))?;
            let uri_text = env
                .new_string(format!("package:{}", package_string.to_string_lossy()))
                .map_err(|error| format!("Could not prepare Android package URI: {error}"))?;
            let uri_text_object = jni::objects::JObject::from(uri_text);
            let uri = env
                .call_static_method(
                    "android/net/Uri",
                    "parse",
                    "(Ljava/lang/String;)Landroid/net/Uri;",
                    &[jni::objects::JValue::Object(&uri_text_object)],
                )
                .map_err(|error| format!("Could not prepare Android package URI: {error}"))?
                .l()
                .map_err(|error| format!("Android package URI is unavailable: {error}"))?;
            env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;Landroid/net/Uri;)V",
                &[
                    jni::objects::JValue::Object(&action_object),
                    jni::objects::JValue::Object(&uri),
                ],
            )
        } else {
            env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[jni::objects::JValue::Object(&action_object)],
            )
        }
        .map_err(|error| format!("Could not prepare Android settings intent: {error}"))?;
        android_start_activity(env, &context, &intent)
    })
}

#[cfg(target_os = "android")]
fn android_install_apk_impl(path: &Path) -> Result<(), String> {
    let path = path.to_string_lossy().to_string();
    with_android_context(|env, context| {
        let path_string = env
            .new_string(&path)
            .map_err(|error| format!("Could not prepare update path: {error}"))?;
        let path_object = jni::objects::JObject::from(path_string);
        let file = env
            .new_object(
                "java/io/File",
                "(Ljava/lang/String;)V",
                &[jni::objects::JValue::Object(&path_object)],
            )
            .map_err(|error| format!("Could not prepare update file: {error}"))?;
        let package_name = env
            .call_method(&context, "getPackageName", "()Ljava/lang/String;", &[])
            .map_err(|error| format!("Could not read Android package name: {error}"))?
            .l()
            .map_err(|error| format!("Android package name is unavailable: {error}"))?;
        let package_jstring = jni::objects::JString::from(package_name);
        let package_name = env
            .get_string(&package_jstring)
            .map_err(|error| format!("Could not decode Android package name: {error}"))?
            .to_string_lossy()
            .into_owned();
        let authority = env
            .new_string(format!("{package_name}.fileprovider"))
            .map_err(|error| format!("Could not prepare update provider: {error}"))?;
        let authority_object = jni::objects::JObject::from(authority);
        let uri = env
            .call_static_method(
                "androidx/core/content/FileProvider",
                "getUriForFile",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/io/File;)Landroid/net/Uri;",
                &[
                    jni::objects::JValue::Object(&context),
                    jni::objects::JValue::Object(&authority_object),
                    jni::objects::JValue::Object(&file),
                ],
            )
            .map_err(|error| {
                format!("Could not expose verified APK to Android installer: {error}")
            })?
            .l()
            .map_err(|error| format!("Verified APK URI is unavailable: {error}"))?;
        let action = env
            .new_string("android.intent.action.VIEW")
            .map_err(|error| format!("Could not prepare Android install action: {error}"))?;
        let action_object = jni::objects::JObject::from(action);
        let intent = env
            .new_object(
                "android/content/Intent",
                "(Ljava/lang/String;)V",
                &[jni::objects::JValue::Object(&action_object)],
            )
            .map_err(|error| format!("Could not prepare Android install intent: {error}"))?;
        let mime = env
            .new_string("application/vnd.android.package-archive")
            .map_err(|error| format!("Could not prepare Android APK MIME type: {error}"))?;
        let mime_object = jni::objects::JObject::from(mime);
        env.call_method(
            &intent,
            "setDataAndType",
            "(Landroid/net/Uri;Ljava/lang/String;)Landroid/content/Intent;",
            &[
                jni::objects::JValue::Object(&uri),
                jni::objects::JValue::Object(&mime_object),
            ],
        )
        .map_err(|error| format!("Could not attach verified APK to installer: {error}"))?;
        env.call_method(
            &intent,
            "addFlags",
            "(I)Landroid/content/Intent;",
            &[jni::objects::JValue::Int(0x10000001)],
        )
        .map_err(|error| format!("Could not grant Android installer access: {error}"))?;
        env.call_method(
            &context,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[jni::objects::JValue::Object(&intent)],
        )
        .map_err(|error| format!("Could not launch Android package installer: {error}"))?;
        Ok(())
    })
}

#[tauri::command]
pub fn cmd_android_can_install_packages() -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        android_can_install_packages_impl()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("Android package installation is only available on Android".to_string())
    }
}

#[tauri::command]
pub fn cmd_android_open_install_permission() -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        android_open_install_permission_impl()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("Android install-source settings are only available on Android".to_string())
    }
}

#[tauri::command]
pub async fn cmd_android_install_update(
    app_handle: tauri::AppHandle,
    apk_path: String,
    sha256: String,
) -> Result<(), String> {
    let verified = canonical_verified_apk(&app_handle, &apk_path)?;
    verify_file_sha256(&verified, &sha256).await?;
    #[cfg(target_os = "android")]
    {
        android_install_apk_impl(&verified)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = verified;
        Err("Android package installation is only available on Android".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_versions_compare_numerically() {
        assert_eq!(
            compare_versions("0.1.10", "0.1.9").unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("v1.0.0", "1.0.0").unwrap(),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_versions("1.2.0", "1.10.0").unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_versions("1.0.0-beta.2", "1.0.0-beta.11").unwrap(),
            std::cmp::Ordering::Less
        );
        assert!(compare_versions("bad", "1.0.0").is_err());
    }

    #[test]
    fn android_asset_validation_rejects_untrusted_sources_and_bad_hashes() {
        assert!(validate_android_asset(
            "https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay.apk",
            &"a".repeat(64),
        )
        .is_ok());
        assert!(
            validate_android_asset("http://github.com/TeraRelay.apk", &"a".repeat(64)).is_err()
        );
        assert!(
            validate_android_asset("https://evil.example/TeraRelay.apk", &"a".repeat(64)).is_err()
        );
        assert!(validate_android_asset(
            "https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay.apk",
            "not-a-hash",
        )
        .is_err());
    }

    #[test]
    fn mobile_manifest_requires_a_safe_android_asset() {
        let parsed = parse_mobile_manifest(
            br#"{
          "version":"0.1.2",
          "notes":"Fixes",
          "publishedAt":"2026-10-10T12:00:00Z",
          "android":{
            "url":"https://github.com/Yashwanth034/TeraRelay/releases/download/v0.1.2/TeraRelay.apk",
            "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
          }
        }"#,
        )
        .unwrap();
        assert_eq!(parsed.version, "0.1.2");
        assert_eq!(parsed.android.as_ref().unwrap().sha256.len(), 64);

        assert!(parse_mobile_manifest(
            br#"{
          "version":"0.1.2",
          "android":{"url":"https://evil.example/TeraRelay.apk","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
        }"#,
        )
        .is_err());
    }

    #[test]
    fn downloaded_apk_filename_is_version_scoped_and_safe() {
        assert_eq!(
            apk_filename_for_version("0.1.2").unwrap(),
            "TeraRelay-0.1.2.apk"
        );
        assert!(apk_filename_for_version("../escape").is_err());
        assert!(apk_filename_for_version("1.0.0/evil").is_err());
    }

    #[test]
    fn mobile_manifest_signature_verification_rejects_tampering() {
        let public_key = "untrusted comment: minisign public key: 9DE7890F0642E81F\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
        let signature = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==\n";
        assert!(verify_minisign_payload(b"test", signature, public_key).is_ok());
        assert!(verify_minisign_payload(b"tampered", signature, public_key).is_err());
    }

    #[tokio::test]
    async fn cached_apk_is_rehashed_immediately_before_install() {
        let path = std::env::temp_dir().join(format!(
            "terarelay-updater-hash-{}-{}.apk",
            std::process::id(),
            rand::random::<u64>()
        ));
        tokio::fs::write(&path, b"abc").await.unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_file_sha256(&path, expected).await.is_ok());
        assert!(verify_file_sha256(&path, &"0".repeat(64)).await.is_err());
        let _ = tokio::fs::remove_file(path).await;
    }
}
