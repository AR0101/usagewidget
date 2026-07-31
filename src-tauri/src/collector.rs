//! Reads usage straight out of the CLIs' own on-disk state. Nothing is sent
//! anywhere and no credentials are touched — these are all plain files under the
//! user's home directory.
//!
//! Ported from the macOS widget's `Collector.swift`; the parsing rules, the
//! de-duplication by `requestId`, and the incremental tail reader all behave the
//! same way, because they were worked out against real transcripts.

use crate::model::*;
use crate::paths::{self, Origin, Roots};
use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Days of history we bucket. 8 covers "today" plus a full week.
const HISTORY_DAYS: usize = 8;

/// How long a WSL liveness answer is reused. Only matters when the sessions live
/// inside a distribution, where each check means spawning `wsl.exe`.
const LIVE_CACHE_SECS: u64 = 30;

pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[derive(Clone)]
struct SessionInfo {
    label: String,
    pid: u32,
}

struct TailState {
    path: PathBuf,
    /// Bytes already parsed.
    offset: u64,
    /// Last context seen, so an idle session keeps a value.
    context: i64,
}

pub struct Collector {
    pub roots: Roots,
    tails: HashMap<String, TailState>,
    /// Cached liveness, so a pulse does not shell into WSL every two seconds.
    live_cache: Option<(Instant, HashSet<u32>)>,
}

impl Collector {
    pub fn new() -> Self {
        Self {
            roots: paths::detect(),
            tails: HashMap::new(),
            live_cache: None,
        }
    }

    pub fn collect(&mut self) -> Stats {
        let started = Instant::now();
        // Re-detect: WSL may not have been running when the widget started, and a
        // brand-new install has no `.claude` until the first session.
        if !self.roots.claude_projects().exists() && !self.roots.codex_sessions().exists() {
            self.roots = paths::detect();
        }
        let mut s = Stats {
            claude: self.claude(),
            codex: self.codex(),
            generated_at: now_epoch(),
            scan_seconds: 0.0,
        };
        s.scan_seconds = started.elapsed().as_secs_f64();
        // The full scan just consumed every byte on disk, so a pulse must resume
        // from EOF or it would count the same lines twice.
        self.rebase_tails(&s.claude.sessions);
        s
    }

    // MARK: - Claude

