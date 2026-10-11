// Gochi Browser - a lightweight, keyboard-driven WebKit browser built with
// Tauri + Rust + macOS WebKit (WKWebView via wry). No frontend bundler: all UI
// is local static HTML under `ui/`.
//
// UI: a single self-drawn tab strip sits in the top row of the window, level
// with the native macOS traffic lights. The native title bar is used as an
// overlay (TitleBarStyle::Overlay -> full-size content view), so the lights
// float above our strip and we leave room for them on the left. Page content
// starts BELOW the strip. Shortcuts are customizable in
//   ~/.config/gochi-browser/settings.json
//
// Webviews per window:
//   - "chrome"  : the trusted app UI (tab strip + omnibox + find bar). Local page.
//   - "tab-N"   : one per browser tab, external web content, below the strip.
//   - "overlay" : an ephemeral centered panel, created on demand. Modes:
//                   Cmd+P -> palette.html   (command palette: search + run any action)
//                   Cmd+O -> switcher.html  (tab switcher)
//                   Cmd+R -> address.html   (omnibox with live suggestions)
//                   Cmd+B -> picker.html    (bookmarks)
//                   Cmd+Y -> picker.html    (history)
//
// Data is persisted as JSON in ~/.config/gochi-browser/ :
//   settings.json  (user config)   session.json (open tabs / restore)
//   bookmarks.json (own bookmarks) history.json (own visit log)
//
// Threading rules (they were the cause of a deadlock):
//   - Creating/closing webviews from a Tauri *command* deadlocks, so any command
//     that does it is `async` (worker thread). Menu handlers spawn a worker too.
//   - The BrowserState mutex is never held while calling into Tauri APIs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    webview::{PageLoadEvent, WebviewBuilder},
    window::Color,
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Theme, TitleBarStyle, Url,
    WebviewUrl,
};

const WIN_W: f64 = 1200.0;
const WIN_H: f64 = 800.0;
const OVERLAY_LABEL: &str = "overlay";
const CHROME_LABEL: &str = "chrome";
/// Height of the custom tab strip drawn in the top row, in logical pixels.
/// The native traffic lights sit in roughly the same row.
const CHROME_H: f64 = 38.0;

/// Colours of the window background (also used by the tab strip CSS).
const TITLEBAR_LIGHT: (u8, u8, u8) = (236, 236, 238);
const TITLEBAR_DARK: (u8, u8, u8) = (30, 30, 32);

/// Page zoom steps (index 4 == 100%).
const ZOOM_STEPS: [f64; 12] =
    [0.5, 0.67, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0];
const ZOOM_DEFAULT_IDX: usize = 4;

/* ================================================================== actions */

/// Where (if anywhere) an action shows up in the native menu bar.
#[derive(Clone, Copy, PartialEq)]
enum MenuLoc {
    None,
    Commands,
    Tabs,
}

struct ActionDef {
    id: &'static str,
    title: &'static str,
    default: Option<&'static str>,
    menu: MenuLoc,
}

const ACTIONS: &[ActionDef] = &[
    // --- navigation / panels -------------------------------------------------
    ActionDef { id: "command_palette", title: "Command Palette", default: Some("CmdOrCtrl+P"), menu: MenuLoc::Commands },
    ActionDef { id: "open_omnibox", title: "Focus Address Bar", default: Some("CmdOrCtrl+L"), menu: MenuLoc::Commands },
    ActionDef { id: "open_address", title: "Open Address…", default: Some("CmdOrCtrl+R"), menu: MenuLoc::Commands },
    ActionDef { id: "open_switcher", title: "Go to Tab…", default: Some("CmdOrCtrl+O"), menu: MenuLoc::Commands },
    ActionDef { id: "open_bookmarks", title: "Open Bookmarks…", default: Some("CmdOrCtrl+B"), menu: MenuLoc::Commands },
    ActionDef { id: "open_history", title: "Open History…", default: Some("CmdOrCtrl+Y"), menu: MenuLoc::Commands },
    ActionDef { id: "toggle_bookmark", title: "Bookmark This Page", default: Some("CmdOrCtrl+D"), menu: MenuLoc::Commands },
    ActionDef { id: "find_in_page", title: "Find in Page…", default: Some("CmdOrCtrl+F"), menu: MenuLoc::Commands },
    // --- page ----------------------------------------------------------------
    ActionDef { id: "reload", title: "Reload Page", default: Some("CmdOrCtrl+Shift+R"), menu: MenuLoc::Commands },
    ActionDef { id: "force_reload", title: "Reload Ignoring Cache", default: Some("CmdOrCtrl+Alt+R"), menu: MenuLoc::Commands },
    ActionDef { id: "stop", title: "Stop Loading", default: None, menu: MenuLoc::Commands },
    ActionDef { id: "back", title: "Back", default: Some("Alt+Left"), menu: MenuLoc::Commands },
    ActionDef { id: "forward", title: "Forward", default: Some("Alt+Right"), menu: MenuLoc::Commands },
    ActionDef { id: "home", title: "Go Home", default: Some("CmdOrCtrl+Shift+H"), menu: MenuLoc::Commands },
    ActionDef { id: "zoom_in", title: "Zoom In", default: Some("CmdOrCtrl+="), menu: MenuLoc::Commands },
    ActionDef { id: "zoom_out", title: "Zoom Out", default: Some("CmdOrCtrl+-"), menu: MenuLoc::Commands },
    ActionDef { id: "zoom_reset", title: "Actual Size", default: Some("CmdOrCtrl+0"), menu: MenuLoc::Commands },
    ActionDef { id: "print_page", title: "Print…", default: None, menu: MenuLoc::Commands },
    ActionDef { id: "toggle_devtools", title: "Toggle Web Inspector", default: Some("CmdOrCtrl+Alt+I"), menu: MenuLoc::Commands },
    ActionDef { id: "clear_history", title: "Clear History", default: None, menu: MenuLoc::Commands },
    // --- tabs ----------------------------------------------------------------
    ActionDef { id: "new_tab", title: "New Tab", default: Some("CmdOrCtrl+T"), menu: MenuLoc::Tabs },
    ActionDef { id: "close_tab", title: "Close Tab", default: Some("CmdOrCtrl+W"), menu: MenuLoc::Tabs },
    ActionDef { id: "reopen_closed_tab", title: "Reopen Closed Tab", default: Some("CmdOrCtrl+Shift+T"), menu: MenuLoc::Tabs },
    ActionDef { id: "duplicate_tab", title: "Duplicate Tab", default: None, menu: MenuLoc::Tabs },
    ActionDef { id: "close_other_tabs", title: "Close Other Tabs", default: None, menu: MenuLoc::Tabs },
    ActionDef { id: "next_tab", title: "Next Tab", default: Some("Ctrl+Tab"), menu: MenuLoc::Tabs },
    ActionDef { id: "prev_tab", title: "Previous Tab", default: Some("Ctrl+Shift+Tab"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_1", title: "Tab 1", default: Some("CmdOrCtrl+1"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_2", title: "Tab 2", default: Some("CmdOrCtrl+2"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_3", title: "Tab 3", default: Some("CmdOrCtrl+3"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_4", title: "Tab 4", default: Some("CmdOrCtrl+4"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_5", title: "Tab 5", default: Some("CmdOrCtrl+5"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_6", title: "Tab 6", default: Some("CmdOrCtrl+6"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_7", title: "Tab 7", default: Some("CmdOrCtrl+7"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_8", title: "Tab 8", default: Some("CmdOrCtrl+8"), menu: MenuLoc::Tabs },
    ActionDef { id: "tab_9", title: "Last Tab", default: Some("CmdOrCtrl+9"), menu: MenuLoc::Tabs },
    // --- window / app --------------------------------------------------------
    ActionDef { id: "minimize_window", title: "Minimize Window", default: None, menu: MenuLoc::None },
    ActionDef { id: "toggle_fullscreen", title: "Toggle Fullscreen", default: None, menu: MenuLoc::None },
    ActionDef { id: "quit", title: "Quit Gochi Browser", default: None, menu: MenuLoc::None },
    // --- editing (native Edit menu is the reliable path; these run in the page)
    ActionDef { id: "undo", title: "Undo", default: None, menu: MenuLoc::None },
    ActionDef { id: "redo", title: "Redo", default: None, menu: MenuLoc::None },
    ActionDef { id: "cut", title: "Cut", default: None, menu: MenuLoc::None },
    ActionDef { id: "copy", title: "Copy", default: None, menu: MenuLoc::None },
    ActionDef { id: "paste", title: "Paste", default: None, menu: MenuLoc::None },
    ActionDef { id: "select_all", title: "Select All", default: None, menu: MenuLoc::None },
    ActionDef { id: "open_full_disk_access", title: "Grant Full Disk Access…", default: None, menu: MenuLoc::None },
];

/* ================================================================== settings */

#[derive(Clone, Deserialize)]
struct Settings {
    /// action id -> accelerator. `null` / "" / "none" disables the shortcut.
    #[serde(default)]
    shortcuts: HashMap<String, Option<String>>,
    /// Search URL: either a base results URL (query appended as `?q=`) or a
    /// template containing the `{query}` placeholder.
    #[serde(default = "default_search")]
    search: String,
    /// Page opened on startup / for new tabs.
    #[serde(default = "default_home")]
    home: String,
    /// Restore the tabs that were open when the app last quit.
    #[serde(default = "default_true")]
    restore_session: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shortcuts: HashMap::new(),
            search: default_search(),
            home: default_home(),
            restore_session: true,
        }
    }
}

fn default_search() -> String {
    "https://www.bing.com/search".to_string()
}
fn default_home() -> String {
    "https://duckduckgo.com/".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Serialize)]
struct ActionInfo {
    id: String,
    title: String,
    shortcut: Option<String>,
}

fn config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/gochi-browser"))
}

