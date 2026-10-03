// Reserved cross-platform upload lifecycle hook.
//
// Android uploads work through the normal TeraRelay transfer path. A native
// Android foreground service is intentionally not started here because the
// public source tree does not ship a custom Android service class. Keeping
// these commands as no-ops preserves the frontend API without triggering JNI
// class-loading failures. A real foreground-service implementation can be
// added later as a dedicated mobile feature with its manifest/notification
// permissions and native lifecycle tests.

pub fn start_foreground_service() {
    #[cfg(target_os = "android")]
    log::debug!("Android foreground upload service is not enabled in this release.");
}

pub fn stop_foreground_service() {
    #[cfg(target_os = "android")]
    log::debug!("Android foreground upload service is not enabled in this release.");
}

#[tauri::command]
pub fn cmd_start_foreground_service() {
    start_foreground_service();
}

#[tauri::command]
pub fn cmd_stop_foreground_service() {
    stop_foreground_service();
}
