//! Diagnostic event capture — ON by default, written to
//! `/tmp/bridge-rs-evt-{user}.log` unless `LIT_BRIDGE_RS_EVENTLOG` overrides the
//! path (or disables it with `off`/`0`/`none`/empty).
//!
//! Defaulting on means a hand-launched bridge captures the trail without anyone
//! remembering to export the env var — the footgun that hid the last relay hang.
//!
//! Every JSONL line the watcher consumes and every event the daemon emits is
//! appended with an absolute millisecond timestamp, so a turn that loses content
//! or repeats can be reconstructed against the Claude transcript instead of
//! guessed at. Cheap: one resolved path lookup per call, then an append.
//!
//! Format (one record per line): `<epoch_ms> <tag> <payload>`
//!   RX   — a raw JSONL transcript line the watcher read
//!   TURN — a watcher state transition (open/complete) with the parts count
//!   EMIT — an event the daemon sent to the client (the consumer's ground truth)
//!
//! The log self-caps: once it passes `MAX_BYTES` it rotates to a single `.1`
//! backup, so a long-lived bridge can't grow an unbounded trail (a 246 MB
//! runaway is what prompted this). On-disk worst case is ~2× `MAX_BYTES`.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rotate once the active log passes this size. 64 MiB keeps a deep trail for
/// reconstructing a hang while capping the file; with the one `.1` backup the
/// on-disk total stays under ~128 MiB.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Resolved once: `Some(path)` if logging is enabled, else `None`. Avoids an env
/// lookup on every poll line after the first call.
///
/// Resolution order:
///   - `LIT_BRIDGE_RS_EVENTLOG` set to a path  → use it
///   - `LIT_BRIDGE_RS_EVENTLOG` = off/0/none/"" → disabled
///   - unset                                   → `/tmp/bridge-rs-evt-{user}.log`
fn target() -> Option<&'static str> {
    static TARGET: OnceLock<Option<String>> = OnceLock::new();
    TARGET
        .get_or_init(|| match std::env::var("LIT_BRIDGE_RS_EVENTLOG") {
            Ok(v) => {
                let t = v.trim();
                match t.to_ascii_lowercase().as_str() {
                    "" | "off" | "0" | "none" | "false" => None,
                    _ => Some(t.to_string()),
                }
            }
            Err(_) => {
                let user = std::env::var("USER")
                    .or_else(|_| std::env::var("LOGNAME"))
                    .unwrap_or_else(|_| "unknown".to_string());
                Some(format!("/tmp/bridge-rs-evt-{user}.log"))
            }
        })
        .as_deref()
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Append one diagnostic record. No-op unless the log file is configured.
pub fn log(tag: &str, payload: &str) {
    let Some(path) = target() else { return };
    // One-line records; collapse newlines in the payload so each event stays atomic.
    let flat = payload.replace('\n', "\\n");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {} {}", epoch_ms(), tag, flat);
        let _ = f.flush();
        // One fstat on the handle we already hold — cheap. Past the cap, rotate.
        if let Ok(meta) = f.metadata() {
            if meta.len() >= MAX_BYTES {
                rotate(path);
            }
        }
    }
}

/// Single-backup rotation: `path` → `path.1` (replacing any prior backup); the
/// next `log()` recreates a fresh `path` via `create(true)`. The re-check under
/// the lock is what makes concurrent callers safe — the thread that wins renames
/// the full log away, and every loser then sees a small file and bails, so a
/// freshly rotated log is never renamed a second time.
fn rotate(path: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() >= MAX_BYTES => {}
        _ => return,
    }
    let _ = std::fs::rename(path, format!("{path}.1"));
}
