//! Data model, ported from the macOS widget's `Model.swift`.
//!
//! Everything here is serialised straight to the webview, so the field names are
//! camelCased to match what the JavaScript expects. Formatting (token suffixes,
//! "3분 전") is deliberately left to the frontend: the clock ticks every second
//! and re-rendering a string is far cheaper than a round trip into Rust.

use serde::Serialize;
use std::ops::{Add, AddAssign};

/// Auto-compaction has been observed firing right at 1M in these logs.
pub const CONTEXT_LIMIT: i64 = 1_000_000;

/// Token totals split by kind. The split matters: on a long conversation the
/// bulk of every request is the same context being re-read, which is billed at a
/// fraction of fresh input — a bare total reads as far more usage than occurred.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_write: i64,
    pub cache_read: i64,
}

impl TokenBreakdown {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

impl Add for TokenBreakdown {
    type Output = Self;
    fn add(self, b: Self) -> Self {
        Self {
            input: self.input + b.input,
            output: self.output + b.output,
            cache_write: self.cache_write + b.cache_write,
            cache_read: self.cache_read + b.cache_read,
        }
    }
}

impl AddAssign for TokenBreakdown {
    fn add_assign(&mut self, b: Self) {
        *self = *self + b;
    }
}

/// A single rate-limit window (Claude 5h / weekly, Codex weekly, …).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub label: String,
    pub percent: f64,
    /// Unix seconds. `None` when the provider did not report one.
    pub resets_at: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub name: String,
    pub tokens: i64,
}

/// One running CLI session and what it has burned in the current window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsage {
    /// sessionId
    pub id: String,
    /// Session name, or the working directory's last component.
    pub label: String,
    pub pid: u32,
    pub tokens: i64,
    /// Context the session's most recent request carried. This is what grows
    /// until auto-compaction fires, so it is the number that says "compact me".
    pub context_tokens: i64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStats {
    pub plan: Option<String>,
    pub limits: Vec<LimitWindow>,
    /// When the provider last refreshed the limit numbers.
    pub limits_fetched_at: Option<f64>,
    pub today: TokenBreakdown,
    pub week: TokenBreakdown,
    /// Rolling 5-hour window, computed from the logs. Unlike the percentages
    /// this is always current, so it stays useful when the cache goes stale.
    pub recent: TokenBreakdown,
    pub models: Vec<ModelUsage>,
    /// Live sessions, biggest first. Usage from sessions that have since exited
    /// is rolled into `exited_session_tokens` rather than listed.
    pub sessions: Vec<SessionUsage>,
    pub exited_session_tokens: i64,
    /// Populated when the data source is missing entirely (CLI never used).
    pub unavailable: Option<String>,
    /// Where the data was read from, so the UI can say "WSL" out loud.
    pub source: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub claude: ProviderStats,
    pub codex: ProviderStats,
    /// Unix seconds; 0 before the first scan finishes.
    pub generated_at: f64,
    /// Seconds the scan took, surfaced by the dev dump.
    pub scan_seconds: f64,
}

/// One live session as seen by a pulse: what it spent since the previous pulse,
/// and the context it is carrying right now.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PulseSession {
    pub id: String,
    pub label: String,
    pub pid: u32,
    /// Tokens billed since the last pulse. Zero when the session was idle.
    pub delta: i64,
    pub context: i64,
}

/// What changed in the live sessions since the previous pulse.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pulse {
    pub sessions: Vec<PulseSession>,
    /// Sum of every session's delta, for rolling 5h/today/week forward.
    pub total: TokenBreakdown,
}
