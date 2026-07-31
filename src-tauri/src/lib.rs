//! UsageWidget — a floating desktop panel showing Claude Code and Codex usage.
//!
//! The Rust side owns the data and the timers; the webview owns the drawing. The
//! split matters for one reason: a full scan walks a few hundred megabytes and
//! must never run on the thread that paints, and a pulse has to land every two
//! seconds regardless of what the UI is doing.

mod collector;
mod live;
mod menu;
mod model;
mod paths;
mod prefs;

use collector::Collector;
use model::Stats;
use prefs::Prefs;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

/// How often the live session bars are updated between full scans. Short,
/// because a pulse only reads the bytes appended since the last one.
const PULSE_SECS: u64 = 2;

pub struct AppState {
    collector: Mutex<Collector>,
    stats: Mutex<Stats>,
    prefs: Mutex<Prefs>,
    prefs_path: std::path::PathBuf,
}

impl AppState {
    fn save_prefs(&self) {
        if let Ok(p) = self.prefs.lock() {
            p.save(&self.prefs_path);
        }
    }
}

/// Everything the webview needs to draw a frame.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    prefs: Prefs,
    stats: Stats,
    /// Set when the widget is reading a WSL home rather than the native one.
    source: Option<String>,
    /// Denominator for the context bars, so the panel and the collector cannot
    /// disagree about what "full" means.
    context_limit: i64,
}

// MARK: - commands

#[tauri::command]
fn get_snapshot(state: tauri::State<AppState>) -> Snapshot {
    let stats = state.stats.lock().unwrap().clone();
    let prefs = state.prefs.lock().unwrap().clone();
    let source = state.collector.lock().unwrap().roots.label.clone();
    Snapshot {
        prefs,
        stats,
        source,
        context_limit: model::CONTEXT_LIMIT,
    }
}

#[tauri::command]
fn refresh_now(app: tauri::AppHandle) {
    std::thread::spawn(move || full_scan(&app));
}

/// The webview measures its own content and asks for a window that fits it. The
/// top-left corner stays put, so the panel does not crawl up the screen every
/// time a session row appears.
#[tauri::command]
fn set_panel_size(window: WebviewWindow, width: f64, height: f64) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let w = (width * scale).round().max(1.0) as u32;
    let h = (height * scale).round().max(1.0) as u32;
    if let Ok(cur) = window.outer_size() {
        if cur.width == w && cur.height == h {
            return;
        }
    }
    let _ = window.set_size(PhysicalSize::new(w, h));
}

/// Applies a preference change coming from the menu or the resize grip. The
/// value is untyped because the menu deals in strings and the grip in numbers;
/// both end up in the same JSON file either way.
#[tauri::command]
fn set_pref(
    app: tauri::AppHandle,
    state: tauri::State<AppState>,
    key: String,
    value: serde_json::Value,
) -> Prefs {
    {
        let mut p = state.prefs.lock().unwrap();
        let s = || value.as_str().unwrap_or_default().to_string();
        let f = || value.as_f64().unwrap_or_default();
        match key.as_str() {
            "layout" => {
                // Remember where this layout was before switching away from it.
                if let Some(w) = app.get_webview_window("panel") {
                    remember_position(&w, &mut p);
                }
                p.layout = s();
            }
            "appearance" => p.appearance = s(),
            "detail" => p.detail = s(),
            "scale" => p.scale = f().clamp(prefs::SCALE_MIN, prefs::SCALE_MAX),
            "rows" => p.rows = f() as u32,
            "width" => p.set_width(f()),
            "opacity" => p.opacity = f(),
            "refreshInterval" => p.refresh_interval = f(),
            "alwaysOnTop" => {
                p.always_on_top = value.as_bool().unwrap_or(true);
                if let Some(w) = app.get_webview_window("panel") {
                    let _ = w.set_always_on_top(p.always_on_top);
                }
            }
            "liveLimits" => {
                p.live_limits = value.as_bool().unwrap_or(true);
                if let Ok(mut c) = state.collector.lock() {
                    c.use_live_limits = p.live_limits;
                }
            }
            "resetSize" => p.reset_size(),
            _ => {}
        }
    }
    state.save_prefs();
    let p = state.prefs.lock().unwrap().clone();
    if key == "layout" {
        if let Some(w) = app.get_webview_window("panel") {
            place(&w, &p);
        }
    }
    p
}

