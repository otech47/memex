//! recall: deterministic index and search over Claude Code, Codex, Kimi Code and Cursor transcripts.
//! One binary: deterministic ranking by default, an optional model pick behind `recall agent`.

use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::Regex as BRegex;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const PROMPT_CAP: usize = 1500;
const PROMPTS_MAX: usize = 80;
const FIRST_CAP: usize = 2000;
const SUMMARY_CAP: usize = 3000;
const PATHS_MAX: usize = 300;
const CONTINUED: &str = "This session is being continued from a previous conversation";

const SKIP_TAGS: &[&str] = &[
    "environment_context", "user_instructions", "permissions", "turn_aborted", "INSTRUCTIONS", "skills",
    "app-context", "system-reminder", "local-command-caveat", "local-command-stdout", "command-name",
    "command-message", "command-args", "task-notification", "tool_result", "image", "no", "hook", "reminder",
    "recommended_plugins",
];
const SKIP_PREFIXES: &[&str] = &[
    "# AGENTS.md instructions", "Stop hook feedback", "Base directory for", "(Re-invocation", "[Request interrupted",
    "[Image:", "Skill /", "Another Claude session", "The fork runs", ">>> ", "Reviewed Codex session",
    "The Codex agent has", "The following is the Codex", "Assess the exact planned", "Planned action JSON",
    "<no retained transcript",
];
const SKIP_SLASH: &[&str] = &["/compact", "/exit", "/model", "/login", "/clear", "/resume", "/quit", "/help", "/status"];
const STOP_WORDS: &str = "a an the and or of to in on for with at by from as is are was were be been being it its this that these those
i me my we our you your he she they them their he's it's i'm i'd i'll we're you're don't doesn't didn't can't won't
do does did doing done have has had having not no yes so if then than but because about into over under again
there here where when which who whom what why how all any both each few more most other some such only own same
than too very just also like get got go went going come came make made want wanted need needed think thought know
knew remember try tried use used using able should would could will can may might must let lets okay ok yeah yes
um uh right thing things something anything stuff kind sort really actually basically maybe probably one two
session sessions find search look looking";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}
fn index_dir() -> PathBuf {
    std::env::var("RECALL_INDEX_DIR").map(PathBuf::from).unwrap_or_else(|_| home().join(".agents").join("recall"))
}
fn index_file() -> PathBuf {
    index_dir().join("index.jsonl")
}

fn stop_words() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| STOP_WORDS.split_whitespace().collect())
}
fn tag_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^<([A-Za-z_\-]+)").unwrap())
}
fn paste_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"</?pasted_content[^>]*>").unwrap())
}
fn path_re() -> &'static BRegex {
    static R: OnceLock<BRegex> = OnceLock::new();
    R.get_or_init(|| BRegex::new(r#""(?:file_path|path|notebook_path)":"((?:/|~/)[^"\\]{3,200})""#).unwrap())
}
fn token_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"[a-z0-9]+").unwrap())
}

/// Porter step 1a and 1b, nothing else: enough to fold plurals and -ed/-ing, never shorter than 4 chars.
fn stem(w: &str) -> String {
    if w.len() < 5 {
        return w.to_string();
    }
    let mut w = w.to_string();
    if w.ends_with("sses") {
        w.truncate(w.len() - 2);
    } else if w.ends_with("ies") {
        w.truncate(w.len() - 2);
    } else if w.ends_with("ss") {
    } else if w.ends_with('s') {
        w.truncate(w.len() - 1);
    }
    for suf in ["ing", "ed"] {
        if w.ends_with(suf) && w.len() - suf.len() >= 4 {
            let base = &w[..w.len() - suf.len()];
            if base.chars().any(|c| "aeiouy".contains(c)) {
                w.truncate(w.len() - suf.len());
                break;
            }
        }
    }
    w
}

fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let stop = stop_words();
    token_re()
        .find_iter(&lower)
        .map(|m| m.as_str())
        .filter(|t| t.len() > 1 && !stop.contains(t))
        .map(stem)
        .collect()
}