fn data_path(name: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(name))
}

fn write_json<T: Serialize>(path: &PathBuf, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string());
    std::fs::write(path, text)
}

fn read_json<T: DeserializeOwned + Default>(path: &PathBuf) -> T {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn load_settings() -> Settings {
    let Some(path) = config_dir().map(|d| d.join("settings.json")) else {
        return Settings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => {
            let _ = write_json(&path, &default_settings_value());
            Settings::default()
        }
    }
}

fn default_settings_value() -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for a in ACTIONS {
        if let Some(acc) = a.default {
            map.insert(a.id.to_string(), serde_json::Value::String(acc.to_string()));
        }
    }
    serde_json::json!({
        "search": default_search(),
        "home": default_home(),
        "restore_session": true,
        "shortcuts": map,
    })
}

fn search_template(app: &AppHandle) -> String {
    app.state::<Settings>().search.clone()
}
fn home_url(app: &AppHandle) -> String {
    app.state::<Settings>().home.clone()
}

fn resolved_shortcut(settings: &Settings, a: &ActionDef) -> Option<String> {
    if let Some(v) = settings.shortcuts.get(a.id) {
        return match v {
            None => None,
            Some(s) if s.is_empty() || s.eq_ignore_ascii_case("none") => None,
            Some(s) => Some(s.clone()),
        };
    }
    a.default.map(|s| s.to_string())
}

/// "CmdOrCtrl+Shift+R" -> "⇧⌘R"
fn pretty_accel(acc: &str) -> String {
    let mut mods = String::new();
    let mut key = String::new();
    for part in acc.split('+') {
        match part.to_ascii_lowercase().as_str() {
            "cmdorctrl" | "commandorcontrol" | "cmd" | "command" | "super" => mods.push('⌘'),
            "ctrl" | "control" => mods.push('⌃'),
            "shift" => mods.push('⇧'),
            "alt" | "option" => mods.push('⌥'),
            "" => key = "+".to_string(),
            other => key = other.to_uppercase(),
        }
    }
    format!("{mods}{key}")
}

/* ================================================================== state */

#[derive(Clone, Serialize)]
struct TabInfo {
    id: String,
    title: String,
    url: String,
    #[serde(default)]
    loading: bool,
}

#[derive(Clone, Serialize)]
struct TabsPayload {
    tabs: Vec<TabInfo>,
    active: Option<String>,
}

#[derive(Clone)]
struct ClosedTab {
    url: String,
    title: String,
}

#[derive(Default)]
struct BrowserState {
    tabs: Vec<TabInfo>,
    active: Option<String>,
    counter: u64,
    overlay: Option<String>,
    /// Stack of recently closed tabs (most recent last).
    closed: Vec<ClosedTab>,
    /// Per-tab zoom factor.
    zoom: HashMap<String, f64>,
    /// Per-tab load generation, used to cancel stale progress pollers.
    load_gen: HashMap<String, u64>,
    /// Tabs with the Web Inspector currently open (debug builds).
    devtools: HashSet<String>,
}

type Shared = Mutex<BrowserState>;

fn payload_of(st: &BrowserState) -> TabsPayload {
    TabsPayload { tabs: st.tabs.clone(), active: st.active.clone() }
}

fn next_tab_id(app: &AppHandle) -> String {
    let state = app.state::<Shared>();
    let mut st = state.lock().unwrap();
    st.counter += 1;
    format!("tab-{}", st.counter)
}

fn all_labels(app: &AppHandle) -> Vec<String> {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    st.tabs.iter().map(|t| t.id.clone()).collect()
}

fn active_tab_id(app: &AppHandle) -> Option<String> {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    st.active.clone()
}

fn require_active(app: &AppHandle) -> Result<String, String> {
    active_tab_id(app).ok_or_else(|| "no active tab".to_string())
}

fn active_title(app: &AppHandle) -> String {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    st.active
        .as_ref()
        .and_then(|id| st.tabs.iter().find(|t| &t.id == id))
        .map(|t| t.title.clone())
        .unwrap_or_else(|| "Gochi Browser".to_string())
}

fn set_window_title(app: &AppHandle, title: &str) {
    if let Some(w) = app.get_window("main") {
        let _ = w.set_title(title);
    }
}

/// Push the current tab list to the tab-strip webview so it re-renders.
fn emit_tabs(app: &AppHandle) {
    let payload = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        payload_of(&st)
    };
    let _ = app.emit_to(CHROME_LABEL, "tabs", payload);
}

#[derive(Clone, Serialize)]
struct LoadState {
    id: String,
    loading: bool,
    progress: f64,
}

fn emit_load(app: &AppHandle, id: &str, loading: bool, progress: f64) {
    let _ = app.emit_to(
        CHROME_LABEL,
        "load",
        LoadState { id: id.to_string(), loading, progress },
    );
}

fn set_tab_loading(app: &AppHandle, id: &str, loading: bool) {
    let state = app.state::<Shared>();
    let mut st = state.lock().unwrap();
    if let Some(t) = st.tabs.iter_mut().find(|t| t.id == id) {
        t.loading = loading;
    }
}

/* ================================================================== geometry */

fn window_logical_size(app: &AppHandle) -> (f64, f64) {
    if let Some(w) = app.get_window("main") {
        if let (Ok(size), Ok(scale)) = (w.inner_size(), w.scale_factor()) {
            let l = size.to_logical::<f64>(scale);
            return (l.width, l.height);
        }
    }
    (WIN_W, WIN_H)
}

