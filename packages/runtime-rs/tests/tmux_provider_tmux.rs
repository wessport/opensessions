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

/// Waits for a dead pane to be cleaned up the way the product guarantees it
/// on any tmux version: by the `pane-died` hook, or else by the server's
/// sweep, which runs on every poll. tmux 3.4 skips the hook for some deaths.
fn wait_for_cleanup(
    provider: &TmuxProvider,
    timeout: Duration,
    mut check: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut next_sweep = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        if Instant::now() >= next_sweep {
            provider.close_dead_content_panes();
            next_sweep += Duration::from_millis(500);
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

/// One window with a fake sidebar pane plus content panes whose shells exit
/// with the given status after one Enter keypress, under real hooks unless
/// `hooks` is false.
struct SidebarWindow {
    window: String,
    sidebar: String,
    content: Vec<String>,
}

fn sidebar_window(
    lab: &PrivateTmux,
    provider: &TmuxProvider,
    exit_codes: &[i32],
    hooks: bool,
) -> SidebarWindow {
    let window = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{window_id}"]);
    let keeper = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{pane_id}"]);
    let mut content = Vec::new();
    for code in exit_codes {
        let command = format!("sh -c 'read line; exit {code}'");
        content.push(lab.stdout(&[
            "split-window",
            "-d",
            "-t",
            &keeper,
            "-P",
            "-F",
            "#{pane_id}",
            &command,
        ]));
    }
    let sidebar = lab.stdout(&[
        "split-window",
        "-hbf",
        "-d",
        "-l",
        "20",
        "-t",
        &keeper,
        "-P",
        "-F",
        "#{pane_id}",
        "sleep 600",
    ]);
    lab.tmux(&["select-pane", "-t", &sidebar, "-T", "opensessions-sidebar"]);
    if hooks {
        // Hooks point at a closed port; their HTTP calls fail fast and silently.
        provider.setup_hooks("127.0.0.1", 9, "/nonexistent/opensessions.token");
    }
    provider.prepare_sidebar_window(&window);
    SidebarWindow {
        window,
        sidebar,
        content,
    }
}

fn window_option(lab: &PrivateTmux, window: &str, option: &str) -> String {
    lab.stdout(&["show-window-options", "-t", window, "-v", option])
}

/// `None` when the pane no longer exists, otherwise whether it is dead.
fn pane_dead(lab: &PrivateTmux, pane: &str) -> Option<bool> {
    lab.stdout(&["list-panes", "-a", "-F", "#{pane_id} #{pane_dead}"])
        .lines()
        .find_map(|line| {
            let (id, dead) = line.split_once(' ')?;
            (id == pane).then(|| dead == "1")
        })
}

/// tmux 3.4 sometimes marks a pane dead without ever recording its exit
/// status, and then runs no `pane-died` hook for it. Under `remain-on-exit
/// failed` tmux itself keeps such a pane, and so does opensessions.
fn exit_status_lost(lab: &PrivateTmux, pane: &str) -> bool {
    lab.stdout(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id} #{pane_dead} #{pane_dead_status}",
    ])
    .lines()
    .any(|line| line.trim_end() == format!("{pane} 1"))
}

fn exit_pane(lab: &PrivateTmux, pane: &str) {
    lab.tmux(&["send-keys", "-t", pane, "Enter"]);
}

#[test]
fn hiding_the_sidebar_restores_remain_on_exit_for_its_window() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[0], true);
    assert_eq!(window_option(&lab, &setup.window, "remain-on-exit"), "on");

    lab.tmux(&["kill-pane", "-t", &setup.sidebar]);
    provider.restore_windows_without_sidebar();

    assert_eq!(window_option(&lab, &setup.window, "remain-on-exit"), "");
    assert_eq!(
        window_option(&lab, &setup.window, "@opensessions_remain_on_exit_previous"),
        ""
    );
    exit_pane(&lab, &setup.content[0]);
    assert!(
        wait_until(Duration::from_secs(3), || pane_dead(
            &lab,
            &setup.content[0]
        )
        .is_none()),
        "exited pane lingered: {:?}",
        pane_dead(&lab, &setup.content[0])
    );
}

#[test]
fn dead_panes_do_not_linger_after_a_sidebar_is_killed_by_hand() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[0], true);

    // The server is not told about the kill; the pane-died hook (or the
    // server's sweep where tmux skips the hook) must cope.
    lab.tmux(&["kill-pane", "-t", &setup.sidebar]);
    exit_pane(&lab, &setup.content[0]);

    assert!(
        wait_for_cleanup(&provider, Duration::from_secs(5), || pane_dead(
            &lab,
            &setup.content[0]
        )
        .is_none()),
        "exited pane lingered: {:?}",
        pane_dead(&lab, &setup.content[0])
    );
    assert!(wait_for_cleanup(&provider, Duration::from_secs(5), || {
        window_option(&lab, &setup.window, "remain-on-exit").is_empty()
    }));
}

