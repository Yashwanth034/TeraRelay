//! Desktop window visibility is independent of the native Drive/network lifetime.
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Manager,
};

/// Login startup is intentionally windowless; a normal launcher or second instance
/// brings the already-running process to the foreground.
#[cfg(target_os = "linux")]
pub fn background_start_requested() -> bool {
    std::env::args_os().any(|arg| arg == "--background")
}

pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window
            .show()
            .and_then(|_| window.unminimize())
            .and_then(|_| window.set_focus())
        {
            log::error!("Could not reopen TeraRelay: {error}");
        }
    }
}

pub fn init(app: &mut App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "background-open", "Open TeraRelay", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "background-quit", "Quit TeraRelay", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut builder = TrayIconBuilder::with_id("terarelay-background")
        .tooltip("TeraRelay — Drive stays connected in the background")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "background-open" => show_main_window(app),
            // Explicit exit preserves the existing native cleanup and transfer journals.
            "background-quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;
    app.manage(tray);
    Ok(())
}