    fn claude(&mut self) -> ProviderStats {
        let mut p = ProviderStats {
            source: self.roots.label.clone(),
            ..Default::default()
        };
        let root = self.roots.claude_projects();
        if !root.exists() {
            p.unavailable = Some("Claude Code 사용 기록 없음".into());
            return p;
        }

        self.read_claude_limits(&mut p);

        let grid = DayGrid::new();
        let mut daily = vec![TokenBreakdown::default(); HISTORY_DAYS];
        let mut today_by_model: HashMap<String, i64> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::with_capacity(200_000);

        let today_idx = HISTORY_DAYS - 1;
        let five_hours_ago = now_epoch() - 5.0 * 3600.0;
        let mut recent = TokenBreakdown::default();
        let running = self.running_sessions(true);
        let mut by_session: HashMap<String, i64> = HashMap::new();
        // Latest request per session, to report the context it is carrying now.
        let mut latest_call: HashMap<String, (f64, i64)> = HashMap::new();

        // Compaction records carry no usage block, so they need their own marker.
        let markers: [&[u8]; 2] = [b"\"usage\"", b"\"compactMetadata\""];

        for file in jsonl_files(&root, grid.window_start()) {
            for_each_line(&file, &markers, |line| {
                let Ok(obj) = serde_json::from_slice::<Value>(line) else {
                    return;
                };

                // A compaction resets the conversation's context. It is the truth
                // about "how big is this session now" until the next request runs.
                if let Some(post) = obj
                    .get("compactMetadata")
                    .and_then(|m| m.get("postTokens"))
                    .and_then(Value::as_i64)
                {
                    if let (Some(sid), Some(epoch)) = (str_at(&obj, "sessionId"), timestamp(&obj)) {
                        let e = latest_call.entry(sid.to_string()).or_insert((0.0, 0));
                        if epoch > e.0 {
                            *e = (epoch, post);
                        }
                    }
                    return;
                }

                let Some(msg) = obj.get("message") else { return };
                let Some(usage) = msg.get("usage") else { return };
                let Some(epoch) = timestamp(&obj) else { return };
                let Some(day) = grid.bucket(epoch) else { return };

                // One assistant turn can be logged more than once (resumed
                // sessions, copied project dirs). requestId is stable per call.
                let key = str_at(&obj, "requestId").or_else(|| str_at(msg, "id"));
                if let Some(k) = key {
                    if !seen.insert(k.to_string()) {
                        return;
                    }
                }

                let b = breakdown(usage);
                let is_sidechain = obj
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                daily[day] += b;
                if epoch >= five_hours_ago {
                    recent += b;
                    // Subagent turns are logged under their parent's sessionId,
                    // so they land in the right bucket without extra work.
                    if let Some(sid) = str_at(&obj, "sessionId") {
                        *by_session.entry(sid.to_string()).or_insert(0) += b.total();
                        // Sidechain turns carry their own smaller context and
                        // would understate the parent, so they are skipped.
                        if !is_sidechain {
                            let e = latest_call.entry(sid.to_string()).or_insert((0.0, 0));
                            if epoch > e.0 {
                                *e = (epoch, b.input + b.cache_read + b.cache_write);
                            }
                        }
                    }
                }
                if day == today_idx {
                    if let Some(model) = str_at(msg, "model") {
                        if model != "<synthetic>" {
                            *today_by_model.entry(pretty_model(model)).or_insert(0) += b.total();
                        }
                    }
                }
            });
        }

        p.today = daily[today_idx];
        p.recent = recent;
        p.week = week_sum(
            &daily,
            &grid,
            p.limits.iter().find(|l| l.label == "주간").and_then(|l| l.resets_at),
        );
        let mut models: Vec<ModelUsage> = today_by_model
            .into_iter()
            .map(|(name, tokens)| ModelUsage { name, tokens })
            .collect();
        models.sort_by(|a, b| b.tokens.cmp(&a.tokens));
        p.models = models;

        for (sid, tokens) in &by_session {
            if !running.contains_key(sid) {
                p.exited_session_tokens += tokens;
            }
        }
        // Every live session is listed, including ones idle all window —
        // "running" is what the user counts, not "recently active".
        let mut sessions: Vec<SessionUsage> = running
            .iter()
            .map(|(sid, info)| SessionUsage {
                id: sid.clone(),
                label: info.label.clone(),
                pid: info.pid,
                tokens: by_session.get(sid).copied().unwrap_or(0),
                context_tokens: latest_call.get(sid).map(|c| c.1).unwrap_or(0),
            })
            .collect();
        sort_sessions(&mut sessions);
        p.sessions = sessions;
        p
    }

    /// Claude Code caches the plan-utilisation response it gets from the API.
    /// We can only read that cache — if the CLI has not run in a while it goes
    /// stale, which is why `limits_fetched_at` is surfaced in the UI.
    fn read_claude_limits(&self, p: &mut ProviderStats) {
        let Ok(data) = std::fs::read(self.roots.claude_json()) else {
            return;
        };
        let Ok(root) = serde_json::from_slice::<Value>(&data) else {
            return;
        };

        if let Some(oauth) = root.get("oauthAccount") {
            let tier = str_at(oauth, "userRateLimitTier")
                .or_else(|| str_at(oauth, "organizationRateLimitTier"));
            p.plan = tier.and_then(pretty_claude_plan);
        }

        let Some(cache) = root.get("cachedUsageUtilization") else {
            return;
        };
        if let Some(ms) = cache.get("fetchedAtMs").and_then(Value::as_f64) {
            p.limits_fetched_at = Some(ms / 1000.0);
        }
        let Some(u) = cache.get("utilization") else {
            return;
        };

        for (key, label) in [("five_hour", "5시간 세션"), ("seven_day", "주간")] {
            let Some(w) = u.get(key) else { continue };
            let Some(pct) = w.get("utilization").and_then(Value::as_f64) else {
                continue;
            };
            p.limits.push(LimitWindow {
                label: label.into(),
                percent: pct,
                resets_at: str_at(w, "resets_at").and_then(parse_iso),
            });
        }
    }

