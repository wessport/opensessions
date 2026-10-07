//! TmuxProvider behavior against a real tmux server on a private socket,
//! for cases where tmux's own target and naming semantics matter.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opensessions_runtime::mux::MuxProvider;
use opensessions_runtime::tmux_provider::{CommandOutput, CommandRunner, TmuxProvider};

static NEXT_LAB: AtomicUsize = AtomicUsize::new(0);

struct PrivateTmux {
    socket: String,
    socket_path: String,
    dir: PathBuf,
}

impl PrivateTmux {
    /// Starts a private tmux server with one detached session, or `None`
    /// when tmux is not installed.
    fn start(first_session: &str) -> Option<Self> {
        if Command::new("tmux").arg("-V").output().is_err() {
            eprintln!("tmux not installed; skipping");
            return None;
        }
        let socket = format!(
            "os-prov-{}-{}-{}",
            std::process::id(),
            NEXT_LAB.fetch_add(1, Ordering::SeqCst),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        let dir = std::env::temp_dir().join(&socket);
        std::fs::create_dir_all(&dir).unwrap();
        let mut lab = Self {
            socket,
            socket_path: String::new(),
            dir,
        };
        let created = lab.tmux(&[
            "new-session",
            "-d",
            "-s",
            first_session,
            "-x",
            "120",
            "-y",
            "30",
        ]);
        assert!(created.status.success(), "{created:?}");
        lab.socket_path = lab.stdout(&["display-message", "-p", "#{socket_path}"]);
        Some(lab)
    }

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

    fn session_names(&self) -> Vec<String> {
        let mut names = self
            .stdout(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn provider(&self) -> TmuxProvider {
        TmuxProvider::new(Arc::new(PrivateRunner {
            socket: self.socket.clone(),
        }))
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
        if !self.socket_path.is_empty() {
            let _ = std::fs::remove_file(&self.socket_path);
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

#[allow(dead_code)]
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
fn session_targets_never_fall_back_to_prefix_matches() {
    let Some(lab) = PrivateTmux::start("api-v2") else {
        return;
    };
    lab.tmux(&["new-session", "-d", "-s", "keep"]);
    let provider = lab.provider();

    // `api` does not exist; tmux would prefix-match it to `api-v2`.
    assert_eq!(provider.get_pane_count("api"), 0);
    assert!(provider.get_session_pane_pids("api").is_empty());
    assert_eq!(provider.get_session_dir("api"), "");
    assert!(provider.list_windows("api").is_empty());
    assert!(provider.list_sidebar_panes(Some("api")).is_empty());
    provider.kill_session("api");

    assert_eq!(lab.session_names(), vec!["api-v2", "keep"]);
    assert_eq!(provider.get_pane_count("api-v2"), 1);

    provider.kill_session("api-v2");
    assert_eq!(lab.session_names(), vec!["keep"]);
}

#[test]
fn rename_reports_the_name_tmux_actually_assigned() {
    let Some(lab) = PrivateTmux::start("draft") else {
        return;
    };
    lab.tmux(&["new-session", "-d", "-s", "api-v2"]);
    let provider = lab.provider();

    // tmux replaces `.`/`:` and expands formats in new names.
    assert_eq!(
        provider.rename_session("draft", "v1.2:x").as_deref(),
        Some("v1_2_x")
    );
    let expanded = provider
        .rename_session("v1_2_x", "n#{session_id}")
        .expect("renamed");
    assert!(expanded.starts_with("n$"), "{expanded}");
    assert_eq!(
        provider.rename_session(&expanded, "plain name").as_deref(),
        Some("plain name")
    );

    // A missing source never falls back to a prefix match.
    assert_eq!(provider.rename_session("api", "stolen"), None);
    assert_eq!(lab.session_names(), vec!["api-v2", "plain name"]);
}