fn content_height(h: f64) -> (f64, f64) {
    let chrome_h = CHROME_H.min(h);
    (chrome_h, (h - chrome_h).max(0.0))
}

fn layout(app: &AppHandle) {
    let Some(window) = app.get_window("main") else { return };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let logical = size.to_logical::<f64>(scale);
    let (chrome_h, content_h) = content_height(logical.height);

    if let Some(chrome) = app.get_webview(CHROME_LABEL) {
        let _ = chrome.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = chrome.set_size(LogicalSize::new(logical.width, chrome_h));
    }

    for label in all_labels(app) {
        if let Some(wv) = app.get_webview(&label) {
            let _ = wv.set_position(LogicalPosition::new(0.0, chrome_h));
            let _ = wv.set_size(LogicalSize::new(logical.width, content_h));
        }
    }

    if let Some(ov) = app.get_webview(OVERLAY_LABEL) {
        let _ = ov.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = ov.set_size(LogicalSize::new(logical.width, logical.height));
    }
}

fn show_only(app: &AppHandle, active: &str) {
    for label in all_labels(app) {
        if let Some(w) = app.get_webview(&label) {
            if label == active {
                let _ = w.show();
                let _ = w.set_focus();
            } else {
                let _ = w.hide();
            }
        }
    }
}

fn focus_active(app: &AppHandle) {
    if let Some(id) = active_tab_id(app) {
        if let Some(w) = app.get_webview(&id) {
            let _ = w.set_focus();
        }
    }
}

/// Focus the tab strip webview (so it receives keystrokes).
fn focus_chrome(app: &AppHandle) {
    if let Some(w) = app.get_webview(CHROME_LABEL) {
        let _ = w.set_focus();
    }
}

/// Send the tab strip a `ui` message without touching focus.
fn notify_chrome(app: &AppHandle, mode: &str) {
    let _ = app.emit_to(CHROME_LABEL, "ui", serde_json::json!({ "mode": mode }));
}

/// Ask the tab strip to switch into a mode ("omnibox" / "find") and focus it.
fn tell_chrome(app: &AppHandle, mode: &str) {
    notify_chrome(app, mode);
    focus_chrome(app);
}

/* ================================================================== macOS glue */

/// Run `f` against the raw WKWebView pointer on the main thread, returning its
/// result (or `None` if the webview is gone / timed out).
#[cfg(target_os = "macos")]
fn with_wk<T: Send + 'static>(
    webview: &tauri::Webview,
    f: impl FnOnce(*mut objc2::runtime::AnyObject) -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    webview
        .with_webview(move |platform| {
            let ptr: *mut objc2::runtime::AnyObject = platform.inner().cast();
            let out = if ptr.is_null() { None } else { Some(f(ptr)) };
            let _ = tx.send(out);
        })
        .ok()?;
    rx.recv_timeout(Duration::from_millis(2000)).ok().flatten()
}

#[cfg(target_os = "macos")]
fn read_load(webview: &tauri::Webview) -> Option<(bool, f64)> {
    with_wk(webview, |ptr| unsafe {
        use objc2::{msg_send, runtime::Bool};
        let loading: Bool = msg_send![ptr, isLoading];
        let progress: f64 = msg_send![ptr, estimatedProgress];
        (loading.as_bool(), progress)
    })
}

/// The main-frame URL, straight from WKWebView (`webview.URL`).
#[cfg(target_os = "macos")]
fn read_wk_url(webview: &tauri::Webview) -> Option<String> {
    with_wk(webview, |ptr| unsafe {
        use objc2::msg_send;
        let nsurl: *mut objc2::runtime::AnyObject = msg_send![ptr, URL];
        if nsurl.is_null() {
            return None;
        }
        let nsstr: *mut objc2::runtime::AnyObject = msg_send![nsurl, absoluteString];
        if nsstr.is_null() {
            return None;
        }
        let cstr: *const std::ffi::c_char = msg_send![nsstr, UTF8String];
        if cstr.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(cstr).to_string_lossy().into_owned())
    })
    .flatten()
}

#[cfg(target_os = "macos")]
fn wk_stop(webview: &tauri::Webview) {
    let _ = with_wk(webview, |ptr| unsafe {
        use objc2::msg_send;
        let _: () = msg_send![ptr, stopLoading];
    });
}

#[cfg(not(target_os = "macos"))]
fn read_load(_webview: &tauri::Webview) -> Option<(bool, f64)> {
    Some((false, 1.0))
}
#[cfg(not(target_os = "macos"))]
fn read_wk_url(_webview: &tauri::Webview) -> Option<String> {
    None
}
#[cfg(not(target_os = "macos"))]
fn wk_stop(_webview: &tauri::Webview) {}

/// Round the visible corners of a page webview (macOS only).
#[cfg(target_os = "macos")]
fn apply_corner_radius(webview: &tauri::Webview, radius: f64) {
    let _ = webview.with_webview(move |platform| unsafe {
        use objc2::msg_send;
        if platform.inner().is_null() {
            return;
        }
        let view: *mut objc2::runtime::AnyObject = platform.inner().cast();
        let _: () = msg_send![view, setWantsLayer: true];
        let layer: *mut objc2::runtime::AnyObject = msg_send![view, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setCornerRadius: radius];
            let _: () = msg_send![layer, setMasksToBounds: true];
        }
    });
}
#[cfg(not(target_os = "macos"))]
fn apply_corner_radius(_webview: &tauri::Webview, _radius: f64) {}

/// Hide the native window title text (it is drawn on top of our tab strip).
#[cfg(target_os = "macos")]
fn hide_window_title(webview: &tauri::Webview) {
    let _ = webview.with_webview(move |platform| unsafe {
        use objc2::msg_send;
        if platform.inner().is_null() {
            return;
        }
        let view: *mut objc2::runtime::AnyObject = platform.inner().cast();
        let ns_window: *mut objc2::runtime::AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return;
        }
        // NSWindowTitleVisibility: 0 = visible, 1 = hidden.
        let _: () = msg_send![ns_window, setTitleVisibility: 1isize];
    });
}
#[cfg(not(target_os = "macos"))]
fn hide_window_title(_webview: &tauri::Webview) {}

/// Create the trusted tab-strip webview in the top row of the window.
fn create_chrome(app: &AppHandle) -> Result<(), String> {
    let (w, h) = window_logical_size(app);
    let (chrome_h, _) = content_height(h);
    let window = app.get_window("main").ok_or("no main window")?;
    let webview = window
        .add_child(
            WebviewBuilder::new(CHROME_LABEL, WebviewUrl::App("chrome.html".into())),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(w, chrome_h),
        )
        .map_err(|e| e.to_string())?;
    hide_window_title(&webview);
    Ok(())
}

/* ================================================================== overlay */

fn close_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview(OVERLAY_LABEL) {
        let _ = w.close();
    }
    let state = app.state::<Shared>();
    state.lock().unwrap().overlay = None;
}

fn overlay_mode_of(app: &AppHandle) -> Option<String> {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    st.overlay.clone()
}

fn open_overlay(app: &AppHandle, mode: &str) -> Result<(), String> {
    if let Some(existing) = overlay_mode_of(app) {
        close_overlay(app);
        if existing == mode {
            focus_active(app);
            return Ok(());
        }
    }

    let page = match mode {
        "address" => "address.html",
        "palette" => "palette.html",
        "bookmarks" | "history" => "picker.html",
        _ => "switcher.html",
    };
    let window = app.get_window("main").ok_or("no main window")?;
    let (w, h) = window_logical_size(app);
    let webview = window
        .add_child(
            WebviewBuilder::new(OVERLAY_LABEL, WebviewUrl::App(page.into())).transparent(true),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(w, h),
        )
        .map_err(|e| e.to_string())?;

    {
        let state = app.state::<Shared>();
        state.lock().unwrap().overlay = Some(mode.to_string());
    }
    let _ = webview.set_focus();
    Ok(())
}