    // MARK: - Codex

    fn codex(&self) -> ProviderStats {
        let mut p = ProviderStats {
            source: self.roots.label.clone(),
            ..Default::default()
        };
        let root = self.roots.codex_sessions();
        if !root.exists() {
            p.unavailable = Some("Codex 사용 기록 없음".into());
            return p;
        }

        let grid = DayGrid::new();
        let mut daily = vec![TokenBreakdown::default(); HISTORY_DAYS];
        let mut latest_limit_epoch = 0.0_f64;
        let mut latest_limits: Option<Value> = None;
        // Codex writes some rate_limit records with a null plan_type, so the plan
        // is tracked separately from the newest record that actually carries one.
        let mut latest_plan_epoch = 0.0_f64;
        let mut latest_plan: Option<String> = None;

        let markers: [&[u8]; 1] = [b"\"token_count\""];

        // Rate limits live only in the newest session, but token counts are
        // spread across every session touched during the window.
        for file in jsonl_files(&root, grid.window_start()) {
            for_each_line(&file, &markers, |line| {
                let Ok(obj) = serde_json::from_slice::<Value>(line) else {
                    return;
                };
                let Some(payload) = obj.get("payload") else {
                    return;
                };
                let epoch = timestamp(&obj).unwrap_or(0.0);

                if let Some(rl) = payload.get("rate_limits") {
                    if rl.get("primary").is_some_and(Value::is_object) && epoch >= latest_limit_epoch
                    {
                        latest_limit_epoch = epoch;
                        latest_limits = Some(rl.clone());
                    }
                    if let Some(plan) = str_at(rl, "plan_type") {
                        if epoch >= latest_plan_epoch {
                            latest_plan_epoch = epoch;
                            latest_plan = Some(plan.to_string());
                        }
                    }
                }
                if let Some(last) = payload
                    .get("info")
                    .and_then(|i| i.get("last_token_usage"))
                {
                    if let Some(day) = grid.bucket(epoch) {
                        // Codex reports input_tokens inclusive of the cached part.
                        let cached = int_at(last, "cached_input_tokens");
                        daily[day] += TokenBreakdown {
                            input: (int_at(last, "input_tokens") - cached).max(0),
                            output: int_at(last, "output_tokens"),
                            cache_write: int_at(last, "cache_write_input_tokens"),
                            cache_read: cached,
                        };
                    }
                }
            });
        }

        p.plan = latest_plan.map(|s| {
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => s,
            }
        });

        if let Some(rl) = latest_limits {
            if latest_limit_epoch > 0.0 {
                p.limits_fetched_at = Some(latest_limit_epoch);
            }
            for key in ["primary", "secondary"] {
                let Some(w) = rl.get(key) else { continue };
                let Some(pct) = w.get("used_percent").and_then(Value::as_f64) else {
                    continue;
                };
                p.limits.push(LimitWindow {
                    label: window_label(int_at(w, "window_minutes")),
                    percent: pct,
                    resets_at: w.get("resets_at").and_then(Value::as_f64),
                });
            }
        }

