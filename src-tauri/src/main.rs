// Gochi Browser - a lightweight WebKit browser prototype built with Tauri + Rust.
//
// Architecture: a single frameless native window hosts several webviews.
//   - "chrome"  : the trusted app UI (custom title bar + a minimal toolbar). A local page.
//   - "tab-N"   : one webview per browser tab, rendering external web content.
//   - "overlay" : an ephemeral full-window centered panel, created on demand so it
//                 always sits on top. Two modes share this one webview:
//                   Cmd+O -> switcher.html (tab switcher, searchable, Up/Down)
//                   Cmd+R -> address.html  (omnibox: search or enter an address)
//
// There is no tab strip and no inline address bar. Everything is keyboard driven:
//   Cmd+T new tab, Cmd+W close tab, Cmd+O tab switcher, Cmd+R address panel,
//   Cmd+Shift+R reload.
//
// Threading rules (they were the cause of a deadlock):
//   - Tauri commands run on the main thread. Creating/closing webviews from the
//     main thread deadlocks, so any command that does it is `async` (runs on a
//     worker thread; Tauri marshals webview calls back to the main thread).
//     Menu handlers (also main thread) spawn a worker thread for the same reason.
//   - The BrowserState mutex is never held while calling into Tauri APIs.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Mutex;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    webview::{PageLoadEvent, WebviewBuilder},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, WebviewUrl,
};

const HOME: &str = "https://duckduckgo.com/";
/// Combined height of the custom title bar + toolbar (chrome webview), in logical pixels.
const CHROME_H: f64 = 76.0;
const WIN_W: f64 = 1200.0;
const WIN_H: f64 = 800.0;
const OVERLAY_LABEL: &str = "overlay";

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
    /// Which overlay is currently open, if any: "tabs" or "address".
    overlay: Option<String>,
}

type SharedState = Mutex<BrowserState>;

/* ------------------------------------------------------------------ state helpers */

fn payload_of(st: &BrowserState) -> TabsPayload {
    TabsPayload {
        tabs: st.tabs.clone(),
        active: st.active.clone(),
    }
}

fn next_tab_id(app: &AppHandle) -> String {
    let state = app.state::<SharedState>();
    let mut st = state.lock().unwrap();
    st.counter += 1;
    format!("tab-{}", st.counter)
}

fn all_labels(app: &AppHandle) -> Vec<String> {
    let state = app.state::<SharedState>();
    let st = state.lock().unwrap();
    st.tabs.iter().map(|t| t.id.clone()).collect()
}

fn active_tab_id(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<SharedState>();
    let st = state.lock().unwrap();
    st.active.clone().ok_or_else(|| "no active tab".to_string())
}

fn active_title(app: &AppHandle) -> String {
    let state = app.state::<SharedState>();
    let st = state.lock().unwrap();
    st.active
        .as_ref()
        .and_then(|id| st.tabs.iter().find(|t| &t.id == id))
        .map(|t| t.title.clone())
        .unwrap_or_else(|| "Gochi Browser".to_string())
}

fn emit_title(app: &AppHandle, title: &str) {
    let _ = app.emit_to("chrome", "title", title.to_string());
}

/* ------------------------------------------------------------------ geometry */

fn window_logical_size(app: &AppHandle) -> (f64, f64) {
    if let Some(w) = app.get_window("main") {
        if let (Ok(size), Ok(scale)) = (w.inner_size(), w.scale_factor()) {
            let l = size.to_logical::<f64>(scale);
            return (l.width, l.height);
        }
    }
    (WIN_W, WIN_H)
}

/// Reposition/resize the chrome, every tab webview, and the overlay.
fn layout(app: &AppHandle) {
    let Some(window) = app.get_window("main") else {
        return;
    };
    let (Ok(size), Ok(scale)) = (window.inner_size(), window.scale_factor()) else {
        return;
    };
    let logical = size.to_logical::<f64>(scale);
    let width = logical.width;
    let content_h = (logical.height - CHROME_H).max(0.0);

    if let Some(chrome) = app.get_webview("chrome") {
        let _ = chrome.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = chrome.set_size(LogicalSize::new(width, CHROME_H.min(logical.height)));
    }

    for label in all_labels(app) {
        if let Some(wv) = app.get_webview(&label) {
            let _ = wv.set_position(LogicalPosition::new(0.0, CHROME_H));
            let _ = wv.set_size(LogicalSize::new(width, content_h));
        }
    }

    if let Some(ov) = app.get_webview(OVERLAY_LABEL) {
        let _ = ov.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = ov.set_size(LogicalSize::new(width, logical.height));
    }
}

/// Show the given tab and hide every other one.
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

/* ------------------------------------------------------------------ overlay */

