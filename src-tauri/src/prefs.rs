//! Panel settings, persisted as one JSON file in the app config directory.
//!
//! Width is stored per layout and *unscaled*, so the zoom level and the width
//! stay independent — the same arrangement the macOS widget uses, and the reason
//! switching between the tall card and the wide bar remembers each one's size.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCALE_MIN: f64 = 0.7;
pub const SCALE_MAX: f64 = 2.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Prefs {
    /// "vertical" | "horizontal"
    pub layout: String,
    /// "dark" | "light" | "system"
    pub appearance: String,
    /// "sessions" | "models" | "both" | "none"
    pub detail: String,
    pub scale: f64,
    pub rows: u32,
    pub width_vertical: f64,
    pub width_horizontal: f64,
    pub opacity: f64,
    pub always_on_top: bool,
    pub refresh_interval: f64,
    pub pos_vertical: Option<(f64, f64)>,
    pub pos_horizontal: Option<(f64, f64)>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            layout: "vertical".into(),
            appearance: "dark".into(),
            detail: "sessions".into(),
            scale: 1.0,
            rows: 5,
            width_vertical: 268.0,
            width_horizontal: 460.0,
            opacity: 1.0,
            always_on_top: true,
            refresh_interval: 60.0,
            pos_vertical: None,
            pos_horizontal: None,
        }
    }
}

impl Prefs {
    pub fn set_width(&mut self, w: f64) {
        // Same bounds as the macOS grip: wide enough that session names stop
        // being truncated, narrow enough to stay a widget.
        let clamped = if self.layout == "horizontal" {
            w.clamp(380.0, 900.0)
        } else {
            w.clamp(230.0, 520.0)
        };
        if self.layout == "horizontal" {
            self.width_horizontal = clamped;
        } else {
            self.width_vertical = clamped;
        }
    }

    pub fn position(&self) -> Option<(f64, f64)> {
        if self.layout == "horizontal" {
            self.pos_horizontal
        } else {
            self.pos_vertical
        }
    }

    pub fn set_position(&mut self, p: Option<(f64, f64)>) {
        if self.layout == "horizontal" {
            self.pos_horizontal = p;
        } else {
            self.pos_vertical = p;
        }
    }

    pub fn reset_size(&mut self) {
        let d = Prefs::default();
        self.scale = d.scale;
        self.rows = d.rows;
        self.width_vertical = d.width_vertical;
        self.width_horizontal = d.width_horizontal;
    }

    pub fn load(path: &PathBuf) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Best-effort: a widget that cannot write its settings should still run.
    pub fn save(&self, path: &PathBuf) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}
