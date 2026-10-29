//! What the agents already write to disk, read without hooks: Claude Code's session
//! registry and subagent transcripts, Codex's rollout files, and Kiro CLI's session files
//! and conversation store.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::Value;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn modified(path: &Path) -> Option<DateTime<Utc>> {
    fs::metadata(path).ok()?.modified().ok().map(DateTime::<Utc>::from)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&Utc))
}

/// The last `max` bytes of a file as whole lines (a partial first line is dropped).
fn tail_lines(path: &Path, max: u64) -> Vec<String> {
    let Ok(mut f) = fs::File::open(path) else { return Vec::new() };
    let Ok(meta) = f.metadata() else { return Vec::new() };
    let off = meta.len().saturating_sub(max);
    if off > 0 && f.seek(SeekFrom::Start(off)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if off > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

/// The first string among `keys` in a tool's input, as one trimmed line.
fn summarize(input: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = match input.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "),
            _ => continue,
        };
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        if s.is_empty() {
            continue;
        }
        let chars: Vec<char> = s.chars().collect();
        return if chars.len() > 60 { format!("{}…", chars[..59].iter().collect::<String>()) } else { s };
    }
    String::new()
}

// ---------------------------------------------------------------- Claude Code

/// One entry of Claude Code's per-process registry, `~/.claude/sessions/<pid>.json`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ClaudeSession {
    #[serde(default)]
    pub pid: i32,
    #[serde(default, rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: i64,
}

pub struct Claude {
    pub home: PathBuf,
}

impl Claude {
    pub fn new() -> Claude {
        let home = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"));
        Claude { home }
    }

    pub fn registry(&self) -> HashMap<i32, ClaudeSession> {
        let mut out = HashMap::new();
        let Ok(entries) = fs::read_dir(self.home.join("sessions")) else { return out };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            if let Ok(s) = serde_json::from_str::<ClaudeSession>(&text)
                && s.pid != 0
            {
                out.insert(s.pid, s);
            }
        }
        out
    }

    /// The transcript directory for a working directory, named the way Claude Code
    /// names project folders: every character outside [A-Za-z0-9] becomes '-'.
    pub fn project_dir(&self, cwd: &str) -> PathBuf {
        self.home.join("projects").join(encode_project(cwd))
    }

    /// Subagent transcripts of a session written to within `within` of now: the
    /// hook-free sign of a live subagent, without its type.
    pub fn active_subagents(
        &self,
        cwd: &str,
        session_id: &str,
        now: DateTime<Utc>,
        within: Duration,
    ) -> Vec<(String, DateTime<Utc>)> {
        let root = self.project_dir(cwd).join(session_id).join("subagents");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(root) else { return out };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl")) else { continue };
            if let Some(m) = e.metadata().ok().and_then(|m| m.modified().ok()).map(DateTime::<Utc>::from)
                && now - m <= within
            {
                out.push((id.to_owned(), m));
            }
        }
        out.sort_by_key(|(_, m)| *m);
        out
    }
}

