//! Background agents: sub-agents the CLI launched with `run_in_background` and
//! did not wait for. A turn can END while one works on for an hour — the TUI
//! shows a row for it (`○ general-purpose  Fixing emitter…  52m · ↓ 509k tokens`),
//! but a chat client sees a finished reply and an idle prompt (games, 2026-10-01).
//!
//! Claude Code writes each one beside the parent transcript:
//!
//!   <project>/<session-id>/subagents/agent-<id>.meta.json
//!       {"agentType":"general-purpose","description":"…","requestShape":"background",…}
//!   <project>/<session-id>/subagents/agent-<id>.jsonl
//!       the agent's own transcript, appended live, same row format as the parent
//!
//! An agent is RUNNING while its transcript's last row is not an assistant row
//! with a terminal `stop_reason`. A resumed agent gets new rows (and a rewritten
//! meta file) and is running again. A foreground sub-agent has no
//! `requestShape` and is not listed: its turn is still open and visible.

use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A transcript untouched this long whose last row is not terminal belongs to an
/// agent that died with its CLI; without a cap it would be listed forever.
const STALE: Duration = Duration::from_secs(30 * 60);
/// How much of the transcript tail to read looking for the last complete row.
const TAIL: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct BackgroundAgent {
    pub id: String,
    pub agent_type: String,
    pub description: String,
    /// When this run began: the meta file's mtime (rewritten on resume).
    pub started_ms: u64,
    /// Last write to the agent's transcript.
    pub last_ms: u64,
}

fn mtime_ms(p: &Path) -> Option<u64> {
    let t = fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// The last newline-terminated row of a transcript, parsed. `None` when the
/// file is empty, unreadable, or its last row is larger than the tail window.
fn last_row(path: &Path) -> Option<Value> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    // Drop a trailing partial line (a row mid-write), then take the last full one.
    let end = buf.iter().rposition(|&b| b == b'\n')?;
    let body = &buf[..end];
    let start = body.iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    if start == 0 && from > 0 {
        return None; // the row began before the window
    }
    serde_json::from_slice(&body[start..]).ok()
}

/// Has this agent's transcript reached a terminal assistant row?
fn finished(row: &Value) -> bool {
    if row.get("type").and_then(|v| v.as_str()) != Some("assistant") {
        return false;
    }
    matches!(
        row.get("message").and_then(|m| m.get("stop_reason")).and_then(|v| v.as_str()),
        Some("end_turn") | Some("stop_sequence") | Some("max_tokens") | Some("refusal")
    )
}

/// The background agents of one CLI session that are running now, oldest first.
pub fn running(subagents_dir: &Path) -> Vec<BackgroundAgent> {
    running_at(subagents_dir, SystemTime::now())
}

fn running_at(subagents_dir: &Path, now: SystemTime) -> Vec<BackgroundAgent> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(subagents_dir) else { return out };
    let now_ms = now.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".meta.json")) else {
            continue;
        };
        let meta_path = entry.path();
        let Some(meta) = fs::read(&meta_path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        if meta.get("requestShape").and_then(|v| v.as_str()) != Some("background") {
            continue;
        }
        let transcript = subagents_dir.join(format!("agent-{id}.jsonl"));
        let started_ms = mtime_ms(&meta_path).unwrap_or(now_ms);
        // No transcript yet = launched this instant; count it from the meta file.
        let last_ms = mtime_ms(&transcript).unwrap_or(started_ms);
        if transcript.exists() {
            if last_row(&transcript).map(|r| finished(&r)).unwrap_or(false) {
                continue;
            }
        }
        if now_ms.saturating_sub(last_ms) > STALE.as_millis() as u64 {
            continue;
        }
        out.push(BackgroundAgent {
            id: id.to_string(),
            agent_type: meta.get("agentType").and_then(|v| v.as_str()).unwrap_or("agent").to_string(),
            description: meta.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            started_ms,
            last_ms,
        });
    }
    out.sort_by(|a, b| a.started_ms.cmp(&b.started_ms).then(a.id.cmp(&b.id)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lbrs-subagents-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn agent(d: &Path, id: &str, shape: Option<&str>, rows: &[&str]) {
        let shape = shape.map(|s| format!(r#","requestShape":"{s}""#)).unwrap_or_default();
        fs::write(
            d.join(format!("agent-{id}.meta.json")),
            format!(r#"{{"agentType":"general-purpose","description":"Task {id}"{shape}}}"#),
        )
        .unwrap();
        let mut f = fs::File::create(d.join(format!("agent-{id}.jsonl"))).unwrap();
        for r in rows {
            writeln!(f, "{r}").unwrap();
        }
    }

    const USER: &str = r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"go"}}"#;
    const TOOL: &str = r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#;
    const DONE: &str = r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"#;

    #[test]
    fn a_background_agent_is_listed_until_its_transcript_ends() {
        let d = dir("lifecycle");
        agent(&d, "aaa", Some("background"), &[USER, TOOL]);
        let got = running(&d);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "aaa");
        assert_eq!(got[0].description, "Task aaa");
        assert_eq!(got[0].agent_type, "general-purpose");

        agent(&d, "aaa", Some("background"), &[USER, TOOL, DONE]);
        assert!(running(&d).is_empty(), "end_turn closes it");

        // Resumed: a new prompt row lands after the terminal one.
        agent(&d, "aaa", Some("background"), &[USER, TOOL, DONE, USER]);
        assert_eq!(running(&d).len(), 1, "a resumed agent is running again");
    }

    #[test]
    fn foreground_subagents_and_stale_or_partial_files_are_handled() {
        let d = dir("filters");
        agent(&d, "fg", None, &[USER, TOOL]); // no requestShape = foreground
        agent(&d, "bg", Some("background"), &[USER, TOOL]);
        // A row mid-write (no trailing newline) must not hide the real last row.
        let mut f = fs::OpenOptions::new().append(true).open(d.join("agent-bg.jsonl")).unwrap();
        write!(f, r#"{{"type":"assistant","message":{{"stop_reason":"end_tu"#).unwrap();
        drop(f);
        let got = running(&d);
        assert_eq!(got.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), vec!["bg"]);

        // Untouched for longer than the cap: the agent died with its CLI.
        let later = SystemTime::now() + STALE + Duration::from_secs(60);
        assert!(running_at(&d, later).is_empty());

        // A meta file with no transcript yet is a launch in progress.
        fs::write(d.join("agent-new.meta.json"), r#"{"agentType":"fork","description":"x","requestShape":"background"}"#).unwrap();
        assert!(running(&d).iter().any(|a| a.id == "new"));
        assert!(running(&dir("missing").join("nope")).is_empty());
    }
}