#[tauri::command]
fn reset_position(app: tauri::AppHandle, state: tauri::State<AppState>) {
    let Some(w) = app.get_webview_window("panel") else {
        return;
    };
    let mut p = state.prefs.lock().unwrap();
    p.set_position(None);
    place(&w, &p);
    p.save(&state.prefs_path);
}

/// Called when a drag ends, so the panel reopens where it was left.
#[tauri::command]
fn store_position(window: WebviewWindow, state: tauri::State<AppState>) {
    let mut p = state.prefs.lock().unwrap();
    remember_position(&window, &mut p);
    p.save(&state.prefs_path);
}

#[tauri::command]
fn show_menu(app: tauri::AppHandle, window: WebviewWindow, state: tauri::State<AppState>) {
    let p = state.prefs.lock().unwrap().clone();
    menu::popup(&app, &window, &p);
}

#[tauri::command]
fn quit(app: tauri::AppHandle, window: WebviewWindow, state: tauri::State<AppState>) {
    {
        let mut p = state.prefs.lock().unwrap();
        remember_position(&window, &mut p);
        p.save(&state.prefs_path);
    }
    app.exit(0);
}

// MARK: - window placement

fn remember_position(w: &WebviewWindow, p: &mut Prefs) {
    if let Ok(pos) = w.outer_position() {
        p.set_position(Some((pos.x as f64, pos.y as f64)));
    }
}

/// Restore the saved corner, or tuck the panel into the top right of the primary
/// monitor. A saved position that is no longer on any monitor is discarded —
/// otherwise unplugging a second screen hides the widget for good.
fn place(w: &WebviewWindow, p: &Prefs) {
    let size = w.outer_size().unwrap_or(PhysicalSize::new(320, 480));
    if let Some((x, y)) = p.position() {
        let pos = PhysicalPosition::new(x as i32, y as i32);
        if on_some_monitor(w, pos, size) {
            let _ = w.set_position(pos);
            return;
        }
    }
    if let Ok(Some(m)) = w.primary_monitor() {
        let mp = m.position();
        let ms = m.size();
        let x = mp.x + ms.width as i32 - size.width as i32 - 12;
        let y = mp.y + 12;
        let _ = w.set_position(PhysicalPosition::new(x, y));
    }
}

fn on_some_monitor(w: &WebviewWindow, pos: PhysicalPosition<i32>, size: PhysicalSize<u32>) -> bool {
    let Ok(monitors) = w.available_monitors() else {
        return false;
    };
    monitors.iter().any(|m| {
        let mp = m.position();
        let ms = m.size();
        // Require a little of the panel to be visible, not merely touching.
        pos.x + (size.width as i32) > mp.x + 24
            && pos.x < mp.x + ms.width as i32 - 24
            && pos.y + (size.height as i32) > mp.y + 24
            && pos.y < mp.y + ms.height as i32 - 24
    })
}

// MARK: - background work

fn full_scan(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let fresh = {
        let mut c = state.collector.lock().unwrap();
        c.collect()
    };
    *state.stats.lock().unwrap() = fresh.clone();
    let _ = app.emit("stats", &fresh);
}

/// Rolls the session bars forward from the tail of each live transcript.
fn pulse(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let p = {
        let mut c = state.collector.lock().unwrap();
        c.pulse()
    };
    let updated = {
        let mut s = state.stats.lock().unwrap();
        collector::apply_pulse(&mut s, &p);
        s.clone()
    };
    let _ = app.emit("stats", &updated);
}

fn spawn_timers(app: tauri::AppHandle) {
    // Full scan: expensive, so it runs on the user's chosen interval.
    let a = app.clone();
    std::thread::spawn(move || {
        full_scan(&a);
        loop {
            let secs = a
                .state::<AppState>()
                .prefs
                .lock()
                .map(|p| p.refresh_interval)
                .unwrap_or(60.0)
                .max(5.0);
            std::thread::sleep(Duration::from_secs_f64(secs));
            full_scan(&a);
        }
    });

    // Pulse: cheap, so it runs often. Started after one interval so it never
    // races the first full scan into an empty Stats.
    let b = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(PULSE_SECS));
        pulse(&b);
    });
}