        p.today = daily[HISTORY_DAYS - 1];
        let reset = p.limits.first().and_then(|l| l.resets_at);
        p.week = week_sum(&daily, &grid, reset);
        p
    }

    // MARK: - live sessions

    /// Every live CLI session, keyed by sessionId. `~/.claude/sessions/<pid>.json`
    /// is written by each running process but is not cleaned up on exit, so the
    /// pid is probed before the entry is trusted.
    fn running_sessions(&mut self, refresh_liveness: bool) -> HashMap<String, SessionInfo> {
        let dir = self.roots.claude_sessions();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return HashMap::new();
        };

        let mut found: Vec<(String, SessionInfo)> = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else { continue };
            let Ok(d) = serde_json::from_slice::<Value>(&data) else {
                continue;
            };
            let (Some(sid), Some(pid)) = (str_at(&d, "sessionId"), int_at_opt(&d, "pid")) else {
                continue;
            };
            let cwd_leaf = str_at(&d, "cwd").and_then(|c| {
                Path::new(c)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            });
            let named = str_at(&d, "name").map(|n| n.trim().to_string());
            let label = match named {
                Some(n) if !n.is_empty() => n,
                _ => cwd_leaf.unwrap_or_else(|| "세션".into()),
            };
            found.push((
                sid.to_string(),
                SessionInfo {
                    label,
                    pid: pid as u32,
                },
            ));
        }

        let candidates: Vec<u32> = found.iter().map(|(_, i)| i.pid).collect();
        let alive = self.live_set(&candidates, refresh_liveness);
        found
            .into_iter()
            .filter(|(_, i)| alive.contains(&i.pid))
            .collect()
    }

    /// Native pid probes are cheap enough to redo every time; the WSL path means
    /// spawning a process, so its answer is cached briefly.
    fn live_set(&mut self, candidates: &[u32], refresh: bool) -> HashSet<u32> {
        if matches!(self.roots.origin, Origin::Native) {
            return paths::live_pids(&self.roots, candidates);
        }
        if !refresh {
            if let Some((at, set)) = &self.live_cache {
                if at.elapsed().as_secs() < LIVE_CACHE_SECS {
                    return set.clone();
                }
            }
        }
        let set = paths::live_pids(&self.roots, candidates);
        self.live_cache = Some((Instant::now(), set.clone()));
        set
    }

    // MARK: - live pulse

    /// Re-reads only the bytes appended to each live transcript since the last
    /// call. A full scan walks a few hundred megabytes and costs about a second;
    /// this normally reads a few kilobytes, which is what makes a two-second
    /// refresh affordable.
    pub fn pulse(&mut self) -> Pulse {
        let mut out = Pulse::default();
        let running = self.running_sessions(false);

        // Drop state for sessions that exited so the map cannot grow forever.
        self.tails.retain(|sid, _| running.contains_key(sid));

        for (sid, info) in &running {
            let path = match self.tails.get(sid) {
                Some(t) => t.path.clone(),
                None => match self.transcript_path(sid) {
                    Some(p) => p,
                    None => continue,
                },
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let first = !self.tails.contains_key(sid);
            let entry = self.tails.entry(sid.clone()).or_insert(TailState {
                path: path.clone(),
                offset: size,
                context: 0,
            });
            entry.path = path.clone();
            // A truncated or replaced file makes the old offset meaningless.
            if entry.offset > size {
                entry.offset = 0;
            }

            let mut delta = TokenBreakdown::default();
            if entry.offset < size {
                // First sighting: no delta is knowable, but the tail still tells
                // us the context, so read a bounded window back from EOF.
                let from = if first {
                    size.saturating_sub(1 << 20)
                } else {
                    entry.offset
                };
                if let Some(data) = read_from(&path, from) {
                    let consumed = parse_tail(&data, first, entry, &mut delta);
                    entry.offset = from + consumed as u64;
                }
            }

            out.total += delta;
            out.sessions.push(PulseSession {
                id: sid.clone(),
                label: info.label.clone(),
                pid: info.pid,
                delta: delta.total(),
                context: entry.context,
            });
        }
        out
    }

    /// Transcripts are named after the session, one directory per project root.
    fn transcript_path(&self, sid: &str) -> Option<PathBuf> {
        let root = self.roots.claude_projects();
        let dirs = std::fs::read_dir(&root).ok()?;
        for d in dirs.flatten() {
            let p = d.path().join(format!("{sid}.jsonl"));
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    /// Point every tail at the current end of its file, discarding pending
    /// deltas — the scan that just ran already counted them. The scan's context
    /// wins too, since it read the whole file rather than a bounded tail.
    fn rebase_tails(&mut self, sessions: &[SessionUsage]) {
        let mut state = HashMap::new();
        for s in sessions {
            let Some(path) = self.transcript_path(&s.id) else {
                continue;
            };
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            state.insert(
                s.id.clone(),
                TailState {
                    path,
                    offset: size,
                    context: s.context_tokens,
                },
            );
        }
        self.tails = state;
    }
}

/// Merges a pulse into the stats the panel is currently showing. The limit
/// percentages are left alone — those come from the account, not from disk.
pub fn apply_pulse(stats: &mut Stats, p: &Pulse) {
    let c = &mut stats.claude;
    if c.unavailable.is_some() {
        return;
    }
    let mut sessions = Vec::with_capacity(p.sessions.len());
    for ps in &p.sessions {
        let prior = c.sessions.iter().find(|s| s.id == ps.id);
        let mut s = SessionUsage {
            id: ps.id.clone(),
            label: ps.label.clone(),
            pid: ps.pid,
            tokens: prior.map(|s| s.tokens).unwrap_or(0) + ps.delta,
            context_tokens: prior.map(|s| s.context_tokens).unwrap_or(0),
        };
        // A zero here means the tail has not seen a usage record yet; the last
        // full scan's number is better than blanking the bar.
        if ps.context > 0 {
            s.context_tokens = ps.context;
        }
        sessions.push(s);
    }
    // Sessions that exited keep their tokens in the window total, just not in
    // the list — matching what the next full scan will report.
    for gone in &c.sessions {
        if !p.sessions.iter().any(|s| s.id == gone.id) {
            c.exited_session_tokens += gone.tokens;
        }
    }
    sort_sessions(&mut sessions);
    c.sessions = sessions;

    if p.total.total() > 0 {
        c.recent += p.total;
        c.today += p.total;
        c.week += p.total;
    }
}

fn sort_sessions(v: &mut [SessionUsage]) {
    v.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
}

// MARK: - tail parsing

fn read_from(path: &Path, from: u64) -> Option<Vec<u8>> {
    let mut f = File::open(path).ok()?;
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Returns how many bytes were consumed — always a whole number of lines, so a
/// record still being written is left for the next pulse.
fn parse_tail(
    data: &[u8],
    skip_delta: bool,
    entry: &mut TailState,
    delta: &mut TokenBreakdown,
) -> usize {
    let Some(last_break) = memchr::memrchr(b'\n', data) else {
        return 0;
    };
    for line in data[..=last_break].split(|&b| b == b'\n') {
        if line.len() < 80 {
            continue;
        }
        let Ok(obj) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if let Some(post) = obj
            .get("compactMetadata")
            .and_then(|m| m.get("postTokens"))
            .and_then(Value::as_i64)
        {
            entry.context = post;
            continue;
        }
        let Some(usage) = obj.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let b = breakdown(usage);
        let is_sidechain = obj
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_sidechain {
            entry.context = b.input + b.cache_read + b.cache_write;
        }
        if !skip_delta {
            *delta += b;
        }
    }
    last_break + 1
}

// MARK: - day bucketing

/// Local midnights, oldest first, with a trailing bound so the last bucket
/// closes. Built from calendar dates rather than by subtracting 86400 seconds,
/// so a DST boundary does not shift every earlier day by an hour.
struct DayGrid {
    starts: [f64; HISTORY_DAYS + 1],
}

impl DayGrid {
    fn new() -> Self {
        let today = Local::now().date_naive();
        let mut starts = [0.0; HISTORY_DAYS + 1];
        for (i, slot) in starts.iter_mut().enumerate() {
            let offset = HISTORY_DAYS as i64 - 1 - i as i64;
            let day = today - Duration::days(offset);
            *slot = midnight(day);
        }
        Self { starts }
    }

    fn window_start(&self) -> f64 {
        self.starts[0]
    }

    /// Index into the day buckets, or `None` when the timestamp is outside.
    fn bucket(&self, epoch: f64) -> Option<usize> {
        if epoch < self.starts[0] || epoch >= self.starts[HISTORY_DAYS] {
            return None;
        }
        // Walk from the end: recent lines dominate, so this hits in 1–2 steps.
        (0..HISTORY_DAYS).rev().find(|&i| epoch >= self.starts[i])
    }
}

fn midnight(day: NaiveDate) -> f64 {
    let naive = day.and_hms_opt(0, 0, 0).expect("midnight always exists");
    Local
        .from_local_datetime(&naive)
        .earliest()
        // A DST spring-forward can delete midnight itself; 01:00 that day is
        // then the first instant, and is what `latest()` reports.
        .or_else(|| Local.from_local_datetime(&naive).latest())
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0)
}

/// Tokens since the current limit window opened, so the number lines up with the
/// percentage above it. Falls back to a rolling 7 days when there is no reset.
fn week_sum(daily: &[TokenBreakdown], grid: &DayGrid, resets_at: Option<f64>) -> TokenBreakdown {
    let mut from = HISTORY_DAYS.saturating_sub(7);
    if let Some(reset) = resets_at {
        // A reset time in the past means that window already rolled over, so the
        // current one started *at* the reset rather than seven days before it.
        let start = if reset > now_epoch() {
            reset - 7.0 * 86400.0
        } else {
            reset
        };
        if let Some(idx) = grid.bucket(start) {
            from = idx;
        }
    }
    daily[from..].iter().fold(TokenBreakdown::default(), |a, b| a + *b)
}

// MARK: - file walking

fn jsonl_files(root: &Path, modified_after: f64) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            // A file last written before the window opened cannot hold lines
            // inside it, so it never has to be opened.
            let fresh = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64() >= modified_after)
                .unwrap_or(false);
            if fresh {
                out.push(path);
            }
        }
    }
    out
}

