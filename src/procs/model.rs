//! Merges the process table with what each agent writes to disk into the list of agents
//! the page shows, the heaviest other programs, host load and alerts.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::scan::{self, Host, HostReader, Proc, Scanner, Table};
use super::sources::{Claude, Codex, Kiro};

/// Executable basename → kind, for CLI agents and local model runtimes.
const AGENTS: &[(&str, &str)] = &[
    ("claude", "claude"),
    ("codex", "codex"),
    ("hcodex", "hcodex"),
    ("opencode", "opencode"),
    ("gemini", "gemini"),
    ("cursor-agent", "cursor"),
    ("aider", "aider"),
    ("copilot", "copilot"),
    ("droid", "droid"),
    ("goose", "goose"),
    ("amp", "amp"),
    ("kiro-cli", "kiro"),
    ("kiro-cli-chat", "kiro"),
    ("q", "kiro"),
    ("qchat", "kiro"),
    ("qwen", "qwen"),
    ("crush", "crush"),
    ("ollama", "ollama"),
    ("llama-server", "llama.cpp"),
    ("llama-cli", "llama.cpp"),
    ("lms", "lmstudio"),
    ("llmster", "lmstudio"),
    ("mlx_lm.server", "mlx"),
    ("vllm", "vllm"),
];

/// macOS bundle → kind, for desktop apps that run models or agents; every helper folds in.
const APPS: &[(&str, &str)] = &[
    ("Claude.app", "claude-app"),
    ("ChatGPT.app", "chatgpt"),
    ("Kiro.app", "kiro-ide"),
    ("Kiro CLI.app", "kiro-desktop"),
    ("KiroCrew.app", "kirocrew"),
    ("Orca.app", "orca"),
    ("Antigravity.app", "antigravity"),
    ("LM Studio.app", "lmstudio"),
    ("Ollama.app", "ollama"),
    ("Cursor.app", "cursor"),
    ("Windsurf.app", "windsurf"),
    ("Zed.app", "zed"),
    ("Trae.app", "trae"),
    ("Void.app", "void"),
];

/// Alert thresholds; the page only shows these, nothing acts on them.
const TREE_CPU_PERCENT: f64 = 150.0;
const TREE_CPU_SECONDS: i64 = 30;
const TREE_RSS: u64 = 2 << 30;
const TOOL_MINUTES: i64 = 5;
const MAX_SUBAGENTS: usize = 4;
const SYSTEM_MEM_PERCENT: f64 = 90.0;
const SYSTEM_LOAD_PER_CORE: f64 = 1.5;
/// A transcript or conversation written this recently means the agent is busy.
const RECENT: i64 = 30;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Subagent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub since: Option<DateTime<Utc>>,
    pub tool: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcRow {
    pub pid: i32,
    pub name: String,
    pub cmd: String,
    pub cpu: f64,
    pub rss: u64,
    pub depth: usize,
    pub stopped: bool,
}

/// One top-level AI agent process with everything known about it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Agent {
    /// Stable identity, `kind:pid`.
    pub key: String,
    pub kind: String,
    pub pid: i32,
    pub name: String,
    pub cwd: String,
    pub session_id: String,
    /// busy | idle | waiting | paused | running | unknown
    pub status: String,
    /// Where the status came from: registry | rollout | kiro | process
    pub source: String,
    pub tool: String,
    pub tool_since: Option<DateTime<Utc>>,
    pub subagents: Vec<Subagent>,
    pub cpu: f64,
    pub tree_cpu: f64,
    pub rss: u64,
    pub tree_rss: u64,
    pub started: Option<DateTime<Utc>>,
    /// Descendants, depth-first, excluding nested agents.
    pub procs: Vec<ProcRow>,
    /// Self and descendants, root first, excluding nested agents.
    pub pids: Vec<i32>,
    pub paused: bool,
    pub alerts: Vec<String>,
    /// The key of the enclosing agent when nested.
    pub parent: String,
    /// The macOS bundle for desktop apps.
    pub bundle: String,
    pub last_event: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct System {
    pub load1: f64,
    pub num_cpu: usize,
    pub cpu_percent: f64,
    pub mem_used: u64,
    pub mem_total: u64,
    pub alerts: Vec<String>,
}