// MARK: - entry point

pub fn run() {
    // `usagewidget --dump` prints what the collector found and exits. This is
    // how the widget gets verified without a screen to look at.
    if std::env::args().any(|a| a == "--dump") {
        dump();
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            refresh_now,
            set_panel_size,
            set_pref,
            reset_position,
            store_position,
            show_menu,
            quit,
        ])
        .setup(|app| {
            let dir = app.path().app_config_dir().unwrap_or_else(|_| ".".into());
            let prefs_path = dir.join("prefs.json");
            let prefs = Prefs::load(&prefs_path);

            let window = app.get_webview_window("panel").expect("panel window");
            let _ = window.set_always_on_top(prefs.always_on_top);
            apply_backdrop(&window);
            place(&window, &prefs);

            let mut collector = Collector::new();
            collector.use_live_limits = prefs.live_limits;

            app.manage(AppState {
                collector: Mutex::new(collector),
                stats: Mutex::new(Stats::default()),
                prefs: Mutex::new(prefs),
                prefs_path,
            });

            menu::register_handler(app.handle());
            spawn_timers(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running UsageWidget");
}

/// Blur behind the panel, so it reads as part of the desktop rather than a box
/// sitting on it. Windows 11 has Acrylic; on 10 the call fails and the flat
/// translucent surface the CSS paints is all there is, which still looks fine.
fn apply_backdrop(window: &WebviewWindow) {
    #[cfg(target_os = "windows")]
    {
        use window_vibrancy::apply_acrylic;
        let _ = apply_acrylic(window, Some((18, 18, 22, 125)));
    }
    #[cfg(target_os = "macos")]
    {
        use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
        let _ = apply_vibrancy(
            window,
            NSVisualEffectMaterial::HudWindow,
            None,
            Some(18.0),
        );
    }
}

fn dump() {
    let mut c = Collector::new();
    // A layout or parity check has no use for live limits, and asking for them
    // means prompting for a credential the check does not need.
    c.use_live_limits = !std::env::args().any(|a| a == "--no-live");
    println!("roots: {:?}  ({:?})", c.roots.home, c.roots.origin);
    let s = c.collect();
    println!("collected in {:.2}s", s.scan_seconds);
    for (name, p) in [("CLAUDE", &s.claude), ("CODEX", &s.codex)] {
        println!("\n== {name} ==");
        println!("  plan:      {}", p.plan.clone().unwrap_or("-".into()));
        if let Some(u) = &p.unavailable {
            println!("  UNAVAILABLE: {u}");
        }
        if p.limits_are_live {
            println!("  source:    계정 실시간");
        } else if p.limits_fetched_at.is_some() {
            println!("  source:    로컬 캐시");
        }
        if let Some(e) = &p.live_error {
            println!("  live err:  {e}");
        }
        for l in &p.limits {
            println!("  {:<12} {:5.1}%", l.label, l.percent);
        }
        for (label, b) in [
            ("recent 5h", &p.recent),
            ("today", &p.today),
            ("week", &p.week),
        ] {
            if b.total() > 0 {
                println!("  {:<12} {:>12}", label, b.total());
            }
        }
        for m in &p.models {
            println!("    model  {:<16} {}", m.name, m.tokens);
        }
        for x in &p.sessions {
            println!(
                "    live   {:<20} {:>12}  ctx {:>10}  pid {}",
                x.label, x.tokens, x.context_tokens, x.pid
            );
        }
        if p.exited_session_tokens > 0 {
            println!("    exited {}", p.exited_session_tokens);
        }
    }

    // Verifies the incremental tail reader: after a full scan the offsets sit at
    // EOF, so a pulse should report only what was written in between.
    if let Some(i) = std::env::args().position(|a| a == "--pulse") {
        let secs: u64 = std::env::args()
            .nth(i + 1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        println!("\n== PULSE (after {secs}s) ==");
        std::thread::sleep(Duration::from_secs(secs));
        let t0 = std::time::Instant::now();
        let p = c.pulse();
        println!(
            "  took {}ms  delta {}",
            t0.elapsed().as_millis(),
            p.total.total()
        );
        for x in &p.sessions {
            println!("    {:<20} +{:<12} ctx {}", x.label, x.delta, x.context);
        }
    }
}