/// Streams a file in chunks so a 90 MB transcript never lands in memory whole,
/// and only hands over lines that actually contain one of `markers`. Most lines
/// in a transcript are user and tool records we do not care about, and skipping
/// the JSON parse for those is most of why a full scan takes a second.
fn for_each_line<F: FnMut(&[u8])>(path: &Path, markers: &[&[u8]], mut body: F) {
    let Ok(mut f) = File::open(path) else { return };

    let mut buf: Vec<u8> = Vec::with_capacity(1 << 23);
    let mut chunk = vec![0u8; 1 << 22]; // 4 MB
    let finders: Vec<memchr::memmem::Finder> =
        markers.iter().map(|m| memchr::memmem::Finder::new(m)).collect();

    let mut emit = |line: &[u8]| {
        if line.len() > 80 && finders.iter().any(|f| f.find(line).is_some()) {
            body(line);
        }
    };

    loop {
        let n = f.read(&mut chunk).unwrap_or(0);
        let at_eof = n == 0;
        if !at_eof {
            buf.extend_from_slice(&chunk[..n]);
        }

        let mut start = 0;
        while let Some(rel) = memchr::memchr(b'\n', &buf[start..]) {
            let end = start + rel;
            emit(&buf[start..end]);
            start = end + 1;
        }

        if at_eof {
            emit(&buf[start..]);
            return;
        }
        if start > 0 {
            buf.drain(..start); // keep only the partial trailing line
        }
    }
}

