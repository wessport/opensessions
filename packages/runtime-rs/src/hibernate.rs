//! Auto-hibernation of idle agent processes.
//!
//! Idle CLI agents (Amp, Claude Code, Codex, ...) keep hundreds of megabytes
//! resident for as long as their tmux pane stays open. Hibernation stops only
//! the agent process inside the pane — never the pane, the session, or the
//! pane's shell — while the tracker keeps the agent row visible as
//! `hibernated` so the user can resume the thread later.
//!
//! Process selection and signal escalation are pure functions over a process
//! table plus a [`ProcessControl`] seam, so they are testable without real
//! processes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Command;
use std::time::Duration;

/// How often the server looks for idle agents to hibernate.
pub const HIBERNATE_POLL_INTERVAL_MS: u64 = 5 * 60 * 1000;
/// How long a SIGTERM'd agent gets to exit before it is SIGKILLed. Amp ignores
/// SIGTERM, so escalation is the normal path for it.
pub const HIBERNATE_TERM_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    /// Full command line as reported by `ps -o args=`.
    pub args: String,
}

/// The agent process found under a pane, plus the agent-named processes in
/// its subtree (for example Amp plugin runtimes) that must not be orphaned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcessTarget {
    pub pid: u32,
    /// The target itself followed by its agent-named descendants, captured
    /// before signalling so escalation can still reach reparented children.
    pub processes: Vec<ProcessEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

impl Signal {
    fn flag(self) -> &'static str {
        match self {
            Self::Term => "-TERM",
            Self::Kill => "-KILL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminationOutcome {
    pub pid: u32,
    /// SIGTERM was delivered to the agent process.
    pub terminated: bool,
    /// At least one process in the target survived the grace period and was
    /// sent SIGKILL.
    pub escalated: bool,
}

pub trait ProcessControl: Send + Sync {
    fn process_table(&self) -> Vec<ProcessEntry>;
    fn signal(&self, pid: u32, signal: Signal) -> bool;
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Uses `ps` and `kill`, matching the runtime's other command-driven probes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessControl;

impl ProcessControl for SystemProcessControl {
    fn process_table(&self) -> Vec<ProcessEntry> {
        Command::new("ps")
            .args(["-axo", "pid=,ppid=,args="])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| parse_process_table(&String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    }

    fn signal(&self, pid: u32, signal: Signal) -> bool {
        if pid <= 1 {
            return false;
        }
        Command::new("kill")
            .args([signal.flag(), &pid.to_string()])
            .output()
            .is_ok_and(|output| output.status.success())
    }
}

pub fn parse_process_table(raw: &str) -> Vec<ProcessEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.trim_start().splitn(2, char::is_whitespace);
            let pid = parts.next()?.parse::<u32>().ok()?;
            let rest = parts.next()?.trim_start();
            let mut parts = rest.splitn(2, char::is_whitespace);
            let ppid = parts.next()?.parse::<u32>().ok()?;
            let args = parts.next().unwrap_or_default().trim().to_string();
            Some(ProcessEntry { pid, ppid, args })
        })
        .collect()
}

/// Executable names for tracker agent ids. Unknown agents are never
/// hibernated because their process cannot be identified safely.
pub fn agent_process_names(agent: &str) -> Option<&'static [&'static str]> {
    Some(match agent {
        "amp" => &["amp", "amp-local"],
        "claude-code" | "claude" => &["claude"],
        "codex" => &["codex"],
        "opencode" => &["opencode"],
        "pi" => &["pi"],
        "droid" => &["droid"],
        _ => return None,
    })
}

const SCRIPT_INTERPRETERS: &[&str] = &[
    "node", "bun", "deno", "python", "python3", "sh", "bash", "zsh",
];

fn program_name(token: &str) -> String {
    let base = token.rsplit('/').next().unwrap_or(token);
    let base = base.strip_prefix('-').unwrap_or(base);
    let base = [".js", ".mjs", ".cjs", ".ts"]
        .iter()
        .find_map(|ext| base.strip_suffix(ext))
        .unwrap_or(base);
    base.to_ascii_lowercase()
}

fn process_matches(entry: &ProcessEntry, names: &[&str]) -> bool {
    let mut tokens = entry.args.split_whitespace();
    let Some(first) = tokens.next().map(program_name) else {
        return false;
    };
    if names.contains(&first.as_str()) {
        return true;
    }
    SCRIPT_INTERPRETERS.contains(&first.as_str())
        && tokens
            .find(|token| !token.starts_with('-'))
            .map(program_name)
            .is_some_and(|script| names.contains(&script.as_str()))
}