pub fn encode_project(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

// ---------------------------------------------------------------- Codex

/// What a rollout's tail says about one Codex process.
#[derive(Clone, Debug, Default)]
pub struct CodexState {
    pub session_id: String,
    pub working: bool,
    pub tool: String,
    pub tool_since: Option<DateTime<Utc>>,
    pub last_event: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
struct Rollout {
    path: PathBuf,
    id: String,
    cwd: String,
    started: Option<DateTime<Utc>>,
    modified: DateTime<Utc>,
}

/// Rooted at a Codex home (`~/.codex`, `~/.hcodex`); maps a process to its rollout
/// file by directory and time and reads the tail for working / idle and the pending call.
pub struct Codex {
    pub home: PathBuf,
    meta: HashMap<PathBuf, Rollout>,
    tails: HashMap<PathBuf, (u64, Option<SystemTime>, CodexState)>,
    candidates: Option<(DateTime<Utc>, Vec<Rollout>)>,
}

impl Codex {
    pub fn for_kind(kind: &str) -> Codex {
        let home = if kind == "codex"
            && let Some(h) = std::env::var_os("CODEX_HOME")
        {
            PathBuf::from(h)
        } else {
            home().join(format!(".{kind}"))
        };
        Codex { home, meta: HashMap::new(), tails: HashMap::new(), candidates: None }
    }

    /// The rollout of a process started at `started` in `cwd`: one created within two
    /// minutes of the start when there is one (a fresh session), else the most recently
    /// written one there (a resumed session keeps appending to its old file).
    pub fn matched(&mut self, cwd: &str, started: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<CodexState> {
        if cwd.is_empty() {
            return None;
        }
        let cands = self.candidates(now);
        let mut fresh: Option<&Rollout> = None;
        let mut resumed: Option<&Rollout> = None;
        for r in &cands {
            if r.cwd != cwd {
                continue;
            }
            if let Some(s) = started
                && r.modified < s - Duration::seconds(5)
            {
                continue;
            }
            if let (Some(s), Some(rs)) = (started, r.started)
                && (rs - s).abs() <= Duration::minutes(2)
                && fresh.is_none_or(|f| f.started < r.started)
            {
                fresh = Some(r);
            }
            if resumed.is_none_or(|m| m.modified < r.modified) {
                resumed = Some(r);
            }
        }
        let best = fresh.or(resumed)?.clone();
        let mut st = self.tail(&best);
        st.session_id = best.id;
        Some(st)
    }

    fn tail(&mut self, r: &Rollout) -> CodexState {
        let meta = fs::metadata(&r.path).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.and_then(|m| m.modified().ok());
        if let Some((s, m, st)) = self.tails.get(&r.path)
            && *s == size
            && *m == modified
        {
            return st.clone();
        }
        let st = parse_rollout_tail(&tail_lines(&r.path, 4 << 20));
        self.tails.insert(r.path.clone(), (size, modified, st.clone()));
        st
    }

    /// Rollouts written in the last three days, newest first; computed once per tick.
    fn candidates(&mut self, now: DateTime<Utc>) -> Vec<Rollout> {
        if let Some((at, list)) = &self.candidates
            && *at == now
        {
            return list.clone();
        }
        let list = self.walk(now);
        self.candidates = Some((now, list.clone()));
        list
    }

    fn walk(&mut self, now: DateTime<Utc>) -> Vec<Rollout> {
        let root = self.home.join("sessions");
        let mut out = Vec::new();
        let mut seen = Vec::new();
        // sessions/YYYY/MM/DD/rollout-*.jsonl; day directories older than a month are skipped.
        for year in read_dirs(&root) {
            for month in read_dirs(&year) {
                for day in read_dirs(&month) {
                    let rel = format!("{}-{}-{}", name_of(&year), name_of(&month), name_of(&day));
                    if let Ok(d) = chrono::NaiveDate::parse_from_str(&rel, "%Y-%m-%d")
                        && now.date_naive() - d > Duration::days(30)
                    {
                        continue;
                    }
                    let Ok(entries) = fs::read_dir(&day) else { continue };
                    for e in entries.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                            continue;
                        }
                        let path = e.path();
                        let Some(m) = modified(&path) else { continue };
                        if now - m > Duration::days(3) {
                            continue;
                        }
                        seen.push(path.clone());
                        let r = match self.meta.get(&path) {
                            Some(r) => r.clone(),
                            None => {
                                let Some(r) = read_rollout_meta(&path) else { continue };
                                self.meta.insert(path.clone(), r.clone());
                                r
                            }
                        };
                        out.push(Rollout { modified: m, ..r });
                    }
                }
            }
        }
        self.meta.retain(|p, _| seen.contains(p));
        self.tails.retain(|p, _| seen.contains(p));
        out.sort_by_key(|r| std::cmp::Reverse(r.started));
        out
    }
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_rollout_meta(path: &Path) -> Option<Rollout> {
    let f = fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(f), &mut first).ok()?;
    let rec: Value = serde_json::from_str(&first).ok()?;
    if rec["type"] != "session_meta" {
        return None;
    }
    let payload = &rec["payload"];
    let ts =
        payload["timestamp"].as_str().filter(|s| !s.is_empty()).or_else(|| rec["timestamp"].as_str()).unwrap_or("");
    Some(Rollout {
        path: path.to_path_buf(),
        id: payload["id"].as_str().unwrap_or("").to_owned(),
        cwd: payload["cwd"].as_str().unwrap_or("").to_owned(),
        started: parse_time(ts),
        modified: Utc::now(),
    })
}

/// Reduces the tail of a rollout to working / idle and the newest pending call.
pub fn parse_rollout_tail(lines: &[String]) -> CodexState {
    let mut st = CodexState::default();
    let mut pending: Vec<(String, String, Option<DateTime<Utc>>)> = Vec::new(); // call id, description, when
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let at = rec["timestamp"].as_str().and_then(parse_time);
        if at.is_some() {
            st.last_event = at;
        }
        let payload = &rec["payload"];
        let ptype = payload["type"].as_str().unwrap_or("");
        match rec["type"].as_str().unwrap_or("") {
            "event_msg" => match ptype {
                "task_started" => st.working = true,
                "task_complete" | "turn_aborted" | "error" => {
                    st.working = false;
                    pending.clear();
                }
                _ => {}
            },
            "response_item" => match ptype {
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    let id =
                        payload["call_id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("#{}", pending.len()));
                    let name = payload["name"].as_str().unwrap_or("");
                    pending.push((id, describe_call(name, payload["arguments"].as_str().unwrap_or("")), at));
                }
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                    if let Some(id) = payload["call_id"].as_str().filter(|s| !s.is_empty()) {
                        pending.retain(|(k, _, _)| k != id);
                    } else if !pending.is_empty() {
                        pending.remove(0);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if st.working
        && let Some((_, name, at)) = pending.last()
    {
        st.tool = name.clone();
        st.tool_since = *at;
    }
    st
}

fn describe_call(name: &str, args: &str) -> String {
    let Ok(input) = serde_json::from_str::<Value>(args) else { return name.to_owned() };
    let s = summarize(&input, &["cmd", "command", "path", "file_path", "pattern", "query"]);
    if s.is_empty() { name.to_owned() } else { format!("{name}: {s}") }
}

// ---------------------------------------------------------------- Kiro CLI

/// A live Kiro subagent session.
#[derive(Clone, Debug)]
pub struct KiroSubagent {
    pub id: String,
    pub title: String,
    pub tool: String,
    pub since: Option<DateTime<Utc>>,
    pub last: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct KiroMeta {
    id: String,
    cwd: String,
    title: String,
    reason: String,
    created: Option<DateTime<Utc>>,
    modified: Option<SystemTime>,
}

/// Directory → when its main conversation was last written.
type Conversations = HashMap<String, DateTime<Utc>>;

/// Kiro CLI's session files under `~/.kiro/sessions/cli` and its conversation store.
pub struct Kiro {
    pub home: PathBuf,
    pub db: PathBuf,
    meta: HashMap<PathBuf, KiroMeta>,
    conversations: Option<(DateTime<Utc>, Conversations)>,
}

impl Kiro {
    pub fn new() -> Kiro {
        let h = home();
        Kiro {
            home: h.join(".kiro"),
            db: h.join("Library/Application Support/kiro-cli/data.sqlite3"),
            meta: HashMap::new(),
            conversations: None,
        }
    }

    pub fn available(&self) -> bool {
        self.home.join("sessions/cli").is_dir()
    }

    /// Subagent sessions in `cwd` whose transcript was written within `within` of now.
    pub fn subagents(&mut self, cwd: &str, now: DateTime<Utc>, within: Duration) -> Vec<KiroSubagent> {
        let dir = self.home.join("sessions/cli");
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else { return out };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".jsonl") else { continue };
            let Some(last) = e.metadata().ok().and_then(|m| m.modified().ok()).map(DateTime::<Utc>::from) else {
                continue;
            };
            if now - last > within {
                continue;
            }
            let Some(m) = self.load_meta(&dir.join(format!("{id}.json"))) else { continue };
            if m.cwd != cwd || m.reason != "subagent" {
                continue;
            }
            out.push(KiroSubagent {
                id: m.id,
                title: m.title,
                tool: kiro_pending_tool(&tail_lines(&e.path(), 256 << 10)),
                since: m.created,
                last,
            });
        }
        self.meta.retain(|p, _| p.exists());
        out.sort_by_key(|s| s.since);
        out
    }

    fn load_meta(&mut self, path: &Path) -> Option<KiroMeta> {
        let modified = fs::metadata(path).ok()?.modified().ok();
        if let Some(m) = self.meta.get(path)
            && m.modified == modified
        {
            return Some(m.clone());
        }
        let rec: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        let id = rec["session_id"].as_str().filter(|s| !s.is_empty())?.to_owned();
        let title = rec["title"]
            .as_str()
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches(['#', ' '])
            .trim()
            .to_owned();
        let chars: Vec<char> = title.chars().collect();
        let title = if chars.len() > 40 { format!("{}…", chars[..39].iter().collect::<String>()) } else { title };
        let m = KiroMeta {
            id,
            cwd: rec["cwd"].as_str().unwrap_or("").to_owned(),
            title,
            reason: rec["session_created_reason"].as_str().unwrap_or("").to_owned(),
            created: rec["created_at"].as_str().and_then(parse_time),
            modified,
        };
        self.meta.insert(path.to_path_buf(), m.clone());
        Some(m)
    }

    /// When the main conversation in `cwd` was last written, from Kiro's SQLite store,
    /// read once per tick for every directory.
    pub fn conversation_updated(&mut self, cwd: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if self.conversations.as_ref().is_none_or(|(at, _)| *at != now) {
            self.conversations = Some((now, self.query_conversations()));
        }
        self.conversations.as_ref()?.1.get(cwd).copied()
    }

    fn query_conversations(&self) -> Conversations {
        let mut out = HashMap::new();
        if !self.db.is_file() {
            return out;
        }
        let Ok(conn) = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return out;
        };
        let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
        let Ok(mut stmt) = conn.prepare("select key, max(updated_at) from conversations_v2 group by key") else {
            return out;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))) else { return out };
        for (key, ms) in rows.flatten() {
            if let Some(t) = DateTime::from_timestamp_millis(ms) {
                out.insert(key, t);
            }
        }
        out
    }
}

