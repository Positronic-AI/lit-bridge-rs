//! Claude Code TUI parser — a faithful Rust port of
//! `lit-bridge/parsers/claude/v2_1.py` (validated against CLI v2.1.x).
//!
//! The Python module is the spec; this port is validated against it byte-for-byte on
//! the shared capture corpus (see `tests/parity.rs`). When Anthropic changes the TUI,
//! add a new version module rather than editing this one.
//!
//! Note on regex: Rust's `regex` crate has no lookaround, so RE_COMPLETION's
//! `(?!.*…)` negative-lookahead is implemented as a post-match check (`is_completion`).

use regex::Regex;

use crate::parser::{SessionState, TuiContentBlock, TuiMessage, TuiParser, TuiState};

/// Startup dialogs: (detect-substring, dismiss-keys, name)
const STARTUP_DIALOGS: &[(&str, &[&str], &str)] = &[
    ("Select login method", &["Enter"], "login-method"),
    // Quick-select "2. Yes, I accept". The arrow-key win32 record (ENHANCED_KEY)
    // does NOT dismiss this dialog under a headless ConPTY; the digit (sent as a
    // win32 text record) does. See send_key's single-char fallback.
    ("Yes, I accept", &["2"], "bypass-permissions"),
    ("Choose the text style", &["Enter"], "theme-selection"),
    ("enable auto mode", &["Enter"], "auto-mode"),
    ("Do you trust", &["Enter"], "workspace-trust"),
    ("I trust this folder", &["Enter"], "workspace-trust"),
    ("safety check", &["Enter"], "workspace-trust"),
];

const DIALOG_STRINGS: &[&str] = &["Enter to confirm", "Esc to cancel"];

pub struct ClaudeV21Parser {
    re_user: Regex,
    re_response: Regex,
    re_completion: Regex, // ^\s*✻\s+(.+)  — lookahead handled in code
    re_spinner_active: Regex,
    re_thinking_spinner: Regex,
    re_separator: Regex,
    re_status: Regex,
    re_version: Regex,
    re_dialog_selection: Regex,
    re_tool_call_start: Regex,
    re_conversation_picker: Regex,
    re_compact_progress: Regex,
}

impl Default for ClaudeV21Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeV21Parser {
    pub fn new() -> Self {
        ClaudeV21Parser {
            re_user: Regex::new(r"^\s*❯\s").unwrap(),
            re_response: Regex::new(r"^\s*●\s").unwrap(),
            re_completion: Regex::new(r"^\s*✻\s+(.+)").unwrap(),
            re_spinner_active: Regex::new(r"^\s*[·✢-✿]\s+.*…").unwrap(),
            re_thinking_spinner: Regex::new(r"^\s*[·✢-✿]\s").unwrap(),
            re_separator: Regex::new(r"^─{10,}$").unwrap(),
            re_status: Regex::new(r"^\s*[⏵▸]").unwrap(),
            re_version: Regex::new(r"Claude Code (v[\d.]+)").unwrap(),
            re_dialog_selection: Regex::new(r"❯\s+\d+\.\s").unwrap(),
            re_tool_call_start: Regex::new(r"^([A-Z]\w*)\(").unwrap(),
            re_conversation_picker: Regex::new(
                r"^\s*[●○◯]\s+\S.*(?:↑/↓|to select|Enter to view|\d+[ms]\d*s?)|^\s*[●○◯]\s*(?:Explore|Plan|main)\b",
            )
            .unwrap(),
            re_compact_progress: Regex::new(r"^\d+%\s+until\s+auto-compact").unwrap(),
        }
    }

    /// RE_COMPLETION with the `(?!.*…)` negative lookahead applied: a `✻` line that
    /// is a completion marker (has a duration), NOT an in-progress spinner with `…`.
    fn completion_dur(&self, line: &str) -> Option<String> {
        let c = self.re_completion.captures(line)?;
        let g = c.get(1).unwrap().as_str();
        if g.contains('…') {
            None
        } else {
            Some(g.to_string())
        }
    }
    fn is_completion(&self, line: &str) -> bool {
        self.completion_dur(line).is_some()
    }