/// A group of non-agent processes: one app bundle or one executable name.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Other {
    pub name: String,
    pub count: usize,
    pub cpu: f64,
    pub rss: u64,
    pub pids: Vec<i32>,
    pub exe: String,
    /// Part of the operating system, or this server: shown without actions.
    pub system: bool,
    #[serde(rename = "self")]
    pub is_self: bool,
}

/// One tick's worth of data, as the page reads it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct View {
    pub available: bool,
    pub self_pid: u32,
    pub at: DateTime<Utc>,
    pub system: System,
    pub agents: Vec<Agent>,
    pub others: Vec<Other>,
    pub errors: Vec<String>,
    pub hook_seen: bool,
}

/// Owns the scanners and the per-agent history between ticks.
pub struct Collector {
    scanner: Scanner,
    host: HostReader,
    claude: Claude,
    codex: HashMap<String, Codex>,
    kiro: Kiro,
    high_since: HashMap<String, DateTime<Utc>>,
    self_pid: i32,
    scanned: bool,
}

impl Collector {
    pub fn new() -> Collector {
        Collector {
            scanner: Scanner::new(),
            host: HostReader::default(),
            claude: Claude::new(),
            codex: HashMap::new(),
            kiro: Kiro::new(),
            high_since: HashMap::new(),
            self_pid: std::process::id() as i32,
            scanned: false,
        }
    }

    /// Whether a scan has happened, so the next one's CPU percentages mean something.
    pub fn scanned(&self) -> bool {
        self.scanned
    }