/* ================================================================== url */

fn encode_query(s: &str) -> String {
    let mut u = Url::parse("https://q.invalid/").expect("valid url");
    u.query_pairs_mut().append_pair("q", s);
    u.query().unwrap_or("").strip_prefix("q=").unwrap_or("").to_string()
}

fn normalize(input: &str, search: &str, home: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return home.to_string();
    }
    if let Ok(u) = Url::parse(s) {
        if matches!(u.scheme(), "http" | "https" | "tauri" | "about" | "file") {
            return u.to_string();
        }
    }
    let looks_like_domain = !s.contains(char::is_whitespace)
        && (s.contains('.') || s.starts_with("localhost") || s.starts_with("127.0.0.1"));
    if looks_like_domain {
        if let Ok(u) = Url::parse(&format!("https://{s}")) {
            return u.to_string();
        }
    }
    if search.contains("{query}") {
        return search.replace("{query}", &encode_query(s));
    }
    let mut u = Url::parse(search).unwrap_or_else(|_| Url::parse(&default_search()).expect("valid"));
    u.query_pairs_mut().append_pair("q", s);
    u.to_string()
}

/* ================================================================== persistence */

#[derive(Serialize, Deserialize, Default)]
struct Session {
    tabs: Vec<String>,
    #[serde(default)]
    active: usize,
}

#[derive(Clone, Serialize, Deserialize)]
struct Bookmark {
    title: String,
    url: String,
    #[serde(default)]
    added: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Visit {
    url: String,
    title: String,
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    visits: u64,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn save_session_async(app: &AppHandle) {
    let (tabs, active) = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        let tabs: Vec<String> =
            st.tabs.iter().map(|t| t.url.clone()).filter(|u| !u.is_empty()).collect();
        let active =
            st.active.as_ref().and_then(|a| st.tabs.iter().position(|t| &t.id == a)).unwrap_or(0);
        (tabs, active)
    };
    std::thread::spawn(move || {
        if let Some(path) = data_path("session.json") {
            let _ = write_json(&path, &Session { tabs, active });
        }
    });
}

fn load_session() -> Option<Session> {
    let path = data_path("session.json")?;
    let text = std::fs::read_to_string(path).ok()?;
    let session: Session = serde_json::from_str(&text).ok()?;
    if session.tabs.is_empty() {
        None
    } else {
        Some(session)
    }
}

/* ---- bookmarks ---- */

fn path_bookmarks() -> Option<PathBuf> {
    data_path("bookmarks.json")
}

fn read_bookmarks_data() -> Vec<Bookmark> {
    path_bookmarks().map(|p| read_json(&p)).unwrap_or_default()
}

fn toggle_bookmark(url: &str, title: &str) -> bool {
    let Some(path) = path_bookmarks() else { return false };
    let mut list: Vec<Bookmark> = read_json(&path);
    if let Some(i) = list.iter().position(|b| b.url == url) {
        list.remove(i);
        let _ = write_json(&path, &list);
        false
    } else {
        list.insert(0, Bookmark { title: title.to_string(), url: url.to_string(), added: now_secs() });
        let _ = write_json(&path, &list);
        true
    }
}

fn delete_bookmark(url: &str) {
    let Some(path) = path_bookmarks() else { return };
    let mut list: Vec<Bookmark> = read_json(&path);
    list.retain(|b| b.url != url);
    let _ = write_json(&path, &list);
}

/* ---- history ---- */

const HISTORY_CAP: usize = 5000;

/// Serializes writes to history.json (visits are recorded from worker threads).
static HISTORY_LOCK: Mutex<()> = Mutex::new(());

fn path_history() -> Option<PathBuf> {
    data_path("history.json")
}

fn read_history_data() -> Vec<Visit> {
    path_history().map(|p| read_json(&p)).unwrap_or_default()
}

fn record_visit(url: String, title: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    std::thread::spawn(move || {
        let Some(path) = path_history() else { return };
        let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut list: Vec<Visit> = read_json(&path);
        if let Some(v) = list.iter_mut().find(|v| v.url == url) {
            v.ts = now_secs();
            v.visits += 1;
            if !title.is_empty() {
                v.title = title;
            }
        } else {
            list.push(Visit { url, title, ts: now_secs(), visits: 1 });
        }
        list.sort_by(|a, b| b.ts.cmp(&a.ts));
        list.truncate(HISTORY_CAP);
        let _ = write_json(&path, &list);
    });
}

fn clear_history_data() {
    if let Some(path) = path_history() {
        let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = write_json(&path, &Vec::<Visit>::new());
    }
}

/* ================================================================== safari data */

const FDA_HINT: &str =
    "Cannot read Safari data. Grant “Full Disk Access” to Gochi Browser in System Settings → Privacy & Security.";

#[derive(Clone, Debug, Serialize)]
struct PickItem {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<f64>,
    #[serde(default)]
    removable: bool,
}

impl PickItem {
    fn new(title: String, url: String) -> Self {
        PickItem { title, url, subtitle: None, ts: None, removable: false }
    }
}

#[derive(Clone, Serialize)]
struct PickerPayload {
    kind: String,
    items: Vec<PickItem>,
}

fn safari_file(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Safari").join(name)
}

fn read_safari_bookmarks() -> Result<Vec<PickItem>, String> {
    let path = safari_file("Bookmarks.plist");
    let value = plist::Value::from_file(&path).map_err(|e| format!("{FDA_HINT} ({e})"))?;
    let mut out = Vec::new();
    collect_bookmarks(&value, "", &mut out);
    Ok(out)
}

fn collect_bookmarks(node: &plist::Value, folder: &str, out: &mut Vec<PickItem>) {
    let Some(dict) = node.as_dictionary() else { return };
    let kind = dict.get("WebBookmarkType").and_then(|v| v.as_string()).unwrap_or("");
    let title = dict.get("Title").and_then(|v| v.as_string()).unwrap_or("");

    match kind {
        "WebBookmarkTypeLeaf" => {
            let url = dict.get("URLString").and_then(|v| v.as_string()).unwrap_or("");
            if url.is_empty() {
                return;
            }
            let leaf_title = dict
                .get("URIDictionary")
                .and_then(|v| v.as_dictionary())
                .and_then(|d| d.get("title"))
                .and_then(|v| v.as_string())
                .unwrap_or(title);
            let folder = if folder.is_empty() { "Safari" } else { folder };
            let mut item = PickItem::new(leaf_title.to_string(), url.to_string());
            item.subtitle = Some(folder.to_string());
            out.push(item);
        }
        _ => {
            let child_folder = if title.is_empty() {
                folder.to_string()
            } else if title == "com.apple.ReadingList" {
                "Reading List".to_string()
            } else {
                title.to_string()
            };
            if let Some(children) = dict.get("Children").and_then(|v| v.as_array()) {
                for child in children {
                    collect_bookmarks(child, &child_folder, out);
                }
            }
        }
    }
}

fn read_safari_history(limit: i64) -> Result<Vec<PickItem>, String> {
    let src = safari_file("History.db");
    if !src.exists() {
        return Err("Safari History.db was not found.".to_string());
    }
    let tmp_dir = std::env::temp_dir().join("gochi-safari");
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
    let tmp = tmp_dir.join("History.db");
    std::fs::copy(&src, &tmp).map_err(|e| format!("{FDA_HINT} ({e})"))?;
    for ext in ["-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{ext}", src.display()));
        if from.exists() {
            let _ = std::fs::copy(&from, format!("{}{ext}", tmp.display()));
        }
    }

    let conn = rusqlite::Connection::open(&tmp).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT hi.url, hv.title, hv.visit_time \
             FROM history_visits hv JOIN history_items hi ON hi.id = hv.history_item \
             ORDER BY hv.visit_time DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for row in rows {
        let (url, title, visit_time) = row.map_err(|e| e.to_string())?;
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        let unix = visit_time + 978_307_200.0; // Core Foundation -> Unix
        let mut item =
            PickItem::new(title.filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone()), url);
        item.ts = Some(unix);
        item.subtitle = Some("Safari".to_string());
        items.push(item);
    }
    Ok(items)
}

