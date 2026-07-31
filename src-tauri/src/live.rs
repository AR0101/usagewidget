//! Fetches plan utilisation from the account directly, instead of waiting for
//! Claude Code to refresh its on-disk cache.
//!
//! This is the one place the widget touches a credential. It reads the OAuth
//! access token Claude Code stored when the user logged in, and calls the same
//! endpoint the CLI calls. Nothing is written, nothing is logged, and the token
//! never leaves this module.
//!
//! Where that token lives depends on the platform, and the macOS original could
//! assume a keychain. Here we try each plausible store in turn and fall back to
//! the CLI's own cache if none of them answers — a missing credential makes the
//! percentages stale, not the widget broken.
//!
//! `/api/oauth/usage` is not a documented endpoint. If it moves, this fails and
//! the panel shows the cached number with the reason in its footer.

use crate::paths::Roots;
use serde_json::Value;
use std::fmt;

const SERVICE: &str = "Claude Code-credentials";
const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

/// Successful responses are reused for this long, so a fast refresh interval
/// does not turn into one request per second.
const MIN_INTERVAL: f64 = 50.0;

pub struct Live {
    pub utilization: Value,
    pub fetched_at: f64,
}

pub enum Failure {
    NoCredential,
    NoToken,
    Expired,
    Http(u16),
    Transport(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::NoCredential => write!(f, "저장된 자격증명 없음"),
            Failure::NoToken => write!(f, "자격증명에 OAuth 토큰 없음"),
            Failure::Expired => write!(f, "토큰 만료됨"),
            Failure::Http(c) => write!(f, "HTTP {c}"),
            Failure::Transport(m) => write!(f, "{m}"),
        }
    }
}

#[derive(Default)]
pub struct LiveUsage {
    cached: Option<Live>,
    last_error: Option<String>,
    last_attempt: f64,
}

impl LiveUsage {
    /// Returns the freshest utilisation available, which may be a previously
    /// fetched one. `None` means nothing has ever succeeded.
    pub fn fetch(&mut self, roots: &Roots) -> Option<&Live> {
        let now = crate::collector::now_epoch();
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|c| now - c.fetched_at < MIN_INTERVAL);
        // Rate-limit failures too, so a missing credential is not retried on
        // every single refresh.
        if fresh || now - self.last_attempt < MIN_INTERVAL {
            return self.cached.as_ref();
        }
        self.last_attempt = now;

        match request(roots, now) {
            Ok(live) => {
                self.cached = Some(live);
                self.last_error = None;
            }
            // Keep serving the last good value rather than blanking the meters.
            Err(e) => self.last_error = Some(e.to_string()),
        }
        self.cached.as_ref()
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }
}

fn request(roots: &Roots, now: f64) -> Result<Live, Failure> {
    let token = access_token(roots)?;

    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .set("anthropic-version", "2023-06-01")
        .timeout(std::time::Duration::from_secs(8))
        .call();

    let body = match resp {
        Ok(r) => r.into_string().map_err(|e| Failure::Transport(e.to_string()))?,
        Err(ureq::Error::Status(code, _)) => return Err(Failure::Http(code)),
        Err(e) => return Err(Failure::Transport(e.to_string())),
    };

    let root: Value = serde_json::from_str(&body)
        .map_err(|_| Failure::Transport("응답 파싱 실패".into()))?;
    // The CLI stores this payload verbatim under `cachedUsageUtilization`, so it
    // may arrive either bare or wrapped.
    let utilization = root.get("utilization").cloned().unwrap_or(root);
    Ok(Live {
        utilization,
        fetched_at: now,
    })
}

/// The token itself. Deliberately returned by value and never held anywhere:
/// the only thing that touches it is the Authorization header above.
fn access_token(roots: &Roots) -> Result<String, Failure> {
    let blob = credential_blob(roots).ok_or(Failure::NoCredential)?;
    let root: Value = serde_json::from_str(&blob).map_err(|_| Failure::NoToken)?;
    let oauth = root.get("claudeAiOauth").ok_or(Failure::NoToken)?;

    // expiresAt is epoch milliseconds. A stale token would just 401, but failing
    // early keeps the footer message useful.
    if let Some(ms) = oauth.get("expiresAt").and_then(Value::as_f64) {
        if ms / 1000.0 < crate::collector::now_epoch() {
            return Err(Failure::Expired);
        }
    }
    match oauth.get("accessToken").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => Ok(t.to_string()),
        _ => Err(Failure::NoToken),
    }
}

/// Each store in turn. The file is first because it is where Claude Code puts
/// the credential when there is no OS keychain to put it in — which is the case
/// on Linux, and therefore inside WSL.
fn credential_blob(roots: &Roots) -> Option<String> {
    if let Ok(s) = std::fs::read_to_string(roots.claude_dir().join(".credentials.json")) {
        if s.contains("claudeAiOauth") {
            return Some(s);
        }
    }
    from_os_store()
}

#[cfg(windows)]
fn from_os_store() -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let target: Vec<u16> = SERVICE.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 {
            return None;
        }
        let c = &*cred;
        let bytes =
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
        CredFree(cred as *mut core::ffi::c_void);
        Some(decode(bytes))
    }
}

/// Credential Manager blobs are whatever the writer put there — usually UTF-16
/// from a .NET caller, UTF-8 from anything else.
#[cfg(windows)]
fn decode(bytes: Vec<u8>) -> String {
    let looks_utf16 = bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).any(|&b| b == 0);
    if !looks_utf16 {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&wide)
}

/// macOS is not a target for this build, but it is where the port gets checked
/// against the original, so the keychain path is here too. `security` prompts
/// for approval the first time, exactly as the Swift widget does.
#[cfg(target_os = "macos")]
fn from_os_store() -> Option<String> {
    let out = std::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", SERVICE, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn from_os_store() -> Option<String> {
    None
}