    fn strip_marker(s: &str, marker: char) -> String {
        let t = s.trim_start();
        let t = t.strip_prefix(marker).unwrap_or(t);
        t.trim_start().to_string()
    }
}

impl TuiParser for ClaudeV21Parser {
    fn detect_state(&self, capture: &str) -> SessionState {
        if capture.trim().is_empty() {
            return SessionState::Dead;
        }

        // Dialogs: only inspect the bottom of the screen (full-capture checks
        // false-positive on conversation text containing dialog strings).
        let all_trimmed: Vec<&str> = capture.trim().split('\n').collect();
        let bottom_slice = &all_trimmed[all_trimmed.len().saturating_sub(10)..];
        let bottom = bottom_slice.join("\n");
        for s in DIALOG_STRINGS {
            if bottom.contains(s) {
                return SessionState::Dialog;
            }
        }
        if self.re_dialog_selection.is_match(&bottom) {
            return SessionState::Dialog;
        }

        let lines: Vec<String> = capture
            .trim()
            .split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if lines.is_empty() {
            return SessionState::Dead;
        }

        let mut has_interrupt = false;
        let mut has_prompt = false;
        for line in lines.iter().rev() {
            if self.re_status.is_match(line) {
                if line.contains("esc to interrupt") || line.contains("interrupt") {
                    has_interrupt = true;
                }
                break;
            }
            if self.re_separator.is_match(line) {
                continue;
            }
            if self.re_compact_progress.is_match(line) {
                continue;
            }
            if line.starts_with('❯') {
                has_prompt = true;
                continue;
            }
            break;
        }

        if has_interrupt {
            return SessionState::Thinking;
        }
        if has_prompt {
            return SessionState::Idle;
        }

        for line in lines.iter().rev() {
            if self.re_status.is_match(line) {
                continue;
            }
            if self.re_separator.is_match(line) {
                continue;
            }
            if self.re_compact_progress.is_match(line) {
                continue;
            }
            if line.starts_with('❯') {
                continue;
            }
            if self.is_completion(line) {
                return SessionState::Idle;
            }
            if self.re_spinner_active.is_match(line) {
                return SessionState::Thinking;
            }
            if self.re_thinking_spinner.is_match(line) {
                return SessionState::Thinking;
            }
            break;
        }

        // Strip conversation picker / chrome from the bottom, then look for an
        // active (unterminated) response bullet.
        let mut content_lines: Vec<&str> = capture.split('\n').collect();
        while let Some(last) = content_lines.last() {
            let s = last.trim();
            if s.is_empty()
                || self.re_conversation_picker.is_match(s)
                || self.re_separator.is_match(s)
                || s.starts_with('❯')
                || self.re_status.is_match(s)
                || self.re_compact_progress.is_match(s)
            {
                content_lines.pop();
            } else {
                break;
            }
        }
        let content_area = content_lines.join("\n");
        if let Some(pos) = content_area.rfind('●') {
            let after = &content_area[pos..];
            if !after.contains('✻') && !after.contains('✽') {
                return SessionState::Responding;
            }
        }

        SessionState::Idle
    }

    fn count_assistant_messages(&self, capture: &str) -> usize {
        capture
            .split('\n')
            .filter(|line| self.re_response.is_match(line.trim()))
            .count()
    }

