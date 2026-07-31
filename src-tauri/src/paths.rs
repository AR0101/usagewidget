//! Finding the CLIs' state directories, and deciding which of the pids they
//! record are still alive.
//!
//! On macOS and Linux this is just `$HOME`. On Windows it is not: plenty of
//! people run Claude Code inside WSL, in which case the transcripts live on the
//! Linux side and are reachable only through the `\\wsl.localhost\<distro>`
//! share. We look at the native profile first and fall back to WSL, so the same
//! binary works either way without the user configuring anything.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
// The WSL variant is only ever constructed on Windows, but the enum stays whole
// on every platform so the rest of the code needs no cfg of its own.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Origin {
    /// Files sit in the profile of the user running the widget.
    Native,
    /// Files sit inside a WSL distribution, reached over the 9p share.
    Wsl { distro: String },
}

#[derive(Debug, Clone)]
pub struct Roots {
    pub origin: Origin,
    /// The home directory that contains `.claude` / `.codex`.
    pub home: PathBuf,
    /// Human-readable, shown in the panel when it is not the plain native case.
    pub label: Option<String>,
}

impl Roots {
    pub fn claude_dir(&self) -> PathBuf {
        self.home.join(".claude")
    }
    pub fn claude_projects(&self) -> PathBuf {
        self.claude_dir().join("projects")
    }
    pub fn claude_sessions(&self) -> PathBuf {
        self.claude_dir().join("sessions")
    }
    pub fn claude_json(&self) -> PathBuf {
        self.home.join(".claude.json")
    }
    pub fn codex_sessions(&self) -> PathBuf {
        self.home.join(".codex").join("sessions")
    }

    /// True when this home has anything worth scanning. Used to decide whether
    /// to keep looking elsewhere.
    fn has_any_cli(&self) -> bool {
        dir_has_entries(&self.claude_projects()) || dir_has_entries(&self.codex_sessions())
    }
}

/// Locate the home directory holding the CLI state. Native profile wins; WSL is
/// consulted only when the native one has no trace of either CLI, so a user who
/// runs Claude Code on Windows directly never pays for the WSL probe.
pub fn detect() -> Roots {
    let native = Roots {
        origin: Origin::Native,
        home: native_home(),
        label: None,
    };
    if native.has_any_cli() || !cfg!(windows) {
        return native;
    }
    wsl_candidates()
        .into_iter()
        .find(|r| r.has_any_cli())
        .unwrap_or(native)
}

fn native_home() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("USERPROFILE") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let (Ok(d), Ok(p)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            return PathBuf::from(format!("{d}{p}"));
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(p) = std::env::var("HOME") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    PathBuf::from(".")
}

fn dir_has_entries(p: &Path) -> bool {
    std::fs::read_dir(p)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

// MARK: - WSL

/// Every `\\wsl.localhost\<distro>\home\<user>` that exists right now.
///
/// The share is only mounted while the distribution is running, so a stopped WSL
/// is invisible here — that is deliberate. Starting it would be a surprising
/// thing for a status widget to do.
#[cfg(windows)]
fn wsl_candidates() -> Vec<Roots> {
    let mut out = Vec::new();
    for distro in wsl_distros() {
        // \\wsl.localhost is the modern spelling; \\wsl$ still works on older builds.
        for prefix in [r"\\wsl.localhost", r"\\wsl$"] {
            let home_root = PathBuf::from(prefix).join(&distro).join("home");
            let Ok(users) = std::fs::read_dir(&home_root) else {
                continue;
            };
            for user in users.flatten() {
                out.push(Roots {
                    origin: Origin::Wsl {
                        distro: distro.clone(),
                    },
                    home: user.path(),
                    label: Some(format!("WSL · {distro}")),
                });
            }
            break; // one spelling reached it; no need for the other
        }
    }
    out
}

#[cfg(not(windows))]
fn wsl_candidates() -> Vec<Roots> {
    Vec::new()
}

/// Distribution names, preferring the mounted shares and falling back to asking
/// `wsl.exe`. Note that `wsl.exe` writes UTF-16LE, not UTF-8.
#[cfg(windows)]
fn wsl_distros() -> Vec<String> {
    for prefix in [r"\\wsl.localhost", r"\\wsl$"] {
        if let Ok(entries) = std::fs::read_dir(prefix) {
            let names: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }
    let Some(out) = run_wsl(&["-l", "-q"]) else {
        return Vec::new();
    };
    out.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Runs `wsl.exe` with no console window and decodes its UTF-16LE output.
#[cfg(windows)]
fn run_wsl(args: &[&str]) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let out = std::process::Command::new("wsl.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let bytes = out.stdout;
    // Some builds answer in UTF-8; an odd length or absent NUL padding gives it away.
    if bytes.len() % 2 != 0 || !bytes.iter().skip(1).step_by(2).any(|&b| b == 0) {
        return String::from_utf8(bytes).ok();
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&wide))
}

// MARK: - process liveness

/// Which of the recorded pids are still running.
///
/// `~/.claude/sessions/<pid>.json` is written by each live process but is not
/// removed on exit, so the pid has to be probed before the entry is trusted.
///
/// The WSL case is the awkward one: those pids belong to the Linux namespace, so
/// asking Windows about them would test unrelated processes. We ask the
/// distribution instead, in a single call rather than one per session.
pub fn live_pids(roots: &Roots, candidates: &[u32]) -> HashSet<u32> {
    match &roots.origin {
        Origin::Native => candidates
            .iter()
            .copied()
            .filter(|&pid| native_pid_alive(pid))
            .collect(),
        Origin::Wsl { distro } => wsl_live_pids(distro, candidates),
    }
}

#[cfg(windows)]
fn native_pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        // A handle can outlive the process, so the exit code is the real test.
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
fn native_pid_alive(pid: u32) -> bool {
    // signal 0 tests for existence without delivering anything. EPERM means the
    // process exists but is owned by someone else — still alive.
    unsafe {
        libc::kill(pid as libc::pid_t, 0) == 0
            || *libc::__error() == libc::EPERM
    }
}

#[cfg(windows)]
fn wsl_live_pids(distro: &str, candidates: &[u32]) -> HashSet<u32> {
    if candidates.is_empty() {
        return HashSet::new();
    }
    // One `ls /proc` beats one `kill -0` per session, and the widget re-checks
    // only on full refreshes — pulses reuse the previous answer.
    let Some(out) = run_wsl(&["-d", distro, "-e", "ls", "/proc"]) else {
        // If the distro cannot be reached, showing every recorded session is a
        // better failure than showing none.
        return candidates.iter().copied().collect();
    };
    let alive: HashSet<u32> = out
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect();
    candidates
        .iter()
        .copied()
        .filter(|p| alive.contains(p))
        .collect()
}

#[cfg(not(windows))]
fn wsl_live_pids(_distro: &str, candidates: &[u32]) -> HashSet<u32> {
    candidates.iter().copied().collect()
}