fn close_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview(OVERLAY_LABEL) {
        let _ = w.close();
    }
    let state = app.state::<SharedState>();
    state.lock().unwrap().overlay = None;
}

fn focus_active(app: &AppHandle) {
    if let Ok(id) = active_tab_id(app) {
        if let Some(w) = app.get_webview(&id) {
            let _ = w.set_focus();
        }
    }
}

/// Open (or toggle) the centered overlay panel. `mode` is "tabs" or "address".
fn open_overlay(app: &AppHandle, mode: &str) -> Result<(), String> {
    let current = {
        let state = app.state::<SharedState>();
        let st = state.lock().unwrap();
        st.overlay.clone()
    };

    if let Some(existing) = current {
        close_overlay(app);
        if existing == mode {
            // Toggle off.
            focus_active(app);
            return Ok(());
        }
    }

    let page = match mode {
        "address" => "address.html",
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
        let state = app.state::<SharedState>();
        state.lock().unwrap().overlay = Some(mode.to_string());
    }
    let _ = webview.set_focus();
    Ok(())
}

/* ------------------------------------------------------------------ url helpers */

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

/* ------------------------------------------------------------------ tabs */

fn tab_builder(app: &AppHandle, id: String, url: Url) -> WebviewBuilder<tauri::Wry> {
    let nav_app = app.clone();
    let nav_id = id.clone();
    let title_app = app.clone();
    let title_id = id.clone();
    let load_app = app.clone();
    let load_id = id.clone();

    WebviewBuilder::new(id, WebviewUrl::External(url))
        .on_navigation(move |url| {
            let state = nav_app.state::<SharedState>();
            let mut st = state.lock().unwrap();
            if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == nav_id) {
                tab.url = url.to_string();
            }
            true
        })
        .on_document_title_changed(move |_webview, title| {
            let is_active = {
                let state = title_app.state::<SharedState>();
                let mut st = state.lock().unwrap();
                if let Some(tab) = st.tabs.iter_mut().find(|t| t.id == title_id) {
                    tab.title = title.clone();
                }
                st.active.as_deref() == Some(title_id.as_str())
            };
            if is_active {
                emit_title(&title_app, &title);
            }
        })
        .on_page_load(move |_webview, payload| {
            let is_active = {
                let state = load_app.state::<SharedState>();
                let st = state.lock().unwrap();
                st.active.as_deref() == Some(load_id.as_str())
            };
            if is_active {
                let loading = matches!(payload.event(), PageLoadEvent::Started);
                let _ = load_app.emit_to("chrome", "loading", loading);
            }
        })
}

fn create_tab(app: &AppHandle, url: Option<String>) -> Result<(), String> {
    // A new tab must never end up underneath the overlay.
    close_overlay(app);

    let id = next_tab_id(app);
    let target = normalize(url.as_deref().unwrap_or(""));
    let parsed = Url::parse(&target).map_err(|e| e.to_string())?;
    let window = app.get_window("main").ok_or("no main window")?;

    let webview = window
        .add_child(
            tab_builder(app, id.clone(), parsed),
            LogicalPosition::new(0.0, CHROME_H),
            LogicalSize::new(WIN_W, WIN_H - CHROME_H),
        )
        .map_err(|e| e.to_string())?;

    {
        let state = app.state::<SharedState>();
        let mut st = state.lock().unwrap();
        st.tabs.push(TabInfo {
            id: id.clone(),
            title: "New Tab".into(),
            url: target,
        });
        st.active = Some(id.clone());
    }

    for label in all_labels(app) {
        if label != id {
            if let Some(w) = app.get_webview(&label) {
                let _ = w.hide();
            }
        }
    }

    emit_title(app, "New Tab");
    let _ = webview.set_focus();
    layout(app);
    Ok(())
}

fn switch_to_impl(app: &AppHandle, id: &str) -> Result<(), String> {
    {
        let state = app.state::<SharedState>();
        let mut st = state.lock().unwrap();
        if !st.tabs.iter().any(|t| t.id == id) {
            return Err("unknown tab".into());
        }
        st.active = Some(id.to_string());
    }
    show_only(app, id);
    emit_title(app, &active_title(app));
    close_overlay(app);
    layout(app);
    Ok(())
}

fn close_tab_impl(app: &AppHandle, id: &str) -> Result<(), String> {
    let (next_active, empty) = {
        let state = app.state::<SharedState>();
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
        emit_title(app, &active_title(app));
    }
    layout(app);
    Ok(())
}

fn close_active_tab_impl(app: &AppHandle) -> Result<(), String> {
    let id = active_tab_id(app)?;
    close_tab_impl(app, &id)
}

/* ------------------------------------------------------------------ commands */