fn open_privacy_settings() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/* ================================================================== tabs */

/// Poll a loading tab until it settles. This is the single source of truth for
/// `loading` and for the tab's URL: it reads the real WKWebView `isLoading`,
/// `estimatedProgress` and main-frame `URL`, so subframe navigations and failed
/// loads can never leave the spinner stuck. When the load settles it records a
/// history visit and persists the session.
fn start_progress(app: &AppHandle, id: &str) {
    let gen = {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        let g = st.load_gen.entry(id.to_string()).or_insert(0);
        *g += 1;
        *g
    };
    let app = app.clone();
    let id = id.to_string();
    std::thread::spawn(move || {
        let mut misses = 0u32;
        let mut last_url = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(120));
            let current = {
                let state = app.state::<Shared>();
                let st = state.lock().unwrap();
                st.load_gen.get(&id).copied()
            };
            if current != Some(gen) {
                return;
            }
            let Some(wv) = app.get_webview(&id) else { return };
            let Some((loading, progress)) = read_load(&wv) else {
                misses += 1;
                if misses > 8 {
                    // Could not read the webview; settle anyway so we never spin forever.
                    set_tab_loading(&app, &id, false);
                    emit_load(&app, &id, false, 1.0);
                    emit_tabs(&app);
                    return;
                }
                continue;
            };
            misses = 0;

            let url = read_wk_url(&wv);
            let (active, url_changed) = {
                let state = app.state::<Shared>();
                let mut st = state.lock().unwrap();
                let Some(tab) = st.tabs.iter_mut().find(|t| t.id == id) else {
                    return;
                };
                tab.loading = loading;
                let mut changed = false;
                if let Some(u) = url {
                    if u != last_url {
                        last_url = u.clone();
                        tab.url = u;
                        changed = true;
                    }
                }
                (st.active.as_deref() == Some(id.as_str()), changed)
            };
            let _ = active;
            if url_changed {
                emit_tabs(&app);
            }
            emit_load(&app, &id, loading, progress);

            if !loading {
                finalize_load(&app, &id);
                emit_tabs(&app);
                return;
            }
        }
    });
}

/// A load just settled: record the visit and persist the session.
fn finalize_load(app: &AppHandle, id: &str) {
    let (url, title) = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        st.tabs
            .iter()
            .find(|t| t.id == id)
            .map(|t| (t.url.clone(), t.title.clone()))
            .unwrap_or_default()
    };
    record_visit(url, title);
    save_session_async(app);
}

fn tab_builder(app: &AppHandle, id: String, url: Url) -> WebviewBuilder<tauri::Wry> {
    let title_app = app.clone();
    let title_id = id.clone();
    let load_app = app.clone();
    let load_id = id.clone();
    let newwin_app = app.clone();

    WebviewBuilder::new(id, WebviewUrl::External(url))
        // NOTE: no `on_navigation` handler on purpose. WebKit calls the
        // navigation-policy delegate for *every* frame (iframes, ads, blocked
        // requests), and a URL/loading update there corrupts the tab state and
        // leaves the spinner spinning forever. Main-frame state is instead read
        // from the WKWebView itself while loading (see `start_progress`).
        .on_document_title_changed(move |_webview, title| {
            let is_active = {
                let state = title_app.state::<Shared>();
                let mut st = state.lock().unwrap();
                if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == title_id) {
                    tab.title = title.clone();
                }
                st.active.as_deref() == Some(title_id.as_str())
            };
            emit_tabs(&title_app);
            if is_active {
                set_window_title(&title_app, &title);
            }
        })
        .on_page_load(move |_webview, payload| match payload.event() {
            PageLoadEvent::Started => {
                // Safe to flag as loading here: this call also starts the
                // poller, which owns `loading` from now on and settles on the
                // real WKWebView `isLoading` value - so subframe/failed loads
                // can't leave the spinner stuck.
                set_tab_loading(&load_app, &load_id, true);
                emit_tabs(&load_app);
                start_progress(&load_app, &load_id);
            }
            PageLoadEvent::Finished => {
                set_tab_loading(&load_app, &load_id, false);
                emit_load(&load_app, &load_id, false, 1.0);
                emit_tabs(&load_app);
            }
        })
        // `target="_blank"` / `window.open` open a new tab in this window instead
        // of being silently dropped by WebKit.
        .on_new_window(move |url, _features| {
            let app = newwin_app.clone();
            let target = url.to_string();
            std::thread::spawn(move || {
                let _ = create_tab(&app, Some(target));
            });
            tauri::webview::NewWindowResponse::Deny
        })
}

fn create_tab(app: &AppHandle, url: Option<String>) -> Result<(), String> {
    close_overlay(app);

    let id = next_tab_id(app);
    let target = normalize(url.as_deref().unwrap_or(""), &search_template(app), &home_url(app));
    let parsed = Url::parse(&target).map_err(|e| e.to_string())?;
    let (w, h) = window_logical_size(app);
    let (chrome_h, content_h) = content_height(h);
    let window = app.get_window("main").ok_or("no main window")?;

    let webview = window
        .add_child(
            tab_builder(app, id.clone(), parsed),
            LogicalPosition::new(0.0, chrome_h),
            LogicalSize::new(w, content_h),
        )
        .map_err(|e| e.to_string())?;

    apply_corner_radius(&webview, 15.0);

    {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        st.tabs.push(TabInfo {
            id: id.clone(),
            title: "New Tab".into(),
            url: target.clone(),
            loading: true,
        });
        st.zoom.insert(id.clone(), ZOOM_STEPS[ZOOM_DEFAULT_IDX]);
        st.active = Some(id.clone());
    }

    for label in all_labels(app) {
        if label != id {
            if let Some(w) = app.get_webview(&label) {
                let _ = w.hide();
            }
        }
    }

    set_window_title(app, "New Tab");
    let _ = webview.set_focus();
    // Watch the first load (covers the case where the initial navigation fails
    // before committing, so the spinner still settles).
    start_progress(app, &id);
    emit_tabs(app);
    notify_chrome(app, "tabs");
    layout(app);
    save_session_async(app);
    Ok(())
}

fn switch_to_impl(app: &AppHandle, id: &str) -> Result<(), String> {
    {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        if !st.tabs.iter().any(|t| t.id == id) {
            return Err("unknown tab".into());
        }
        st.active = Some(id.to_string());
    }
    show_only(app, id);
    set_window_title(app, &active_title(app));
    close_overlay(app);
    emit_tabs(app);
    notify_chrome(app, "tabs");
    layout(app);
    save_session_async(app);
    Ok(())
}