#[test]
fn sidebar_windows_keep_dead_panes_when_the_user_wants_them() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    lab.tmux(&["set-option", "-gw", "remain-on-exit", "on"]);
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[0, 0], true);

    exit_pane(&lab, &setup.content[0]);
    assert!(wait_until(Duration::from_secs(3), || {
        pane_dead(&lab, &setup.content[0]) == Some(true)
    }));
    // The pane-died hook has run once the second pane is dead too; the
    // first must still be there.
    exit_pane(&lab, &setup.content[1]);
    assert!(wait_until(Duration::from_secs(3), || {
        pane_dead(&lab, &setup.content[1]) == Some(true)
    }));
    thread::sleep(Duration::from_millis(300));
    provider.close_dead_content_panes();
    assert_eq!(pane_dead(&lab, &setup.content[0]), Some(true));
    assert_eq!(pane_dead(&lab, &setup.content[1]), Some(true));
}

#[test]
fn sidebar_windows_honour_remain_on_exit_failed() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    lab.tmux(&["set-option", "-gw", "remain-on-exit", "failed"]);
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[3, 0], true);

    exit_pane(&lab, &setup.content[0]);
    exit_pane(&lab, &setup.content[1]);

    let removed = wait_for_cleanup(&provider, Duration::from_secs(5), || {
        pane_dead(&lab, &setup.content[1]).is_none()
    });
    assert!(
        removed || exit_status_lost(&lab, &setup.content[1]),
        "cleanly exited pane lingered"
    );
    assert!(wait_until(Duration::from_secs(3), || {
        pane_dead(&lab, &setup.content[0]) == Some(true)
    }));
    provider.close_dead_content_panes();
    assert_eq!(pane_dead(&lab, &setup.content[0]), Some(true));
}

/// Waits until every pane is dead, without any cleanup running.
fn wait_until_dead(lab: &PrivateTmux, panes: &[String]) {
    assert!(
        wait_until(Duration::from_secs(5), || panes
            .iter()
            .all(|pane| pane_dead(lab, pane) == Some(true))),
        "panes never died: {panes:?}"
    );
}

#[test]
fn the_sweep_alone_removes_dead_panes_the_user_would_not_keep() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    lab.tmux(&["set-option", "-gw", "remain-on-exit", "failed"]);
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[3, 0, 0], false);
    for pane in &setup.content {
        exit_pane(&lab, pane);
    }
    wait_until_dead(&lab, &setup.content);
    // A recorded status shows up right away; one still missing is lost.
    wait_until(Duration::from_secs(1), || {
        !setup
            .content
            .iter()
            .any(|pane| exit_status_lost(&lab, pane))
    });
    let lost = setup
        .content
        .iter()
        .map(|pane| exit_status_lost(&lab, pane))
        .collect::<Vec<_>>();

    provider.close_dead_content_panes();

    assert_eq!(pane_dead(&lab, &setup.content[0]), Some(true));
    for (pane, lost) in setup.content.iter().zip(lost).skip(1) {
        let expected = if lost { Some(true) } else { None };
        assert_eq!(
            pane_dead(&lab, pane),
            expected,
            "{pane} (status lost: {lost})"
        );
    }
    assert_eq!(pane_dead(&lab, &setup.sidebar), Some(false));
    assert_eq!(window_option(&lab, &setup.window, "remain-on-exit"), "on");
}

#[test]
fn the_sweep_alone_keeps_dead_panes_under_remain_on_exit_on() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    lab.tmux(&["set-option", "-gw", "remain-on-exit", "on"]);
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[0, 3], false);
    for pane in &setup.content {
        exit_pane(&lab, pane);
    }
    wait_until_dead(&lab, &setup.content);

    provider.close_dead_content_panes();

    assert_eq!(pane_dead(&lab, &setup.content[0]), Some(true));
    assert_eq!(pane_dead(&lab, &setup.content[1]), Some(true));
}

#[test]
fn the_sweep_alone_restores_a_window_whose_sidebar_is_gone() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    let provider = lab.provider();
    let setup = sidebar_window(&lab, &provider, &[0], false);
    lab.tmux(&["kill-pane", "-t", &setup.sidebar]);
    exit_pane(&lab, &setup.content[0]);
    wait_until_dead(&lab, &setup.content);

    provider.close_dead_content_panes();

    assert_eq!(pane_dead(&lab, &setup.content[0]), None);
    assert_eq!(window_option(&lab, &setup.window, "remain-on-exit"), "");
    assert_eq!(
        window_option(&lab, &setup.window, "@opensessions_remain_on_exit_previous"),
        ""
    );
}

/// A tmux client attached through a pseudo-terminal, or `None` when
/// python3 is not installed. Killed on drop.
struct AttachedClient(std::process::Child);