fn iso_from_ms(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_iso_secs(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    let num = |a: usize, e: usize| -> Option<i64> { s.get(a..e)?.parse().ok() };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn clean_prompt(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if SKIP_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return None;
    }
    if let Some(c) = tag_re().captures(t) {
        if SKIP_TAGS.contains(&&c[1]) {
            return None;
        }
    }
    let mut t = t.to_string();
    if t.starts_with("<pasted_content") {
        t = paste_re().replace_all(&t, "").trim().to_string();
    }
    if t.starts_with('/') {
        let head = t.split_whitespace().next().unwrap_or("");
        if SKIP_SLASH.contains(&head) {
            return None;
        }
    }
    Some(t)
}

struct Session {
    harness: &'static str,
    id: String,
    path: String,
    mtime: f64,
    size: u64,
    cwd: String,
    branch: String,
    started: String,
    ended: String,
    title: String,
    summary: String,
    prompts: Vec<String>,
    paths: Vec<String>,
    kind: String,
    turns: u64,
}

fn stat(p: &Path) -> (f64, u64) {
    match fs::metadata(p) {
        Ok(m) => {
            let mt = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            (mt, m.len())
        }
        Err(_) => (0.0, 0),
    }
}

impl Session {
    fn new(harness: &'static str, id: String, path: &Path) -> Session {
        let (mtime, size) = stat(path);
        Session {
            harness, id, path: path.to_string_lossy().into_owned(), mtime, size,
            cwd: String::new(), branch: String::new(), started: String::new(), ended: String::new(),
            title: String::new(), summary: String::new(), prompts: vec![], paths: vec![], kind: "user".into(), turns: 0,
        }
    }
    fn add_prompt(&mut self, text: &str) {
        let Some(t) = clean_prompt(text) else { return };
        if t.starts_with(CONTINUED) {
            if self.summary.chars().count() < SUMMARY_CAP {
                let joined = format!("{} {}", self.summary, t);
                self.summary = take_chars(&joined, SUMMARY_CAP).to_string();
            }
            return;
        }
        self.turns += 1;
        if self.prompts.len() < PROMPTS_MAX {
            self.prompts.push(take_chars(&t, PROMPT_CAP).to_string());
        }
    }
    fn add_paths(&mut self, raw: &[u8]) {
        if self.paths.len() >= PATHS_MAX {
            return;
        }
        for c in path_re().captures_iter(raw) {
            let p = String::from_utf8_lossy(&c[1]).into_owned();
            if !self.paths.contains(&p) {
                self.paths.push(p);
                if self.paths.len() >= PATHS_MAX {
                    return;
                }
            }
        }
    }
    fn record(&self) -> Value {
        let first = self.prompts.first().map(|p| take_chars(p, FIRST_CAP).to_string()).unwrap_or_default();
        let project = if self.cwd.is_empty() {
            String::new()
        } else {
            self.cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
        };
        json!({
            "harness": self.harness, "id": self.id, "path": self.path, "mtime": self.mtime, "size": self.size,
            "cwd": self.cwd, "project": project, "branch": self.branch, "started": self.started, "ended": self.ended,
            "title": self.title, "summary": self.summary, "first": first, "prompts": self.prompts,
            "paths": self.paths, "turns": self.turns, "kind": self.kind,
            "resume": resume_cmd(self.harness, &self.id, &self.cwd),
        })
    }
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_./~-".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn resume_cmd(harness: &str, sid: &str, cwd: &str) -> String {
    let cd = if cwd.is_empty() { String::new() } else { format!("cd {} && ", shell_quote(cwd)) };
    match harness {
        "claude" => format!("{cd}claude --resume {sid}"),
        "codex" => format!("{cd}codex resume {sid}"),
        "kimi" => format!("{cd}kimi --session {sid}"),
        "cursor" => format!("{cd}agent --resume {sid}"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- parsers

fn text_of_content(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter(|p| matches!(p.get("type").and_then(Value::as_str), Some("text") | Some("input_text")))
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn find_in(hay: &[u8], needle: &[u8], end: usize) -> Option<usize> {
    let end = end.min(hay.len());
    find(&hay[..end], needle)
}
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(hay, needle)
}
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}
fn slice_str(raw: &[u8], a: usize, b: usize) -> String {
    let b = b.min(raw.len());
    if a >= b {
        return String::new();
    }
    String::from_utf8_lossy(&raw[a..b]).into_owned()
}

fn for_each_line(path: &Path, mut f: impl FnMut(&[u8])) -> std::io::Result<()> {
    let file = fs::File::open(path)?;
    let mut rd = BufReader::with_capacity(1 << 20, file);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    loop {
        buf.clear();
        let n = rd.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        f(&buf);
    }
    Ok(())
}

fn parse_claude(path: &Path) -> std::io::Result<Session> {
    let stem_name = path.file_name().unwrap().to_string_lossy();
    let sid = stem_name.trim_end_matches(".jsonl").to_string();
    let mut s = Session::new("claude", sid, path);
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    for_each_line(path, |raw| {
        if raw.starts_with(b"{\"type\":\"") {
            if raw.starts_with(b"{\"type\":\"ai-title\"") || raw.starts_with(b"{\"type\":\"custom-title\"") || raw.starts_with(b"{\"type\":\"summary\"") {
                let Ok(o) = serde_json::from_slice::<Value>(raw) else { return };
                match o.get("type").and_then(Value::as_str) {
                    Some("custom-title") => {
                        let t = o.get("customTitle").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            s.title = t.to_string();
                        }
                    }
                    Some("ai-title") if s.title.is_empty() => {
                        s.title = o.get("aiTitle").and_then(Value::as_str).unwrap_or("").to_string();
                    }
                    Some("summary") => {
                        let joined = format!("{} {}", s.summary, o.get("summary").and_then(Value::as_str).unwrap_or(""));
                        s.summary = take_chars(&joined, SUMMARY_CAP).to_string();
                    }
                    _ => {}
                }
            }
            return;
        }
        if let Some(i) = find(raw, b"\"timestamp\":\"") {
            let ts = slice_str(raw, i + 13, i + 33);
            if first_ts.is_none() {
                first_ts = Some(ts.clone());
            }
            last_ts = Some(ts);
        }
        if find_in(raw, b"\"type\":\"user\"", 300).is_some() {
            if find_in(raw, b"\"tool_result\"", 600).is_some() {
                return;
            }
            let Ok(o) = serde_json::from_slice::<Value>(raw) else { return };
            if s.cwd.is_empty() {
                if let Some(c) = o.get("cwd").and_then(Value::as_str) {
                    if !c.is_empty() {
                        s.cwd = c.to_string();
                        s.branch = o.get("gitBranch").and_then(Value::as_str).unwrap_or("").to_string();
                        if o.get("sessionKind").and_then(Value::as_str) == Some("bg") {
                            s.kind = "background".into();
                        }
                    }
                }
            }
            if o.get("isMeta").and_then(Value::as_bool).unwrap_or(false) || o.get("isSidechain").and_then(Value::as_bool).unwrap_or(false) {
                return;
            }
            let text = text_of_content(o.get("message").and_then(|m| m.get("content")));
            s.add_prompt(&text);
        } else if find_in(raw, b"\"type\":\"assistant\"", 300).is_some() {
            if contains(raw, b"\"file_path\":\"") || contains(raw, b"\"path\":\"") {
                s.add_paths(raw);
            }
            if s.cwd.is_empty() {
                if let Some(i) = find(raw, b"\"cwd\":\"") {
                    if let Some(j) = raw[i + 7..].iter().position(|&b| b == b'"') {
                        s.cwd = slice_str(raw, i + 7, i + 7 + j);
                    }
                }
            }
        }
    })?;
    let fix = |t: Option<String>| -> String {
        match t {
            Some(t) => {
                let t = t.trim_end_matches('.');
                if t.is_empty() { String::new() } else { format!("{}Z", take_chars(t, 19)) }
            }
            None => String::new(),
        }
    };
    s.started = fix(first_ts);
    s.ended = fix(last_ts);
    Ok(s)
}

fn load_codex_titles() -> HashMap<String, String> {
    let mut titles = HashMap::new();
    let p = home().join(".codex").join("session_index.jsonl");
    if let Ok(f) = fs::File::open(&p) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(o) = serde_json::from_str::<Value>(&line) {
                if let Some(id) = o.get("id").and_then(Value::as_str) {
                    titles.insert(id.to_string(), o.get("thread_name").and_then(Value::as_str).unwrap_or("").to_string());
                }
            }
        }
    }
    titles
}