/// "name: summary" for the last toolUse in a Kiro transcript with no ToolResults yet.
pub fn kiro_pending_tool(lines: &[String]) -> String {
    let mut pending: Vec<(String, String)> = Vec::new();
    for line in lines {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let content = rec["data"]["content"].as_array();
        match rec["kind"].as_str().unwrap_or("") {
            "Prompt" => pending.clear(),
            "AssistantMessage" => {
                for c in content.into_iter().flatten() {
                    if c["kind"] != "toolUse" {
                        continue;
                    }
                    let d = &c["data"];
                    let Some(id) = d["toolUseId"].as_str().filter(|s| !s.is_empty()) else { continue };
                    let name = d["name"].as_str().unwrap_or("").to_owned();
                    let s = summarize(
                        &d["input"],
                        &["command", "path", "file_path", "pattern", "query", "task_list_description", "prompt"],
                    );
                    pending.push((id.to_owned(), if s.is_empty() { name } else { format!("{name}: {s}") }));
                }
            }
            "ToolResults" => {
                for c in content.into_iter().flatten() {
                    if c["kind"] == "toolResult"
                        && let Some(id) = c["data"]["toolUseId"].as_str()
                    {
                        pending.retain(|(k, _)| k != id);
                    }
                }
            }
            _ => {}
        }
    }
    pending.last().map(|(_, d)| d.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folders_are_named_like_claude_names_them() {
        assert_eq!(encode_project("/Users/me/github/Fusebox"), "-Users-me-github-Fusebox");
        assert_eq!(encode_project("/tmp/a.b c"), "-tmp-a-b-c");
    }

    #[test]
    fn a_rollout_tail_says_working_and_names_the_pending_call() {
        let lines: Vec<String> = [
            r#"{"timestamp":"2026-10-29T03:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-10-29T03:00:01Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"cargo   test -p core\"}"}}"#,
            r#"{"timestamp":"2026-10-29T03:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1"}}"#,
            r#"{"timestamp":"2026-10-29T03:00:03Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c2","arguments":"{\"cmd\":[\"go\",\"build\",\"./...\"]}"}}"#,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let st = parse_rollout_tail(&lines);
        assert!(st.working);
        assert_eq!(st.tool, "exec_command: go build ./...");
        assert_eq!(st.tool_since, parse_time("2026-10-29T03:00:03Z"));
        assert_eq!(st.last_event, parse_time("2026-10-29T03:00:03Z"));
        let mut done = lines.clone();
        done.push(
            r#"{"timestamp":"2026-10-29T03:00:09Z","type":"event_msg","payload":{"type":"task_complete"}}"#.into(),
        );
        let st = parse_rollout_tail(&done);
        assert!(!st.working);
        assert_eq!(st.tool, "", "an idle session has no pending call");
    }

    #[test]
    fn a_kiro_transcript_names_the_tool_without_a_result() {
        let lines: Vec<String> = [
            r#"{"kind":"Prompt","data":{}}"#,
            r#"{"kind":"AssistantMessage","data":{"content":[{"kind":"toolUse","data":{"toolUseId":"t1","name":"execute_bash","input":{"command":"ls -la"}}}]}}"#,
            r#"{"kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"t1"}}]}}"#,
            r#"{"kind":"AssistantMessage","data":{"content":[{"kind":"toolUse","data":{"toolUseId":"t2","name":"fs_read","input":{"path":"/x/y.rs"}}}]}}"#,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(kiro_pending_tool(&lines), "fs_read: /x/y.rs");
        assert_eq!(kiro_pending_tool(&lines[..3]), "");
    }

    #[test]
    fn codex_matches_a_fresh_rollout_over_a_resumed_one() {
        let dir = std::env::temp_dir().join(format!("fusebox-codex-{}", uuid::Uuid::new_v4()));
        let day = dir.join("sessions/2026/10/29");
        fs::create_dir_all(&day).unwrap();
        let now = Utc::now();
        let started = now - Duration::minutes(10);
        let write = |name: &str, id: &str, at: DateTime<Utc>| {
            let meta = format!(
                r#"{{"timestamp":"{}","type":"session_meta","payload":{{"id":"{id}","cwd":"/work","timestamp":"{}"}}}}"#,
                at.to_rfc3339(),
                at.to_rfc3339()
            );
            let turn = r#"{"timestamp":"2026-10-29T03:00:00Z","type":"event_msg","payload":{"type":"task_started"}}"#;
            fs::write(day.join(name), format!("{meta}\n{turn}\n")).unwrap();
        };
        write("rollout-old.jsonl", "old", started - Duration::hours(5));
        write("rollout-new.jsonl", "new", started + Duration::seconds(20));
        let mut c = Codex { home: dir.clone(), meta: HashMap::new(), tails: HashMap::new(), candidates: None };
        let st = c.matched("/work", Some(started), now).expect("a rollout matches");
        assert_eq!(st.session_id, "new");
        assert!(st.working);
        assert!(c.matched("/elsewhere", Some(started), now).is_none());
        fs::remove_file(day.join("rollout-new.jsonl")).unwrap();
        let st = c.matched("/work", Some(started), now + Duration::seconds(1)).expect("the resumed rollout remains");
        assert_eq!(st.session_id, "old");
        fs::remove_dir_all(&dir).unwrap();
    }
}