impl AttachedClient {
    fn attach(lab: &PrivateTmux, session: &str) -> Option<Self> {
        let script = r#"
import os, pty, select, sys
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp("tmux", ["tmux", "-L", sys.argv[1], "attach-session", "-t", "=" + sys.argv[2]])
while True:
    ready, _, _ = select.select([fd], [], [], 0.2)
    try:
        if ready and not os.read(fd, 65536):
            break
    except OSError:
        break
"#;
        let child = Command::new("python3")
            .env_remove("TMUX")
            .args(["-c", script, &lab.socket, session])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let client = Self(child);
        assert!(
            wait_until(Duration::from_secs(5), || client_sessions(lab)
                .contains(&session.to_string())),
            "client never attached to {session}"
        );
        Some(client)
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn client_sessions(lab: &PrivateTmux) -> Vec<String> {
    lab.stdout(&["list-clients", "-F", "#{client_session}"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Sessions `zeta`, `work`, `alpha` created in that order (usually within
/// one second), with a client attached to `work`, whose only window holds a
/// fake sidebar and one content pane that exits after one Enter keypress.
/// Real hooks are installed unless `hooks` is false.
fn closing_session_lab(hooks: bool) -> Option<(PrivateTmux, TmuxProvider, String, AttachedClient)> {
    let lab = PrivateTmux::start("zeta")?;
    lab.tmux(&[
        "new-session",
        "-d",
        "-s",
        "work",
        "sh -c 'read line; exit 0'",
    ]);
    lab.tmux(&["new-session", "-d", "-s", "alpha"]);
    let provider = lab.provider();
    let content = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{pane_id}"]);
    let window = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{window_id}"]);
    let sidebar = lab.stdout(&[
        "split-window",
        "-hbf",
        "-d",
        "-l",
        "20",
        "-t",
        &content,
        "-P",
        "-F",
        "#{pane_id}",
        "sleep 600",
    ]);
    lab.tmux(&["select-pane", "-t", &sidebar, "-T", "opensessions-sidebar"]);
    if hooks {
        // Hooks point at a closed port; their HTTP calls fail fast and silently.
        provider.setup_hooks("127.0.0.1", 9, "/nonexistent/opensessions.token");
    }
    provider.prepare_sidebar_window(&window);
    let client = AttachedClient::attach(&lab, "work")?;
    Some((lab, provider, content, client))
}

/// The sidebar lists sessions in creation order and falls back to the
/// previous one; tmux-side cleanup must agree instead of using name order.
#[test]
fn closing_the_last_content_pane_falls_back_to_the_previously_created_session() {
    let Some((lab, provider, content, _client)) = closing_session_lab(true) else {
        return;
    };

    exit_pane(&lab, &content);

    assert!(
        wait_for_cleanup(&provider, Duration::from_secs(5), || !lab
            .session_names()
            .contains(&"work".to_string())),
        "work was not closed: {:?}",
        lab.session_names()
    );
    assert_eq!(client_sessions(&lab), vec!["zeta".to_string()]);
}

#[test]
fn killing_the_last_content_pane_falls_back_to_the_previously_created_session() {
    let Some((lab, _provider, content, _client)) = closing_session_lab(true) else {
        return;
    };

    lab.tmux(&["kill-pane", "-t", &content]);

    assert!(
        wait_until(Duration::from_secs(5), || !lab
            .session_names()
            .contains(&"work".to_string())),
        "work was not closed: {:?}",
        lab.session_names()
    );
    assert_eq!(client_sessions(&lab), vec!["zeta".to_string()]);
}

#[test]
fn the_sweep_alone_closes_a_window_left_without_content_and_falls_back() {
    let Some((lab, provider, content, _client)) = closing_session_lab(false) else {
        return;
    };
    exit_pane(&lab, &content);
    wait_until_dead(&lab, std::slice::from_ref(&content));
    assert!(lab.session_names().contains(&"work".to_string()));

    provider.close_dead_content_panes();

    assert!(
        wait_until(Duration::from_secs(3), || !lab
            .session_names()
            .contains(&"work".to_string())),
        "work was not closed: {:?}",
        lab.session_names()
    );
    assert_eq!(client_sessions(&lab), vec!["zeta".to_string()]);
}

/// The server sweeps dead panes when the tmux state fingerprint changes, so
/// a pane's death alone must change it.
#[test]
fn a_pane_dying_changes_the_tmux_state_fingerprint() {
    let Some(lab) = PrivateTmux::start("work") else {
        return;
    };
    lab.tmux(&["set-option", "-gw", "remain-on-exit", "on"]);
    let provider = lab.provider();
    let pane = lab.stdout(&[
        "split-window",
        "-d",
        "-t",
        "=work:",
        "-P",
        "-F",
        "#{pane_id}",
        "sh -c 'read line; exit 0'",
    ]);
    // Let the shell settle so only its death differs between fingerprints.
    thread::sleep(Duration::from_millis(300));
    let alive = provider.state_fingerprint();
    assert!(alive.is_some());

    exit_pane(&lab, &pane);
    wait_until_dead(&lab, std::slice::from_ref(&pane));

    assert_ne!(provider.state_fingerprint(), alive);
}