fn parse_codex(path: &Path, titles: &HashMap<String, String>) -> std::io::Result<Option<Session>> {
    let mut s: Option<Session> = None;
    let mut last_ts: Option<String> = None;
    let mut skip = false;
    for_each_line(path, |raw| {
        if skip {
            return;
        }
        match s.as_mut() {
            None => {
                if !contains(raw, b"\"type\":\"session_meta\"") {
                    return;
                }
                let Ok(o) = serde_json::from_slice::<Value>(raw) else { return };
                let p = o.get("payload").cloned().unwrap_or(Value::Null);
                if p.get("thread_source").and_then(Value::as_str).unwrap_or("user") != "user" {
                    skip = true;
                    return;
                }
                let id = p.get("id").and_then(Value::as_str).or_else(|| p.get("session_id").and_then(Value::as_str)).unwrap_or("").to_string();
                let mut ns = Session::new("codex", id, path);
                ns.cwd = p.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
                let ts = p.get("timestamp").and_then(Value::as_str).or_else(|| o.get("timestamp").and_then(Value::as_str)).unwrap_or("");
                ns.started = format!("{}Z", take_chars(ts, 19));
                if let Some(g) = p.get("git").and_then(Value::as_object) {
                    ns.branch = g.get("branch").and_then(Value::as_str).unwrap_or("").to_string();
                }
                ns.title = titles.get(&ns.id).cloned().unwrap_or_default();
                s = Some(ns);
            }
            Some(s) => {
                if let Some(i) = find(raw, b"\"timestamp\":\"") {
                    last_ts = Some(slice_str(raw, i + 13, i + 32));
                }
                if contains(raw, b"\"role\":\"user\"") && raw.starts_with(b"{\"timestamp\"") && find_in(raw, b"\"type\":\"response_item\"", 120).is_some() {
                    let Ok(o) = serde_json::from_slice::<Value>(raw) else { return };
                    if let Some(p) = o.get("payload") {
                        if p.get("type").and_then(Value::as_str) == Some("message") && p.get("role").and_then(Value::as_str) == Some("user") {
                            s.add_prompt(&text_of_content(p.get("content")));
                        }
                    }
                } else if contains(raw, b"\"path\":\"") || contains(raw, b"\"file_path\":\"") {
                    s.add_paths(raw);
                }
            }
        }
    })?;
    if skip {
        return Ok(None);
    }
    Ok(s.map(|mut s| {
        s.ended = match last_ts {
            Some(t) => format!("{t}Z"),
            None => s.started.clone(),
        };
        s
    }))
}

fn parse_kimi(state_path: &Path) -> Option<Session> {
    let d = state_path.parent()?;
    let wire = d.join("agents").join("main").join("wire.jsonl");
    let src: &Path = if wire.exists() { &wire } else { state_path };
    let st: Value = serde_json::from_slice(&fs::read(state_path).ok()?).ok()?;
    let id = st.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| d.file_name().unwrap().to_string_lossy().into_owned());
    let mut s = Session::new("kimi", id, src);
    s.cwd = st.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
    if st.get("archived").and_then(Value::as_bool).unwrap_or(false) {
        s.kind = "archived".into();
    }
    let mut first_t: Option<i64> = None;
    let mut last_t: Option<i64> = None;
    if wire.exists() {
        let _ = for_each_line(&wire, |raw| {
            if let Some(i) = rfind(raw, b"\"time\":") {
                let digits: String = raw[i + 7..].iter().take(13).take_while(|b| b.is_ascii_digit()).map(|&b| b as char).collect();
                if let Ok(t) = digits.parse::<i64>() {
                    if first_t.is_none() {
                        first_t = Some(t);
                    }
                    last_t = Some(t);
                }
            }
            if raw.starts_with(b"{\"type\":\"context.append_message\"") {
                let Ok(o) = serde_json::from_slice::<Value>(raw) else { return };
                if let Some(m) = o.get("message") {
                    if m.get("role").and_then(Value::as_str) == Some("user") && m.get("origin").and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("user") {
                        s.add_prompt(&text_of_content(m.get("content")));
                    }
                }
            } else if raw.starts_with(b"{\"type\":\"metadata\"") {
                if let Ok(o) = serde_json::from_slice::<Value>(raw) {
                    if let Some(c) = o.get("created_at").and_then(Value::as_i64) {
                        first_t = Some(c);
                    }
                }
            } else if raw.starts_with(b"{\"type\":\"tool.call\"") {
                s.add_paths(raw);
            }
        });
    }
    if s.prompts.is_empty() {
        if let Some(lp) = st.get("lastPrompt").and_then(Value::as_str) {
            if !lp.is_empty() {
                s.add_prompt(lp);
            }
        }
    }
    s.started = first_t.map(iso_from_ms).unwrap_or_default();
    s.ended = last_t.map(iso_from_ms).unwrap_or_else(|| s.started.clone());
    Some(s)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::rfind(hay, needle)
}

fn parse_cursor(meta_path: &Path) -> Option<Session> {
    let d = meta_path.parent()?;
    let meta: Value = serde_json::from_slice(&fs::read(meta_path).ok()?).ok()?;
    let mut s = Session::new("cursor", d.file_name()?.to_string_lossy().into_owned(), meta_path);
    s.cwd = meta.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
    s.title = meta.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    s.started = iso_from_ms(meta.get("createdAtMs").and_then(Value::as_i64).unwrap_or(0));
    s.ended = iso_from_ms(meta.get("updatedAtMs").and_then(Value::as_i64).unwrap_or(0));
    let ph = d.join("prompt_history.json");
    if ph.exists() {
        if let Ok(hist) = serde_json::from_slice::<Value>(&fs::read(&ph).unwrap_or_default()) {
            if let Some(a) = hist.as_array() {
                for p in a.iter().rev() {
                    if let Some(t) = p.as_str() {
                        s.add_prompt(t);
                    }
                }
            }
        }
        let (mt, sz) = stat(&ph);
        s.mtime = s.mtime.max(mt);
        s.size += sz;
    }
    let log = home().join(".cursor").join("sessions").join(format!("{}.jsonl", s.id));
    if log.exists() {
        let _ = for_each_line(&log, |raw| {
            if find_in(raw, b"\"event\":\"preToolUse\"", 60).is_some() {
                s.add_paths(raw);
            }
        });
    }
    Some(s)
}

// ---------------------------------------------------------------- index

fn list_dir(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(p).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect()).unwrap_or_default();
    v.sort();
    v
}
fn name_matches(p: &Path, prefix: &str, suffix: &str) -> bool {
    let n = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    n.starts_with(prefix) && n.ends_with(suffix)
}

