// Gochi Browser - a lightweight WebKit browser prototype built with Tauri + Rust.
//
// UI: there is NO self-drawn chrome at all. The window is frameless-at-the-top
// (transparent native title bar, macOS traffic lights kept, content full-bleed).
// Everything is driven by keyboard shortcuts, all customizable in
//   ~/.config/gochi-browser/settings.json
//
// Webviews per window:
//   - "tab-N"   : one per browser tab, external web content, fills the window.
//                 A thin transparent drag strip is injected at the top so the
//                 window can still be moved (the page otherwise covers the
//                 native title bar).
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
    AppHandle, LogicalPosition, LogicalSize, Manager, TitleBarStyle, Url, WebviewUrl,
};

const HOME: &str = "https://duckduckgo.com/";
const WIN_W: f64 = 1200.0;
const WIN_H: f64 = 800.0;
const OVERLAY_LABEL: &str = "overlay";

/// Transparent strip injected into every page so the window stays draggable.
const DRAG_SCRIPT: &str = r#"
(function () {
  try {
    var ID = "__gochi_drag__";
    function install() {
      if (document.getElementById(ID)) return;
      var root = document.documentElement || document.body;
      if (!root) return;
      var d = document.createElement("div");
      d.id = ID;
      d.setAttribute("data-tauri-drag-region", "");
      d.style.cssText =
        "position:fixed;top:0;left:0;right:0;height:8px;z-index:2147483647;";
      root.appendChild(d);
    }
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", install);
    } else {
      install();
    }
  } catch (e) {}
})();
"#;

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
];

#[derive(Clone, Default, serde::Deserialize)]
struct Settings {
    /// action id -> accelerator. `null` / "" / "none" disables the shortcut.
    #[serde(default)]
    shortcuts: HashMap<String, Option<String>>,
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
    let root = serde_json::json!({ "shortcuts": map });
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

    for label in all_labels(app) {
        if let Some(wv) = app.get_webview(&label) {
            let _ = wv.set_position(LogicalPosition::new(0.0, 0.0));
            let _ = wv.set_size(LogicalSize::new(logical.width, logical.height));
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

fn normalize(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return HOME.to_string();
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
    let mut u = Url::parse("https://duckduckgo.com/").expect("valid search url");
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
        .initialization_script(DRAG_SCRIPT)
        .on_navigation(move |url| {
            let state = nav_app.state::<Shared>();
            let mut st = state.lock().unwrap();
            if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == nav_id) {
                tab.url = url.to_string();
            }
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
            if is_active {
                set_window_title(&title_app, &title);
            }
        })
}

fn create_tab(app: &AppHandle, url: Option<String>) -> Result<(), String> {
    close_overlay(app);

    let id = next_tab_id(app);
    let target = normalize(url.as_deref().unwrap_or(""));
    let parsed = Url::parse(&target).map_err(|e| e.to_string())?;
    let window = app.get_window("main").ok_or("no main window")?;

    let webview = window
        .add_child(
            tab_builder(app, id.clone(), parsed),
            LogicalPosition::new(0.0, 0.0),
            LogicalSize::new(WIN_W, WIN_H),
        )
        .map_err(|e| e.to_string())?;

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
        other => Err(format!("unknown action: {other}")),
    }
}

/* ================================================================== commands */

#[tauri::command]
fn get_tabs(app: AppHandle) -> TabsPayload {
    let state = app.state::<Shared>();
    let st = state.lock().unwrap();
    payload_of(&st)
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
    let parsed = Url::parse(&normalize(&url)).map_err(|e| e.to_string())?;
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
            navigate
        ])
        .setup(move |app| {
            app.manage(Mutex::new(BrowserState::default()));
            app.manage(setup_settings);

            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("Gochi Browser")
                .inner_size(WIN_W, WIN_H)
                .min_inner_size(480.0, 360.0)
                .resizable(true)
                .visible(false)
                .build()?;

            // Transparent native title bar: content is full-bleed while macOS
            // keeps its traffic lights.
            let _ = window.set_title_bar_style(TitleBarStyle::Overlay);

            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    layout(&handle);
                }
            });

            create_tab(app.handle(), Some(HOME.to_string()))?;

            window.show()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