/// Finds the shallowest descendant of `pane_pid` that runs `agent`. The pane
/// root itself is never selected: killing it would close the pane.
pub fn find_agent_process(
    pane_pid: u32,
    agent: &str,
    table: &[ProcessEntry],
) -> Option<AgentProcessTarget> {
    let names = agent_process_names(agent)?;
    let mut children_of = HashMap::<u32, Vec<&ProcessEntry>>::new();
    for entry in table {
        if entry.pid != entry.ppid {
            children_of.entry(entry.ppid).or_default().push(entry);
        }
    }

    let mut visited = HashSet::from([pane_pid]);
    let mut queue = VecDeque::from([pane_pid]);
    let mut target = None;
    while let Some(pid) = queue.pop_front() {
        for child in children_of.get(&pid).into_iter().flatten() {
            if !visited.insert(child.pid) {
                continue;
            }
            if process_matches(child, names) {
                target = Some(*child);
                break;
            }
            queue.push_back(child.pid);
        }
        if target.is_some() {
            break;
        }
    }
    let target = target?;

    let mut processes = vec![target.clone()];
    let mut queue = VecDeque::from([target.pid]);
    let mut visited = HashSet::from([target.pid]);
    while let Some(pid) = queue.pop_front() {
        for child in children_of.get(&pid).into_iter().flatten() {
            if !visited.insert(child.pid) {
                continue;
            }
            if process_matches(child, names) {
                processes.push((*child).clone());
            }
            queue.push_back(child.pid);
        }
    }

    Some(AgentProcessTarget {
        pid: target.pid,
        processes,
    })
}

/// Sends SIGTERM to every target that is still running with the command line
/// it was selected with, waits once for `grace`, then SIGKILLs any captured
/// process that is still running with the same command line.
pub fn terminate_agent_processes(
    control: &dyn ProcessControl,
    targets: &[AgentProcessTarget],
    grace: Duration,
) -> Vec<TerminationOutcome> {
    // The targets come from an earlier process-table snapshot; re-read it so
    // a pid recycled since then is never signalled.
    let running = process_args_by_pid(control);
    let mut outcomes = targets
        .iter()
        .map(|target| TerminationOutcome {
            pid: target.pid,
            terminated: target
                .processes
                .first()
                .is_some_and(|process| running.get(&process.pid) == Some(&process.args))
                && control.signal(target.pid, Signal::Term),
            escalated: false,
        })
        .collect::<Vec<_>>();
    if !outcomes.iter().any(|outcome| outcome.terminated) {
        return outcomes;
    }

    control.sleep(grace);
    let survivors = process_args_by_pid(control);
    for (target, outcome) in targets.iter().zip(outcomes.iter_mut()) {
        if !outcome.terminated {
            continue;
        }
        for process in &target.processes {
            // Matching the command line guards against signalling a recycled
            // pid that now belongs to an unrelated process.
            if survivors.get(&process.pid) == Some(&process.args)
                && control.signal(process.pid, Signal::Kill)
            {
                outcome.escalated = true;
            }
        }
    }
    outcomes
}

