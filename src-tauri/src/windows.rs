use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tauri::{
    webview::{DownloadEvent, PageLoadEvent},
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_notification::NotificationExt;

#[allow(dead_code)]
const DEEPLINK_URLS: &[&str] = &[
    "outlook.live.com/mail/deeplink",
    "outlook.office365.com/mail/deeplink",
    "outlook.office.com/mail/deeplink",
    "outlook.cloud.microsoft/mail/deeplink",
    "outlook.live.com/calendar/0/deeplink",
    "outlook.office365.com/calendar/0/deeplink",
    "outlook.office.com/calendar/0/deeplink",
    "outlook.cloud.microsoft/calendar/0/deeplink",
];

/// Microsoft and Outlook domains (and their subdomains) that should stay in-app.
const MICROSOFT_HOST_SUFFIXES: &[&str] = &[
    "outlook.com",
    "cloud.microsoft",
    "microsoft365.com",
    "office.com",
    "office.net",
    "office365.com",
    "office365.net",
    "officeppe.net",
    "live.com",
    "live.net",
    "microsoft.com",
    "microsoftusercontent.com",
    "microsoftonline.com",
    "msauth.net",
    "msocdn.com",
    "msftauth.net",
    "1drv.ms",
    "sharepoint.com",
    "sharepointonline.com",
    "onenote.com",
    "onedrive.com",
    "svc.ms",
    "windows.net",
];

/// Outlook on the web rejects the stock WebKitGTK user agent ("…Linux x86_64 …
/// Safari/605.1.15") with a 500 `OwaBasicUnsupportedException` page during
/// boot. WebKitGTK is a Safari-class engine, so present the webview as desktop
/// Safari on macOS: engine and UA then agree, which keeps OWA on the code path
/// it uses for Apple's WebKit instead of a Chromium-only path it cannot run.
///
/// Keep this in sync with `wry`'s WebKitGTK `WebSettings:user-agent`, which is
/// used for both `navigator.userAgent` and the `User-Agent` request header.
const OUTLOOK_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Safari/605.1.15";

/// Check if a URL belongs to a Microsoft domain that should stay in-app
fn is_microsoft_url(url: &url::Url) -> bool {
    match url.scheme() {
        "about" | "data" => return true,
        "blob" => {
            return url
                .path()
                .parse::<url::Url>()
                .map(|inner_url| is_microsoft_url(&inner_url))
                .unwrap_or(true);
        }
        _ => {}
    }

    match url.host_str() {
        Some(host) => {
            let h = host.to_lowercase();
            if host_matches_suffix(&h, "safelinks.protection.outlook.com") {
                return false;
            }
            MICROSOFT_HOST_SUFFIXES
                .iter()
                .any(|domain| host_matches_suffix(&h, domain))
        }
        None => false,
    }
}

/// Check whether a saved file path's extension is a PDF / Office document that
/// we want to auto-open with the OS default viewer (Preview, Word, Excel, ...)
/// after a successful download. We only treat the inline preview as "working"
/// when the file actually lands on disk and is launched in a real viewer, since
/// WKWebView cannot render Outlook's Office Online / PDFTron preview iframe.
fn path_is_pdf_or_doc(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|value| value.to_ascii_lowercase())
            .as_deref(),
        Some(
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "rtf" | "odt" | "ods" | "odp"
        )
    )
}