// MARK: - JSON helpers

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn int_at(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn int_at_opt(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(Value::as_i64)
}

fn timestamp(obj: &Value) -> Option<f64> {
    str_at(obj, "timestamp").and_then(parse_iso)
}

fn breakdown(usage: &Value) -> TokenBreakdown {
    TokenBreakdown {
        input: int_at(usage, "input_tokens"),
        output: int_at(usage, "output_tokens"),
        cache_write: int_at(usage, "cache_creation_input_tokens"),
        cache_read: int_at(usage, "cache_read_input_tokens"),
    }
}

// MARK: - naming

fn window_label(minutes: i64) -> String {
    match minutes {
        10080 => "주간".into(),
        1440 => "일간".into(),
        300 => "5시간".into(),
        m if m > 0 && m % 1440 == 0 => format!("{}일", m / 1440),
        m if m > 0 && m % 60 == 0 => format!("{}시간", m / 60),
        _ => "사용량".into(),
    }
}

fn pretty_claude_plan(tier: &str) -> Option<String> {
    let t = tier.to_ascii_lowercase();
    for (needle, name) in [
        ("max_20x", "Max 20×"),
        ("max_5x", "Max 5×"),
        ("max", "Max"),
        ("pro", "Pro"),
        ("team", "Team"),
        ("free", "Free"),
    ] {
        if t.contains(needle) {
            return Some(name.into());
        }
    }
    None
}

/// `claude-opus-4-8-20260101` -> `Opus 4.8`
fn pretty_model(id: &str) -> String {
    let mut s = id;
    // Bedrock-style ids stack their prefixes ("us.anthropic.claude-sonnet-5"),
    // so keep peeling until none of them matches rather than making one pass.
    loop {
        let before = s;
        for p in ["us.anthropic.", "anthropic.", "claude-"] {
            if let Some(rest) = s.strip_prefix(p) {
                s = rest;
            }
        }
        if s == before {
            break;
        }
    }
    let mut parts: Vec<&str> = s.split('-').collect();
    // Drop a trailing date stamp: haiku-4-5-20251001 -> haiku-4-5
    if let Some(last) = parts.last() {
        if last.len() == 8 && last.chars().all(|c| c.is_ascii_digit()) {
            parts.pop();
        }
    }
    let mut words: Vec<String> = Vec::new();
    let mut nums: Vec<&str> = Vec::new();
    for part in parts {
        if !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) {
            nums.push(part);
        } else {
            let mut c = part.chars();
            if let Some(f) = c.next() {
                words.push(f.to_uppercase().collect::<String>() + c.as_str());
            }
        }
    }
    let name = words.join(" ");
    if nums.is_empty() {
        name
    } else {
        format!("{name} {}", nums.join("."))
    }
}