fn process_args_by_pid(control: &dyn ProcessControl) -> HashMap<u32, String> {
    control
        .process_table()
        .into_iter()
        .map(|entry| (entry.pid, entry.args))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn entry(pid: u32, ppid: u32, args: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid,
            args: args.to_string(),
        }
    }

    fn amp_pane_table() -> Vec<ProcessEntry> {
        vec![
            entry(1, 0, "/sbin/launchd"),
            entry(100, 1, "-zsh"),
            entry(200, 100, "/Users/me/.amp/bin/amp threads continue T-1"),
            entry(
                201,
                200,
                "/Users/me/.amp/bin/amp run /tmp/plugin-runtime.ts /me/plugins/btw.ts",
            ),
            entry(202, 200, "node /work/node_modules/.bin/vite"),
            entry(300, 1, "/Users/me/.amp/bin/amp"),
        ]
    }

    #[test]
    fn parses_ps_rows_with_spaces_in_arguments() {
        let table = parse_process_table("  100     1 -zsh\n  200   100 /a b/amp run x\nbad row\n");
        assert_eq!(
            table,
            vec![entry(100, 1, "-zsh"), entry(200, 100, "/a b/amp run x")]
        );
    }

    #[test]
    fn selects_the_shallowest_agent_descendant_and_its_agent_children() {
        let target = find_agent_process(100, "amp", &amp_pane_table()).expect("amp target");

        assert_eq!(target.pid, 200);
        assert_eq!(
            target
                .processes
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![200, 201],
            "only agent-named descendants are captured; user tools are left alone"
        );
    }

    #[test]
    fn never_selects_processes_outside_the_pane_tree() {
        let table = vec![
            entry(100, 1, "-zsh"),
            entry(101, 100, "vim notes.md"),
            entry(300, 1, "/Users/me/.amp/bin/amp"),
        ];
        assert_eq!(find_agent_process(100, "amp", &table), None);
    }

    #[test]
    fn never_selects_the_pane_root_even_when_it_is_the_agent() {
        let table = vec![entry(100, 1, "/Users/me/.amp/bin/amp")];
        assert_eq!(find_agent_process(100, "amp", &table), None);
    }

    #[test]
    fn matches_script_launched_agents_by_script_name() {
        let table = vec![
            entry(100, 1, "-zsh"),
            entry(
                110,
                100,
                "node /usr/local/lib/node_modules/.bin/claude --resume",
            ),
        ];
        let target = find_agent_process(100, "claude-code", &table).expect("claude target");
        assert_eq!(target.pid, 110);
    }

    #[test]
    fn does_not_match_agent_names_as_substrings() {
        let table = vec![
            entry(100, 1, "-zsh"),
            entry(110, 100, "/usr/bin/sample 1"),
            entry(111, 100, "/usr/bin/pip install x"),
        ];
        assert_eq!(find_agent_process(100, "amp", &table), None);
        assert_eq!(find_agent_process(100, "pi", &table), None);
    }

    #[test]
    fn unknown_agents_are_never_targeted() {
        let table = vec![entry(100, 1, "-zsh"), entry(110, 100, "gemini")];
        assert_eq!(find_agent_process(100, "gemini", &table), None);
    }

    struct FakeControl {
        /// The table before signalling; `None` means the process table is
        /// unchanged since the target was selected.
        table_before_term: Option<Vec<ProcessEntry>>,
        table_after_grace: Vec<ProcessEntry>,
        fail_term: HashSet<u32>,
        signals: Mutex<Vec<(u32, Signal)>>,
        slept: Mutex<Vec<Duration>>,
    }

    impl FakeControl {
        fn new(table_after_grace: Vec<ProcessEntry>) -> Self {
            Self {
                table_before_term: None,
                table_after_grace,
                fail_term: HashSet::new(),
                signals: Mutex::new(Vec::new()),
                slept: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProcessControl for FakeControl {
        fn process_table(&self) -> Vec<ProcessEntry> {
            if self.slept.lock().unwrap().is_empty() {
                return self
                    .table_before_term
                    .clone()
                    .unwrap_or_else(amp_pane_table);
            }
            self.table_after_grace.clone()
        }

        fn signal(&self, pid: u32, signal: Signal) -> bool {
            self.signals.lock().unwrap().push((pid, signal));
            !(signal == Signal::Term && self.fail_term.contains(&pid))
        }

        fn sleep(&self, duration: Duration) {
            self.slept.lock().unwrap().push(duration);
        }
    }

    #[test]
    fn escalates_to_kill_when_the_agent_ignores_term() {
        let table = amp_pane_table();
        let target = find_agent_process(100, "amp", &table).unwrap();
        let control = FakeControl::new(table);

        let outcomes = terminate_agent_processes(&control, &[target], HIBERNATE_TERM_GRACE);

        assert_eq!(
            outcomes,
            vec![TerminationOutcome {
                pid: 200,
                terminated: true,
                escalated: true,
            }]
        );
        assert_eq!(
            *control.signals.lock().unwrap(),
            vec![
                (200, Signal::Term),
                (200, Signal::Kill),
                (201, Signal::Kill)
            ]
        );
        assert_eq!(*control.slept.lock().unwrap(), vec![HIBERNATE_TERM_GRACE]);
    }

    #[test]
    fn does_not_kill_when_the_agent_exits_on_term() {
        let table = amp_pane_table();
        let target = find_agent_process(100, "amp", &table).unwrap();
        let control = FakeControl::new(vec![entry(100, 1, "-zsh")]);

        let outcomes = terminate_agent_processes(&control, &[target], HIBERNATE_TERM_GRACE);

        assert!(outcomes[0].terminated);
        assert!(!outcomes[0].escalated);
        assert_eq!(*control.signals.lock().unwrap(), vec![(200, Signal::Term)]);
    }

    #[test]
    fn does_not_kill_a_recycled_pid() {
        let table = amp_pane_table();
        let target = find_agent_process(100, "amp", &table).unwrap();
        let control = FakeControl::new(vec![entry(200, 1, "/usr/bin/unrelated")]);

        let outcomes = terminate_agent_processes(&control, &[target], HIBERNATE_TERM_GRACE);

        assert!(!outcomes[0].escalated);
        assert_eq!(*control.signals.lock().unwrap(), vec![(200, Signal::Term)]);
    }

    #[test]
    fn does_not_term_a_pid_recycled_since_the_target_was_selected() {
        let table = amp_pane_table();
        let target = find_agent_process(100, "amp", &table).unwrap();
        let mut control = FakeControl::new(table);
        control.table_before_term = Some(vec![entry(200, 1, "/usr/bin/unrelated")]);

        let outcomes = terminate_agent_processes(&control, &[target], HIBERNATE_TERM_GRACE);

        assert!(!outcomes[0].terminated);
        assert!(control.signals.lock().unwrap().is_empty());
        assert!(control.slept.lock().unwrap().is_empty());
    }

    #[test]
    fn skips_waiting_and_escalation_when_term_cannot_be_delivered() {
        let table = amp_pane_table();
        let target = find_agent_process(100, "amp", &table).unwrap();
        let mut control = FakeControl::new(table);
        control.fail_term.insert(200);

        let outcomes = terminate_agent_processes(&control, &[target], HIBERNATE_TERM_GRACE);

        assert!(!outcomes[0].terminated);
        assert!(!outcomes[0].escalated);
        assert!(control.slept.lock().unwrap().is_empty());
        assert_eq!(*control.signals.lock().unwrap(), vec![(200, Signal::Term)]);
    }
}