/// Check if a URL is an ad/tracking URL that should be silently blocked
/// Domains whose subdomains are all ad/tracking/attribution endpoints. Matching
/// is suffix-based so `evil-adnxs.com.attacker.tld` cannot slip through the way
/// a plain `contains()` check would.
const AD_HOST_SUFFIXES: &[&str] = &[
    // Cookie-sync, intent data and attribution pixels. `company-target.com` is
    // the Bombora sync endpoint Outlook's ad slots redirect to
    // (`s.company-target.com/s/sync?exc=lr`), which the old code happily handed
    // to the system browser.
    "company-target.com",
    "bombora.com",
    "ml314.com",
    "adsrvr.org",
    "agkn.com",
    "bluekai.com",
    "demdex.net",
    "rlcdn.com",
    "liadm.com",
    "crwdcntrl.net",
    "exelator.com",
    "mathtag.com",
    "tapad.com",
    "semasio.net",
    // Ad exchanges, SSPs and DSPs
    "adnxs.com",
    "adnxs.net",
    "amazon-adsystem.com",
    "adsystem.com",
    "adsafeprotected.com",
    "moatads.com",
    "serving-sys.com",
    "sizmek.com",
    "rubiconproject.com",
    "magnite.com",
    "pubmatic.com",
    "openx.net",
    "casalemedia.com",
    "indexww.com",
    "33across.com",
    "turn.com",
    "bidswitch.net",
    "contextweb.com",
    "sharethrough.com",
    "spotxchange.com",
    "tidaltv.com",
    "tremorhub.com",
    "videologygroup.com",
    "yieldmo.com",
    "smartadserver.com",
    "adform.net",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "adcolony.com",
    "adtechus.com",
    // Google / Meta measurement
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "googletagmanager.com",
    "google-analytics.com",
    "scorecardresearch.com",
    "quantserve.com",
    "facebook.net",
];

/// Fragments for ad SDK hosts that do not follow a stable domain shape.
const AD_HOST_FRAGMENTS: &[&str] = &["adsdkprod", "adsdk"];

fn host_matches_suffix(host: &str, domain: &str) -> bool {
    host == domain
        || (host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

fn is_ad_url(url: &url::Url) -> bool {
    match url.host_str() {
        Some(host) => {
            let h = host.to_lowercase();
            h.starts_with("ads.")
                || h.starts_with("ad.")
                || AD_HOST_SUFFIXES
                    .iter()
                    .any(|domain| host_matches_suffix(&h, domain))
                || AD_HOST_FRAGMENTS.iter().any(|fragment| h.contains(fragment))
        }
        None => false,
    }
}

/// Get homepage URL from config, with fallback
fn get_homepage_url(app: &AppHandle) -> String {
    let store = tauri_plugin_store::StoreBuilder::new(app, "Settings")
        .build()
        .ok();
    if let Some(store) = store {
        if let Some(val) = store.get("homepageUrl") {
            if let Some(s) = val.as_str() {
                return s.to_string();
            }
        }
    }
    "https://outlook.live.com/mail".to_string()
}

/// Get config value as string with default
fn get_config_string(app: &AppHandle, key: &str, default: &str) -> String {
    let store = tauri_plugin_store::StoreBuilder::new(app, "Settings")
        .build()
        .ok();
    if let Some(store) = store {
        if let Some(val) = store.get(key) {
            if let Some(s) = val.as_str() {
                return s.to_string();
            }
        }
    }
    default.to_string()
}

/// Set config value
pub fn set_config_value(app: &AppHandle, key: &str, value: &str) {
    if let Ok(store) = tauri_plugin_store::StoreBuilder::new(app, "Settings").build() {
        store.set(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
        let _ = store.save();
    }
}

static LAST_FRAME_SAVE: Mutex<Option<Instant>> = Mutex::new(None);
static NEXT_DOWNLOAD_NAME: Mutex<Option<String>> = Mutex::new(None);

pub fn set_pending_download_name(name: String) {
    let name = sanitize_file_name(&name);
    if name.is_empty() {
        return;
    }

    eprintln!("[freelook] pending download name: {}", name);
    if let Ok(mut pending) = NEXT_DOWNLOAD_NAME.lock() {
        *pending = Some(name);
    }
}

fn take_pending_download_name() -> Option<String> {
    NEXT_DOWNLOAD_NAME
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
}

fn sanitize_file_name(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect::<String>();

    sanitized
        .trim()
        .trim_matches('.')
        .trim()
        .chars()
        .take(180)
        .collect()
}

fn unique_download_path(download_dir: &Path, file_name: &str) -> PathBuf {
    let candidate = download_dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());

    for index in 1..1000 {
        let name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({index}).{extension}"),
            _ => format!("{stem} ({index})"),
        };
        let candidate = download_dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    candidate
}

fn download_name(url: &url::Url, destination: &std::path::Path) -> std::ffi::OsString {
    if let Some(name) = take_pending_download_name() {
        return std::ffi::OsString::from(name);
    }

    if let Some(name) = destination.file_name().filter(|name| !name.is_empty()) {
        return name.to_os_string();
    }

    if url.scheme() == "blob" {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        return std::ffi::OsString::from(format!("freelook-attachment-{}.pdf", timestamp));
    }

    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| !name.is_empty())
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("download"))
}

