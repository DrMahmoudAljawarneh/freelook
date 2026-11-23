// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Silence the deprecation notice libayatana-appindicator prints on every tray
/// init:
///
///   libayatana-appindicator-WARNING: libayatana-appindicator is deprecated.
///   Please use libayatana-appindicator-glib in newly written code.
///
/// There is no Rust binding for libayatana-appindicator-glib yet (the
/// `tray-icon`/`libappindicator` crates still link the old ABI), so the choice
/// is this one line of noise or no tray icon at all. Keep the tray, drop the
/// message. Only this G_LOG_DOMAIN is filtered; all other GLib output passes
/// through untouched.
fn silence_appindicator_deprecation() {
    glib::log_set_handler(
        Some("libayatana-appindicator"),
        glib::LogLevels::LEVEL_WARNING
            | glib::LogLevels::LEVEL_MESSAGE
            | glib::LogLevels::LEVEL_INFO,
        false,
        false,
        |_domain, _level, _message| {},
    );
}

fn main() {
    silence_appindicator_deprecation();
    freelook_lib::run()
}
