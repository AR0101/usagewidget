//! The right-click settings menu, rebuilt every time it opens so the check marks
//! reflect the current preferences.
//!
//! Item ids are `group:value` strings. Routing on a parsed id keeps this to one
//! handler instead of a closure per item, which matters at ~30 items.

use crate::prefs::Prefs;
use tauri::menu::{CheckMenuItemBuilder, ContextMenu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_autostart::ManagerExt;

const ZOOMS: [f64; 7] = [0.8, 0.9, 1.0, 1.15, 1.3, 1.5, 1.75];
const ROWS: [u32; 5] = [3, 5, 7, 10, 14];
const OPACITIES: [f64; 5] = [1.0, 0.9, 0.75, 0.6, 0.45];
const INTERVALS: [(&str, f64); 4] = [("30초", 30.0), ("1분", 60.0), ("5분", 300.0), ("15분", 900.0)];

pub fn popup(app: &AppHandle, window: &WebviewWindow, p: &Prefs) {
    let Ok(menu) = build(app, p) else { return };
    // `popup` wants the underlying window, not the webview wrapper around it.
    let _ = menu.popup(window.as_ref().window());
}

fn build(app: &AppHandle, p: &Prefs) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let near = |a: f64, b: f64| (a - b).abs() < 0.01;

    let shape = SubmenuBuilder::new(app, "모양")
        .item(&check(app, "layout:vertical", "세로", p.layout == "vertical")?)
        .item(&check(app, "layout:horizontal", "가로", p.layout == "horizontal")?)
        .build()?;

    let theme = SubmenuBuilder::new(app, "테마")
        .item(&check(app, "appearance:dark", "다크", p.appearance == "dark")?)
        .item(&check(app, "appearance:light", "라이트", p.appearance == "light")?)
        .item(&check(app, "appearance:system", "시스템 따름", p.appearance == "system")?)
        .build()?;

    let detail = SubmenuBuilder::new(app, "세부 항목")
        .item(&check(app, "detail:sessions", "세션별", p.detail == "sessions")?)
        .item(&check(app, "detail:models", "모델별", p.detail == "models")?)
        .item(&check(app, "detail:both", "둘 다", p.detail == "both")?)
        .item(&check(app, "detail:none", "표시 안 함", p.detail == "none")?)
        .build()?;

    let mut zoom = SubmenuBuilder::new(app, "확대");
    for v in ZOOMS {
        let label = format!("{}%", (v * 100.0).round() as i64);
        zoom = zoom.item(&check(app, &format!("scale:{v}"), &label, near(p.scale, v))?);
    }
    let zoom = zoom.build()?;

    let mut rows = SubmenuBuilder::new(app, "목록 행 수");
    for n in ROWS {
        rows = rows.item(&check(app, &format!("rows:{n}"), &format!("{n}행"), p.rows == n)?);
    }
    let rows = rows.build()?;

    let mut opacity = SubmenuBuilder::new(app, "투명도");
    for v in OPACITIES {
        let label = format!("{}%", (v * 100.0).round() as i64);
        opacity = opacity.item(&check(app, &format!("opacity:{v}"), &label, near(p.opacity, v))?);
    }
    let opacity = opacity.build()?;

    let mut rate = SubmenuBuilder::new(app, "새로고침 주기");
    for (label, v) in INTERVALS {
        rate = rate.item(&check(
            app,
            &format!("refreshInterval:{v}"),
            label,
            near(p.refresh_interval, v),
        )?);
    }
    let rate = rate.build()?;

    let autostart = app.autolaunch().is_enabled().unwrap_or(false);

    MenuBuilder::new(app)
        .item(&MenuItemBuilder::with_id("refresh", "지금 새로고침").build(app)?)
        .separator()
        .item(&shape)
        .item(&theme)
        .item(&detail)
        .item(&zoom)
        .item(&rows)
        .item(&check(app, "alwaysOnTop", "항상 위에 표시", p.always_on_top)?)
        .item(&opacity)
        .item(&rate)
        .separator()
        .item(&check(app, "autostart", "로그인 시 자동 실행", autostart)?)
        .item(&MenuItemBuilder::with_id("resetSize", "크기 초기화").build(app)?)
        .item(&MenuItemBuilder::with_id("resetPosition", "화면 오른쪽 위로 정렬").build(app)?)
        .separator()
        .item(&MenuItemBuilder::with_id("quit", "종료").build(app)?)
        .build()
}

fn check(
    app: &AppHandle,
    id: &str,
    label: &str,
    on: bool,
) -> tauri::Result<tauri::menu::CheckMenuItem<tauri::Wry>> {
    CheckMenuItemBuilder::with_id(id, label).checked(on).build(app)
}

/// One handler for every item. Preference changes go through the same `set_pref`
/// path the resize grip uses, then the webview is told to re-read and redraw.
pub fn register_handler(app: &AppHandle) {
    let handle = app.clone();
    app.on_menu_event(move |_app, event| {
        let id = event.id().0.clone();
        let (group, value) = match id.split_once(':') {
            Some((g, v)) => (g, Some(v.to_string())),
            None => (id.as_str(), None),
        };

        match group {
            "refresh" => {
                let h = handle.clone();
                std::thread::spawn(move || crate::full_scan(&h));
                return;
            }
            "resetPosition" => {
                let state = handle.state::<crate::AppState>();
                crate::reset_position(handle.clone(), state);
            }
            "quit" => {
                if let Some(w) = handle.get_webview_window("panel") {
                    let state = handle.state::<crate::AppState>();
                    crate::quit(handle.clone(), w, state);
                }
                return;
            }
            "autostart" => {
                let auto = handle.autolaunch();
                let on = auto.is_enabled().unwrap_or(false);
                let _ = if on { auto.disable() } else { auto.enable() };
            }
            "alwaysOnTop" => {
                let state = handle.state::<crate::AppState>();
                let now = !state.prefs.lock().map(|p| p.always_on_top).unwrap_or(true);
                crate::set_pref(
                    handle.clone(),
                    state,
                    "alwaysOnTop".into(),
                    serde_json::Value::Bool(now),
                );
            }
            "resetSize" => {
                let state = handle.state::<crate::AppState>();
                crate::set_pref(
                    handle.clone(),
                    state,
                    "resetSize".into(),
                    serde_json::Value::Null,
                );
            }
            _ => {
                let Some(v) = value else { return };
                // Numeric groups arrive as strings from the id; everything else
                // stays a string, which is what `set_pref` expects.
                let json = match v.parse::<f64>() {
                    Ok(n) => serde_json::json!(n),
                    Err(_) => serde_json::json!(v),
                };
                let state = handle.state::<crate::AppState>();
                crate::set_pref(handle.clone(), state, group.to_string(), json);
            }
        }
        let _ = handle.emit("prefs-changed", ());
    });
}