fn discover() -> Vec<(&'static str, PathBuf)> {
    let h = home();
    let mut out = vec![];
    for proj in list_dir(&h.join(".claude").join("projects")) {
        for f in list_dir(&proj) {
            if f.is_file() && name_matches(&f, "", ".jsonl") && !name_matches(&f, "agent-", "") {
                out.push(("claude", f));
            }
        }
    }
    for y in list_dir(&h.join(".codex").join("sessions")) {
        for m in list_dir(&y) {
            for d in list_dir(&m) {
                for f in list_dir(&d) {
                    if name_matches(&f, "rollout-", ".jsonl") {
                        out.push(("codex", f));
                    }
                }
            }
        }
    }
    for wd in list_dir(&h.join(".kimi-code").join("sessions")) {
        if !name_matches(&wd, "wd_", "") {
            continue;
        }
        for sd in list_dir(&wd) {
            if name_matches(&sd, "session_", "") && sd.join("state.json").is_file() {
                out.push(("kimi", sd.join("state.json")));
            }
        }
    }
    for ws in list_dir(&h.join(".cursor").join("chats")) {
        for chat in list_dir(&ws) {
            if chat.join("meta.json").is_file() {
                out.push(("cursor", chat.join("meta.json")));
            }
        }
    }
    out
}

fn fingerprint(harness: &str, p: &Path) -> (String, f64, u64) {
    let (mt, size) = stat(p);
    match harness {
        "kimi" => {
            let w = p.parent().unwrap().join("agents").join("main").join("wire.jsonl");
            if w.exists() {
                let (mt, size) = stat(&w);
                (w.to_string_lossy().into_owned(), mt, size)
            } else {
                (p.to_string_lossy().into_owned(), mt, size)
            }
        }
        "cursor" => {
            let ph = p.parent().unwrap().join("prompt_history.json");
            let (mut mt, mut size) = (mt, size);
            if ph.exists() {
                let (m2, s2) = stat(&ph);
                size += s2;
                mt = mt.max(m2);
            }
            (p.to_string_lossy().into_owned(), mt, size)
        }
        _ => (p.to_string_lossy().into_owned(), mt, size),
    }
}

fn load_index() -> Vec<Value> {
    let mut recs = vec![];
    if let Ok(f) = fs::File::open(index_file()) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                recs.push(v);
            }
        }
    }
    recs
}

fn build_index(full: bool, verbose: bool) -> Vec<Value> {
    let t0 = Instant::now();
    let old: HashMap<String, Value> = if full {
        HashMap::new()
    } else {
        load_index().into_iter().filter_map(|r| r.get("path").and_then(Value::as_str).map(|p| (p.to_string(), r.clone()))).collect()
    };
    let titles = load_codex_titles();
    let items = discover();
    let results: Vec<Option<(Value, bool)>> = items
        .par_iter()
        .map(|(harness, p)| {
            let (key, mt, size) = fingerprint(harness, p);
            if let Some(r) = old.get(&key) {
                let same = (r.get("mtime").and_then(Value::as_f64).unwrap_or(-1.0) - mt).abs() < 1e-6 && r.get("size").and_then(Value::as_u64) == Some(size);
                if same {
                    let mut r = r.clone();
                    if *harness == "codex" && r.get("title").and_then(Value::as_str).unwrap_or("").is_empty() {
                        if let Some(t) = r.get("id").and_then(Value::as_str).and_then(|id| titles.get(id)) {
                            if !t.is_empty() {
                                r["title"] = Value::String(t.clone());
                            }
                        }
                    }
                    return Some((r, true));
                }
            }
            let s = match *harness {
                "claude" => parse_claude(p).ok(),
                "codex" => parse_codex(p, &titles).ok().flatten(),
                "kimi" => parse_kimi(p),
                _ => parse_cursor(p),
            };
            s.map(|mut s| {
                s.mtime = mt;
                s.size = size;
                (s.record(), false)
            })
        })
        .collect();
    let mut out = vec![];
    let (mut reused, mut parsed) = (0, 0);
    for r in results.into_iter().flatten() {
        if r.1 { reused += 1 } else { parsed += 1 }
        out.push(r.0);
    }
    fs::create_dir_all(index_dir()).ok();
    let tmp = index_file().with_extension("jsonl.tmp");
    {
        let mut f = std::io::BufWriter::new(fs::File::create(&tmp).expect("write index"));
        for r in &out {
            serde_json::to_writer(&mut f, r).unwrap();
            f.write_all(b"\n").unwrap();
        }
    }
    fs::rename(&tmp, index_file()).expect("rename index");
    if verbose {
        eprintln!("indexed {} sessions ({} parsed, {} reused) in {:.2}s", out.len(), parsed, reused, t0.elapsed().as_secs_f64());
    }
    out
}

// ---------------------------------------------------------------- search

fn s(r: &Value, k: &str) -> String {
    r.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}
fn field_weight(f: &str) -> f64 {
    match f {
        "title" => 4.0, "project" => 3.0, "branch" => 2.0, "first" => 2.5, "prompts" => 1.0, "summary" => 1.5, "paths" => 1.5,
        _ => 1.0,
    }
}
fn doc_fields(r: &Value) -> Vec<(&'static str, String)> {
    let prompts: Vec<String> = r.get("prompts").and_then(Value::as_array).map(|a| a.iter().skip(1).map(|p| p.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
    let paths: Vec<String> = r.get("paths").and_then(Value::as_array).map(|a| a.iter().map(|p| p.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
    vec![
        ("title", s(r, "title")),
        ("project", format!("{} {}", s(r, "project"), s(r, "cwd").replace('/', " "))),
        ("branch", s(r, "branch")),
        ("first", s(r, "first")),
        ("prompts", prompts.join("\n")),
        ("summary", s(r, "summary")),
        ("paths", paths.join(" ")),
    ]
}

struct Args {
    cmd: String,
    args: Vec<String>,
    harness: Option<String>,
    project: Option<String>,
    since: Option<String>,
    until: Option<String>,
    limit: usize,
    json: bool,
    all: bool,
    full: bool,
    no_refresh: bool,
    threads: Option<usize>,
    exclude: Vec<String>,
    agent: String,
    model: Option<String>,
    resume: bool,
}

fn filter_recs<'a>(recs: &'a [Value], a: &Args) -> Vec<&'a Value> {
    recs.iter()
        .filter(|r| {
            if let Some(h) = &a.harness {
                if s(r, "harness") != *h { return false; }
            }
            let id = s(r, "id");
            if a.exclude.iter().any(|x| id.starts_with(x.as_str())) { return false; }
            if let Some(p) = &a.project {
                let hay = format!("{}{}", s(r, "project"), s(r, "cwd")).to_lowercase();
                if !hay.contains(&p.to_lowercase()) { return false; }
            }
            if let Some(since) = &a.since {
                if s(r, "ended").as_str() < since.as_str() { return false; }
            }
            if let Some(until) = &a.until {
                let st = s(r, "started");
                let st = if st.is_empty() { "9".to_string() } else { st };
                if st.as_str() > format!("{until}T99").as_str() { return false; }
            }
            if !a.all {
                let k = s(r, "kind");
                if k != "user" && k != "archived" { return false; }
                if r.get("turns").and_then(Value::as_u64).unwrap_or(0) == 0 { return false; }
            }
            true
        })
        .collect()
}

fn age_days(r: &Value) -> f64 {
    let ended = s(r, "ended");
    match parse_iso_secs(take_chars(&ended, 19)) {
        Some(t) => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
            ((now - t as f64) / 86400.0).max(0.0)
        }
        None => 365.0,
    }
}

type Hit<'a> = (f64, Vec<String>, &'a Value);

