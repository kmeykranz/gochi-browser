// Gochi Browser - a lightweight WebKit browser prototype built with Tauri + Rust.
//
// UI: a single self-drawn tab strip sits in the top row of the window, level
// with the native macOS traffic lights. The native title bar is used as an
// overlay (TitleBarStyle::Overlay -> full-size content view), so the traffic
// lights float above our strip and we leave room for them on the left. Page
// content starts BELOW the strip. Shortcuts are customizable in
//   ~/.config/gochi-browser/settings.json
//
// Webviews per window:
//   - "chrome"  : the trusted tab strip (local page). Drawn at the very top,
//                 level with the traffic lights.
//   - "tab-N"   : one per browser tab, external web content, below the strip.
//   - "overlay" : an ephemeral centered panel, created on demand. Three modes:
//                   Cmd+P -> palette.html   (command palette: search + run any action)
//                   Cmd+O -> switcher.html  (tab switcher)
//                   Cmd+R -> address.html   (omnibox)
//
// Threading rules (they were the cause of a deadlock):
//   - Tauri commands run on the main thread. Creating/closing webviews from the
//     main thread deadlocks, so any command that does it is `async` (worker
//     thread). Menu handlers (main thread) spawn a worker thread for the same
//     reason.
//   - The BrowserState mutex is never held while calling into Tauri APIs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    webview::WebviewBuilder,
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Theme, TitleBarStyle, Url,
    WebviewUrl, window::Color,
};

const WIN_W: f64 = 1200.0;
const WIN_H: f64 = 800.0;
const OVERLAY_LABEL: &str = "overlay";
const CHROME_LABEL: &str = "chrome";
/// Height of the custom tab strip drawn in the top row, in logical pixels.
/// The native traffic lights are vertically centred within roughly the same row.
const CHROME_H: f64 = 38.0;

/// Colour of the native (transparent) title bar strip that sits above the page.
const TITLEBAR_LIGHT: (u8, u8, u8) = (236, 236, 238);
const TITLEBAR_DARK: (u8, u8, u8) = (30, 30, 32);

/* ================================================================== actions */

struct ActionDef {
    id: &'static str,
    title: &'static str,
    default: Option<&'static str>,
    /// Whether the action gets its own item in the macOS "Commands" menu.
    in_menu: bool,
}

const ACTIONS: &[ActionDef] = &[
    ActionDef { id: "command_palette", title: "Command Palette", default: Some("CmdOrCtrl+P"), in_menu: true },
    ActionDef { id: "open_address", title: "Open Address…", default: Some("CmdOrCtrl+R"), in_menu: true },
    ActionDef { id: "open_switcher", title: "Go to Tab…", default: Some("CmdOrCtrl+O"), in_menu: true },
    ActionDef { id: "open_bookmarks", title: "Open Bookmarks…", default: Some("CmdOrCtrl+B"), in_menu: true },
    ActionDef { id: "open_history", title: "Open History…", default: Some("CmdOrCtrl+Y"), in_menu: true },
    ActionDef { id: "new_tab", title: "New Tab", default: Some("CmdOrCtrl+T"), in_menu: true },
    ActionDef { id: "close_tab", title: "Close Tab", default: Some("CmdOrCtrl+W"), in_menu: true },
    ActionDef { id: "next_tab", title: "Next Tab", default: Some("Ctrl+Tab"), in_menu: true },
    ActionDef { id: "prev_tab", title: "Previous Tab", default: Some("Ctrl+Shift+Tab"), in_menu: true },
    ActionDef { id: "reload", title: "Reload Page", default: Some("CmdOrCtrl+Shift+R"), in_menu: true },
    ActionDef { id: "back", title: "Back", default: Some("Alt+Left"), in_menu: true },
    ActionDef { id: "forward", title: "Forward", default: Some("Alt+Right"), in_menu: true },
    // Not in the menu (handled by the native menus or rarely needed), but runnable
    // from the command palette.
    ActionDef { id: "minimize_window", title: "Minimize Window", default: None, in_menu: false },
    ActionDef { id: "toggle_fullscreen", title: "Toggle Fullscreen", default: None, in_menu: false },
    ActionDef { id: "quit", title: "Quit Gochi Browser", default: None, in_menu: false },
    ActionDef { id: "undo", title: "Undo", default: None, in_menu: false },
    ActionDef { id: "redo", title: "Redo", default: None, in_menu: false },
    ActionDef { id: "cut", title: "Cut", default: None, in_menu: false },
    ActionDef { id: "copy", title: "Copy", default: None, in_menu: false },
    ActionDef { id: "paste", title: "Paste", default: None, in_menu: false },
    ActionDef { id: "select_all", title: "Select All", default: None, in_menu: false },
    ActionDef { id: "open_full_disk_access", title: "Grant Full Disk Access…", default: None, in_menu: false },
];