    fn extract_messages(&self, capture: &str) -> Vec<TuiMessage> {
        let lines: Vec<&str> = capture.split('\n').collect();
        let mut messages = Vec::new();
        let mut i = 0;

        while i < lines.len() {
            let stripped = lines[i].trim();

            if self.re_user.is_match(stripped) && stripped.chars().count() > 2 {
                let mut text = Self::strip_marker(stripped, '❯');
                i += 1;
                while i < lines.len() {
                    let s = lines[i].trim();
                    if self.re_response.is_match(s)
                        || self.re_separator.is_match(s)
                        || self.re_status.is_match(s)
                        || (self.re_user.is_match(s) && s.chars().count() > 2)
                    {
                        break;
                    }
                    if !s.is_empty() {
                        text.push('\n');
                        text.push_str(s);
                    }
                    i += 1;
                }
                messages.push(TuiMessage {
                    role: "user",
                    content: text.trim().to_string(),
                    duration: None,
                });
                continue;
            }

            if self.re_response.is_match(stripped) {
                let first_line = Self::strip_marker(stripped, '●');
                let mut duration = None;
                let mut content_lines: Vec<String> = Vec::new();
                if !first_line.is_empty() {
                    content_lines.push(first_line);
                }
                i += 1;
                while i < lines.len() {
                    let s = lines[i].trim();
                    if let Some(d) = self.completion_dur(s) {
                        duration = Some(d);
                        i += 1;
                        break;
                    }
                    if self.re_response.is_match(s)
                        || self.re_separator.is_match(s)
                        || self.re_status.is_match(s)
                        || self.re_user.is_match(s)
                    {
                        break;
                    }
                    content_lines.push(lines[i].trim_end().to_string());
                    i += 1;
                }
                let text = content_lines.join("\n").trim_end().to_string();
                messages.push(TuiMessage {
                    role: "assistant",
                    content: text,
                    duration,
                });
                continue;
            }

            i += 1;
        }

        messages
    }

    fn extract_content_blocks(&self, capture: &str) -> Vec<TuiContentBlock> {
        let lines: Vec<&str> = capture.split('\n').collect();
        let mut blocks = Vec::new();
        let mut i = 0;

        while i < lines.len() {
            let stripped = lines[i].trim();

            if stripped.is_empty()
                || self.re_user.is_match(stripped)
                || self.re_separator.is_match(stripped)
                || self.re_status.is_match(stripped)
                || self.is_completion(stripped)
            {
                i += 1;
                continue;
            }

            if self.re_response.is_match(stripped) {
                let first_line = Self::strip_marker(stripped, '●');

                if let Some(tm) = self.re_tool_call_start.captures(&first_line) {
                    let tool_name = tm.get(1).unwrap().as_str().to_string();
                    let mut tool_input = first_line[tool_name.len()..].to_string();
                    if let Some(r) = tool_input.strip_prefix('(') {
                        tool_input = r.to_string();
                    }
                    if let Some(r) = tool_input.strip_suffix(')') {
                        tool_input = r.to_string();
                    }
                    blocks.push(TuiContentBlock {
                        typ: "tool_call",
                        content: tool_input.trim().to_string(),
                        tool_name: Some(tool_name),
                    });

                    i += 1;
                    let mut output_lines: Vec<String> = Vec::new();
                    while i < lines.len() {
                        let s = lines[i].trim();
                        if self.re_response.is_match(s)
                            || self.re_user.is_match(s)
                            || self.re_separator.is_match(s)
                            || self.re_status.is_match(s)
                            || self.is_completion(s)
                        {
                            break;
                        }
                        output_lines.push(lines[i].trim_end().to_string());
                        i += 1;
                    }
                    if !output_lines.is_empty() {
                        let output = output_lines.join("\n").trim_end().to_string();
                        if !output.is_empty() {
                            blocks.push(TuiContentBlock {
                                typ: "tool_output",
                                content: output,
                                tool_name: None,
                            });
                        }
                    }
                    continue;
                }

                let mut content_lines: Vec<String> = Vec::new();
                if !first_line.is_empty() {
                    content_lines.push(first_line);
                }
                i += 1;
                while i < lines.len() {
                    let s = lines[i].trim();
                    if self.re_response.is_match(s)
                        || self.re_user.is_match(s)
                        || self.re_separator.is_match(s)
                        || self.re_status.is_match(s)
                        || self.is_completion(s)
                    {
                        break;
                    }
                    content_lines.push(lines[i].trim_end().to_string());
                    i += 1;
                }
                let text = content_lines.join("\n").trim_end().to_string();
                if !text.is_empty() {
                    blocks.push(TuiContentBlock {
                        typ: "text",
                        content: text,
                        tool_name: None,
                    });
                }
                continue;
            }

            i += 1;
        }

        blocks
    }