fn search<'a>(recs: &[&'a Value], query: &str, a: &Args) -> Vec<Hit<'a>> {
    let q = tokens(query);
    if q.is_empty() {
        return vec![];
    }
    let qset: HashSet<String> = q.into_iter().collect();
    let n = recs.len() as f64;
    let per_doc: Vec<HashMap<(&'static str, String), u64>> = recs
        .par_iter()
        .map(|r| {
            let mut counts: HashMap<(&'static str, String), u64> = HashMap::new();
            for (fname, text) in doc_fields(r) {
                if text.is_empty() { continue; }
                for t in tokens(&text) {
                    if qset.contains(&t) {
                        *counts.entry((fname, t)).or_insert(0) += 1;
                    }
                }
            }
            counts
        })
        .collect();
    let mut df: HashMap<&str, u64> = qset.iter().map(|t| (t.as_str(), 0)).collect();
    for counts in &per_doc {
        let seen: HashSet<&str> = counts.keys().map(|(_, t)| t.as_str()).collect();
        for t in seen {
            *df.get_mut(t).unwrap() += 1;
        }
    }
    let idf: HashMap<&str, f64> = qset.iter().map(|t| (t.as_str(), (1.0 + n / (1.0 + df[t.as_str()] as f64)).ln())).collect();
    let mut results: Vec<Hit> = vec![];
    for (r, counts) in recs.iter().zip(per_doc.iter()) {
        if counts.is_empty() { continue; }
        let mut score = 0.0;
        let mut matched: HashSet<String> = HashSet::new();
        for ((fname, t), c) in counts {
            score += idf[t.as_str()] * field_weight(fname) * (*c as f64).ln_1p();
            matched.insert(t.clone());
        }
        let cov = matched.len() as f64 / qset.len() as f64;
        score *= 0.5 + cov;
        score *= 1.0 + 0.15 * (-age_days(r) / 90.0).exp();
        let mut m: Vec<String> = matched.into_iter().collect();
        m.sort();
        results.push((score, m, r));
    }
    results.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap().then_with(|| s(x.2, "ended").cmp(&s(y.2, "ended"))));
    results.truncate(a.limit);
    results
}

fn grep_sessions<'a>(recs: &[&'a Value], pattern: &str, a: &Args) -> Vec<Hit<'a>> {
    let rx = match regex::bytes::RegexBuilder::new(pattern).case_insensitive(true).build() {
        Ok(r) => r,
        Err(e) => { eprintln!("bad regex: {e}"); std::process::exit(2); }
    };
    let mut results: Vec<Hit> = recs
        .par_iter()
        .filter_map(|r| {
            let p = s(r, "path");
            let f = fs::File::open(&p).ok()?;
            if f.metadata().ok()?.len() == 0 { return None; }
            let mm = unsafe { Mmap::map(&f).ok()? };
            let mut n = 0u64;
            for _ in rx.find_iter(&mm) {
                n += 1;
                if n >= 10000 { break; }
            }
            if n > 0 { Some((n as f64, vec![], *r)) } else { None }
        })
        .collect();
    results.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap().then_with(|| s(x.2, "ended").cmp(&s(y.2, "ended"))));
    results.truncate(a.limit);
    results
}

// ---------------------------------------------------------------- output

fn one_line(t: &str, n: usize) -> String {
    let joined = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= n { joined } else { format!("{}…", take_chars(&joined, n - 1)) }
}

fn pretty(v: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(v, &mut ser).unwrap();
    String::from_utf8(buf).unwrap()
}

fn round3(x: f64) -> f64 {
    format!("{x:.3}").parse().unwrap()
}

fn hit_json(score: f64, matched: &[String], r: &Value) -> Value {
    let mut d = Map::new();
    for k in ["harness", "id", "project", "cwd", "branch", "started", "ended", "title", "turns", "kind", "resume"] {
        d.insert(k.to_string(), r.get(k).cloned().unwrap_or(Value::Null));
    }
    d.insert("score".into(), json!(round3(score)));
    d.insert("matched".into(), json!(matched));
    d.insert("first".into(), json!(one_line(&s(r, "first"), 300)));
    d.insert("summary".into(), json!(one_line(&s(r, "summary"), 300)));
    let ps: Vec<String> = r.get("prompts").and_then(Value::as_array).map(|a| a.iter().take(15).map(|p| one_line(p.as_str().unwrap_or(""), 160)).collect()).unwrap_or_default();
    d.insert("prompts".into(), json!(ps));
    Value::Object(d)
}

fn print_results(results: &[Hit], as_json: bool) {
    if as_json {
        let out: Vec<Value> = results.iter().map(|(score, matched, r)| hit_json(*score, matched, r)).collect();
        println!("{}", pretty(&Value::Array(out)));
        return;
    }
    if results.is_empty() {
        println!("no matches");
        return;
    }
    for (i, (score, matched, r)) in results.iter().enumerate() {
        let title = s(r, "title");
        let first = s(r, "first");
        let head = if title.is_empty() { one_line(&first, 90) } else { title.clone() };
        println!("{:>2}. {:6.2}  {:<6} {}  {:<22} {}", i + 1, score, s(r, "harness"), take_chars(&s(r, "started"), 10), one_line(&s(r, "project"), 22), s(r, "id"));
        println!("      {}", one_line(&head, 110));
        if !title.is_empty() && !first.is_empty() {
            println!("      > {}", one_line(&first, 105));
        }
        if !matched.is_empty() {
            println!("      terms: {}   turns: {}", matched.join(" "), r.get("turns").and_then(Value::as_u64).unwrap_or(0));
        }
    }
    println!();
    println!("resume: {}", s(results[0].2, "resume"));
}

