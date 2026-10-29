//! The process table: every process with its parent, command line, resident memory, CPU
//! since the previous scan, state and start time, plus the working directory of the
//! processes the collector asks about. Only the `macos` scanner talks to the OS; the
//! table and tree helpers are plain data, so everything above them tests on any host.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use chrono::{DateTime, Utc};

/// One process in a table.
#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// The executable's basename, as `ps` shows it.
    pub name: String,
    /// The executable's full path, when known.
    pub exe: String,
    /// The command line; `args[0]` is the program as it was started.
    pub args: Vec<String>,
    /// Percent of one core since the previous scan; 0 on the first.
    pub cpu: f64,
    /// Resident memory in bytes.
    pub rss: u64,
    pub stopped: bool,
    pub started: Option<DateTime<Utc>>,
    /// The working directory, when it was asked for and could be read.
    pub cwd: String,
}

impl Proc {
    /// A compact command description: the program's basename and its arguments, trimmed.
    pub fn short(&self, max: usize) -> String {
        let mut s = self.name.clone();
        if self.args.len() > 1 {
            s.push(' ');
            s.push_str(&self.args[1..].join(" "));
        } else if s.is_empty() {
            s = self.args.join(" ");
        }
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let chars: Vec<char> = s.chars().collect();
        if max > 0 && chars.len() > max {
            let mut out: String = chars[..max - 1].iter().collect();
            out.push('…');
            out
        } else {
            s
        }
    }
}

/// One scan of the process table, with the tree linked.
#[derive(Debug, Default)]
pub struct Table {
    pub procs: BTreeMap<i32, Proc>,
    children: HashMap<i32, Vec<i32>>,
}

impl Table {
    pub fn new(procs: Vec<Proc>) -> Table {
        let mut t = Table { procs: procs.into_iter().map(|p| (p.pid, p)).collect(), children: HashMap::new() };
        t.link();
        t
    }

    /// Builds the parent → children index; children are in pid order so output is stable.
    fn link(&mut self) {
        self.children.clear();
        for p in self.procs.values() {
            if p.ppid != p.pid && self.procs.contains_key(&p.ppid) {
                self.children.entry(p.ppid).or_default().push(p.pid);
            }
        }
    }

    pub fn get(&self, pid: i32) -> Option<&Proc> {
        self.procs.get(&pid)
    }

    pub fn children(&self, pid: i32) -> &[i32] {
        self.children.get(&pid).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The parents of a process, nearest first, up to the root; cycles end the walk.
    pub fn ancestors(&self, pid: i32) -> Vec<&Proc> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = self.procs.get(&pid).map(|p| p.ppid);
        while let Some(ppid) = cur {
            if !seen.insert(ppid) {
                break;
            }
            match self.procs.get(&ppid) {
                Some(p) if p.pid != ppid || p.ppid != ppid => {
                    out.push(p);
                    cur = Some(p.ppid);
                }
                _ => break,
            }
        }
        out
    }

    /// Visits `root` and its descendants depth-first, parents before children, not
    /// descending below any process for which `stop` is true (nor visiting it).
    pub fn walk_until(&self, root: i32, stop: &dyn Fn(&Proc) -> bool, visit: &mut dyn FnMut(&Proc, usize)) {
        fn go(
            t: &Table,
            pid: i32,
            depth: usize,
            stop: &dyn Fn(&Proc) -> bool,
            visit: &mut dyn FnMut(&Proc, usize),
            seen: &mut HashSet<i32>,
        ) {
            let Some(p) = t.procs.get(&pid) else { return };
            if !seen.insert(pid) {
                return;
            }
            visit(p, depth);
            for &c in t.children(pid) {
                if let Some(cp) = t.procs.get(&c)
                    && !stop(cp)
                {
                    go(t, c, depth + 1, stop, visit, seen);
                }
            }
        }
        go(self, root, 0, stop, visit, &mut HashSet::new());
    }