    fn extract_new_response(&self, baseline_count: usize, capture: &str) -> String {
        let messages = self.extract_messages(capture);
        let assistant: Vec<&TuiMessage> =
            messages.iter().filter(|m| m.role == "assistant").collect();
        if assistant.len() <= baseline_count {
            return String::new();
        }
        assistant[baseline_count..]
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn extract_raw_response(
        &self,
        baseline_count: usize,
        capture: &str,
        sent_content: Option<&str>,
        baseline_completions: usize,
    ) -> String {
        let lines: Vec<&str> = capture.split('\n').collect();

        // Trim the bottom panel (separator, prompt, status, picker, chrome).
        let mut content_end = lines.len();
        while content_end > 0 {
            let s = lines[content_end - 1].trim();
            if s.is_empty()
                || self.re_separator.is_match(s)
                || s.contains("──────────")
                || s.contains('❯')
                || self.re_status.is_match(s)
                || self.re_conversation_picker.is_match(s)
                || s.starts_with('○')
                || s.starts_with('◯')
                || s.contains("Claude Code")
                || s.contains("auto-compact")
                || s.contains("bypass permissions")
                || s.contains("esc to interrupt")
                || s.contains("paste again to expand")
                || s.contains("Run /doctor")
                || s.contains("Auto-update failed")
                || s.contains("context used")
                || s.contains("to select")
                || s.contains("Enter to view")
            {
                content_end -= 1;
            } else {
                break;
            }
        }

        let mut start_idx: Option<usize> = None;
        let mut end_idx = content_end;

        // Strategy 1: prompt landmark — first ● after the last ❯.
        let mut last_prompt_idx: Option<usize> = None;
        for i in (0..content_end).rev() {
            if self.re_user.is_match(lines[i]) {
                last_prompt_idx = Some(i);
                break;
            }
        }
        if let Some(lp) = last_prompt_idx {
            for i in (lp + 1)..content_end {
                if self.re_response.is_match(lines[i].trim()) {
                    start_idx = Some(i);
                    break;
                }
            }
            if let Some(si) = start_idx {
                for i in (si + 1)..content_end {
                    if self.is_completion(lines[i].trim()) {
                        end_idx = i + 1;
                    }
                }
            }
        }

        // Strategy 2: bullet counting.
        if start_idx.is_none() {
            let mut bullet = 0;
            for i in 0..content_end {
                if self.re_response.is_match(lines[i].trim()) {
                    bullet += 1;
                    if bullet == baseline_count + 1 && start_idx.is_none() {
                        start_idx = Some(i);
                    }
                }
                if start_idx.is_some() && self.is_completion(lines[i].trim()) {
                    end_idx = i + 1;
                }
            }
        }

        // Strategy 3: needle search on the sent message.
        if start_idx.is_none() {
            if let Some(sent) = sent_content {
                let first = sent.trim().split('\n').next().unwrap_or("");
                let needle: String = first.chars().take(80).collect();
                if !needle.is_empty() {
                    let mut sent_line: Option<usize> = None;
                    for i in (0..content_end).rev() {
                        if lines[i].contains(&needle) {
                            sent_line = Some(i);
                            break;
                        }
                    }
                    if let Some(sl) = sent_line {
                        for i in (sl + 1)..content_end {
                            if self.re_response.is_match(lines[i].trim()) {
                                start_idx = Some(i);
                                break;
                            }
                        }
                        if let Some(si) = start_idx {
                            end_idx = content_end;
                            for i in (si + 1)..content_end {
                                if self.is_completion(lines[i].trim()) {
                                    end_idx = i + 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Strategy 4: last-bullet fallback (guarded by a NEW ✻).
        if start_idx.is_none() {
            let total: usize = (0..content_end)
                .filter(|&i| self.is_completion(lines[i].trim()))
                .count();
            if total > baseline_completions {
                let mut baseline_star_idx: i64 = -1;
                let mut star = 0;
                for i in 0..content_end {
                    if self.is_completion(lines[i].trim()) {
                        star += 1;
                        if star <= baseline_completions {
                            baseline_star_idx = i as i64;
                        }
                    }
                }
                let mut last_bullet: Option<usize> = None;
                for i in (0..content_end).rev() {
                    if self.re_response.is_match(lines[i].trim()) {
                        last_bullet = Some(i);
                        break;
                    }
                }
                if let Some(lb) = last_bullet {
                    // The bullet must belong to the CURRENT turn, i.e. sit AFTER the
                    // current prompt (`❯`). Without this, during the thinking phase
                    // (a ✻ spinner is up but the model hasn't written a new `●` yet)
                    // this fallback grabbed the PREVIOUS turn's last bullet — the prior
                    // response — and streamed it back as if it were the new one. It
                    // self-corrected once a real new `●` landed, but the user saw their
                    // last answer flash as the "response" while waiting.
                    let after_prompt = last_prompt_idx.map(|lp| lb > lp).unwrap_or(true);
                    if (lb as i64) > baseline_star_idx && after_prompt {
                        start_idx = Some(lb);
                        for i in (lb + 1)..content_end {
                            if self.is_completion(lines[i].trim()) {
                                end_idx = i + 1;
                            }
                        }
                    }
                }
            }
        }

        // Pre-response thinking / compaction spinner.
        if start_idx.is_none() {
            let mut scanned = 0;
            for i in (0..content_end).rev() {
                let s = lines[i].trim();
                if s.is_empty() {
                    continue;
                }
                if self.re_spinner_active.is_match(s) || self.re_thinking_spinner.is_match(s) {
                    start_idx = Some(i);
                    end_idx = content_end;
                    break;
                }
                scanned += 1;
                if scanned >= 3 {
                    break;
                }
            }
        }

        match start_idx {
            None => String::new(),
            Some(si) => lines[si..end_idx].join("\n").trim_end().to_string(),
        }
    }

    fn turn_complete(&self, capture: &str) -> bool {
        let lines: Vec<&str> = capture.split('\n').collect();
        let mut last_user = None;
        for i in (0..lines.len()).rev() {
            let t = lines[i].trim();
            if self.re_user.is_match(t) && t.chars().count() > 2 {
                last_user = Some(i);
                break;
            }
        }
        let start = last_user.map(|i| i + 1).unwrap_or(0);
        for line in &lines[start..] {
            if self.is_completion(line.trim()) {
                return true;
            }
        }
        false
    }

    fn is_startup_dialog(&self, capture: &str) -> Option<(String, Vec<String>, String)> {
        for (detect, keys, name) in STARTUP_DIALOGS {
            if capture.contains(detect) {
                return Some((
                    detect.to_string(),
                    keys.iter().map(|k| k.to_string()).collect(),
                    name.to_string(),
                ));
            }
        }
        None
    }

    fn parse(&self, capture: &str) -> TuiState {
        let state = self.detect_state(capture);
        let messages = self.extract_messages(capture);
        let head: String = capture.chars().take(300).collect();
        let version = self
            .re_version
            .captures(&head)
            .map(|c| c.get(1).unwrap().as_str().to_string());
        let mut errors = Vec::new();
        if capture.contains('✗') {
            for line in capture.split('\n') {
                if line.contains('✗') {
                    errors.push(line.trim().to_string());
                }
            }
        }
        TuiState {
            state,
            messages,
            version,
            errors,
        }
    }
}