#[derive(Clone, serde::Deserialize)]
struct Settings {
    /// action id -> accelerator. `null` / "" / "none" disables the shortcut.
    #[serde(default)]
    shortcuts: HashMap<String, Option<String>>,
    /// Search URL. Either a base results URL (the query is appended as `?q=`) or
    /// a template containing the `{query}` placeholder.
    #[serde(default = "default_search")]
    search: String,
    /// Page opened on startup and for new tabs.
    #[serde(default = "default_home")]
    home: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            shortcuts: HashMap::new(),
            search: default_search(),
            home: default_home(),
        }
    }
}

fn default_search() -> String {
    "https://www.bing.com/search".to_string()
}

fn default_home() -> String {
    "https://duckduckgo.com/".to_string()
}

fn search_template(app: &AppHandle) -> String {
    app.state::<Settings>().search.clone()
}

fn home_url(app: &AppHandle) -> String {
    app.state::<Settings>().home.clone()
}

#[derive(Clone, serde::Serialize)]
struct ActionInfo {
    id: String,
    title: String,
    shortcut: Option<String>,
    hint: Option<String>,
}

fn settings_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/gochi-browser/settings.json"))
}

fn default_settings_json() -> String {
    let mut map = serde_json::Map::new();
    for a in ACTIONS {
        if let Some(acc) = a.default {
            map.insert(a.id.to_string(), serde_json::Value::String(acc.to_string()));
        }
    }
    let root = serde_json::json!({
        "search": default_search(),
        "home": default_home(),
        "shortcuts": map,
    });
    serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string())
}

fn load_settings() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => {
            // First run: drop a template the user can edit.
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, default_settings_json());
            Settings::default()
        }
    }
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
            other => key = other.to_uppercase(),
        }
    }
    format!("{mods}{key}")
}

/* ================================================================== state */

#[derive(Clone, serde::Serialize)]
struct TabInfo {
    id: String,
    title: String,
    url: String,
}

#[derive(Clone, serde::Serialize)]
struct TabsPayload {
    tabs: Vec<TabInfo>,
    active: Option<String>,
}