fn switch_to_index(app: &AppHandle, index: usize) -> Result<(), String> {
    let (ids, len) = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        (st.tabs.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), st.tabs.len())
    };
    if len == 0 {
        return Ok(());
    }
    let idx = index.min(len - 1);
    switch_to_impl(app, &ids[idx])
}

fn close_tab_impl(app: &AppHandle, id: &str) -> Result<(), String> {
    let (next_active, empty) = {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        let idx = st
            .tabs
            .iter()
            .position(|t| t.id == id)
            .ok_or_else(|| "unknown tab".to_string())?;
        let removed = st.tabs.remove(idx);
        st.closed.push(ClosedTab { url: removed.url.clone(), title: removed.title.clone() });
        if st.closed.len() > 25 {
            st.closed.remove(0);
        }
        st.zoom.remove(id);
        st.load_gen.remove(id);
        // Bump the generation so any progress poller for this tab stops.
        if st.active.as_deref() == Some(id) {
            st.active = if st.tabs.is_empty() {
                None
            } else {
                let next = if idx < st.tabs.len() { idx } else { st.tabs.len() - 1 };
                Some(st.tabs[next].id.clone())
            };
        }
        (st.active.clone(), st.tabs.is_empty())
    };

    if let Some(w) = app.get_webview(id) {
        let _ = w.close();
    }

    if empty {
        if let Some(window) = app.get_window("main") {
            let _ = window.close();
        }
        return Ok(());
    }

    if let Some(active) = next_active {
        show_only(app, &active);
        set_window_title(app, &active_title(app));
    }
    emit_tabs(app);
    layout(app);
    save_session_async(app);
    Ok(())
}

fn close_active_tab_impl(app: &AppHandle) -> Result<(), String> {
    let id = require_active(app)?;
    close_tab_impl(app, &id)
}

fn cycle_tab(app: &AppHandle, delta: isize) {
    let (ids, active) = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        (st.tabs.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), st.active.clone())
    };
    if ids.len() < 2 {
        return;
    }
    let idx = active.and_then(|a| ids.iter().position(|x| *x == a)).unwrap_or(0);
    let n = ids.len() as isize;
    let next = ((idx as isize + delta).rem_euclid(n)) as usize;
    let _ = switch_to_impl(app, &ids[next]);
}

fn reopen_closed_tab(app: &AppHandle) -> Result<(), String> {
    let closed = {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        st.closed.pop()
    };
    match closed {
        Some(t) if !t.url.is_empty() => {
            create_tab(app, Some(t.url.clone()))?;
            // Show the remembered title until the page reports its own.
            if let Some(id) = active_tab_id(app) {
                {
                    let state = app.state::<Shared>();
                    let mut st = state.lock().unwrap();
                    if let Some(tab) = st.tabs.iter_mut().find(|x| x.id == id) {
                        tab.title = t.title.clone();
                    }
                }
                set_window_title(app, &t.title);
                emit_tabs(app);
            }
            Ok(())
        }
        _ => Err("no closed tab".into()),
    }
}

fn close_other_tabs(app: &AppHandle) -> Result<(), String> {
    let (active, others) = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        let active = st.active.clone();
        let others: Vec<String> =
            st.tabs.iter().filter(|t| Some(&t.id) != active.as_ref()).map(|t| t.id.clone()).collect();
        (active, others)
    };
    if active.is_none() {
        return Ok(());
    }
    for id in others {
        let _ = close_tab_impl(app, &id);
    }
    Ok(())
}

fn duplicate_tab(app: &AppHandle) -> Result<(), String> {
    let url = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        st.active
            .as_ref()
            .and_then(|a| st.tabs.iter().find(|t| &t.id == a))
            .map(|t| t.url.clone())
            .ok_or_else(|| "no active tab".to_string())?
    };
    create_tab(app, Some(url))
}

/* ================================================================== zoom / find / devtools */

fn set_tab_zoom(app: &AppHandle, delta: isize) -> Result<(), String> {
    let id = require_active(app)?;
    let current = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        st.zoom.get(&id).copied().unwrap_or(1.0)
    };
    let idx = ZOOM_STEPS
        .iter()
        .position(|s| (s - current).abs() < 0.001)
        .unwrap_or(ZOOM_DEFAULT_IDX);
    let next = (idx as isize + delta).clamp(0, ZOOM_STEPS.len() as isize - 1) as usize;
    let zoom = ZOOM_STEPS[next];
    if let Some(wv) = app.get_webview(&id) {
        let _ = wv.set_zoom(zoom);
    }
    {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        st.zoom.insert(id, zoom);
    }
    Ok(())
}

fn reset_tab_zoom(app: &AppHandle) -> Result<(), String> {
    let id = require_active(app)?;
    let zoom = ZOOM_STEPS[ZOOM_DEFAULT_IDX];
    if let Some(wv) = app.get_webview(&id) {
        let _ = wv.set_zoom(zoom);
    }
    {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        st.zoom.insert(id, zoom);
    }
    Ok(())
}

fn eval_active(app: &AppHandle, js: &str) -> Result<(), String> {
    let id = require_active(app)?;
    let w = app.get_webview(&id).ok_or("tab gone")?;
    let _ = w.set_focus();
    w.eval(js).map_err(|e| e.to_string())
}

/// Search the active page. `reset` clears the current selection first (start a
/// fresh search); otherwise it advances to the next/previous match. Note: this
/// deliberately does NOT focus the page, so the inline find bar keeps keyboard
/// focus while typing.
fn find_in_page(app: &AppHandle, query: &str, forward: bool, reset: bool) -> Result<(), String> {
    let id = require_active(app)?;
    let w = app.get_webview(&id).ok_or("tab gone")?;
    let q = serde_json::to_string(query).unwrap_or_else(|_| "\"\"".to_string());
    let backwards = if forward { "false" } else { "true" };
    let clear = if reset { "try{window.getSelection().removeAllRanges();}catch(e){}" } else { "" };
    let js = format!(
        "(function(){{{clear}return window.find({q}, false, {backwards}, true, false, true, false);}})()"
    );
    w.eval(js).map_err(|e| e.to_string())
}

fn toggle_devtools(app: &AppHandle) -> Result<(), String> {
    let id = require_active(app)?;
    let w = app.get_webview(&id).ok_or("tab gone")?;
    #[cfg(any(debug_assertions, feature = "devtools"))]
    {
        let open = {
            let state = app.state::<Shared>();
            let st = state.lock().unwrap();
            st.devtools.contains(&id)
        };
        if open {
            w.close_devtools();
        } else {
            w.open_devtools();
        }
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        if open {
            st.devtools.remove(&id);
        } else {
            st.devtools.insert(id);
        }
    }
    #[cfg(not(any(debug_assertions, feature = "devtools")))]
    {
        let _ = w;
        return Err("devtools are disabled in release builds".into());
    }
    Ok(())
}

/* ================================================================== action dispatch */