fn find_one<'a>(recs: &'a [Value], prefix: &str) -> &'a Value {
    let hits: Vec<&Value> = recs.iter().filter(|r| { let id = s(r, "id"); id.starts_with(prefix) || id.ends_with(prefix) }).collect();
    if hits.is_empty() {
        eprintln!("no session matching {prefix}");
        std::process::exit(1);
    }
    if hits.len() > 1 {
        eprintln!("ambiguous: {}", hits.iter().take(5).map(|h| s(h, "id")).collect::<Vec<_>>().join(", "));
        std::process::exit(1);
    }
    hits[0]
}

fn cmd_show(recs: &[Value], prefix: &str, as_json: bool) {
    let r = find_one(recs, prefix);
    if as_json {
        println!("{}", pretty(r));
        return;
    }
    println!("{}  {}\nproject: {}  cwd: {}  branch: {}", s(r, "harness"), s(r, "id"), s(r, "project"), s(r, "cwd"), s(r, "branch"));
    println!("started: {}  ended: {}  turns: {}  kind: {}", s(r, "started"), s(r, "ended"), r.get("turns").and_then(Value::as_u64).unwrap_or(0), s(r, "kind"));
    if !s(r, "title").is_empty() {
        println!("title: {}", s(r, "title"));
    }
    if !s(r, "summary").is_empty() {
        println!("summary: {}", one_line(&s(r, "summary"), 600));
    }
    println!("prompts:");
    if let Some(ps) = r.get("prompts").and_then(Value::as_array) {
        for (i, p) in ps.iter().enumerate() {
            println!("  {:>2}. {}", i + 1, one_line(p.as_str().unwrap_or(""), 200));
        }
    }
    let paths: Vec<String> = r.get("paths").and_then(Value::as_array).map(|a| a.iter().take(25).map(|p| p.as_str().unwrap_or("").to_string()).collect()).unwrap_or_default();
    println!("paths: {}", paths.join(", "));
    println!("file: {}\nresume: {}", s(r, "path"), s(r, "resume"));
}


// ---------------------------------------------------------------- cli

const USAGE: &str = r#"recall: describe a past coding-agent session in your own words and get its id and resume command. Reads every Claude Code, Codex, Kimi Code and Cursor transcript on this machine.

usage
  recall "<what you remember>"          rank every session by how well its prompts, title, project and touched files match your words. No model call. Prints the top ten and the resume command of the first.
  recall agent "<what you remember>"    same ranking, then one model call reads the top ten prompt lists and picks. Claude sonnet by default, 3 to 10 seconds.
  recall show <id>                      one session in full: metadata, every user prompt, touched paths, transcript file, resume command. Any unambiguous prefix or suffix of the id works.
  recall resume <id>                    print just the resume command
  recall grep <regex>                   scan the raw transcripts for a regex (case-insensitive) and rank sessions by hit count. Slower, but sees assistant output and tool results too.
  recall index                          refresh the index. Incremental: only transcripts whose mtime or size changed are reparsed. Every other command refreshes first.
  recall stats                          sessions per harness and where the index lives

options
  -r, --resume            run the resume command of the pick (or of the first hit) instead of only printing it
  -a, --agent <name>      who answers in agent mode: claude (default), codex, kimi, cursor, local. Codex, kimi and cursor take 25 to 40 seconds to start in print mode. local is the ollama server on this machine.
  -m, --model <name>      model for that agent, passed through as is. Default sonnet for claude (haiku is correct too but 3x slower here), qwen3.8:27b-mlx for local, the agent's own default otherwise.
  -n, --limit <n>         how many hits to print, or how many candidates the model sees (default 10)
  -f, --from <harness>    only claude, codex, kimi or cursor sessions
  -p, --project <text>    only sessions whose project directory or path contains this, case-insensitive
  -s, --since <date>      only sessions that ended on or after YYYY-MM-DD. Without it, phrases like "yesterday", "last week", "a few weeks ago", "last month" set a wide window on their own.
  -u, --until <date>      only sessions that started on or before YYYY-MM-DD
  -x, --exclude <id>      drop sessions whose id starts with this. Repeatable. An agent should pass its own session id, since the session asking always matches itself.
  -j, --json              machine output. One object per hit with id, harness, project, dates, title, score, matched terms, first prompt, summary and up to 15 prompts. For show, the whole record. For agent, the pick.
  -A, --all               include background sessions and sessions with no user prompt
  -F, --full              index: ignore the cache and reparse every transcript
  -N, --no-refresh        use the index as it is on disk, skip the refresh
  -T, --threads <n>       worker threads for indexing and grep (default: all cores)
  -h, --help              this text

environment
  RECALL_INDEX_DIR   where the index lives (default ~/.agents/recall)
  RECALL_AGENT       default for --agent
  RECALL_MODEL       default for --model

examples
  recall "the one where I ported my claude skills and hooks to codex, then kimi, then cursor"
  recall -r "yesterday's kimi session about the tycho dashboard being slow"
  recall agent "the fedi e2e run that kept timing out"
  recall agent -a codex -m gpt-5 "review of the miniapp api debugger"
  recall -f codex -s 2026-09-01 hooks skills
  recall grep 'AGENTS\.md.*symlink' -f claude -n 5
  recall show a122183c

what gets indexed
  Claude Code   ~/.claude/projects/<slug>/<id>.jsonl, subagent agent-*.jsonl files skipped
  Codex         ~/.codex/sessions/Y/M/D/rollout-*.jsonl, only threads a user started, names from session_index.jsonl
  Kimi Code     ~/.kimi-code/sessions/wd_*/session_*/agents/main/wire.jsonl plus state.json
  Cursor        ~/.cursor/chats/<workspace>/<id>/meta.json and prompt_history.json, tool paths from ~/.cursor/sessions/<id>.jsonl"#;

const COMMANDS: &[&str] = &["agent", "search", "index", "grep", "show", "resume", "stats"];