// MARK: - timestamps

/// Both CLIs write RFC3339. Parse the common shape by hand — this runs on every
/// usage line, and a general-purpose date parser is far too slow at that rate.
pub fn parse_iso(s: &str) -> Option<f64> {
    let u = s.as_bytes();
    if u.len() < 19 || u[4] != b'-' || u[7] != b'-' || u[10] != b'T' {
        return None;
    }
    let num = |a: usize, b: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &u[a..b] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as i64;
        }
        Some(v)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);

    // Days from civil (Howard Hinnant's algorithm) — no calendar, no allocation.
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let mut t = (days * 86_400 + h * 3600 + mi * 60 + sec) as f64;

    // Trailing offset: "Z" or "+09:00". Fractional seconds are ignored.
    let mut i = 19;
    if i < u.len() && u[i] == b'.' {
        i += 1;
        while i < u.len() && u[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < u.len() && (u[i] == b'+' || u[i] == b'-') && u.len() >= i + 6 {
        if let (Some(oh), Some(om)) = (num(i + 1, i + 3), num(i + 4, i + 6)) {
            let off = (oh * 3600 + om * 60) as f64;
            t += if u[i] == b'+' { -off } else { off };
        }
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_both_clis_write() {
        // 2026-01-01T00:00:00Z
        assert_eq!(parse_iso("2026-01-01T00:00:00Z"), Some(1_767_225_600.0));
        // Fractional seconds are ignored, not rejected.
        assert_eq!(parse_iso("2026-01-01T00:00:00.123Z"), Some(1_767_225_600.0));
        // A +09:00 stamp is nine hours earlier in UTC.
        assert_eq!(
            parse_iso("2026-01-01T09:00:00+09:00"),
            Some(1_767_225_600.0)
        );
        assert_eq!(parse_iso("not a date"), None);
    }

    #[test]
    fn model_names_lose_their_date_stamps() {
        assert_eq!(pretty_model("claude-opus-4-8-20260101"), "Opus 4.8");
        assert_eq!(pretty_model("claude-haiku-4-5"), "Haiku 4.5");
        assert_eq!(pretty_model("us.anthropic.claude-sonnet-5"), "Sonnet 5");
    }

    #[test]
    fn a_partial_trailing_record_is_left_for_the_next_pulse() {
        let mut entry = TailState {
            path: PathBuf::new(),
            offset: 0,
            context: 0,
        };
        let mut delta = TokenBreakdown::default();
        // Two complete lines and one still being written.
        let whole = b"{\"a\":1}\n{\"b\":2}\n{\"c\":".to_vec();
        let consumed = parse_tail(&whole, false, &mut entry, &mut delta);
        assert_eq!(consumed, 16, "only the bytes up to the last newline");
    }
}