    /// Resource use over `root` and its descendants, with every pid root first.
    pub fn aggregate(&self, root: i32, stop: &dyn Fn(&Proc) -> bool) -> Subtree {
        let mut t = Subtree::default();
        self.walk_until(root, stop, &mut |p, _| {
            t.cpu += p.cpu;
            t.rss += p.rss;
            t.pids.push(p.pid);
        });
        t
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Subtree {
    pub cpu: f64,
    pub rss: u64,
    pub pids: Vec<i32>,
}

/// The outermost macOS application bundle an executable lives in ("Claude.app" for
/// "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/…"), or "".
pub fn bundle(exe: &str) -> &str {
    let Some(i) = exe.find(".app/") else { return "" };
    let head = &exe[..i + 4];
    head.rsplit('/').next().unwrap_or("")
}

/// The executable's basename, unwrapping common interpreters so "node /x/bin/gemini"
/// yields "gemini".
pub fn exe_base(args: &[String], name: &str) -> String {
    let Some(first) = args.first() else { return name.to_lowercase() };
    let base = basename(first).to_lowercase();
    if matches!(base.as_str(), "node" | "bun" | "deno" | "python" | "python3" | "ruby" | "perl") {
        for a in &args[1..] {
            if a.starts_with('-') {
                continue;
            }
            return basename(a).to_lowercase();
        }
    }
    base
}

fn basename(p: &str) -> &str {
    Path::new(p).file_name().and_then(|s| s.to_str()).unwrap_or(p)
}

/// Takes repeated scans; CPU percentages come from the delta between consecutive ones.
#[derive(Default)]
pub struct Scanner {
    prev: HashMap<i32, (f64, Instant)>,
    /// Working directories already read, keyed by pid, valid while the start time matches.
    cwd_cache: HashMap<i32, (Option<DateTime<Utc>>, String)>,
    /// Command lines already read, likewise.
    args_cache: HashMap<i32, Cached>,
}

/// A process's command line, valid while its start time matches.
#[derive(Default)]
struct Cached {
    started: Option<DateTime<Utc>>,
    exe: String,
    args: Vec<String>,
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner::default()
    }

    /// Reads the whole table. `want_cwd` says which processes are worth the extra call
    /// that learns their working directory.
    #[cfg(target_os = "macos")]
    pub fn scan(&mut self, want_cwd: &dyn Fn(&Proc) -> bool) -> Table {
        let now = Instant::now();
        let mut next = HashMap::new();
        let mut procs = Vec::new();
        // `ps` sees every process; libproc answers only for this user's, so it adds what
        // it can (a better name, memory, CPU) on top of the ps row.
        for row in macos::ps_table() {
            let pid = row.pid;
            let mut p = Proc {
                pid,
                ppid: row.ppid,
                name: row.name,
                exe: row.exe,
                stopped: row.stopped,
                started: row.started,
                ..Default::default()
            };
            if let Some(b) = macos::basic(pid) {
                if !b.name.is_empty() {
                    p.name = b.name;
                }
                if b.started.is_some() {
                    p.started = b.started;
                }
                p.stopped = b.stopped;
            }
            if let Some((secs, rss)) = macos::task(pid) {
                p.rss = rss;
                next.insert(pid, (secs, now));
                if let Some(&(prev, t)) = self.prev.get(&pid) {
                    let dt = now.duration_since(t).as_secs_f64();
                    if dt > 0.0 {
                        p.cpu = ((secs - prev) / dt * 100.0).max(0.0);
                    }
                }
            }
            match self.args_cache.get(&pid) {
                Some(c) if c.started == p.started => {
                    if !c.exe.is_empty() {
                        p.exe = c.exe.clone();
                    }
                    p.args = c.args.clone();
                }
                _ => {
                    if let Some(exe) = macos::path(pid) {
                        p.exe = exe;
                    }
                    p.args = macos::args(pid).unwrap_or_default();
                    if p.args.is_empty() {
                        p.args = vec![if p.exe.is_empty() { p.name.clone() } else { p.exe.clone() }];
                    }
                    self.args_cache
                        .insert(pid, Cached { started: p.started, exe: p.exe.clone(), args: p.args.clone() });
                }
            }
            if p.exe.is_empty() {
                p.exe = p.args[0].clone();
            }
            procs.push(p);
        }
        self.prev = next;
        let live: HashSet<i32> = procs.iter().map(|p| p.pid).collect();
        self.args_cache.retain(|pid, _| live.contains(pid));
        let mut table = Table::new(procs);
        let wanted: Vec<i32> = table.procs.values().filter(|p| want_cwd(p)).map(|p| p.pid).collect();
        for pid in wanted {
            let started = table.procs[&pid].started;
            let cwd = match self.cwd_cache.get(&pid) {
                Some((s, cwd)) if *s == started => cwd.clone(),
                _ => {
                    let cwd = macos::cwd(pid).unwrap_or_default();
                    self.cwd_cache.insert(pid, (started, cwd.clone()));
                    cwd
                }
            };
            if let Some(p) = table.procs.get_mut(&pid) {
                p.cwd = cwd;
            }
        }
        self.cwd_cache.retain(|pid, (s, _)| table.procs.get(pid).is_some_and(|p| p.started == *s));
        table
    }

    #[cfg(not(target_os = "macos"))]
    pub fn scan(&mut self, _want_cwd: &dyn Fn(&Proc) -> bool) -> Table {
        Table::new(Vec::new())
    }
}

/// Host-wide load: the one-minute load average, cores, CPU busy since the previous
/// reading, and memory in use.
#[derive(Clone, Debug, Default)]
pub struct Host {
    pub load1: f64,
    pub num_cpu: usize,
    pub cpu_percent: f64,
    pub mem_used: u64,
    pub mem_total: u64,
}

/// Reads host load; `prev_ticks` is the CPU tick reading from the previous call.
#[derive(Default)]
pub struct HostReader {
    prev_ticks: Option<[u64; 4]>,
}

impl HostReader {
    #[cfg(target_os = "macos")]
    pub fn read(&mut self) -> Host {
        let mut h = Host { num_cpu: macos::num_cpu(), load1: macos::load1(), ..Default::default() };
        if let Some(ticks) = macos::cpu_ticks() {
            if let Some(prev) = self.prev_ticks {
                let d: Vec<f64> = (0..4).map(|i| ticks[i].saturating_sub(prev[i]) as f64).collect();
                let total: f64 = d.iter().sum();
                if total > 0.0 {
                    h.cpu_percent = (total - d[2]) / total * 100.0;
                }
            }
            self.prev_ticks = Some(ticks);
        }
        let (used, total) = macos::memory();
        h.mem_used = used;
        h.mem_total = total;
        h
    }