#[derive(Default)]
struct BrowserState {
    tabs: Vec<TabInfo>,
    active: Option<String>,
    counter: u64,
    overlay: Option<String>,
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

fn layout(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let logical = size.to_logical::<f64>(scale);
    let chrome_h = CHROME_H.min(logical.height);
    let content_h = (logical.height - chrome_h).max(0.0);

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

/// Round the visible corners of a page webview (macOS only).
#[cfg(target_os = "macos")]
fn apply_corner_radius(webview: &tauri::Webview, radius: f64) {
    // Safety: `platform.inner()` is a raw WKWebView pointer; we only touch it
    // through Objective-C `msg_send!`.
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

/// Hide the native window title text. With `TitleBarStyle::Overlay` the title
/// bar is drawn on top of our tab strip, so the (centred) title would overlap
/// the tabs. The traffic lights stay visible.
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
    let window = app.get_window("main").ok_or("no main window")?;
    let webview = window
        .add_child(
            WebviewBuilder::new(CHROME_LABEL, WebviewUrl::App("chrome.html".into())),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(w, CHROME_H.min(h)),
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

fn open_overlay(app: &AppHandle, mode: &str) -> Result<(), String> {
    let current = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        st.overlay.clone()
    };
    if let Some(existing) = current {
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

/// Percent-encode a search query (application/x-www-form-urlencoded style).
fn encode_query(s: &str) -> String {
    let mut u = Url::parse("https://q.invalid/").expect("valid url");
    u.query_pairs_mut().append_pair("q", s);
    u.query()
        .unwrap_or("")
        .strip_prefix("q=")
        .unwrap_or("")
        .to_string()
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
    // Fall back to a web search using the configured engine.
    if search.contains("{query}") {
        return search.replace("{query}", &encode_query(s));
    }
    let mut u = Url::parse(search).unwrap_or_else(|_| Url::parse(&default_search()).expect("valid"));
    u.query_pairs_mut().append_pair("q", s);
    u.to_string()
}

/* ================================================================== tabs */

fn tab_builder(app: &AppHandle, id: String, url: Url) -> WebviewBuilder<tauri::Wry> {
    let nav_app = app.clone();
    let nav_id = id.clone();
    let title_app = app.clone();
    let title_id = id.clone();

    WebviewBuilder::new(id, WebviewUrl::External(url))
        .on_navigation(move |url| {
            let state = nav_app.state::<Shared>();
            {
                let mut st = state.lock().unwrap();
                if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == nav_id) {
                    tab.url = url.to_string();
                }
            }
            emit_tabs(&nav_app);
            true
        })
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
}

fn create_tab(app: &AppHandle, url: Option<String>) -> Result<(), String> {
    close_overlay(app);

    let id = next_tab_id(app);
    let target = normalize(url.as_deref().unwrap_or(""), &search_template(app), &home_url(app));
    let parsed = Url::parse(&target).map_err(|e| e.to_string())?;
    let (w, h) = window_logical_size(app);
    let chrome_h = CHROME_H.min(h);
    let window = app.get_window("main").ok_or("no main window")?;

    let webview = window
        .add_child(
            tab_builder(app, id.clone(), parsed),
            LogicalPosition::new(0.0, chrome_h),
            LogicalSize::new(w, (h - chrome_h).max(0.0)),
        )
        .map_err(|e| e.to_string())?;

    // Round the page corners for the card-like content area look.
    apply_corner_radius(&webview, 15.0);

    {
        let state = app.state::<Shared>();
        let mut st = state.lock().unwrap();
        st.tabs.push(TabInfo { id: id.clone(), title: "New Tab".into(), url: target });
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
    emit_tabs(app);
    layout(app);
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
    layout(app);
    Ok(())
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
        st.tabs.remove(idx);
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
    let idx = active
        .and_then(|a| ids.iter().position(|x| *x == a))
        .unwrap_or(0);
    let n = ids.len() as isize;
    let next = ((idx as isize + delta).rem_euclid(n)) as usize;
    let _ = switch_to_impl(app, &ids[next]);
}

/* ================================================================== action dispatch */

fn eval_active(app: &AppHandle, js: &str) -> Result<(), String> {
    let id = require_active(app)?;
    let w = app.get_webview(&id).ok_or("tab gone")?;
    let _ = w.set_focus();
    w.eval(js).map_err(|e| e.to_string())
}

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

    match id {
        "new_tab" => create_tab(app, None),
        "close_tab" => close_active_tab_impl(app),
        "next_tab" => {
            cycle_tab(app, 1);
            Ok(())
        }
        "prev_tab" => {
            cycle_tab(app, -1);
            Ok(())
        }
        "reload" => {
            let tab = require_active(app)?;
            app.get_webview(&tab)
                .ok_or("tab gone")?
                .reload()
                .map_err(|e| e.to_string())
        }
        "back" => eval_active(app, "history.length > 1 && history.back()"),
        "forward" => eval_active(app, "history.forward()"),
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

/* ================================================================== safari data */

const FDA_HINT: &str =
    "Cannot read Safari data. Grant “Full Disk Access” to Gochi Browser in System Settings → Privacy & Security.";

#[derive(Clone, Debug, serde::Serialize)]
struct PickItem {
    title: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ts: Option<f64>,
}

#[derive(Clone, serde::Serialize)]
struct PickerPayload {
    kind: String,
    items: Vec<PickItem>,
}

/// `~/Library/Safari` paths need Full Disk Access; missing permission shows up as
/// an I/O error, which we translate into a friendly message.
fn safari_file(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Safari").join(name)
}

fn read_bookmarks() -> Result<Vec<PickItem>, String> {
    let path = safari_file("Bookmarks.plist");
    let value = plist::Value::from_file(&path)
        .map_err(|e| format!("{FDA_HINT} ({e})"))?;
    let mut out = Vec::new();
    collect_bookmarks(&value, "", &mut out);
    Ok(out)
}

fn collect_bookmarks(node: &plist::Value, folder: &str, out: &mut Vec<PickItem>) {
    let Some(dict) = node.as_dictionary() else {
        return;
    };
    let kind = dict
        .get("WebBookmarkType")
        .and_then(|v| v.as_string())
        .unwrap_or("");
    let title = dict
        .get("Title")
        .and_then(|v| v.as_string())
        .unwrap_or("");

    match kind {
        "WebBookmarkTypeLeaf" => {
            let url = dict
                .get("URLString")
                .and_then(|v| v.as_string())
                .unwrap_or("");
            if url.is_empty() {
                return;
            }
            let leaf_title = dict
                .get("URIDictionary")
                .and_then(|v| v.as_dictionary())
                .and_then(|d| d.get("title"))
                .and_then(|v| v.as_string())
                .unwrap_or(title);
            let folder = if folder.is_empty() { "Bookmarks" } else { folder };
            out.push(PickItem {
                title: leaf_title.to_string(),
                url: url.to_string(),
                subtitle: Some(folder.to_string()),
                ts: None,
            });
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

fn read_history(limit: i64) -> Result<Vec<PickItem>, String> {
    let src = safari_file("History.db");
    if !src.exists() {
        return Err("Safari History.db was not found.".to_string());
    }

    // Work on a copy so we don't fight Safari's WAL lock.
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

    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for row in rows {
        let (url, title, visit_time) = row.map_err(|e| e.to_string())?;
        if url.is_empty() || !seen.insert(url.clone()) {
            continue;
        }
        // Safari stores Core Foundation absolute time (seconds since 2001-01-01).
        let unix = visit_time + 978_307_200.0;
        items.push(PickItem {
            title: title.filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone()),
            url,
            subtitle: None,
            ts: Some(unix),
        });
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
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    st.overlay.clone()
}

/// Items for the generic picker overlay (bookmarks or history).
#[tauri::command]
async fn picker_items(app: AppHandle) -> Result<PickerPayload, String> {
    let mode = {
        let state = app.state::<Shared>();
        let st = state.lock().unwrap();
        st.overlay.clone()
    };
    match mode.as_deref() {
        Some("bookmarks") => Ok(PickerPayload { kind: "bookmarks".into(), items: read_bookmarks()? }),
        Some("history") => Ok(PickerPayload { kind: "history".into(), items: read_history(2000)? }),
        other => Err(format!("no picker overlay open ({other:?})")),
    }
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

#[tauri::command]
fn open_privacy_settings_cmd() -> Result<(), String> {
    open_privacy_settings()
}

#[tauri::command]
fn list_actions(app: AppHandle) -> Vec<ActionInfo> {
    let settings = app.state::<Settings>();
    ACTIONS
        .iter()
        .map(|a| {
            let acc = resolved_shortcut(&settings, a);
            ActionInfo {
                id: a.id.to_string(),
                title: a.title.to_string(),
                shortcut: acc.as_deref().map(pretty_accel),
                hint: None,
            }
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

#[tauri::command]
async fn navigate(app: AppHandle, url: String) -> Result<(), String> {
    let tab = require_active(&app)?;
    let parsed = Url::parse(&normalize(&url, &search_template(&app), &home_url(&app)))
        .map_err(|e| e.to_string())?;
    app.get_webview(&tab)
        .ok_or("tab gone")?
        .navigate(parsed)
        .map_err(|e| e.to_string())
}

/* ================================================================== menu */

fn menu_item(app: &AppHandle, a: &ActionDef, settings: &Settings) -> Option<MenuItem<tauri::Wry>> {
    let acc = resolved_shortcut(settings, a)?;
    MenuItem::with_id(app, a.id, a.title, true, Some(acc.as_str())).ok()
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

    let items: Vec<MenuItem<tauri::Wry>> = ACTIONS
        .iter()
        .filter(|a| a.in_menu)
        .filter_map(|a| menu_item(app, a, settings))
        .collect();
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        items.iter().map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    let commands_menu = Submenu::with_items(app, "Commands", true, &refs)?;

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

    Menu::with_items(app, &[&app_menu, &edit_menu, &commands_menu, &window_menu])
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
            navigate,
            overlay_mode,
            picker_items,
            open_url,
            open_privacy_settings_cmd
        ])
        .setup(move |app| {
            app.manage(Mutex::new(BrowserState::default()));
            app.manage(setup_settings);

            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("")
                .inner_size(WIN_W, WIN_H)
                .min_inner_size(480.0, 360.0)
                .resizable(true)
                .visible(false)
                .build()?;

            // Overlay title bar: the traffic lights float above our tab strip, and
            // the content view spans the whole window so the strip can sit in the
            // top row, level with the lights.
            let _ = window.set_title_bar_style(TitleBarStyle::Overlay);

            // The transparent title bar shows the window's background colour, so
            // give it something that matches the current system appearance.
            let dark = matches!(window.theme(), Ok(Theme::Dark));
            let (r, g, b) = if dark { TITLEBAR_DARK } else { TITLEBAR_LIGHT };
            let _ = window.set_background_color(Some(Color(r, g, b, 255)));

            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    layout(&handle);
                }
            });

            // Build the tab strip first, then the starting page below it.
            create_chrome(app.handle())?;

            // Open the configured home page on startup.
            create_tab(app.handle(), None)?;

            window.show()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