/// Save the main window's current position and size to config (debounced to 500ms).
pub fn save_window_frame(app: &AppHandle) {
    let now = Instant::now();
    if let Ok(mut last) = LAST_FRAME_SAVE.lock() {
        if let Some(prev) = *last {
            if now.duration_since(prev).as_millis() < 500 {
                return;
            }
        }
        *last = Some(now);
    }

    if let Some(window) = app.get_webview_window("main") {
        let scale = window.scale_factor().unwrap_or(1.0);
        if let Ok(pos) = window.outer_position() {
            let x = pos.x as f64 / scale;
            let y = pos.y as f64 / scale;
            set_config_value(app, "windowFrameX", &x.to_string());
            set_config_value(app, "windowFrameY", &y.to_string());
        }
        if let Ok(size) = window.outer_size() {
            let w = size.width as f64 / scale;
            let h = size.height as f64 / scale;
            set_config_value(app, "windowFrameWidth", &w.to_string());
            set_config_value(app, "windowFrameHeight", &h.to_string());
        }
    }
}

/// Force-save the window frame (bypasses debounce, for use on close).
pub fn save_window_frame_now(app: &AppHandle) {
    if let Ok(mut last) = LAST_FRAME_SAVE.lock() {
        *last = None;
    }
    save_window_frame(app);
}