#[tauri::command]
fn get_tabs(app: AppHandle) -> TabsPayload {
    let state = app.state::<SharedState>();
    let st = state.lock().unwrap();
    payload_of(&st)
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
async fn close_active_tab(app: AppHandle) -> Result<(), String> {
    close_active_tab_impl(&app)
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
async fn dismiss_overlay(app: AppHandle) -> Result<(), String> {
    close_overlay(&app);
    focus_active(&app);
    Ok(())
}

#[tauri::command]
async fn navigate(app: AppHandle, url: String) -> Result<(), String> {
    let id = active_tab_id(&app)?;
    let parsed = Url::parse(&normalize(&url)).map_err(|e| e.to_string())?;
    app.get_webview(&id)
        .ok_or("tab gone")?
        .navigate(parsed)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn go_back(app: AppHandle) -> Result<(), String> {
    let id = active_tab_id(&app)?;
    app.get_webview(&id)
        .ok_or("tab gone")?
        .eval("history.length > 1 && history.back()")
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn go_forward(app: AppHandle) -> Result<(), String> {
    let id = active_tab_id(&app)?;
    app.get_webview(&id)
        .ok_or("tab gone")?
        .eval("history.forward()")
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reload(app: AppHandle) -> Result<(), String> {
    let id = active_tab_id(&app)?;
    app.get_webview(&id)
        .ok_or("tab gone")?
        .reload()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn window_minimize(app: AppHandle) {
    if let Some(w) = app.get_window("main") {
        let _ = w.minimize();
    }
}

#[tauri::command]
fn window_toggle_maximize(app: AppHandle) {
    if let Some(w) = app.get_window("main") {
        if w.is_maximized().unwrap_or(false) {
            let _ = w.unmaximize();
        } else {
            let _ = w.maximize();
        }
    }
}

#[tauri::command]
fn window_close(app: AppHandle) {
    if let Some(w) = app.get_window("main") {
        let _ = w.close();
    }
}

#[tauri::command]
fn window_start_drag(app: AppHandle) {
    if let Some(w) = app.get_window("main") {
        let _ = w.start_dragging();
    }
}

/* ------------------------------------------------------------------ menu */

fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
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

    let go_menu = Submenu::with_items(
        app,
        "Go",
        true,
        &[
            &MenuItem::with_id(app, "open-address", "Open Address…", true, Some("CmdOrCtrl+R"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "new-tab", "New Tab", true, Some("CmdOrCtrl+T"))?,
            &MenuItem::with_id(app, "close-tab", "Close Tab", true, Some("CmdOrCtrl+W"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "open-switcher", "Go to Tab…", true, Some("CmdOrCtrl+O"))?,
        ],
    )?;

    let view_menu = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &MenuItem::with_id(
                app,
                "reload",
                "Reload Page",
                true,
                Some("CmdOrCtrl+Shift+R"),
            )?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::fullscreen(app, None)?,
        ],
    )?;

    Menu::with_items(app, &[&app_menu, &edit_menu, &go_menu, &view_menu])
}

fn main() {
    tauri::Builder::default()
        .menu(|handle| build_menu(handle))
        .on_menu_event(|app, event| {
            let handle = app.clone();
            match event.id().as_ref() {
                "new-tab" => {
                    std::thread::spawn(move || {
                        let _ = create_tab(&handle, None);
                    });
                }
                "close-tab" => {
                    std::thread::spawn(move || {
                        let _ = close_active_tab_impl(&handle);
                    });
                }
                "open-switcher" => {
                    std::thread::spawn(move || {
                        let _ = open_overlay(&handle, "tabs");
                    });
                }
                "open-address" => {
                    std::thread::spawn(move || {
                        let _ = open_overlay(&handle, "address");
                    });
                }
                "reload" => {
                    if let Ok(id) = active_tab_id(app) {
                        if let Some(w) = app.get_webview(&id) {
                            let _ = w.reload();
                        }
                    }
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_tabs,
            new_tab,
            close_tab,
            close_active_tab,
            switch_to,
            open_switcher,
            open_address,
            dismiss_overlay,
            navigate,
            go_back,
            go_forward,
            reload,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_start_drag
        ])
        .setup(|app| {
            app.manage(Mutex::new(BrowserState::default()));

            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("Gochi Browser")
                .inner_size(WIN_W, WIN_H)
                .min_inner_size(480.0, 360.0)
                .decorations(false)
                .resizable(true)
                .build()?;

            window.add_child(
                WebviewBuilder::new("chrome", WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(WIN_W, CHROME_H),
            )?;

            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    layout(&handle);
                }
            });

            create_tab(app.handle(), Some(HOME.to_string()))?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