    #[cfg(not(target_os = "macos"))]
    pub fn read(&mut self) -> Host {
        Host::default()
    }
}

/// libproc, sysctl and Mach host statistics, each wrapped so nothing above sees a pointer.
#[cfg(target_os = "macos")]
mod macos {
    use std::mem::{MaybeUninit, size_of};

    use chrono::{DateTime, Utc};
    use libc::{c_int, c_void};

    use super::Proc;

    /// One row of `ps`: what every process shows, whoever owns it.
    pub struct PsRow {
        pub pid: i32,
        pub ppid: i32,
        pub name: String,
        pub exe: String,
        pub stopped: bool,
        pub started: Option<DateTime<Utc>>,
    }

    /// The whole table in one `ps` call: pid, parent, state, start time and executable.
    pub fn ps_table() -> Vec<PsRow> {
        let Ok(out) = std::process::Command::new("ps").args(["-axww", "-o", "pid=,ppid=,stat=,lstart=,comm="]).output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().filter_map(parse_ps_line).collect()
    }

    /// "  123   1 Ss   Wed Oct 29 10:22:41 2026 /usr/bin/login"
    pub fn parse_ps_line(line: &str) -> Option<PsRow> {
        let mut it = line.split_whitespace();
        let pid: i32 = it.next()?.parse().ok()?;
        let ppid: i32 = it.next()?.parse().ok()?;
        let stat = it.next()?;
        let lstart: Vec<&str> = (0..5).filter_map(|_| it.next()).collect();
        let exe = it.collect::<Vec<_>>().join(" ");
        if lstart.len() != 5 {
            return None;
        }
        // "Wed Oct 29 10:22:41 2026", local time
        let started = chrono::NaiveDateTime::parse_from_str(&lstart[1..].join(" "), "%b %d %H:%M:%S %Y")
            .ok()
            .and_then(|t| t.and_local_timezone(chrono::Local).single())
            .map(|t| t.with_timezone(&Utc));
        let name = std::path::Path::new(&exe)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| exe.clone());
        Some(PsRow { pid, ppid, name, exe, stopped: stat.starts_with('T'), started })
    }