/// Create and show the main (mail) window
pub fn create_main_window(app: &AppHandle) -> Result<(), String> {
    // Read config values
    let x: f64 = get_config_string(app, "windowFrameX", "100")
        .parse()
        .unwrap_or(100.0);
    let y: f64 = get_config_string(app, "windowFrameY", "100")
        .parse()
        .unwrap_or(100.0);
    let width: f64 = get_config_string(app, "windowFrameWidth", "1400")
        .parse()
        .unwrap_or(1400.0);
    let height: f64 = get_config_string(app, "windowFrameHeight", "900")
        .parse()
        .unwrap_or(900.0);
    let homepage_url = get_homepage_url(app);
    let app_handle = app.clone();

    let window = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External(homepage_url.parse().unwrap()),
    )
    .title("Freelook")
    .user_agent(OUTLOOK_USER_AGENT)
    .inner_size(width, height)
    .position(x, y)
    .visible(false)
    .disable_drag_drop_handler()
    .on_download(|webview, event| {
        match event {
            DownloadEvent::Requested { url, destination } => {
                eprintln!("[freelook] download requested: {}", url);
                // Save directly to the user's ~/Downloads directory with a
                // sensible suggested filename. We previously tried a native
                // save dialog here, but any modal panel (NSSavePanel via
                // `tauri-plugin-dialog`'s `blocking_save_file`, or even the
                // underlying rfd `beginSheet` modal) freezes the WKWebView
                // because the dialog's nested event pump conflicts with
                // Tauri's main-thread event loop. So accept always and let
                // `download_name()` pick the file name from (1) any pending
                // name the JS-side link interceptor cached via
                // `set_pending_download_name`, (2) the existing destination
                // filename, or (3) the URL's last path segment.
                if let Ok(download_dir) = webview.app_handle().path().download_dir() {
                    let suggested_name = download_name(&url, destination);
                    let suggested_str = suggested_name.to_string_lossy();
                    *destination = unique_download_path(&download_dir, suggested_str.as_ref());
                    eprintln!("[freelook] download destination: {:?}", destination);
                }
                true
            }
            DownloadEvent::Finished { url, path, success } => {
                eprintln!(
                    "[freelook] download finished: {} path={:?} success={}",
                    url, path, success
                );
                if success {
                    // Auto-open PDF / Office documents with the OS default viewer
                    // so users get a working preview despite WKWebView showing
                    // "Something went wrong" inside Outlook's inline preview
                    // pane. The browser cookie store in the user's external
                    // default browser does not share auth with our WebView, so
                    // externalising the URL to Safari / Chrome fails auth and
                    // redirects to Microsoft login; opening the downloaded
                    // local file works because Preview.app / Word / Excel read
                    // it directly.
                    if let Some(saved) = path.as_ref() {
                        if path_is_pdf_or_doc(saved) {
                            let _ = tauri_plugin_opener::open_path(
                                saved.as_path(),
                                None::<&str>,
                            );
                        }
                    }

                    let name = path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .and_then(|f| f.to_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| "download".to_string());
                    let folder = path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .and_then(|parent| parent.file_name())
                        .and_then(|f| f.to_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| "Downloads".to_string());
                    let _ = webview
                        .app_handle()
                        .notification()
                        .builder()
                        .title("Freelook: download complete")
                        .body(format!("Saved to {folder}/{name}"))
                        .show();
                }
                true
            }
            _ => true,
        }
    })
    .on_navigation(move |url| {
        if is_microsoft_url(url) {
            // Let Microsoft URLs navigate inside the embedded Outlook view so
            // inline previews (images, HTML, etc.) render and the Download
            // buttons reach on_download. WKWebView cannot render Outlook's
            // Office Online / PDFTron preview iframe ("Something went wrong")
            // but Microsoft's own preview UI shows a Download button as
            // fallback, and on_download auto-opens the saved PDF / Office doc
            // in Preview.app / Word / Excel.
            true
        } else if is_ad_url(url) {
            eprintln!("[freelook] blocked tracker navigation: {url}");
            false
        } else if matches!(url.scheme(), "mailto" | "tel") {
            eprintln!("[freelook] external scheme navigation: {url}");
            let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
            false
        } else {
            // Block unsolicited / iframe navigations to non-Microsoft URLs
            // without launching the external browser; user-initiated external
            // link clicks are handled by `interceptClick` and `on_new_window`.
            eprintln!("[freelook] blocked external navigation: {url}");
            false
        }
    })
    .on_new_window(|url, _features| {
        if is_microsoft_url(&url) {
            tauri::webview::NewWindowResponse::Allow
        } else if is_ad_url(&url) {
            eprintln!("[freelook] blocked tracker popup: {url}");
            tauri::webview::NewWindowResponse::Deny
        } else {
            let _ = tauri_plugin_opener::open_url(url.as_str(), None::<&str>);
            tauri::webview::NewWindowResponse::Deny
        }
    })
    .on_page_load(move |_window, payload| {
        if payload.event() == PageLoadEvent::Finished {
            let _ = apply_main_settings(&app_handle);
        }
    })
    .build()
    .map_err(|e| e.to_string())?;

    // If the remembered frame falls outside every monitor (display unplugged,
    // resolution changed, different multi-monitor layout) the window would be
    // created off-screen and look like "the app did not open". Recenter it.
    let monitor = window
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| window.primary_monitor().ok().flatten());
    if let (Ok(pos), Some(monitor)) = (window.outer_position(), monitor) {
        let mpos = monitor.position();
        let msize = monitor.size();
        let margin = 80;
        let on_screen = pos.x + margin >= mpos.x
            && pos.y + margin >= mpos.y
            && pos.x < mpos.x + msize.width as i32 - margin
            && pos.y < mpos.y + msize.height as i32 - margin;
        if !on_screen {
            eprintln!(
                "[freelook] saved window position {pos:?} is off-screen, recentering"
            );
            let _ = window.center();
        }
    }

    Ok(())
}

pub fn apply_main_settings(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        let zoom = get_config_string(app, "zoomFactor", "1.0")
            .parse::<f64>()
            .unwrap_or(1.0);
        window.set_zoom(zoom).map_err(|e| e.to_string())?;

        inject_main_css(&window, &get_main_css(app))?;

        // Inject link click interceptor
        let _ = window.eval(&get_link_interceptor_js());

        // Inject floating refresh button
        let _ = window.eval(get_refresh_button_js());
    }
    Ok(())
}