fn dispatch_action(app: &AppHandle, id: &str) -> Result<(), String> {
    // Overlay-opening actions handle their own toggling.
    match id {
        "open_switcher" => return open_overlay(app, "tabs"),
        "open_address" => return open_overlay(app, "address"),
        "command_palette" => return open_overlay(app, "palette"),
        "open_bookmarks" => return open_overlay(app, "bookmarks"),
        "open_history" => return open_overlay(app, "history"),
        _ => {}
    }

    // Any other action dismisses a currently open overlay first.
    close_overlay(app);

    if let Some(rest) = id.strip_prefix("tab_") {
        if let Ok(n) = rest.parse::<usize>() {
            return switch_to_index(app, n.saturating_sub(1));
        }
    }

    match id {
        "open_omnibox" => {
            tell_chrome(app, "omnibox");
            Ok(())
        }
        "find_in_page" => {
            tell_chrome(app, "find");
            Ok(())
        }
        "toggle_bookmark" => {
            let (url, title) = {
                let state = app.state::<Shared>();
                let st = state.lock().unwrap();
                st.active
                    .as_ref()
                    .and_then(|a| st.tabs.iter().find(|t| &t.id == a))
                    .map(|t| (t.url.clone(), t.title.clone()))
                    .ok_or_else(|| "no active tab".to_string())?
            };
            if url.is_empty() || url == "about:blank" {
                return Err("nothing to bookmark".into());
            }
            let added = toggle_bookmark(&url, &title);
            notify_chrome(app, if added { "bookmarked" } else { "unbookmarked" });
            Ok(())
        }
        "new_tab" => create_tab(app, None),
        "close_tab" => close_active_tab_impl(app),
        "reopen_closed_tab" => reopen_closed_tab(app),
        "duplicate_tab" => duplicate_tab(app),
        "close_other_tabs" => close_other_tabs(app),
        "next_tab" => {
            cycle_tab(app, 1);
            Ok(())
        }
        "prev_tab" => {
            cycle_tab(app, -1);
            Ok(())
        }
        "reload" => reload_active(app, false),
        "force_reload" => reload_active(app, true),
        "stop" => {
            let tab = require_active(app)?;
            if let Some(wv) = app.get_webview(&tab) {
                wk_stop(&wv);
            }
            set_tab_loading(app, &tab, false);
            emit_tabs(app);
            Ok(())
        }
        "back" => eval_active(app, "window.history.back()"),
        "forward" => eval_active(app, "window.history.forward()"),
        "home" => {
            let home = home_url(app);
            navigate_active(app, &home)
        }
        "zoom_in" => set_tab_zoom(app, 1),
        "zoom_out" => set_tab_zoom(app, -1),
        "zoom_reset" => reset_tab_zoom(app),
        "print_page" => {
            let tab = require_active(app)?;
            app.get_webview(&tab).ok_or("tab gone")?.print().map_err(|e| e.to_string())
        }
        "toggle_devtools" => toggle_devtools(app),
        "clear_history" => {
            clear_history_data();
            Ok(())
        }
        "undo" => eval_active(app, "document.execCommand('undo')"),
        "redo" => eval_active(app, "document.execCommand('redo')"),
        "cut" => eval_active(app, "document.execCommand('cut')"),
        "copy" => eval_active(app, "document.execCommand('copy')"),
        "paste" => eval_active(app, "document.execCommand('paste')"),
        "select_all" => eval_active(app, "document.execCommand('selectAll')"),
        "minimize_window" => {
            if let Some(w) = app.get_window("main") {
                let _ = w.minimize();
            }
            Ok(())
        }
        "toggle_fullscreen" => {
            if let Some(w) = app.get_window("main") {
                let fs = w.is_fullscreen().unwrap_or(false);
                let _ = w.set_fullscreen(!fs);
            }
            Ok(())
        }
        "quit" => {
            app.exit(0);
            Ok(())
        }
        "open_full_disk_access" => open_privacy_settings(),
        other => Err(format!("unknown action: {other}")),
    }
}

fn navigate_active(app: &AppHandle, url: &str) -> Result<(), String> {
    let tab = require_active(app)?;
    let parsed = Url::parse(url).map_err(|e| e.to_string())?;
    app.get_webview(&tab).ok_or("tab gone")?.navigate(parsed).map_err(|e| e.to_string())
}

fn reload_active(app: &AppHandle, ignore_cache: bool) -> Result<(), String> {
    let tab = require_active(app)?;
    let wv = app.get_webview(&tab).ok_or("tab gone")?;
    set_tab_loading(app, &tab, true);
    emit_tabs(app);
    // The poller settles the spinner even if the reload never commits.
    start_progress(app, &tab);
    if ignore_cache {
        // Reload bypassing the cache.
        let url = wv.url().map_err(|e| e.to_string())?;
        wv.navigate(url).map_err(|e| e.to_string())?;
        return Ok(());
    }
    wv.reload().map_err(|e| e.to_string())
}

/* ================================================================== commands */

#[tauri::command]
fn get_tabs(app: AppHandle) -> TabsPayload {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    payload_of(&st)
}

/// Which overlay panel is currently open ("bookmarks" / "history" / …).
#[tauri::command]
fn overlay_mode(app: AppHandle) -> Option<String> {
    overlay_mode_of(&app)
}

/// Items for the generic picker overlay (bookmarks or history).
#[tauri::command]
async fn picker_items(app: AppHandle) -> Result<PickerPayload, String> {
    let mode = overlay_mode_of(&app);
    match mode.as_deref() {
        Some("bookmarks") => {
            let mut items: Vec<PickItem> = read_bookmarks_data()
                .into_iter()
                .map(|b| {
                    let mut it = PickItem::new(
                        if b.title.is_empty() { b.url.clone() } else { b.title },
                        b.url,
                    );
                    it.subtitle = Some("Bookmarks".into());
                    it.removable = true;
                    it
                })
                .collect();
            match read_safari_bookmarks() {
                Ok(mut safari) => items.append(&mut safari),
                Err(e) => {
                    if items.is_empty() {
                        return Err(e);
                    }
                }
            }
            Ok(PickerPayload { kind: "bookmarks".into(), items })
        }
        Some("history") => {
            let mut seen = HashSet::new();
            let mut items: Vec<PickItem> = Vec::new();
            for v in read_history_data() {
                if v.url.is_empty() || !seen.insert(v.url.clone()) {
                    continue;
                }
                let mut it = PickItem::new(
                    if v.title.is_empty() { v.url.clone() } else { v.title },
                    v.url,
                );
                it.ts = Some(v.ts as f64);
                items.push(it);
            }
            if let Ok(safari) = read_safari_history(1500) {
                for it in safari {
                    if seen.insert(it.url.clone()) {
                        items.push(it);
                    }
                }
            }
            items.sort_by(|a, b| b.ts.unwrap_or(0.0).partial_cmp(&a.ts.unwrap_or(0.0)).unwrap());
            items.truncate(2000);
            Ok(PickerPayload { kind: "history".into(), items })
        }
        other => Err(format!("no picker overlay open ({other:?})")),
    }
}

/// Live suggestions for the address panel.
#[tauri::command]
fn suggest(query: String) -> Vec<PickItem> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<PickItem> = Vec::new();
    let mut seen = HashSet::new();

    let matches = |title: &str, url: &str| -> bool {
        q.is_empty()
            || title.to_lowercase().contains(&q)
            || url.to_lowercase().contains(&q)
    };

    for b in read_bookmarks_data() {
        if matches(&b.title, &b.url) && seen.insert(b.url.clone()) {
            let mut it = PickItem::new(
                if b.title.is_empty() { b.url.clone() } else { b.title },
                b.url,
            );
            it.subtitle = Some("Bookmark".into());
            out.push(it);
        }
    }

    let mut history: Vec<Visit> = read_history_data();
    history.sort_by(|a, b| b.ts.cmp(&a.ts));
    for v in history {
        if out.len() >= 12 {
            break;
        }
        if v.url.is_empty() || !matches(&v.title, &v.url) || !seen.insert(v.url.clone()) {
            continue;
        }
        let mut it = PickItem::new(
            if v.title.is_empty() { v.url.clone() } else { v.title },
            v.url,
        );
        it.ts = Some(v.ts as f64);
        out.push(it);
    }
    out.truncate(12);
    out
}

#[tauri::command]
fn remove_bookmark(url: String) {
    delete_bookmark(&url);
}