fn parse_args() -> Args {
    let mut a = Args {
        cmd: String::new(), args: vec![], harness: None, project: None, since: None, until: None, limit: 10, json: false, all: false, full: false, no_refresh: false, threads: None, exclude: vec![],
        agent: std::env::var("RECALL_AGENT").unwrap_or_else(|_| "claude".into()), model: std::env::var("RECALL_MODEL").ok(), resume: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let mut positional: Vec<String> = vec![];
    while i < argv.len() {
        let arg = &argv[i];
        let (flag, inline_val) = match arg.split_once('=') { Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())), _ => (arg.clone(), None) };
        let take = |i: &mut usize| -> String {
            if let Some(v) = &inline_val { return v.clone(); }
            *i += 1;
            argv.get(*i).cloned().unwrap_or_else(|| { eprintln!("{flag} needs a value"); std::process::exit(2) })
        };
        match flag.as_str() {
            "-f" | "--from" | "--harness" => a.harness = Some(take(&mut i)),
            "-p" | "--project" => a.project = Some(take(&mut i)),
            "-s" | "--since" => a.since = Some(take(&mut i)),
            "-u" | "--until" => a.until = Some(take(&mut i)),
            "-n" | "--limit" => a.limit = take(&mut i).parse().unwrap_or(10),
            "-T" | "--threads" => a.threads = take(&mut i).parse().ok(),
            "-x" | "--exclude" => a.exclude.push(take(&mut i)),
            "-a" | "--agent" => a.agent = take(&mut i),
            "-m" | "--model" => a.model = Some(take(&mut i)),
            "-r" | "--resume" => a.resume = true,
            "-j" | "--json" => a.json = true,
            "-A" | "--all" => a.all = true,
            "-F" | "--full" => a.full = true,
            "-N" | "--no-refresh" => a.no_refresh = true,
            "-h" | "--help" => { println!("{USAGE}"); std::process::exit(0) }
            _ => {
                if arg.starts_with('-') && arg.len() > 1 && !arg.contains(' ') { eprintln!("unknown option: {arg}\n"); println!("{USAGE}"); std::process::exit(2); }
                positional.push(arg.clone())
            }
        }
        i += 1;
    }
    if positional.is_empty() { println!("{USAGE}"); std::process::exit(2); }
    // a known first word is a command, unless it is followed by more words than that command takes ("show me the session where ...")
    let first = positional[0].as_str();
    let takes = match first { "show" | "resume" => Some(1), "index" | "stats" => Some(0), "agent" | "search" | "grep" => None, _ => Some(usize::MAX) };
    let is_cmd = COMMANDS.contains(&first) && takes.map_or(true, |n| positional.len() - 1 <= n);
    if is_cmd { a.cmd = positional.remove(0); } else { a.cmd = "search".into(); }
    a.args = positional;
    if (a.cmd == "show" || a.cmd == "resume") && a.args.is_empty() { eprintln!("{} needs a session id\n", a.cmd); println!("{USAGE}"); std::process::exit(2); }
    if (a.cmd == "agent" || a.cmd == "search" || a.cmd == "grep") && a.args.is_empty() { println!("{USAGE}"); std::process::exit(2); }
    a
}