fn inject_main_css(window: &tauri::WebviewWindow, css: &str) -> Result<(), String> {
    let escaped_css = css
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace('$', "\\$");
    let js = format!(
        r#"
        (() => {{
            const id = "freelook-main-css";
            let style = document.getElementById(id);
            if (!style) {{
                style = document.createElement("style");
                style.id = id;
                document.head.appendChild(style);
            }}
            style.textContent = `{}`;
        }})();
        "#,
        escaped_css
    );
    window.eval(&js).map_err(|e| e.to_string())
}

/// Show the main window
pub fn show_main_window(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Reset window frame to defaults
pub fn reset_window_frame(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window
            .set_size(tauri::LogicalSize::new(1400.0, 900.0))
            .map_err(|e| e.to_string())?;
        window.center().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Open settings window (create if needed, show if exists)
pub fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        window.show().map_err(|e| e.to_string())?;
        window.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(
        &app,
        "settings",
        WebviewUrl::App("view/setting.html".into()),
    )
    .title("Settings")
    .inner_size(960.0, 740.0)
    .min_inner_size(800.0, 500.0)
    .visible(true)
    .build()
    .map_err(|e| e.to_string())?;

    let _ = window.set_focus();

    Ok(())
}

/// Restart the application
pub fn restart_app(app: AppHandle) -> Result<(), String> {
    app.restart();
}

/// Refresh (reload) the main webview page
pub fn refresh_page(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("main") {
        window
            .eval("location.reload()")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Inject CSS into a webview.
///
/// Safe under `require-trusted-types-for 'script'`: `insertAdjacentHTML` with a
/// raw string throws `TypeError: This assignment requires a TrustedHTML`, while
/// `style.textContent` is not a Trusted Types sink.
pub fn css_inject(app: AppHandle, webview_label: String, css: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&webview_label) {
        let escaped_css = css
            .replace('\\', "\\\\")
            .replace('`', "\\`")
            .replace('$', "\\$");
        let js = format!(
            r#"
            (() => {{
                const id = "freelook-injected-css";
                let style = document.getElementById(id);
                if (!style) {{
                    style = document.createElement("style");
                    style.id = id;
                    (document.head || document.documentElement).appendChild(style);
                }}
                style.textContent = `{}`;
            }})();
            "#,
            escaped_css
        );
        window.eval(&js).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Check if a URL is a deeplink
#[allow(dead_code)]
pub fn is_deeplink(url: &str) -> bool {
    DEEPLINK_URLS.iter().any(|dl| url.contains(dl))
}

/// Check if a URL is a calendar deeplink (which we deny)
#[allow(dead_code)]
pub fn is_calendar_deeplink(url: &str) -> bool {
    url.contains("outlook.live.com/calendar/0/deeplink")
        || url.contains("outlook.office365.com/calendar/0/deeplink")
        || url.contains("outlook.office.com/calendar/0/deeplink")
        || url.contains("outlook.cloud.microsoft/calendar/0/deeplink")
}

/// Get the CSS for ad blocking
pub fn get_main_css(app: &AppHandle) -> String {
    let vertical = get_config_string(app, "verticalAdsClass", "pBKjV");
    let small = get_config_string(app, "smallAdsClass", "X1Kvq");
    let premium = get_config_string(app, "premiumAdsClass", "VPtFl");

    format!(
        r#"
        .{vertical} {{ display: none !important; }}
        .{small} {{ display: none !important; }}
        .{premium} {{ display: none !important; }}
        "#
    )
}

/// Get the JavaScript that injects a floating refresh button.
///
/// The SVG is built with `createElementNS`/`setAttribute` rather than
/// `innerHTML`: Outlook's page is served with
/// `require-trusted-types-for 'script'`, and a raw-string `innerHTML`
/// assignment throws `TypeError: This assignment requires a TrustedHTML`
/// inside that CSP.
fn get_refresh_button_js() -> &'static str {
    r#"
    (function() {
        if (document.getElementById('freelook-refresh-btn')) return;

        const btn = document.createElement('div');
        btn.id = 'freelook-refresh-btn';
        btn.title = 'Refresh page';

        const SVG_NS = 'http://www.w3.org/2000/svg';
        const svg = document.createElementNS(SVG_NS, 'svg');
        svg.setAttribute('width', '20');
        svg.setAttribute('height', '20');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'white');
        svg.setAttribute('stroke-width', '2.5');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');

        const polyline = document.createElementNS(SVG_NS, 'polyline');
        polyline.setAttribute('points', '23 4 23 10 17 10');

        const path = document.createElementNS(SVG_NS, 'path');
        path.setAttribute('d', 'M20.49 15a9 9 0 1 1-2.12-9.36L23 10');

        svg.appendChild(polyline);
        svg.appendChild(path);
        btn.appendChild(svg);

        btn.style.cssText = 'position:fixed;bottom:24px;right:24px;z-index:2147483647;width:44px;height:44px;border-radius:50%;background:rgba(0,120,212,0.85);display:flex;align-items:center;justify-content:center;cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,0.3);transition:background 0.2s,transform 0.15s;backdrop-filter:blur(4px);';

        btn.addEventListener('mouseenter', function() {
            btn.style.background = 'rgba(0,120,212,1)';
            btn.style.transform = 'scale(1.1)';
        });
        btn.addEventListener('mouseleave', function() {
            btn.style.background = 'rgba(0,120,212,0.85)';
            btn.style.transform = 'scale(1)';
        });
        btn.addEventListener('click', function(e) {
            e.preventDefault();
            e.stopPropagation();
            location.reload();
        });

        (document.body || document.documentElement).appendChild(btn);
    })();
    "#
}

/// Get the no-frame CSS
#[allow(dead_code)]
pub fn get_no_frame_css() -> String {
    r#"
    ._1Kg3ffZABPxXxDqcmoxkBA {
        padding-top: 30px !important;
        -webkit-app-region: drag;
    }
    .ms-FocusZone,
    ._3Nd2PGu67wifhuPZp2Sfj5 {
        -webkit-app-region: no-drag;
    }
    "#
    .to_string()
}

/// JavaScript that intercepts non-Microsoft link clicks and opens them externally,
/// and remembers a suggested filename for the next download so the save dialog has a
/// sensible default name.
fn get_link_interceptor_js() -> String {
    let ms_domains_json =
        serde_json::to_string(MICROSOFT_HOST_SUFFIXES).unwrap_or_else(|_| "[]".to_string());
    let template = r#"
    (function() {
        if (window.__freelook_link_interceptor__) return;
        window.__freelook_link_interceptor__ = true;

        var MS_DOMAINS = __MS_DOMAINS__;

        function isMsUrl(href) {
            if (!href || href === '#' || href.startsWith('javascript:')) return true;
            if (href.startsWith('mailto:') || href.startsWith('tel:')) return true;
            if (href.startsWith('about:') || href.startsWith('data:')) return true;
            try {
                var u = new URL(href, location.href);
                if (u.protocol === 'blob:') {
                    try {
                        u = new URL(u.pathname);
                    } catch(e) {
                        return true;
                    }
                }
                var h = u.hostname.toLowerCase();
                if (h === 'safelinks.protection.outlook.com' || h.endsWith('.safelinks.protection.outlook.com')) {
                    return false;
                }
                return MS_DOMAINS.some(function(domain) {
                    return h === domain || h.endsWith('.' + domain);
                });
            } catch(e) { return true; }
        }

        function filenameFromText(text) {
            if (!text) return null;
            var normalized = text.replace(/\s+/g, ' ').trim();
            var match = normalized.match(/[^\\/:*?"<>|\r\n]+?\.(?:pdf|docx?|xlsx?|pptx?|csv|txt|rtf|zip|7z|rar|png|jpe?g|gif|heic|webp|eml|msg)\b/i);
            if (match) return match[0].replace(/^(download|open|preview|attachment)\s+/i, '').trim();

            match = normalized.match(/\b[A-Za-z0-9][A-Za-z0-9_-]{5,}(?:\s+[A-Za-z0-9][A-Za-z0-9_-]{2,}){0,5}\b/);
            if (!match) return null;
            var candidate = match[0].replace(/^(download|open|preview|attachment)\s+/i, '').trim();
            if (!/[._-]/.test(candidate)) return null;
            return candidate + '.pdf';
        }

        function extractAttachmentName(target) {
            var node = target && target.nodeType === 1 ? target : target && target.parentElement;
            if (!node) return null;

            var candidates = [];
            var current = node;
            for (var depth = 0; current && depth < 6; depth++, current = current.parentElement) {
                ['aria-label', 'title', 'download', 'data-filename', 'data-file-name', 'data-name'].forEach(function(attr) {
                    var value = current.getAttribute && current.getAttribute(attr);
                    if (value) candidates.push(value);
                });
                if (current.innerText) candidates.push(current.innerText);
            }

            var attachmentNode = node.closest && node.closest('[aria-label*="Attachment"], [aria-label*="attachment"], [title*="Attachment"], [title*="attachment"], [role="listitem"], [role="button"]');
            if (attachmentNode) {
                ['aria-label', 'title'].forEach(function(attr) {
                    var value = attachmentNode.getAttribute && attachmentNode.getAttribute(attr);
                    if (value) candidates.push(value);
                });
                if (attachmentNode.innerText) candidates.push(attachmentNode.innerText);
            }

            document.querySelectorAll('[aria-label], [title], [role="button"], [role="listitem"]').forEach(function(item) {
                var text = '';
                ['aria-label', 'title'].forEach(function(attr) {
                    var value = item.getAttribute && item.getAttribute(attr);
                    if (value) text += ' ' + value;
                });
                if (item.innerText) text += ' ' + item.innerText;
                if (/\.(pdf|docx?|xlsx?|pptx?)\b/i.test(text) || /\b[A-Za-z0-9]+[_-][A-Za-z0-9_-]+\b/.test(text)) {
                    candidates.push(text);
                }
            });

            for (var i = 0; i < candidates.length; i++) {
                var name = filenameFromText(candidates[i]);
                if (name) return name;
            }
            return null;
        }

        function rememberDownloadName(e) {
            var name = extractAttachmentName(e.target);
            if (!name) return;
            window.__TAURI__.core.invoke('set_pending_download_name', { name: name });
        }

        function interceptClick(e) {
            var el = e.target;
            while (el && el.tagName !== 'A') el = el.parentElement;
            if (!el) return;
            var href = el.getAttribute('href');
            if (!href || href === '#' || href.startsWith('javascript:')) return;
            // Let Microsoft link clicks navigate naturally inside the embedded
            // view so inline previews render and "Download" buttons reach
            // on_download (which shows the save dialog and auto-opens PDF /
            // Office documents with the OS default viewer).
            if (isMsUrl(href)) return;

            e.preventDefault();
            e.stopPropagation();
            window.__TAURI__.core.invoke('open_external_url', {
                url: new URL(href, location.href).href
            });
        }

        document.addEventListener('pointerdown', rememberDownloadName, true);
        document.addEventListener('mousedown', rememberDownloadName, true);
        document.addEventListener('click', rememberDownloadName, true);
        document.addEventListener('click', interceptClick, true);

        // Inject click handler into same-origin iframes too (skip ad/tracking iframes).
        function attachToMailIframes() {
            document.querySelectorAll('iframe').forEach(function(iframe) {
                var src = iframe.src || '';
                if (src.includes('adnxs') || src.includes('adsdk') || src.includes('doubleclick') || src.includes('ads.')) return;
                try {
                    var doc = iframe.contentDocument;
                    if (doc && !doc.__freelook_interceptor__) {
                        doc.__freelook_interceptor__ = true;
                        doc.addEventListener('click', interceptClick, true);
                    }
                } catch(e) {} // Cross-origin, skip
            });
        }

        attachToMailIframes();
        setTimeout(attachToMailIframes, 1000);
        setTimeout(attachToMailIframes, 3000);
        document.addEventListener('load', function(e) {
            if (e.target && e.target.tagName === 'IFRAME') {
                attachToMailIframes();
            }
        }, true);
        if (document.readyState !== 'complete') {
            window.addEventListener('load', attachToMailIframes);
        }
    })();
    "#;
    template.replace("__MS_DOMAINS__", &ms_domains_json)
}

/// Get the unread observer JavaScript
pub fn get_unread_js(app: &AppHandle) -> String {
    let unread_class = get_config_string(app, "unreadMessageClass", "");
    if unread_class.is_empty() {
        return String::new();
    }

    format!(
        r#"
        setTimeout(() => {{
            let unreadSpan = document.querySelector(".{unread_class}");
            if (!unreadSpan) return;

            window.__TAURI__.core.invoke('update_unread', {{ hasUnread: unreadSpan.hasChildNodes() }});

            let observer = new MutationObserver(mutations => {{
                window.__TAURI__.core.invoke('update_unread', {{ hasUnread: unreadSpan.hasChildNodes() }});

                var messages = document.querySelectorAll('div[role="listbox"][aria-label="Message list"]');
                if (messages.length) {{
                    var unread = messages[0].querySelectorAll('div[aria-label^="Unread"]');
                    var body = "";
                    for (var i = 0; i < unread.length; i++) {{
                        if (body.length) body += "\\n";
                        body += unread[i].getAttribute("aria-label").substring(7, 127);
                    }}
                    if (unread.length) {{
                        window.__TAURI__.notification.sendNotification({{
                            title: "Freelook - receiving " + unread.length + " NEW mails",
                            body: body,
                        }});
                    }}
                }}
            }});

            observer.observe(unreadSpan, {{ childList: true }});

            // Reminder observer
            let reminders = document.getElementsByClassName("_1BWPyOkN5zNVyfbTDKK1gM");
            let height = 0;
            let reminderObserver = new MutationObserver(mutations => {{
                if (reminders[0].clientHeight > height) {{
                    window.__TAURI__.core.invoke('show_main_window');
                }}
                height = reminders[0].clientHeight;
            }});

            if (reminders.length) {{
                reminderObserver.observe(reminders[0], {{ childList: true }});
            }}
        }}, 10000);
        "#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_outlook_and_microsoft_domains() {
        for raw in [
            "https://outlook.live.com/mail/0/inbox",
            "https://outlook.office.com/mail/",
            "https://outlook.office365.com/mail/deeplink/read/123",
            "https://outlook.cloud.microsoft/mail/",
            "https://m365.cloud.microsoft/apps",
            "https://outlook.com/",
            "https://www.microsoft365.com/",
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            "https://login.live.com/",
            "https://login.windows.net/tenant/oauth2/authorize",
            "https://attachment.outlook.live.net/owa/auth/123",
            "blob:https://outlook.cloud.microsoft/1234-5678",
            "about:blank",
        ] {
            let url = url::Url::parse(raw).unwrap();
            assert!(is_microsoft_url(&url), "expected in-app for {raw}");
        }
    }

    #[test]
    fn rejects_external_and_spoofed_domains() {
        for raw in [
            "https://example.com/article",
            "https://notoutlook.com/",
            "https://outlook.com.evil.com/",
            "https://cloud.microsoft.evil.com/",
            "https://nam12.safelinks.protection.outlook.com/?url=https%3A%2F%2Fexample.com",
            "mailto:user@example.com",
            "tel:+1234567890",
        ] {
            let url = url::Url::parse(raw).unwrap();
            assert!(!is_microsoft_url(&url), "expected external for {raw}");
        }
    }

    #[test]
    fn link_interceptor_shares_microsoft_domains() {
        let js = get_link_interceptor_js();
        assert!(js.contains("cloud.microsoft"));
        assert!(js.contains("microsoft365.com"));
        assert!(js.contains("outlook.com"));
        assert!(js.contains("safelinks.protection.outlook.com"));
    }
}