    pub fn tick(&mut self, now: DateTime<Utc>, others: usize) -> View {
        self.scanned = true;
        let table = self.scanner.scan(&|p| kind_of_raw(p).is_some());
        let mut v = View { available: true, self_pid: self.self_pid as u32, at: now, ..Default::default() };
        let self_pid = self.self_pid;
        let is_agent = |p: &Proc| kind_of(&table, p, self_pid).is_some();

        let mut agents: BTreeMap<i32, Agent> = BTreeMap::new();
        for p in table.procs.values() {
            let Some(kind) = kind_of(&table, p, self_pid) else { continue };
            let tree = table.aggregate(p.pid, &is_agent);
            let mut a = Agent {
                key: format!("{kind}:{}", p.pid),
                kind: kind.to_owned(),
                pid: p.pid,
                cwd: p.cwd.clone(),
                started: p.started,
                cpu: p.cpu,
                rss: p.rss,
                status: "unknown".into(),
                source: "process".into(),
                tree_cpu: tree.cpu,
                tree_rss: tree.rss,
                pids: tree.pids,
                paused: p.stopped,
                procs: descendants(&table, p.pid, &is_agent),
                ..Default::default()
            };
            for q in table.ancestors(p.pid) {
                if let Some(k) = kind_of(&table, q, self_pid) {
                    a.parent = format!("{k}:{}", q.pid);
                    break;
                }
            }
            self.enrich(&mut a, p, now);
            let b = scan::bundle(p.args.first().map(String::as_str).unwrap_or(&p.exe));
            if !b.is_empty() {
                a.name = b.trim_end_matches(".app").to_owned();
                a.bundle = b.to_owned();
                if a.status == "unknown" {
                    a.status = "running".into();
                }
            }
            if a.paused {
                a.status = "paused".into();
            }
            if a.name.is_empty() && !a.cwd.is_empty() {
                a.name = std::path::Path::new(&a.cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
            agents.insert(p.pid, a);
        }
        absorb_strays(&table, &mut agents);

        let mut in_agent = HashSet::new();
        for a in agents.values_mut() {
            a.alerts = self.agent_alerts(a, now);
            in_agent.extend(a.pids.iter().copied());
        }
        v.agents = agents.into_values().collect();
        v.others = self.others(&table, &in_agent, others);
        v.system = self.system();
        v
    }

    /// Session state from the registry, rollouts and Kiro's files.
    fn enrich(&mut self, a: &mut Agent, p: &Proc, now: DateTime<Utc>) {
        match a.kind.as_str() {
            "claude" => {
                let registry = self.claude.registry();
                if let Some(r) = registry.get(&p.pid) {
                    a.session_id = r.session_id.clone();
                    a.name = r.name.clone();
                    a.source = "registry".into();
                    if !r.cwd.is_empty() {
                        a.cwd = r.cwd.clone();
                    }
                    if !r.status.is_empty() {
                        let (status, tool) = registry_status(&r.status);
                        a.status = status.into();
                        a.tool = tool.into();
                    }
                    if r.updated_at > 0 {
                        a.last_event = DateTime::from_timestamp_millis(r.updated_at);
                    }
                }
                if !a.session_id.is_empty() && a.status == "busy" {
                    for (id, at) in self.claude.active_subagents(&a.cwd, &a.session_id, now, Duration::seconds(RECENT))
                    {
                        a.subagents.push(Subagent {
                            id,
                            kind: "subagent".into(),
                            since: Some(at),
                            tool: String::new(),
                        });
                    }
                }
            }
            "codex" | "hcodex" => {
                let kind = a.kind.clone();
                let src = self.codex.entry(kind.clone()).or_insert_with(|| Codex::for_kind(&kind));
                if let Some(st) = src.matched(&a.cwd, p.started, now) {
                    a.session_id = st.session_id;
                    a.source = "rollout".into();
                    a.last_event = st.last_event;
                    if st.working {
                        a.status = "busy".into();
                        a.tool = st.tool;
                        a.tool_since = st.tool_since;
                    } else {
                        a.status = "idle".into();
                    }
                }
            }
            "kiro" if self.kiro.available() && !a.cwd.is_empty() => {
                a.source = "kiro".into();
                a.status = "idle".into();
                if let Some(t) = self.kiro.conversation_updated(&a.cwd, now) {
                    a.last_event = Some(t);
                    if now - t < Duration::seconds(RECENT) {
                        a.status = "busy".into();
                    }
                }
                for s in self.kiro.subagents(&a.cwd, now, Duration::seconds(RECENT)) {
                    a.status = "busy".into();
                    if a.last_event.is_none_or(|l| s.last > l) {
                        a.last_event = Some(s.last);
                    }
                    a.subagents.push(Subagent { id: s.id, kind: s.title, since: s.since, tool: s.tool });
                }
            }
            _ => {}
        }
    }

    /// Everything outside the agent trees, grouped by bundle or executable, top `n`.
    fn others(&self, table: &Table, in_agent: &HashSet<i32>, n: usize) -> Vec<Other> {
        if n == 0 {
            return Vec::new();
        }
        let mut groups: HashMap<String, Other> = HashMap::new();
        for p in table.procs.values() {
            if in_agent.contains(&p.pid) {
                continue;
            }
            let mut key = scan::bundle(&p.exe).to_owned();
            if key.is_empty() {
                key = scan::exe_base(&p.args, &p.name);
            }
            if key.is_empty() {
                key = "?".into();
            }
            let g = groups.entry(key.clone()).or_insert_with(|| Other {
                name: key,
                exe: p.exe.clone(),
                ..Default::default()
            });
            g.count += 1;
            g.cpu += p.cpu;
            g.rss += p.rss;
            g.pids.push(p.pid);
            if p.pid == self.self_pid {
                g.is_self = true;
            }
        }
        let mut out: Vec<Other> = groups.into_values().collect();
        for o in &mut out {
            o.system = o.is_self || system_path(&o.exe);
        }
        out.sort_by(|a, b| b.cpu.partial_cmp(&a.cpu).unwrap_or(std::cmp::Ordering::Equal).then(b.rss.cmp(&a.rss)));
        out.truncate(n);
        out
    }

    fn agent_alerts(&mut self, a: &Agent, now: DateTime<Utc>) -> Vec<String> {
        let mut out = Vec::new();
        if a.tree_cpu > TREE_CPU_PERCENT {
            let since = *self.high_since.entry(a.key.clone()).or_insert(now);
            if now - since >= Duration::seconds(TREE_CPU_SECONDS) {
                out.push(format!("CPU {:.0}% for {}", a.tree_cpu, fmt_dur(now - since)));
            }
        } else {
            self.high_since.remove(&a.key);
        }
        if a.tree_rss > TREE_RSS {
            out.push(format!("RAM {}", fmt_bytes(a.tree_rss)));
        }
        if let Some(since) = a.tool_since
            && !a.tool.is_empty()
            && now - since > Duration::minutes(TOOL_MINUTES)
        {
            out.push(format!("tool running {}", fmt_dur(now - since)));
        }
        if a.subagents.len() > MAX_SUBAGENTS {
            out.push(format!("{} subagents", a.subagents.len()));
        }
        out
    }

    fn system(&mut self) -> System {
        let Host { load1, num_cpu, cpu_percent, mem_used, mem_total } = self.host.read();
        let mut s = System { load1, num_cpu, cpu_percent, mem_used, mem_total, alerts: Vec::new() };
        if mem_total > 0 {
            let pct = mem_used as f64 / mem_total as f64 * 100.0;
            if pct > SYSTEM_MEM_PERCENT {
                s.alerts.push(format!("memory {pct:.0}%"));
            }
        }
        if num_cpu > 0 && load1 / num_cpu as f64 > SYSTEM_LOAD_PER_CORE {
            s.alerts.push(format!("load {load1:.1} on {num_cpu} cores"));
        }
        s
    }
}

/// The agent kind of a process, or None. A chain of same-kind processes ("node
/// …/bin/codex" → codex, "kiro-cli" → "kiro-cli-chat" → bun → "kiro-cli-chat acp")
/// is one agent: only its topmost process counts, the rest are its children, as is
/// any bundle process under an agent (a browser it opened, a helper it ships).
fn kind_of(table: &Table, p: &Proc, self_pid: i32) -> Option<&'static str> {
    if p.pid == self_pid {
        return None;
    }
    let kind = kind_of_raw(p)?;
    let is_app = !scan::bundle(p.args.first().map(String::as_str).unwrap_or(&p.exe)).is_empty();
    for q in table.ancestors(p.pid) {
        if q.pid == self_pid {
            continue;
        }
        match kind_of_raw(q) {
            Some(qk) if qk == kind || is_app => return None,
            _ => {}
        }
    }
    Some(kind)
}

/// The kind a process would have on its own, before the chain-folding rule.
fn kind_of_raw(p: &Proc) -> Option<&'static str> {
    // The program as it was started: "kiro-cli" through a symlink is the CLI agent even
    // though the binary lives inside "Kiro CLI.app".
    let b = scan::bundle(p.args.first().map(String::as_str).unwrap_or(&p.exe));
    if !b.is_empty() {
        return APPS.iter().find(|(name, _)| *name == b).map(|(_, kind)| *kind);
    }
    let base = scan::exe_base(&p.args, &p.name);
    AGENTS.iter().find(|(name, _)| *name == base).map(|(_, kind)| *kind)
}