// ---------------------------------------------------------------- agent mode

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn date_ago(days: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let (y, m, d) = civil_from_days(now / 86400 - days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A wide window from the phrasing. Wide on purpose: the ranking and the model see exact dates anyway.
fn date_window(desc: &str) -> Option<String> {
    let l = desc.to_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| l.contains(w));
    if has(&["yesterday", "last night", "this morning", "earlier today"]) { return Some(date_ago(3)); }
    if has(&["few days", "couple of days", "this week", "last week", "days ago"]) { return Some(date_ago(21)); }
    if has(&["few weeks", "couple of weeks", "couple weeks", "weeks ago"]) { return Some(date_ago(75)); }
    if has(&["last month", "a month ago", "month ago"]) { return Some(date_ago(90)); }
    if has(&["months ago", "few months"]) { return Some(date_ago(300)); }
    None
}

fn build_prompt(desc: &str, cands: &[Value]) -> String {
    let mut lines: Vec<String> = vec![];
    for (i, c) in cands.iter().enumerate() {
        lines.push(format!("[{}] {} {}..{} project={} turns={} branch={}", i + 1, s(c, "harness"), take_chars(&s(c, "started"), 10), take_chars(&s(c, "ended"), 10), s(c, "project"), c.get("turns").and_then(Value::as_u64).unwrap_or(0), s(c, "branch")));
        if !s(c, "title").is_empty() { lines.push(format!("    title: {}", s(c, "title"))); }
        if !s(c, "summary").is_empty() { lines.push(format!("    compaction summary: {}", take_chars(&s(c, "summary"), 240))); }
        if let Some(ps) = c.get("prompts").and_then(Value::as_array) {
            for (j, p) in ps.iter().take(12).enumerate() {
                lines.push(format!("    prompt {}: {}", j + 1, take_chars(p.as_str().unwrap_or(""), 160)));
            }
        }
        lines.push(String::new());
    }
    format!(
        "Today is {today}. A user is looking for one past coding-agent session. Their description, dictated from memory:\n\n\"\"\"{desc}\"\"\"\n\n\
Candidates, ranked by a deterministic keyword search over each session's user prompts. The rank is a strong prior: candidate [1] is the best keyword match. \
Prefer a lower rank only when its prompts clearly show the user doing what the description says. Judge by what the user asked for in the prompts, not by the title, \
which is auto-generated from the first prompt and often names only how the session started. The user may misremember dates and which tool they used.\n\n{body}\n\
Pick the single candidate whose prompts best match what the user describes. If none plausibly matches, pick 0.\n\
Reply with one line of JSON only, no prose: {{\"pick\": <number>, \"confidence\": \"high\"|\"medium\"|\"low\", \"why\": \"<one short sentence>\"}}",
        today = date_ago(0), body = lines.join("\n")
    )
}

fn run_capture(cmd: &mut std::process::Command) -> Result<String, String> {
    let out = cmd.stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().map_err(|e| format!("{}: {e}", cmd.get_program().to_string_lossy()))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ask(agent: &str, model: Option<&str>, prompt: &str) -> Result<String, String> {
    use std::process::Command;
    match agent {
        "claude" => run_capture(Command::new("claude").args(["-p", prompt, "--model", model.unwrap_or("sonnet"), "--no-session-persistence", "--strict-mcp-config", "--mcp-config", r#"{"mcpServers":{}}"#, "--setting-sources", "", "--disallowedTools", "*"])),
        "codex" => {
            let out = std::env::temp_dir().join(format!("recall-{}.txt", std::process::id()));
            let mut c = Command::new("codex");
            c.args(["exec", "--ephemeral", "--skip-git-repo-check"]);
            if let Some(m) = model { c.args(["-m", m]); }
            c.arg("-o").arg(&out).arg(prompt).stdout(std::process::Stdio::null());
            run_capture(&mut c)?;
            let text = fs::read_to_string(&out).unwrap_or_default();
            fs::remove_file(&out).ok();
            Ok(text)
        }
        "kimi" => {
            let mut c = Command::new("kimi");
            c.args(["-p", prompt]);
            if let Some(m) = model { c.args(["-m", m]); }
            run_capture(&mut c)
        }
        "cursor" => {
            let mut c = Command::new("agent");
            c.args(["-p", "--trust", "--output-format", "text"]);
            if let Some(m) = model { c.args(["--model", m]); }
            c.arg(prompt);
            run_capture(&mut c)
        }
        "local" => {
            let body = json!({"model": model.unwrap_or("qwen3.8:27b-mlx"), "prompt": prompt, "stream": false, "format": "json", "think": false}).to_string();
            let raw = run_capture(Command::new("curl").args(["-s", "-m", "180", "http://127.0.0.1:11434/api/generate", "-d", &body]))?;
            let v: Value = serde_json::from_str(&raw).map_err(|_| "ollama at 127.0.0.1:11434 gave no answer (is it running?)".to_string())?;
            Ok(s(&v, "response"))
        }
        other => Err(format!("unknown agent {other}: use claude, codex, kimi, cursor or local")),
    }
}

fn parse_pick(raw: &str) -> (usize, String, String) {
    let rx = Regex::new(r#"(?s)\{[^{}]*"pick"[^{}]*\}"#).unwrap();
    if let Some(m) = rx.find(raw) {
        if let Ok(o) = serde_json::from_str::<Value>(m.as_str()) {
            let pick = o.get("pick").and_then(Value::as_u64).unwrap_or(0) as usize;
            return (pick, s(&o, "confidence"), s(&o, "why"));
        }
    }
    (0, "low".into(), "model gave no usable answer".into())
}

fn exec_resume(cmd: &str) -> ! {
    use std::os::unix::process::CommandExt;
    eprintln!("resuming: {cmd}");
    let e = std::process::Command::new("bash").arg("-lc").arg(cmd).exec();
    eprintln!("could not run the resume command: {e}");
    std::process::exit(1)
}

fn cmd_agent(hits: &[Hit], desc: &str, a: &Args, search_secs: f64) {
    if hits.is_empty() {
        println!("no candidate sessions matched the description");
        std::process::exit(1);
    }
    let cands: Vec<Value> = hits.iter().map(|(score, matched, r)| hit_json(*score, matched, r)).collect();
    let prompt = build_prompt(desc, &cands);
    let t = Instant::now();
    let raw = match ask(&a.agent, a.model.as_deref(), &prompt) { Ok(r) => r, Err(e) => { eprintln!("{e}"); std::process::exit(1) } };
    let model_secs = t.elapsed().as_secs_f64();
    let (pick, conf, why) = parse_pick(&raw);
    let model_name = a.model.clone().unwrap_or_else(|| if a.agent == "claude" { "sonnet".into() } else if a.agent == "local" { "qwen3.8:27b-mlx".into() } else { "default".into() });
    if a.json {
        let picked = if (1..=cands.len()).contains(&pick) { cands[pick - 1].clone() } else { Value::Null };
        println!("{}", pretty(&json!({"pick": picked, "confidence": conf, "why": why, "candidates": cands, "search_secs": round3(search_secs), "model_secs": round3(model_secs), "agent": a.agent, "model": model_name})));
        return;
    }
    if !(1..=cands.len()).contains(&pick) {
        println!("no confident match ({why}). top candidates:\n");
        for (i, c) in cands.iter().enumerate() {
            let head = if s(c, "title").is_empty() { s(c, "first") } else { s(c, "title") };
            println!("{:>2}. {:<6} {}  {:<18} {}  {}", i + 1, s(c, "harness"), take_chars(&s(c, "started"), 10), one_line(&s(c, "project"), 18), take_chars(&s(c, "id"), 14), one_line(&head, 70));
        }
        eprintln!("\nsearch {search_secs:.2}s  model {model_secs:.1}s ({} {model_name})", a.agent);
        std::process::exit(1);
    }
    let c = &cands[pick - 1];
    let head = if s(c, "title").is_empty() { one_line(&s(c, "first"), 100) } else { s(c, "title") };
    println!("{}  {}  {}  {}", s(c, "harness"), take_chars(&s(c, "started"), 10), s(c, "project"), s(c, "id"));
    println!("{head}");
    println!("{conf}: {why}");
    println!();
    println!("{}", s(c, "resume"));
    eprintln!("\nsearch {search_secs:.2}s  model {model_secs:.1}s ({} {model_name})", a.agent);
    if a.resume { exec_resume(&s(c, "resume")); }
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

fn main() {
    // default SIGPIPE so `| head` ends the process quietly instead of panicking
    unsafe { signal(13, 0); }
    let mut a = parse_args();
    if let Some(t) = a.threads {
        rayon::ThreadPoolBuilder::new().num_threads(t).build_global().ok();
    }
    if a.cmd == "index" {
        build_index(a.full, true);
        return;
    }
    let t0 = Instant::now();
    let quiet = a.json || a.cmd == "agent" || a.cmd == "resume";
    let recs = if a.no_refresh && index_file().exists() { load_index() } else { build_index(false, !quiet) };
    match a.cmd.as_str() {
        "stats" => {
            let mut by: Map<String, Value> = Map::new();
            for r in &recs {
                let h = s(r, "harness");
                let n = by.get(&h).and_then(Value::as_u64).unwrap_or(0);
                by.insert(h, json!(n + 1));
            }
            println!("{}", pretty(&json!({"sessions": recs.len(), "by_harness": by, "index": index_file().to_string_lossy()})));
        }
        "show" => cmd_show(&recs, &a.args[0], a.json),
        "resume" => {
            let cmd = s(find_one(&recs, &a.args[0]), "resume");
            if a.resume { exec_resume(&cmd); }
            println!("{cmd}");
        }
        _ => {
            let query = a.args.join(" ");
            if a.cmd != "grep" && a.since.is_none() {
                a.since = date_window(&query);
            }
            let subset = filter_recs(&recs, &a);
            let results = if a.cmd == "grep" { grep_sessions(&subset, &query, &a) } else { search(&subset, &query, &a) };
            let search_secs = t0.elapsed().as_secs_f64();
            if a.cmd == "agent" {
                cmd_agent(&results, &query, &a, search_secs);
                return;
            }
            print_results(&results, a.json);
            if a.resume {
                if let Some(first) = results.first() { exec_resume(&s(first.2, "resume")); }
            }
        }
    }
}