    /// Mach time units per nanosecond, as libproc reports CPU time in them.
    #[allow(deprecated)] // mach_timebase_info: the mach2 crate is not worth a dependency for one call
    fn timebase() -> f64 {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: the struct is what the call fills.
        if unsafe { libc::mach_timebase_info(&mut tb) } != 0 || tb.denom == 0 {
            return 1.0;
        }
        tb.numer as f64 / tb.denom as f64
    }

    /// Parent, names, state and start time. None when the process is gone or hidden.
    pub fn basic(pid: i32) -> Option<Proc> {
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: the buffer is exactly one proc_bsdinfo, which is what the flavor fills.
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr() as *mut c_void, size) };
        if n != size {
            return None;
        }
        // SAFETY: the call filled the whole struct.
        let info = unsafe { info.assume_init() };
        let name = cstr(&info.pbi_name);
        let name = if name.is_empty() { cstr(&info.pbi_comm) } else { name };
        let started = (info.pbi_start_tvsec > 0)
            .then(|| {
                DateTime::from_timestamp(
                    info.pbi_start_tvsec as i64,
                    (info.pbi_start_tvusec as u32).saturating_mul(1000),
                )
            })
            .flatten();
        Some(Proc {
            pid,
            ppid: info.pbi_ppid as i32,
            name,
            stopped: info.pbi_status == libc::SSTOP,
            started,
            ..Default::default()
        })
    }

    /// CPU seconds (user + system) and resident bytes; None for another user's process.
    pub fn task(pid: i32) -> Option<(f64, u64)> {
        let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
        let size = size_of::<libc::proc_taskinfo>() as c_int;
        // SAFETY: the buffer is exactly one proc_taskinfo, which is what the flavor fills.
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, info.as_mut_ptr() as *mut c_void, size) };
        if n != size {
            return None;
        }
        // SAFETY: the call filled the whole struct.
        let info = unsafe { info.assume_init() };
        Some(((info.pti_total_user + info.pti_total_system) as f64 * timebase() / 1e9, info.pti_resident_size))
    }

    pub fn path(pid: i32) -> Option<String> {
        let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes, as the call requires.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }

    /// The command line from `sysctl KERN_PROCARGS2`: argc, the executable path, then
    /// the arguments, all NUL-separated.
    pub fn args(pid: i32) -> Option<Vec<String>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a null buffer asks for the size; mib is a valid 3-element name.
        if unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) } != 0
            || size < 4
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: the buffer is `size` bytes and sysctl writes at most that many.
        if unsafe {
            libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr() as *mut c_void, &mut size, std::ptr::null_mut(), 0)
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        let argc = u32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
        let mut i = 4;
        // the executable path
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        while i < buf.len() && buf[i] == 0 {
            i += 1;
        }
        let mut out = Vec::with_capacity(argc);
        while out.len() < argc && i < buf.len() {
            let start = i;
            while i < buf.len() && buf[i] != 0 {
                i += 1;
            }
            out.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
            i += 1;
        }
        Some(out)
    }

    pub fn cwd(pid: i32) -> Option<String> {
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::uninit();
        let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
        // SAFETY: the buffer is exactly one proc_vnodepathinfo, which is what the flavor fills.
        let n =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr() as *mut c_void, size) };
        if n != size {
            return None;
        }
        // SAFETY: the call filled the whole struct; vip_path is a NUL-terminated path.
        let info = unsafe { info.assume_init() };
        let bytes: Vec<u8> =
            info.pvi_cdir.vip_path.iter().flatten().map(|&c| c as u8).take_while(|&c| c != 0).collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn cstr(chars: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = chars.iter().map(|&c| c as u8).take_while(|&c| c != 0).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn num_cpu() -> usize {
        // SAFETY: sysconf takes a constant and returns a number.
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n > 0 { n as usize } else { 1 }
    }

    pub fn load1() -> f64 {
        let mut avg = [0f64; 3];
        // SAFETY: the array holds the three values getloadavg writes.
        if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } > 0 { avg[0] } else { 0.0 }
    }

    /// Ticks user, system, idle, nice since boot.
    #[allow(deprecated)] // mach_host_self: the mach2 crate is not worth a dependency for one call
    pub fn cpu_ticks() -> Option<[u64; 4]> {
        let mut info = MaybeUninit::<libc::host_cpu_load_info>::uninit();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        // SAFETY: the buffer is one host_cpu_load_info and count says so.
        let rc = unsafe {
            libc::host_statistics(
                libc::mach_host_self(),
                libc::HOST_CPU_LOAD_INFO,
                info.as_mut_ptr() as libc::host_info_t,
                &mut count,
            )
        };
        if rc != 0 {
            return None;
        }
        // SAFETY: the call filled the struct.
        let info = unsafe { info.assume_init() };
        Some([info.cpu_ticks[0] as u64, info.cpu_ticks[1] as u64, info.cpu_ticks[2] as u64, info.cpu_ticks[3] as u64])
    }

    /// (used, total) bytes; used is what is neither free nor inactive.
    #[allow(deprecated)]
    pub fn memory() -> (u64, u64) {
        let mut total: u64 = 0;
        let mut len = size_of::<u64>();
        let name = c"hw.memsize";
        // SAFETY: the buffer is one u64 and len says so.
        unsafe {
            libc::sysctlbyname(name.as_ptr(), &mut total as *mut u64 as *mut c_void, &mut len, std::ptr::null_mut(), 0)
        };
        let mut vm = MaybeUninit::<libc::vm_statistics64>::uninit();
        let mut count = (size_of::<libc::vm_statistics64>() / size_of::<libc::integer_t>()) as u32;
        // SAFETY: the buffer is one vm_statistics64 and count is its size in integers.
        let rc = unsafe {
            libc::host_statistics64(
                libc::mach_host_self(),
                libc::HOST_VM_INFO64,
                vm.as_mut_ptr() as libc::host_info64_t,
                &mut count,
            )
        };
        if rc != 0 {
            return (0, total);
        }
        // SAFETY: the call filled the struct.
        let vm = unsafe { vm.assume_init() };
        // SAFETY: sysconf takes a constant and returns a number.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
        let available = (vm.free_count as u64 + vm.inactive_count as u64) * page;
        (total.saturating_sub(available), total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, name: &str, cpu: f64, rss: u64) -> Proc {
        Proc {
            pid,
            ppid,
            name: name.into(),
            exe: format!("/bin/{name}"),
            args: vec![name.into()],
            cpu,
            rss,
            ..Default::default()
        }
    }

    #[test]
    fn bundles_and_interpreters_name_the_program() {
        assert_eq!(
            bundle("/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"),
            "Claude.app"
        );
        assert_eq!(bundle("/usr/bin/zsh"), "");
        assert_eq!(exe_base(&["node".into(), "/x/bin/gemini".into()], ""), "gemini");
        assert_eq!(
            exe_base(
                &["/opt/homebrew/bin/node".into(), "--max-old-space-size=4096".into(), "/x/bin/claude".into()],
                ""
            ),
            "claude"
        );
        assert_eq!(exe_base(&["/usr/local/bin/Codex".into()], ""), "codex");
        assert_eq!(exe_base(&[], "Zsh"), "zsh");
    }

    #[test]
    fn a_tree_aggregates_below_its_root_until_told_to_stop() {
        let t = Table::new(vec![
            p(1, 0, "launchd", 0.0, 0),
            p(10, 1, "claude", 5.0, 100),
            p(11, 10, "zsh", 1.0, 10),
            p(12, 11, "cargo", 90.0, 1000),
            p(13, 10, "codex", 7.0, 50),
            p(14, 13, "node", 2.0, 20),
        ]);
        let all = t.aggregate(10, &|_| false);
        assert_eq!(all, Subtree { cpu: 105.0, rss: 1180, pids: vec![10, 11, 12, 13, 14] });
        let own = t.aggregate(10, &|q| q.name == "codex");
        assert_eq!(own, Subtree { cpu: 96.0, rss: 1110, pids: vec![10, 11, 12] }, "a nested agent is its own tree");
        assert_eq!(t.ancestors(12).iter().map(|q| q.pid).collect::<Vec<_>>(), vec![11, 10, 1]);
        assert_eq!(t.children(10), &[11, 13]);
    }

    #[test]
    fn a_short_command_keeps_the_program_and_trims_the_rest() {
        let long = Proc {
            name: "cargo".into(),
            args: vec!["cargo".into(), "build".into(), "--release".into()],
            ..Default::default()
        };
        assert_eq!(long.short(120), "cargo build --release");
        assert_eq!(long.short(10), "cargo bui…");
        let bare = Proc { args: vec!["/x/y".into()], ..Default::default() };
        assert_eq!(bare.short(120), "/x/y");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_ps_row_parses_into_parent_state_start_and_program() {
        let r = macos::parse_ps_line("  5408  2338 Ss   Wed Oct 29 10:22:41 2026 /usr/bin/login").unwrap();
        assert_eq!(
            (r.pid, r.ppid, r.stopped, r.name.as_str(), r.exe.as_str()),
            (5408, 2338, false, "login", "/usr/bin/login")
        );
        assert!(r.started.is_some());
        let r = macos::parse_ps_line("81448 2254 T    Wed Oct 29 10:22:41 2026 /Applications/Orca.app/Contents/Frameworks/Orca Helper (Renderer).app/Contents/MacOS/Orca Helper (Renderer)").unwrap();
        assert!(r.stopped);
        assert_eq!(r.name, "Orca Helper (Renderer)");
        assert!(macos::parse_ps_line("garbage").is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_scanner_sees_this_process_and_a_stopped_child() {
        use std::process::Command;
        let mut child = Command::new("sleep").arg("30").spawn().expect("spawn sleep");
        let pid = child.id() as i32;
        // SAFETY: a plain signal to the child this test owns.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut s = Scanner::new();
        let me = std::process::id() as i32;
        let t = s.scan(&|p| p.pid == me);
        let mine = t.get(me).expect("this process is in the table");
        assert!(mine.rss > 0, "resident memory is read");
        assert_eq!(mine.cwd, std::env::current_dir().unwrap().display().to_string(), "cwd is read on request");
        assert!(mine.args.iter().any(|a| a.contains("fusebox")), "{:?}", mine.args);
        let c = t.get(pid).expect("the child is in the table");
        assert_eq!(c.ppid, me);
        assert!(c.stopped, "SIGSTOP shows as stopped");
        assert_eq!(c.name, "sleep");
        assert!(t.children(me).contains(&pid));
        assert!(t.get(1).is_some_and(|l| l.name == "launchd"), "root's processes are in the table too");
        // A second scan a moment later gives CPU percentages.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t2 = s.scan(&|_| false);
        assert!(t2.procs.values().any(|p| p.cpu > 0.0), "something on the machine used CPU");
        // SAFETY: ending the child this test owns.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();
        let h = HostReader::default().read();
        assert!(h.num_cpu > 0 && h.mem_total > 0 && h.mem_used > 0 && h.mem_used < h.mem_total, "{h:?}");
    }
}