#[tauri::command]
fn clear_history() {
    clear_history_data();
}

/// Open a URL in the current tab (or a new one) and dismiss the overlay.
#[tauri::command]
async fn open_url(app: AppHandle, url: String, new_tab: bool) -> Result<(), String> {
    let parsed = Url::parse(&url).map_err(|e| e.to_string())?;
    if new_tab {
        return create_tab(&app, Some(parsed.to_string()));
    }
    let id = require_active(&app)?;
    app.get_webview(&id)
        .ok_or("tab gone")?
        .navigate(parsed)
        .map_err(|e| e.to_string())?;
    close_overlay(&app);
    focus_active(&app);
    Ok(())
}

/// Submit text from the omnibox: normalize it (URL or search) and open it.
#[tauri::command]
async fn submit_url(app: AppHandle, input: String, new_tab: bool) -> Result<(), String> {
    let target = normalize(&input, &search_template(&app), &home_url(&app));
    if new_tab {
        return create_tab(&app, Some(target));
    }
    navigate_active(&app, &target)?;
    focus_active(&app);
    Ok(())
}

/// Search in the active page (inline find bar).
#[tauri::command]
async fn find_query(app: AppHandle, query: String, forward: bool, reset: bool) -> Result<(), String> {
    find_in_page(&app, &query, forward, reset)
}

/// Stop loading a specific tab.
#[tauri::command]
async fn stop_tab(app: AppHandle, id: String) -> Result<(), String> {
    if let Some(wv) = app.get_webview(&id) {
        wk_stop(&wv);
    }
    set_tab_loading(&app, &id, false);
    emit_tabs(&app);
    Ok(())
}

/// Give keyboard focus back to the page (after the strip had it).
#[tauri::command]
fn focus_page(app: AppHandle) {
    focus_active(&app);
}

#[tauri::command]
fn open_privacy_settings_cmd() -> Result<(), String> {
    open_privacy_settings()
}

#[tauri::command]
fn list_actions(app: AppHandle) -> Vec<ActionInfo> {
    let settings = app.state::<Settings>();
    ACTIONS
        .iter()
        .map(|a| ActionInfo {
            id: a.id.to_string(),
            title: a.title.to_string(),
            shortcut: resolved_shortcut(&settings, a).as_deref().map(pretty_accel),
        })
        .collect()
}

#[tauri::command]
async fn new_tab(app: AppHandle, url: Option<String>) -> Result<(), String> {
    create_tab(&app, url)
}

#[tauri::command]
async fn close_tab(app: AppHandle, id: String) -> Result<(), String> {
    close_tab_impl(&app, &id)
}

#[tauri::command]
async fn switch_to(app: AppHandle, id: String) -> Result<(), String> {
    switch_to_impl(&app, &id)
}

#[tauri::command]
async fn open_switcher(app: AppHandle) -> Result<(), String> {
    open_overlay(&app, "tabs")
}

#[tauri::command]
async fn open_address(app: AppHandle) -> Result<(), String> {
    open_overlay(&app, "address")
}

#[tauri::command]
async fn open_palette(app: AppHandle) -> Result<(), String> {
    open_overlay(&app, "palette")
}

#[tauri::command]
async fn dismiss_overlay(app: AppHandle) -> Result<(), String> {
    close_overlay(&app);
    focus_active(&app);
    Ok(())
}

#[tauri::command]
async fn run_action(app: AppHandle, id: String) -> Result<(), String> {
    dispatch_action(&app, &id)
}

/* ================================================================== menu */

fn menu_item(app: &AppHandle, a: &ActionDef, settings: &Settings) -> Option<MenuItem<tauri::Wry>> {
    let acc = resolved_shortcut(settings, a);
    MenuItem::with_id(app, a.id, a.title, true, acc.as_deref()).ok()
}

fn submenu_items(
    app: &AppHandle,
    settings: &Settings,
    loc: MenuLoc,
) -> Vec<MenuItem<tauri::Wry>> {
    ACTIONS
        .iter()
        .filter(|a| a.menu == loc)
        .filter_map(|a| menu_item(app, a, settings))
        .collect()
}

fn build_menu(app: &AppHandle, settings: &Settings) -> tauri::Result<Menu<tauri::Wry>> {
    let app_menu = Submenu::with_items(
        app,
        "Gochi Browser",
        true,
        &[
            &PredefinedMenuItem::about(app, None, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::hide(app, None)?,
            &PredefinedMenuItem::hide_others(app, None)?,
            &PredefinedMenuItem::show_all(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::quit(app, None)?,
        ],
    )?;

    let edit_menu = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;

    let commands = submenu_items(app, settings, MenuLoc::Commands);
    let command_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        commands.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let commands_menu = Submenu::with_items(app, "Commands", true, &command_refs)?;

    let tabs = submenu_items(app, settings, MenuLoc::Tabs);
    let tab_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        tabs.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let tabs_menu = Submenu::with_items(app, "Tabs", true, &tab_refs)?;

    let window_menu = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
        ],
    )?;

    Menu::with_items(app, &[&app_menu, &edit_menu, &commands_menu, &tabs_menu, &window_menu])
}

/* ================================================================== main */

fn main() {
    let settings = load_settings();
    let menu_settings = settings.clone();
    let setup_settings = settings.clone();

    tauri::Builder::default()
        .menu(move |handle| build_menu(handle, &menu_settings))
        .on_menu_event(|app, event| {
            let id = event.id().as_ref().to_string();
            if !ACTIONS.iter().any(|a| a.id == id) {
                return;
            }
            let handle = app.clone();
            std::thread::spawn(move || {
                let _ = dispatch_action(&handle, &id);
            });
        })
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            list_actions,
            new_tab,
            close_tab,
            switch_to,
            open_switcher,
            open_address,
            open_palette,
            dismiss_overlay,
            run_action,
            overlay_mode,
            picker_items,
            suggest,
            remove_bookmark,
            clear_history,
            open_url,
            submit_url,
            find_query,
            stop_tab,
            focus_page,
            open_privacy_settings_cmd
        ])
        .setup(move |app| {
            app.manage(Mutex::new(BrowserState::default()));
            app.manage(setup_settings.clone());

            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("")
                .inner_size(WIN_W, WIN_H)
                .min_inner_size(480.0, 360.0)
                .resizable(true)
                .visible(false)
                .build()?;

            // Overlay title bar: the traffic lights float above our tab strip and
            // the content view spans the whole window.
            let _ = window.set_title_bar_style(TitleBarStyle::Overlay);

            let dark = matches!(window.theme(), Ok(Theme::Dark));
            let (r, g, b) = if dark { TITLEBAR_DARK } else { TITLEBAR_LIGHT };
            let _ = window.set_background_color(Some(Color(r, g, b, 255)));

            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    layout(&handle);
                }
            });

            // Build the tab strip first, then the pages below it.
            create_chrome(app.handle())?;

            let restored = if setup_settings.restore_session { load_session() } else { None };
            match restored {
                Some(session) => {
                    for url in &session.tabs {
                        let _ = create_tab(app.handle(), Some(url.clone()));
                    }
                    // Restore the previously active tab.
                    let ids: Vec<String> = {
                        let state = app.state::<Shared>();
                        let st = state.lock().unwrap();
                        st.tabs.iter().map(|t| t.id.clone()).collect()
                    };
                    if let Some(id) = ids.get(session.active.min(ids.len().saturating_sub(1))) {
                        let _ = switch_to_impl(app.handle(), id);
                    }
                }
                None => {
                    create_tab(app.handle(), None)?;
                }
            }

            window.show()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
