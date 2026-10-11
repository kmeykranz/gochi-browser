// Gochi Browser - a lightweight WebKit browser prototype built with Tauri + Rust.
//
// Architecture: a single native window hosts two webviews.
//   - "chrome"  : the trusted app UI (toolbar / address bar), a local Tauri page.
//   - "content" : the actual web page, an external URL rendered by the system WebKit.
// The chrome talks to Rust through commands; the content webview reports its
// navigation state back through events.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::{
    webview::{PageLoadEvent, WebviewBuilder},
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, Webview, WebviewUrl,
};

/// Home page opened on startup.
const HOME: &str = "https://duckduckgo.com/";
/// Height of the browser toolbar (chrome webview), in logical pixels.
const TOOLBAR_H: f64 = 44.0;
const WIN_W: f64 = 1200.0;
const WIN_H: f64 = 800.0;

/// Convenience accessor for the web content webview.
fn content(app: &AppHandle) -> Result<Webview, String> {
    app.get_webview("content")
        .ok_or_else(|| "content webview not found".to_string())
}

/// Turn arbitrary address-bar input into a navigable URL.
/// Accepts full URLs, bare domains, and falls back to a web search.
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

#[tauri::command]
fn navigate(app: AppHandle, url: String) -> Result<(), String> {
    let parsed = Url::parse(&normalize(&url)).map_err(|e| e.to_string())?;
    content(&app)?.navigate(parsed).map_err(|e| e.to_string())
}

#[tauri::command]
fn go_back(app: AppHandle) -> Result<(), String> {
    content(&app)?
        .eval("history.length > 1 && history.back()")
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn go_forward(app: AppHandle) -> Result<(), String> {
    content(&app)?
        .eval("history.forward()")
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reload(app: AppHandle) -> Result<(), String> {
    content(&app)?.reload().map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            navigate, go_back, go_forward, reload
        ])
        .setup(|app| {
            // Native window with no webview attached; we add children ourselves.
            let window = tauri::window::WindowBuilder::new(app, "main")
                .title("Gochi Browser")
                .inner_size(WIN_W, WIN_H)
                .min_inner_size(480.0, 360.0)
                .build()?;

            // Toolbar (trusted app UI).
            window.add_child(
                WebviewBuilder::new("chrome", WebviewUrl::App("index.html".into())),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(WIN_W, TOOLBAR_H),
            )?;

            // Web content (untrusted, external).
            let nav_target = app.handle().clone();
            let title_target = app.handle().clone();
            let load_target = app.handle().clone();

            let content_builder = WebviewBuilder::new(
                "content",
                WebviewUrl::External(Url::parse(HOME).expect("valid home url")),
            )
            .on_navigation(move |url| {
                let _ = nav_target.emit_to("chrome", "nav", url.to_string());
                true
            })
            .on_document_title_changed(move |_webview, title| {
                let _ = title_target.emit_to("chrome", "title", title);
            })
            .on_page_load(move |_webview, payload| {
                let loading = matches!(payload.event(), PageLoadEvent::Started);
                let _ = load_target.emit_to("chrome", "loading", loading);
            });

            let content = window.add_child(
                content_builder,
                LogicalPosition::new(0.0, TOOLBAR_H),
                LogicalSize::new(WIN_W, WIN_H - TOOLBAR_H),
            )?;

            // Keep the content webview filling the area below the toolbar on resize.
            // Child webviews don't follow the window unless auto resize is enabled,
            // and auto resize would scale proportionally (breaking the fixed toolbar).
            let resized = content.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    if let (Ok(size), Ok(scale)) =
                        (resized.window().inner_size(), resized.window().scale_factor())
                    {
                        let logical = size.to_logical::<f64>(scale);
                        let height = (logical.height - TOOLBAR_H).max(0.0);
                        let _ = resized.set_position(LogicalPosition::new(0.0, TOOLBAR_H));
                        let _ = resized.set_size(LogicalSize::new(logical.width, height));
                    }
                }
            });

            // Seed the address bar with the home page.
            let _ = app.emit_to("chrome", "nav", HOME.to_string());

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
