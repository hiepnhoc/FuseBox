//! The Processes page: every AI agent and the heaviest other programs on this machine,
//! read from the process table and what the agents write to disk, and the signals the
//! dashboard may send them so a build can take the machine back.
//!
//! The collector is a port of github.com/hiepln/agents-monitor, so nothing has to be
//! installed beside Fusebox.

mod model;
mod scan;
mod sources;

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub use model::Collector;

/// One scan serves every poll that lands within this long.
const CACHE_FOR: Duration = Duration::from_millis(1500);
/// The first scan of a run is followed by a second this much later, so CPU percentages
/// mean something on the first page load.
const PRIME_AFTER: Duration = Duration::from_millis(600);
/// The non-agent groups listed when the page does not say how many.
pub const DEFAULT_OTHERS: usize = 12;

struct Shared {
    collector: Option<Collector>,
    cache: Option<(Instant, usize, Value)>,
}

static SHARED: Mutex<Shared> = Mutex::const_new(Shared { collector: None, cache: None });

/// The page's data. Never an error: a failed scan comes back as `available: false`
/// with the reason.
pub async fn snapshot(others: usize) -> Value {
    let mut shared = SHARED.lock().await;
    if let Some((at, n, v)) = shared.cache.as_ref()
        && *n == others
        && at.elapsed() < CACHE_FOR
    {
        return v.clone();
    }
    if !cfg!(target_os = "macos") {
        return json!({ "available": false, "error": "The process table is only read on macOS." });
    }
    let mut collector = shared.collector.take().unwrap_or_else(Collector::new);
    let (collector, value) = tokio::task::spawn_blocking(move || {
        if !collector.scanned() {
            collector.tick(Utc::now(), 0);
            std::thread::sleep(PRIME_AFTER);
        }
        let view = collector.tick(Utc::now(), others);
        (
            collector,
            serde_json::to_value(view).unwrap_or_else(|e| json!({ "available": false, "error": e.to_string() })),
        )
    })
    .await
    .map(|(c, v)| (Some(c), v))
    .unwrap_or_else(|e| (None, json!({ "available": false, "error": format!("the process scan failed: {e}") })));
    shared.collector = collector;
    shared.cache = Some((Instant::now(), others, value.clone()));
    value
}

/// What the page may do to a process tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Pause,
    Resume,
    Term,
    Kill,
}

#[cfg(unix)]
impl Action {
    fn signal(self) -> libc::c_int {
        match self {
            Action::Pause => libc::SIGSTOP,
            Action::Resume => libc::SIGCONT,
            Action::Term => libc::SIGTERM,
            Action::Kill => libc::SIGKILL,
        }
    }
}

/// The order a signal visits a tree given root-first: children first when stopping or
/// ending it, so a parent cannot respawn them in between; parents first when resuming.
pub fn order(action: Action, pids: &[i32]) -> Vec<i32> {
    let mut out: Vec<i32> = pids.iter().copied().filter(|&p| p > 1).collect();
    if action != Action::Resume {
        out.reverse();
    }
    out
}

/// Sends the action's signal to every pid. Processes that are already gone are fine;
/// this server itself is never a target.
#[cfg(unix)]
pub fn signal(action: Action, pids: &[i32]) -> Result<usize> {
    let me = std::process::id() as i32;
    if pids.contains(&me) {
        bail!("that would stop Fusebox itself");
    }
    let mut sent = 0;
    let mut errors = Vec::new();
    for pid in order(action, pids) {
        // SAFETY: kill(2) with a pid above 1 and a standard signal number has no memory effects.
        let rc = unsafe { libc::kill(pid, action.signal()) };
        if rc == 0 {
            sent += 1;
            continue;
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            errors.push(format!("pid {pid}: {e}"));
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(sent)
}

/// Windows has no POSIX signals, and the page only reads the process table on macOS,
/// so there is nothing to act on here: refuse with the same self-protection as above.
#[cfg(not(unix))]
pub fn signal(_action: Action, pids: &[i32]) -> Result<usize> {
    let me = std::process::id() as i32;
    if pids.contains(&me) {
        bail!("that would stop Fusebox itself");
    }
    bail!("process signals are only supported on macOS and Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_walk_children_first_except_when_resuming() {
        assert_eq!(order(Action::Kill, &[1, 10, 20, 30]), vec![30, 20, 10], "pid 1 is never a target");
        assert_eq!(order(Action::Term, &[10, 20]), vec![20, 10]);
        assert_eq!(order(Action::Pause, &[10, 20]), vec![20, 10]);
        assert_eq!(order(Action::Resume, &[10, 20]), vec![10, 20]);
    }

    #[test]
    fn the_server_never_signals_itself() {
        let me = std::process::id() as i32;
        let e = signal(Action::Term, &[me]).unwrap_err();
        assert!(e.to_string().contains("Fusebox itself"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_gone_process_is_not_an_error() {
        // A pid this high is not in use on a test machine.
        assert_eq!(signal(Action::Term, &[i32::MAX - 7]).unwrap(), 0);
    }

    #[test]
    fn actions_deserialize_in_lowercase() {
        assert_eq!(serde_json::from_str::<Action>("\"kill\"").unwrap(), Action::Kill);
        assert!(serde_json::from_str::<Action>("\"KILL\"").is_err());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_snapshot_reads_this_machine_and_is_cached_briefly() {
        let v = snapshot(5).await;
        assert_eq!(v["available"], true, "{v}");
        assert!(v["system"]["num_cpu"].as_u64().unwrap() > 0);
        assert!(v["others"].as_array().unwrap().len() <= 5);
        for o in v["others"].as_array().unwrap() {
            assert_eq!(o["system"], o["self"] == true || o["exe"].as_str().is_some_and(model::system_path), "{o}");
        }
        let again = snapshot(5).await;
        assert_eq!(v["at"], again["at"], "the second poll within the window is the same scan");
    }
}
