//! Hibernation against a real tmux server on a private socket: a
//! SIGTERM-ignoring "amp" process is SIGKILLed while its pane survives.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opensessions_runtime::hibernate::{
    HIBERNATE_TERM_GRACE, ProcessControl, SystemProcessControl, find_agent_process,
    terminate_agent_processes,
};
use opensessions_runtime::mux::MuxProvider;
use opensessions_runtime::tmux_provider::{CommandOutput, CommandRunner, TmuxProvider};

struct PrivateTmux {
    socket: String,
    dir: PathBuf,
    socket_path: Option<PathBuf>,
}

impl PrivateTmux {
    fn tmux(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .env_remove("TMUX")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .expect("run tmux")
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8_lossy(&self.tmux(args).stdout)
            .trim()
            .to_string()
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct PrivateRunner {
    socket: String,
}

impl CommandRunner for PrivateRunner {
    fn run(&self, args: &[String]) -> CommandOutput {
        let output = Command::new("tmux")
            .env_remove("TMUX")
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .expect("run tmux");
        CommandOutput {
            exit_code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }
}

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    check()
}

#[test]
fn term_ignoring_agent_is_killed_and_its_pane_survives() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let unique = format!(
        "opensessions-hibernate-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&dir).unwrap();
    let fake_amp = dir.join("amp");
    std::os::unix::fs::symlink("/bin/sleep", &fake_amp).unwrap();
    let mut lab = PrivateTmux {
        socket: unique.clone(),
        dir,
        socket_path: None,
    };

    // The pane shell ignores TERM and the ignored disposition is inherited by
    // the agent across exec, mimicking Amp. The shell keeps running after the
    // agent dies, so the pane must survive.
    let command = format!("trap '' TERM; '{}' 300; sleep 300", fake_amp.display());
    let created = lab.tmux(&[
        "new-session",
        "-d",
        "-s",
        "hibernate",
        "-x",
        "80",
        "-y",
        "24",
        &command,
    ]);
    assert!(created.status.success(), "{created:?}");
    lab.socket_path = Some(PathBuf::from(lab.stdout(&[
        "display-message",
        "-p",
        "#{socket_path}",
    ])));
    let pane_id = lab.stdout(&["display-message", "-p", "-t", "hibernate", "#{pane_id}"]);

    let provider = TmuxProvider::new(Arc::new(PrivateRunner {
        socket: unique.clone(),
    }));
    let pane_pid = provider.get_pane_pid(&pane_id).expect("pane pid");
    let control = SystemProcessControl;
    let mut target = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            target = find_agent_process(pane_pid, "amp", &control.process_table());
            target.is_some()
        }),
        "fake amp never appeared under pane pid {pane_pid}"
    );
    let target = target.unwrap();
    assert_ne!(target.pid, pane_pid);

    let outcomes = terminate_agent_processes(
        &control,
        std::slice::from_ref(&target),
        HIBERNATE_TERM_GRACE,
    );

    assert!(outcomes[0].terminated);
    assert!(
        outcomes[0].escalated,
        "TERM is ignored, so KILL is required"
    );
    assert!(
        wait_until(Duration::from_secs(3), || {
            !control
                .process_table()
                .iter()
                .any(|entry| entry.pid == target.pid && entry.args == target.processes[0].args)
        }),
        "fake amp survived SIGKILL"
    );
    assert_eq!(
        lab.stdout(&[
            "list-panes",
            "-t",
            "hibernate",
            "-F",
            "#{pane_id} #{pane_dead}"
        ]),
        format!("{pane_id} 0"),
        "pane must survive hibernation"
    );
    assert_eq!(provider.get_pane_pid(&pane_id), Some(pane_pid));
    assert!(
        control
            .process_table()
            .iter()
            .any(|entry| entry.pid == pane_pid),
        "pane shell must survive hibernation"
    );
}