/// The registry's status words (busy, shell, compacting, waiting, permission, idle) as
/// the page's, with the raw word as the tool hint when it says what the agent does.
fn registry_status(s: &str) -> (&'static str, &str) {
    match s {
        "idle" => ("idle", ""),
        "waiting" | "permission" => ("waiting", ""),
        "shell" => ("busy", "shell"),
        "compacting" => ("busy", "compacting"),
        "busy" => ("busy", ""),
        other => ("busy", other),
    }
}

fn descendants(table: &Table, root: i32, stop: &dyn Fn(&Proc) -> bool) -> Vec<ProcRow> {
    let mut rows = Vec::new();
    table.walk_until(root, stop, &mut |p, depth| {
        if depth > 0 {
            rows.push(ProcRow {
                pid: p.pid,
                name: p.name.clone(),
                cmd: p.short(120),
                cpu: p.cpu,
                rss: p.rss,
                depth: depth - 1,
                stopped: p.stopped,
            });
        }
    });
    rows
}

/// Folds bundle processes re-parented to launchd (crash handlers, helpers whose parent
/// restarted) into the app row of the same bundle, so one desktop app is one row; the
/// lowest pid, the main executable started first, survives.
fn absorb_strays(table: &Table, agents: &mut BTreeMap<i32, Agent>) {
    let mut main_by_bundle: HashMap<String, i32> = HashMap::new();
    for a in agents.values() {
        if a.bundle.is_empty() {
            continue;
        }
        let e = main_by_bundle.entry(a.bundle.clone()).or_insert(a.pid);
        if a.pid < *e {
            *e = a.pid;
        }
    }
    let strays: Vec<i32> =
        agents.values().filter(|a| !a.bundle.is_empty() && main_by_bundle[&a.bundle] != a.pid).map(|a| a.pid).collect();
    for pid in strays {
        let Some(a) = agents.remove(&pid) else { continue };
        let Some(m) = agents.get_mut(&main_by_bundle[&a.bundle]) else { continue };
        m.tree_cpu += a.tree_cpu;
        m.tree_rss += a.tree_rss;
        m.pids.extend(a.pids);
        if let Some(p) = table.get(pid) {
            m.procs.push(ProcRow {
                pid: p.pid,
                name: p.name.clone(),
                cmd: p.short(120),
                cpu: p.cpu,
                rss: p.rss,
                depth: 0,
                stopped: p.stopped,
            });
            m.procs.extend(a.procs);
        }
    }
}

/// Programs that are the operating system rather than something the user started.
pub fn system_path(exe: &str) -> bool {
    ["/System/", "/usr/libexec/", "/usr/sbin/", "/sbin/", "/Library/Apple/"].iter().any(|p| exe.starts_with(p))
}

fn fmt_dur(d: Duration) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1}G", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{}M", b >> 20)
    } else {
        format!("{}K", b >> 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, exe: &str, args: &[&str], cpu: f64, rss: u64) -> Proc {
        let name = std::path::Path::new(exe).file_name().unwrap().to_string_lossy().into_owned();
        Proc {
            pid,
            ppid,
            name,
            exe: exe.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cpu,
            rss,
            ..Default::default()
        }
    }

    /// A desktop app's process: launchd starts it by its full path.
    fn app(pid: i32, ppid: i32, exe: &str, cpu: f64, rss: u64) -> Proc {
        p(pid, ppid, exe, &[exe], cpu, rss)
    }

    fn collector() -> Collector {
        let mut c = Collector::new();
        c.self_pid = 999_999;
        c
    }

    #[test]
    fn agents_are_found_by_name_or_bundle_and_chains_fold_into_one() {
        let c = collector();
        let t = Table::new(vec![
            p(1, 0, "/sbin/launchd", &["launchd"], 0.0, 0),
            p(10, 1, "/opt/homebrew/bin/node", &["node", "/x/bin/codex"], 1.0, 10),
            p(11, 10, "/x/codex-aarch64", &["codex"], 50.0, 500),
            p(12, 11, "/bin/zsh", &["zsh", "-c", "cargo test"], 90.0, 100),
            app(20, 1, "/Applications/Claude.app/Contents/MacOS/Claude", 2.0, 200),
            app(
                21,
                20,
                "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper",
                3.0,
                300,
            ),
            p(30, 1, "/usr/libexec/trustd", &["trustd"], 0.1, 5),
            p(40, 1, "/Applications/Kiro CLI.app/Contents/MacOS/kiro-cli", &["kiro-cli", "chat"], 0.5, 40),
        ]);
        let k = |pid: i32| kind_of(&t, t.get(pid).unwrap(), c.self_pid);
        assert_eq!(k(10), Some("codex"), "the node wrapper is the agent");
        assert_eq!(k(11), None, "the binary under it is a child");
        assert_eq!(k(20), Some("claude-app"));
        assert_eq!(k(21), None, "helpers fold into the app");
        assert_eq!(k(30), None);
        assert_eq!(
            k(40),
            Some("kiro"),
            "started by name through a symlink, it is the CLI agent, not the app it ships in"
        );
    }

    #[test]
    fn registry_words_become_status_and_tool() {
        assert_eq!(registry_status("shell"), ("busy", "shell"));
        assert_eq!(registry_status("permission"), ("waiting", ""));
        assert_eq!(registry_status("idle"), ("idle", ""));
        assert_eq!(registry_status("thinking"), ("busy", "thinking"));
    }

    #[test]
    fn strays_of_a_bundle_join_its_main_row() {
        let t = Table::new(vec![
            p(20, 1, "/Applications/Orca.app/Contents/MacOS/Orca", &["Orca"], 2.0, 200),
            p(25, 1, "/Applications/Orca.app/Contents/Frameworks/x/crashpad", &["crashpad"], 1.0, 50),
        ]);
        let mut agents = BTreeMap::new();
        agents.insert(
            20,
            Agent {
                key: "orca:20".into(),
                pid: 20,
                bundle: "Orca.app".into(),
                tree_cpu: 2.0,
                tree_rss: 200,
                pids: vec![20],
                ..Default::default()
            },
        );
        agents.insert(
            25,
            Agent {
                key: "orca:25".into(),
                pid: 25,
                bundle: "Orca.app".into(),
                tree_cpu: 1.0,
                tree_rss: 50,
                pids: vec![25],
                ..Default::default()
            },
        );
        absorb_strays(&t, &mut agents);
        assert_eq!(agents.len(), 1);
        let m = &agents[&20];
        assert_eq!((m.tree_cpu, m.tree_rss), (3.0, 250));
        assert_eq!(m.pids, vec![20, 25]);
        assert_eq!(m.procs.len(), 1);
        assert_eq!(m.procs[0].name, "crashpad");
    }

    #[test]
    fn a_tick_lists_agents_others_and_marks_the_system_and_this_server() {
        // The live table on this machine: whatever runs, the collector must not panic,
        // its own process is marked, and OS daemons carry no actions.
        let mut c = Collector::new();
        let v = c.tick(Utc::now(), 200);
        assert!(v.available);
        // Run from a terminal this process is one of the others; run by an agent (a test
        // under Claude Code) it sits inside that agent's tree. Either way it is never an
        // agent row, and as one of the others it carries no actions.
        let me = v.self_pid as i32;
        let among_others = v.others.iter().find(|o| o.is_self);
        let in_a_tree = v.agents.iter().any(|a| a.pids.contains(&me));
        assert!(among_others.is_some() || in_a_tree, "this process is somewhere in the view");
        if let Some(o) = among_others {
            assert!(o.system && o.pids.contains(&me));
        }
        assert!(v.agents.iter().all(|a| a.pid != me));
        for o in &v.others {
            if o.exe.starts_with("/System/") {
                assert!(o.system, "{}", o.exe);
            }
        }
        assert!(fmt_bytes(3 * (1 << 30) / 2).starts_with("1.5G"));
        assert_eq!(fmt_dur(Duration::seconds(3700)), "1h01m");
    }

    #[test]
    fn a_hot_tree_alerts_only_after_it_stays_hot() {
        let mut c = collector();
        let now = Utc::now();
        let a = Agent { key: "claude:1".into(), tree_cpu: 200.0, ..Default::default() };
        assert!(c.agent_alerts(&a, now).is_empty(), "not on the first hot tick");
        let later = now + Duration::seconds(TREE_CPU_SECONDS + 1);
        assert_eq!(c.agent_alerts(&a, later), vec![format!("CPU 200% for {}", fmt_dur(later - now))]);
        let cool = Agent { key: "claude:1".into(), tree_cpu: 10.0, tree_rss: 3 << 30, ..Default::default() };
        assert_eq!(c.agent_alerts(&cool, later), vec!["RAM 3.0G".to_string()]);
        assert!(c.high_since.is_empty(), "cooling down forgets the start");
    }
}
