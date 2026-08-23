use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_websockets::Message;

const W: &str = "36";
const SIDEBAR_SESSIONS: &[&str] = &[
    "opensessions",
    "effect-ts",
    "lazydiff",
    "os-demo-feat-agent-panel",
    "os-demo-preview",
];

#[test]
fn tmux_sidebar_keyboard_focus_and_worktree_flow() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-focus");

    lab.wait_for_text("opensessions", "sessions");
    lab.wait_for_text("effect-ts", "effect-ts");

    let source = lab.sidebar_pane("opensessions");
    let tab_destination = lab.sidebar_pane("os-demo-feat-agent-panel");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    sleep(Duration::from_millis(250));

    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    assert_eq!(
        lab.active_pane(),
        source,
        "sidebar pane must be active before keyboard shortcut"
    );
    lab.tmux_ok(["send-keys", "-t", source.as_str(), "4"]);
    lab.wait_for_client_session("os-demo-feat-agent-panel");
    lab.wait_for_capture("os-demo-feat-agent-panel", |text| {
        row_with(text, "os-demo-feat-agent-panel").is_some_and(|row| row.contains("▌"))
    });

    let source_after_tab = lab.capture_pane(&source);
    assert!(
        row_with(&source_after_tab, "opensessions")
            .is_some_and(|row| row.trim_start().starts_with("▌")),
        "old source sidebar should rehome to its own confirmed active session after settled state; got:\n{source_after_tab}",
    );
    let effect_after_tab = lab.capture_pane(&tab_destination);
    assert!(
        row_with(&effect_after_tab, "os-demo-feat-agent-panel")
            .is_some_and(|row| row.contains("▌")),
        "destination sidebar should focus the destination concrete session; got:\n{effect_after_tab}",
    );

    let worktree_source = lab.sidebar_pane("os-demo-feat-agent-panel");
    let worktree_dest = lab.sidebar_pane("os-demo-preview");
    lab.tmux_ok(["switch-client", "-t", "os-demo-feat-agent-panel"]);
    lab.tmux_ok(["select-pane", "-t", worktree_source.as_str()]);
    lab.wait_for_client_session("os-demo-feat-agent-panel");
    sleep(Duration::from_millis(250));

    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Up"]);
    lab.wait_for_capture_pane(&worktree_source, |text| {
        row_with(text, "os-demo-worktrees").is_some_and(|row| row.trim_start().starts_with("›"))
    });
    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Enter"]);
    lab.wait_for_capture_pane(&worktree_source, |text| {
        text.contains("▸ os-demo-worktrees")
    });
    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Enter"]);
    lab.wait_for_capture_pane(&worktree_source, |text| {
        text.contains("▾ os-demo-worktrees")
    });
    let expanded_worktree = lab.capture_pane(&worktree_source);
    assert_worktree_group_columns(&expanded_worktree);

    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Down"]);
    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Down"]);
    lab.wait_for_capture_pane(&worktree_source, |text| {
        row_with(text, "os-demo-preview").is_some_and(|row| row.contains("›"))
    });
    lab.tmux_ok(["send-keys", "-t", worktree_source.as_str(), "Enter"]);
    lab.wait_for_client_session("os-demo-preview");
    lab.wait_for_capture_pane(&worktree_dest, |text| {
        row_with(text, "os-demo-preview").is_some_and(|row| row.contains("▌"))
            && !row_with(text, "os-demo-worktrees")
                .is_some_and(|row| row.trim_start().starts_with("›"))
    });

    let destination = lab.capture_pane(&worktree_dest);
    assert!(
        row_with(&destination, "os-demo-preview").is_some_and(|row| row.contains("▌")),
        "destination worktree child should own active/focused row; got:\n{destination}",
    );
    assert!(
        !row_with(&destination, "os-demo-worktrees")
            .is_some_and(|row| row.trim_start().starts_with("›")),
        "worktree group header must not remain focused after switching to concrete child; got:\n{destination}",
    );
}

#[test]
fn tmux_sidebar_renames_session_without_losing_connected_sidebars() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-rename-session");
    let second_window = lab.spawn_window_with_sidebar("opensessions", "rename-second-sidebar");
    let third_window = lab.spawn_window_with_sidebar("opensessions", "rename-third-sidebar");
    let other = lab.sidebar_pane_in_window("opensessions", &third_window);
    let source = lab.sidebar_pane_in_window("opensessions", &second_window);
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-window", "-t", second_window.as_str()]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    sleep(Duration::from_millis(250));

    lab.tmux_ok(["send-keys", "-t", source.as_str(), "r"]);
    lab.wait_for_capture_pane(&source, |text| text.contains("Rename session"));
    for _ in 0.."opensessions".len() {
        lab.tmux_ok(["send-keys", "-t", source.as_str(), "BSpace"]);
    }
    lab.tmux_ok([
        "send-keys",
        "-t",
        source.as_str(),
        "-l",
        "renamed-opensessions",
    ]);
    lab.tmux_ok(["send-keys", "-t", source.as_str(), "Enter"]);

    lab.wait_for_client_session("renamed-opensessions");
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "renamed-opensessions").is_some_and(|row| row.contains("▌"))
    });
    lab.wait_for_capture_pane(&other, |text| {
        row_with(text, "renamed-opensessions").is_some_and(|row| row.contains("▌"))
    });
    assert!(
        lab.tmux(["has-session", "-t", "=renamed-opensessions"])
            .is_empty()
    );
}

#[test]
fn tmux_sidebar_concurrent_ensure_keeps_one_sidebar_per_window() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-concurrent-ensure");
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let port = lab.port.to_string();
    let pid_file = lab.root.join("server.pid");
    lab.tmux_ok([
        "set-environment",
        "-g",
        "OPENSESSIONS_DIR",
        workspace_root.to_str().unwrap(),
    ]);
    lab.tmux_ok(["set-environment", "-g", "OPENSESSIONS_PORT", &port]);
    lab.tmux_ok([
        "set-environment",
        "-g",
        "OPENSESSIONS_PID_FILE",
        pid_file.to_str().unwrap(),
    ]);

    lab.tmux_ok([
        "new-session",
        "-d",
        "-x",
        "160",
        "-y",
        "40",
        "-s",
        "on-demand",
        "sh",
    ]);
    let window_id = lab.tmux(["display-message", "-p", "-t", "on-demand", "#{window_id}"]);
    let pane_id = lab.tmux(["display-message", "-p", "-t", "on-demand", "#{pane_id}"]);
    let context = format!("|on-demand|{window_id}|{pane_id}|1");
    let start = std::sync::Arc::new(std::sync::Barrier::new(12));
    let token = lab.auth_token();
    let requests = (0..12)
        .map(|_| {
            let start = start.clone();
            let context = context.clone();
            let token = token.clone();
            let port = lab.port;
            std::thread::spawn(move || {
                start.wait();
                post_body(port, "/ensure-sidebar", "text/plain", &context, &token);
            })
        })
        .collect::<Vec<_>>();
    for request in requests {
        request.join().expect("join concurrent ensure request");
    }

    let sidebars = lab
        .tmux([
            "list-panes",
            "-t",
            "on-demand",
            "-F",
            "#{pane_id}\t#{pane_title}",
        ])
        .lines()
        .filter(|line| line.ends_with("\topensessions-sidebar"))
        .count();
    assert_eq!(
        sidebars,
        1,
        "concurrent ensure requests must not create duplicate sidebars in {window_id}\n{}",
        lab.tmux([
            "list-panes",
            "-t",
            "on-demand",
            "-F",
            "#{pane_id} #{pane_title} #{pane_start_command}",
        ]),
    );
}

#[test]
fn tmux_sidebars_are_ready_before_switching_across_many_background_sessions() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-background-session-ready");
    lab.wait_for_sidebar_mode("ready");
    let snapshots_before = lab.debug_log_occurrences("snapshot_json mode=");
    let background_sessions = (0..14)
        .map(|index| format!("background-ready-{index}"))
        .collect::<Vec<_>>();
    for session in &background_sessions {
        lab.tmux_ok([
            "new-session",
            "-d",
            "-x",
            "160",
            "-y",
            "40",
            "-s",
            session,
            "sh",
        ]);
    }

    let sidebars = background_sessions
        .iter()
        .map(|session| {
            let sidebar = lab.wait_for_sidebar_pane(session);
            lab.wait_for_capture_pane(&sidebar, |text| text.contains("sessions"));
            (session, sidebar)
        })
        .collect::<Vec<_>>();
    let snapshots_during_warmup = lab
        .debug_log_occurrences("snapshot_json mode=")
        .saturating_sub(snapshots_before);
    assert!(
        snapshots_during_warmup <= background_sessions.len(),
        "background sidebar warmup recomputed {snapshots_during_warmup} snapshots for {} new sessions",
        background_sessions.len(),
    );
    let (target, sidebar) = sidebars.last().expect("background target");
    lab.tmux_ok(["switch-client", "-t", target]);
    lab.wait_for_client_session(target);

    assert_eq!(
        lab.sidebar_pane(target),
        *sidebar,
        "session switching must reuse the pre-spawned sidebar"
    );
    assert_eq!(
        lab.sidebar_panes()
            .iter()
            .filter(|pane| pane.session == target.as_str())
            .count(),
        1,
        "new background session must have exactly one sidebar"
    );
}

#[test]
fn tmux_sidebar_x_on_expanded_worktree_child_opens_child_kill_confirm() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-worktree-child-kill");

    let source = lab.sidebar_pane("os-demo-feat-agent-panel");
    lab.tmux_ok(["switch-client", "-t", "os-demo-feat-agent-panel"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_client_session("os-demo-feat-agent-panel");
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "os-demo-worktrees").is_some()
    });

    lab.send_sidebar_key(&source, "Up");
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "os-demo-worktrees").is_some_and(|row| row.contains("▾"))
    });

    lab.send_sidebar_key(&source, "j");
    lab.send_sidebar_key(&source, "j");
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "os-demo-preview").is_some_and(|row| row.contains("›"))
    });

    lab.send_sidebar_key(&source, "x");
    lab.wait_for_capture_pane(&source, |text| {
        text.contains("Kill session?") && text.contains("os-demo-preview")
    });

    lab.send_sidebar_key(&source, "y");
    lab.wait_for_session_absent("os-demo-preview");
}

#[test]
fn tmux_sidebar_x_on_active_worktree_child_kills_that_child_after_confirm() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-active-worktree-child-kill");

    let source = lab.sidebar_pane("os-demo-feat-agent-panel");
    let session_names = lab.session_names();
    let target_index = session_names
        .iter()
        .position(|name| name == "os-demo-feat-agent-panel")
        .expect("active worktree child should be present");
    let expected_fallback = target_index
        .checked_sub(1)
        .and_then(|index| session_names.get(index))
        .or_else(|| session_names.get(target_index + 1))
        .cloned()
        .expect("active worktree child should have a fallback session");
    lab.tmux_ok(["switch-client", "-t", "os-demo-feat-agent-panel"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_client_session("os-demo-feat-agent-panel");
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "os-demo-feat-agent-panel").is_some_and(|row| row.contains("▌"))
    });

    lab.send_sidebar_key(&source, "x");
    lab.wait_for_capture_pane(&source, |text| {
        text.contains("Kill session?") && text.contains("os-demo-feat-agent-panel")
    });

    lab.send_sidebar_key(&source, "y");
    lab.wait_for_session_absent("os-demo-feat-agent-panel");
    lab.wait_for_client_session(&expected_fallback);
}

#[test]
fn tmux_sidebar_click_spawns_live_sidebar_for_session_with_spaces() {
    let _guard = e2e_serial_guard();
    let mut lab = Lab::new("opensessions-e2e-spaced-session");
    lab.setup_repos();
    lab.setup_tmux();
    let spaced_dir = lab.root.join("jobseeker-feedback");
    fs::create_dir_all(&spaced_dir).expect("create spaced-session directory");
    lab.tmux_ok([
        "new-session",
        "-d",
        "-x",
        "160",
        "-y",
        "40",
        "-s",
        "jobseeker feedback",
        "-c",
        spaced_dir.to_str().unwrap(),
        "sh",
    ]);
    lab.start_server();
    lab.spawn_sidebars();

    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_capture_pane(&source, |text| text.contains("jobseeker feedback"));
    lab.click_session_row(&source, "jobseeker feedback");
    lab.wait_for_client_session("jobseeker feedback");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let panes = lab.tmux([
            "list-panes",
            "-t",
            "=jobseeker feedback:",
            "-F",
            "#{pane_id}\t#{pane_title}\t#{pane_dead}",
        ]);
        if let Some(pane) = panes.lines().find_map(|line| {
            let mut parts = line.split('\t');
            let pane = parts.next()?;
            let title = parts.next()?;
            let dead = parts.next()?;
            (title == "opensessions-sidebar" && dead == "0").then(|| pane.to_string())
        }) && lab.capture_pane(&pane).contains("sessions")
        {
            assert!(
                lab.tmux(["list-panes", "-a", "-F", "#{pane_id}"])
                    .lines()
                    .any(|pane| pane == source),
                "switching to a spaced session removed the source sidebar"
            );
            return;
        }
        sleep(Duration::from_millis(100));
    }
    panic!(
        "spaced session did not get a live sidebar; panes:\n{}\n\nlogs:\n{}",
        lab.tmux([
            "list-panes",
            "-t",
            "=jobseeker feedback:",
            "-F",
            "#{pane_id} title=#{pane_title} dead=#{pane_dead} status=#{pane_dead_status} command=#{pane_current_command}"
        ]),
        lab.logs(),
    );
}

#[test]
fn tmux_sidebar_reorders_worktree_group_as_block() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-reorder-worktree-group");
    let initial_order = lab.session_names();
    let first = position(&initial_order, "os-demo-feat-agent-panel")
        .expect("first worktree session should be present");
    let second = position(&initial_order, "os-demo-preview")
        .expect("second worktree session should be present");
    assert_eq!(
        second,
        first + 1,
        "worktree sessions should begin as one contiguous block: {initial_order:?}"
    );
    assert!(first > 0, "worktree block must have a preceding block");
    let mut expected_up = initial_order.clone();
    let members = expected_up.drain(first..=second).collect::<Vec<_>>();
    expected_up.splice(first - 1..first - 1, members);

    lab.reorder_worktree_group(-1);
    lab.wait_for_session_order(|names| names == expected_up);

    lab.reorder_worktree_group(1);
    lab.wait_for_session_order(|names| names == initial_order);
}

#[test]
fn tmux_sidebar_reorders_normal_session_across_worktree_group_boundary() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-reorder-worktree-boundary");
    lab.tmux_ok([
        "new-session",
        "-d",
        "-x",
        "160",
        "-y",
        "40",
        "-s",
        "z-normal",
        "-c",
        lab.root.join("opensessions").to_str().unwrap(),
        "sh",
    ]);

    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    assert_eq!(
        lab.active_pane(),
        source,
        "sidebar pane must be active before reorder focus setup"
    );
    lab.wait_for_session_order(|names| {
        position(names, "os-demo-preview").is_some_and(|preview| {
            position(names, "z-normal").is_some_and(|normal| normal > preview)
        })
    });

    lab.reorder_session("z-normal", -1);

    lab.wait_for_session_order(|names| {
        position(names, "z-normal").is_some_and(|normal| {
            position(names, "os-demo-feat-agent-panel")
                .is_some_and(|first_worktree| normal < first_worktree)
        })
    });

    lab.reorder_session("z-normal", 1);

    lab.wait_for_session_order(|names| {
        position(names, "os-demo-preview").is_some_and(|preview| {
            position(names, "z-normal").is_some_and(|normal| normal > preview)
        })
    });
}

#[test]
fn tmux_sidebar_rehomes_stale_focus_when_returning_to_session() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-rehome-return-focus");
    let second_window = lab.spawn_window_with_sidebar("opensessions", "second-sidebar");
    let source = lab.sidebar_pane("opensessions");
    let second = lab.sidebar_pane_in_window("opensessions", &second_window);
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains("▌"))
    });
    lab.wait_for_capture_pane(&second, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains("▌"))
    });

    lab.move_focus_off_active(&source, "opensessions");
    lab.move_focus_off_active(&second, "opensessions");
    let stale_capture = lab.capture_pane(&source);
    assert!(
        has_non_active_focus_marker(&stale_capture, "opensessions"),
        "test setup should leave a stale non-active focus row before switching away; got:\n{stale_capture}",
    );
    let second_stale_capture = lab.capture_pane(&second);
    assert!(
        has_non_active_focus_marker(&second_stale_capture, "opensessions"),
        "test setup should leave every opensessions sidebar with stale temporary focus; got:\n{second_stale_capture}",
    );

    lab.tmux_ok(["send-keys", "-t", source.as_str(), "1"]);
    lab.wait_for_client_session("effect-ts");
    let effect = lab.sidebar_pane("effect-ts");
    lab.tmux_ok(["select-pane", "-t", effect.as_str()]);
    lab.click_session_row(&effect, "opensessions");

    let first_visible = lab.first_capture_after_client_session("opensessions", &source);
    assert!(
        row_with(&first_visible, "opensessions").is_some_and(|row| row.contains("▌"))
            && !has_non_active_focus_marker(&first_visible, "opensessions"),
        "first visible opensessions sidebar frame after sidebar-driven switch must not show stale temporary focus; got:\n{first_visible}",
    );

    lab.wait_for_capture_pane(&source, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains("▌"))
            && !has_non_active_focus_marker(text, "opensessions")
    });
    lab.wait_for_capture_pane(&second, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains("▌"))
            && !has_non_active_focus_marker(text, "opensessions")
    });
}

#[test]
fn tmux_sidebar_tracks_agent_state_per_focused_pane() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-agent-pane-state");

    let seen_pane = lab.spawn_agent_pane("opensessions", "Seen - amp - focused");
    let unseen_pane = lab.spawn_agent_pane("opensessions", "Unseen - amp - background");
    let waiting_pane = lab.spawn_agent_pane("opensessions", "Approval - amp - waiting");
    assert_ne!(seen_pane, unseen_pane, "seen/background panes must differ");
    assert_ne!(seen_pane, waiting_pane, "seen/waiting panes must differ");
    assert_ne!(
        unseen_pane, waiting_pane,
        "background/waiting panes must differ"
    );
    sleep(Duration::from_millis(500));
    lab.restart_server();
    let sidebar = lab.sidebar_pane("opensessions");

    lab.post_agent_event("opensessions", "amp", "done", "seen-thread", &seen_pane);
    lab.post_agent_event("opensessions", "amp", "done", "unseen-thread", &unseen_pane);
    lab.post_agent_event(
        "opensessions",
        "amp",
        "waiting",
        "waiting-thread",
        &waiting_pane,
    );

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", seen_pane.as_str()]);
    sleep(Duration::from_millis(100));
    lab.focus_agent_pane("opensessions", "amp", "seen-thread", &seen_pane);

    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains("◉ ● ✓"))
    });
}

#[test]
fn tmux_sidebar_marks_done_event_seen_when_its_pane_is_already_focused() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-focused-pane-done-seen");

    let focused_pane = lab.spawn_agent_pane("opensessions", "Focused - amp - done");
    let background_pane = lab.spawn_agent_pane("opensessions", "Background - amp - done");
    assert_ne!(focused_pane, background_pane, "agent panes must differ");
    sleep(Duration::from_millis(500));
    lab.restart_server();
    let sidebar = lab.sidebar_pane("opensessions");

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", focused_pane.as_str()]);
    sleep(Duration::from_millis(250));

    lab.post_agent_event(
        "opensessions",
        "amp",
        "done",
        "focused-done-thread",
        &focused_pane,
    );
    lab.post_agent_event(
        "opensessions",
        "amp",
        "done",
        "background-done-thread",
        &background_pane,
    );

    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions").is_some_and(|row| {
            row.contains("● ✓") && !row.contains("● ●") && !row.contains("● ✓ +")
        })
    });
}

#[test]
fn tmux_sidebar_keeps_session_and_agent_panel_seen_state_in_sync_per_focused_pane() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-agent-seen-state-machine");

    let first_pane = lab.spawn_agent_pane("opensessions", "Agent One - amp - working");
    let second_pane = lab.spawn_agent_pane("opensessions", "Agent Two - amp - working");
    assert_ne!(first_pane, second_pane, "agent panes must differ");
    sleep(Duration::from_millis(500));
    lab.restart_server();
    let sidebar = lab.sidebar_pane("opensessions");

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", first_pane.as_str()]);
    sleep(Duration::from_millis(250));
    lab.post_watcher_like_agent_event(
        "opensessions",
        "amp",
        "running",
        "agent-one-thread",
        "Agent One",
    );
    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains('⠹'))
            && text.lines().any(|line| line.contains("⠹ Agent One"))
    });

    lab.tmux_ok(["select-pane", "-t", second_pane.as_str()]);
    sleep(Duration::from_millis(250));
    lab.post_watcher_like_agent_event(
        "opensessions",
        "amp",
        "running",
        "agent-two-thread",
        "Agent Two",
    );
    lab.wait_for_capture_pane(&sidebar, |text| {
        text.lines().any(|line| line.contains("⠹ Agent One"))
            && text.lines().any(|line| line.contains("⠹ Agent Two"))
    });

    lab.post_watcher_like_agent_event(
        "opensessions",
        "amp",
        "done",
        "agent-one-thread",
        "Agent One",
    );
    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains('●'))
            && text.lines().any(|line| line.contains("● Agent One"))
            && text.lines().any(|line| line.contains("⠹ Agent Two"))
    });

    lab.tmux_ok(["select-pane", "-t", first_pane.as_str()]);
    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions")
            .is_some_and(|row| row.contains('✓') && row.contains('⠹') && !row.contains('●'))
            && text.lines().any(|line| line.contains("✓ Agent One"))
            && text.lines().any(|line| line.contains("⠹ Agent Two"))
            && !text.lines().any(|line| line.contains("● Agent One"))
    });

    lab.post_watcher_like_agent_event(
        "opensessions",
        "amp",
        "done",
        "agent-two-thread",
        "Agent Two",
    );
    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions").is_some_and(|row| row.contains('●'))
            && text.lines().any(|line| line.contains("✓ Agent One"))
            && text.lines().any(|line| line.contains("● Agent Two"))
    });

    lab.tmux_ok(["select-pane", "-t", second_pane.as_str()]);
    lab.wait_for_capture_pane(&sidebar, |text| {
        row_with(text, "opensessions")
            .is_some_and(|row| row.contains('✓') && !row.contains('●') && !row.contains('⠹'))
            && text.lines().any(|line| line.contains("✓ Agent One"))
            && text.lines().any(|line| line.contains("✓ Agent Two"))
            && !text.lines().any(|line| line.contains("● Agent"))
    });
}

#[test]
fn tmux_sidebar_width_is_fixed_and_rejects_manual_sidebar_resize() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-width");
    lab.assert_width_hooks_are_well_quoted();
    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_all_sidebar_widths(36);
    sleep(Duration::from_millis(1500));

    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "40"]);
    sleep(Duration::from_millis(250));
    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "42"]);
    sleep(Duration::from_millis(250));
    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "1"]);

    lab.wait_for_all_sidebar_widths(36);
}

#[test]
fn tmux_sidebar_width_slider_and_mouse_drag_are_width_authors() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-width-slider");
    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.wait_for_all_sidebar_widths(36);
    lab.wait_for_capture_pane(&source, |text| text.contains("sessions"));

    lab.send_sidebar_key(&source, "w");
    lab.wait_for_capture_pane(&source, |text| text.contains("Sidebar width"));
    lab.send_sidebar_key(&source, "Right");
    lab.send_sidebar_key(&source, "Right");
    lab.wait_for_all_sidebar_widths(38);
    assert_eq!(
        lab.tmux(["show-option", "-gqv", "@opensessions_width"]),
        "38"
    );
    lab.send_sidebar_key(&source, "Enter");

    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "50"]);
    lab.wait_for_all_sidebar_widths(38);

    lab.send_sidebar_key(&source, "w");
    lab.wait_for_capture_pane(&source, |text| text.contains("Sidebar width"));
    for _ in 0..4 {
        lab.send_sidebar_key(&source, "H");
    }
    lab.wait_for_all_sidebar_widths(20);
    assert_eq!(
        lab.tmux(["show-option", "-gqv", "@opensessions_width"]),
        "20"
    );
    lab.send_sidebar_key(&source, "Enter");

    lab.send_sidebar_key(&source, "w");
    lab.wait_for_capture_pane(&source, |text| {
        text.contains("Sidebar width") && text.contains("20 columns")
    });
    lab.send_sidebar_key(&source, "Right");
    lab.wait_for_all_sidebar_widths(21);
    lab.send_sidebar_key(&source, "Esc");

    lab.wait_for_config_sidebar_width(21);

    lab.restart_server();
    lab.wait_for_all_sidebar_widths(21);
    assert_eq!(
        lab.tmux(["show-option", "-gqv", "@opensessions_width"]),
        "21"
    );

    let source = lab.sidebar_pane("opensessions");
    let mouse_binding = lab.tmux(["list-keys", "-T", "root", "MouseDrag1Border"]);
    assert!(
        mouse_binding.contains("@opensessions_mouse_resize_window")
            && mouse_binding.contains("/set-sidebar-width"),
        "OpenSessions did not extend the default mouse border binding:\n{mouse_binding}"
    );
    let window_id = lab.tmux([
        "display-message",
        "-p",
        "-t",
        source.as_str(),
        "#{window_id}",
    ]);
    lab.tmux_ok([
        "set-option",
        "-gq",
        "@opensessions_mouse_resize_window",
        window_id.as_str(),
    ]);
    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "43"]);
    sleep(Duration::from_millis(150));
    assert_eq!(
        lab.tmux([
            "display-message",
            "-p",
            "-t",
            source.as_str(),
            "#{pane_width}"
        ]),
        "43",
        "mouse resize transaction was repaired before it could be accepted"
    );
    post_body(
        lab.port,
        "/set-sidebar-width",
        "text/plain",
        "43",
        &lab.auth_token(),
    );
    lab.tmux_ok(["set-option", "-gu", "@opensessions_mouse_resize_window"]);
    lab.wait_for_all_sidebar_widths(43);
    lab.wait_for_config_sidebar_width(43);

    lab.restart_server();
    lab.wait_for_all_sidebar_widths(43);
}

#[test]
fn tmux_sidebar_quit_closes_the_server_and_every_sidebar_client() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-quit");
    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.wait_for_capture_pane(&source, |text| text.contains("sessions"));

    lab.send_sidebar_key(&source, "q");

    lab.wait_for_server_exit();
    lab.wait_for_no_sidebar_processes();
}

#[test]
fn tmux_sidebar_server_exits_when_its_tmux_namespace_disappears() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-missing-tmux");

    lab.tmux_ok(["kill-server"]);

    lab.wait_for_server_exit();
}

#[test]
fn tmux_sidebar_stays_closed_across_session_and_window_switch_hooks() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-open-close");
    lab.wait_for_all_sidebar_widths(36);

    post_hook(lab.port, "/toggle", &lab.auth_token());
    lab.wait_for_no_sidebar_processes();

    lab.tmux_ok(["switch-client", "-t", "effect-ts"]);
    sleep(Duration::from_millis(800));
    lab.assert_no_sidebar_panes("closed sidebar must not respawn after session switch");

    lab.tmux_ok(["new-window", "-d", "-t", "effect-ts", "sh"]);
    lab.tmux_ok(["select-window", "-t", "effect-ts:1"]);
    sleep(Duration::from_millis(800));
    lab.assert_no_sidebar_panes("closed sidebar must not respawn after window switch");
}

#[test]
fn tmux_sidebar_multiple_clients_keep_independent_active_rows() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-multiclient");
    lab.spawn_attached_client_for("effect-ts");
    lab.wait_for_client_sessions(["opensessions", "effect-ts"]);

    let opensessions_pane = lab.sidebar_pane("opensessions");
    let effect_pane = lab.sidebar_pane("effect-ts");
    lab.wait_for_capture_pane(&opensessions_pane, |capture| {
        row_with(capture, "opensessions").is_some_and(|row| row.contains("▌"))
    });
    lab.wait_for_capture_pane(&effect_pane, |capture| {
        row_with(capture, "effect-ts").is_some_and(|row| row.contains("▌"))
    });
    let opensessions = lab.capture_pane(&opensessions_pane);
    let effect = lab.capture_pane(&effect_pane);

    assert_active_row(&opensessions, "opensessions");
    assert_active_row(&effect, "effect-ts");
}

#[test]
fn tmux_sidebar_state_is_isolated_per_tmux_socket() {
    let _guard = e2e_serial_guard();
    let lab_a = started_lab("opensessions-e2e-socket-a");
    let lab_b = started_lab("opensessions-e2e-socket-b");

    let source = lab_a.sidebar_pane("opensessions");
    lab_a.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab_a.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab_a.wait_for_all_sidebar_widths(36);
    lab_b.wait_for_all_sidebar_widths(36);
    sleep(Duration::from_millis(1500));

    lab_a.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "40"]);
    sleep(Duration::from_millis(250));
    lab_a.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "42"]);

    lab_a.wait_for_all_sidebar_widths(36);
    lab_b.wait_for_all_sidebar_widths(36);
    assert_ne!(
        lab_a.port, lab_b.port,
        "isolated servers must use distinct ports"
    );
}

#[test]
fn tmux_sidebar_q_in_main_pane_does_not_quit_opensessions() {
    let _guard = e2e_serial_guard();
    let mut lab = started_lab("opensessions-e2e-q-main-pane");
    let main = lab.main_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", main.as_str()]);
    lab.tmux_ok(["send-keys", "-t", main.as_str(), "q"]);
    sleep(Duration::from_millis(700));

    assert!(
        lab.server_is_running(),
        "server exited after q in main pane"
    );
    assert_eq!(lab.sidebar_panes().len(), SIDEBAR_SESSIONS.len());
}

#[test]
fn tmux_sidebar_pane_exit_does_not_steal_sidebar_width() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-pane-exit");
    let sidebar = lab.sidebar_pane("opensessions");
    let main = lab.main_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", sidebar.as_str()]);
    lab.wait_for_all_sidebar_widths(36);

    lab.tmux_ok(["split-window", "-h", "-t", main.as_str(), "sh"]);
    lab.wait_for_non_sidebar_pane_count("opensessions", 2);
    lab.tmux_ok(["kill-pane", "-t", main.as_str()]);

    lab.wait_for_non_sidebar_pane_count("opensessions", 1);
    lab.wait_for_all_sidebar_widths(36);
}

#[test]
fn tmux_sidebar_closes_window_cleanly_when_last_content_pane_exits() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-last-pane-exit");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    let first_window = lab.current_window_index("opensessions");
    let second_window = lab.spawn_window_with_sidebar("opensessions", "scratch");
    lab.tmux_ok([
        "switch-client",
        "-t",
        &format!("opensessions:{second_window}"),
    ]);
    lab.wait_for_active_window("opensessions", &second_window);

    let main = lab.main_pane_in_window("opensessions", &second_window);
    lab.tmux_ok(["kill-pane", "-t", main.as_str()]);

    lab.wait_for_window_absent("opensessions", &second_window);
    lab.wait_for_active_window("opensessions", &first_window);
    assert!(
        !lab.logs().contains("resize-pane") && !lab.logs().contains("returned 1"),
        "last content pane exit should not surface resize-hook failures; logs:\n{}",
        lab.logs(),
    );
}

#[test]
fn tmux_sidebar_bulk_window_cleanup_keeps_active_window() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-window-cleanup");

    let active_window = lab.current_window_index("opensessions");
    let first_cleanup = lab.spawn_window_with_sidebar("opensessions", "cleanup-one");
    let second_cleanup = lab.spawn_window_with_sidebar("opensessions", "cleanup-two");
    let source = lab.sidebar_pane_in_window("opensessions", &active_window);
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok([
        "select-window",
        "-t",
        &format!("opensessions:{active_window}"),
    ]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_active_window("opensessions", &active_window);

    lab.send_sidebar_key(&source, "W");
    lab.wait_for_capture_pane(&source, |text| {
        text.contains("Windows")
            && text.contains("cleanup-one")
            && text.contains("cleanup-two")
            && text.contains("● 0")
    });
    lab.send_sidebar_key(&source, "Down");
    lab.send_sidebar_key(&source, "Enter");
    lab.wait_for_active_window("opensessions", &first_cleanup);

    let cleanup_sidebar = lab.sidebar_pane_in_window("opensessions", &first_cleanup);
    lab.send_sidebar_key(&cleanup_sidebar, "W");
    lab.wait_for_capture_pane(&cleanup_sidebar, |text| {
        text.contains("Windows") && text.contains("● 1")
    });
    lab.send_sidebar_key(&cleanup_sidebar, "Up");
    lab.send_sidebar_key(&cleanup_sidebar, "Right");
    lab.send_sidebar_key(&cleanup_sidebar, "Down");
    lab.send_sidebar_key(&cleanup_sidebar, "Down");
    lab.send_sidebar_key(&cleanup_sidebar, "Right");
    lab.send_sidebar_key(&cleanup_sidebar, "Enter");
    lab.wait_for_capture_pane(&cleanup_sidebar, |text| {
        text.contains("Close 2 windows?") && text.contains("Processes in these windows")
    });
    lab.send_sidebar_key(&cleanup_sidebar, "Enter");

    lab.wait_for_window_absent("opensessions", &active_window);
    lab.wait_for_window_absent("opensessions", &second_cleanup);
    lab.wait_for_active_window("opensessions", &first_cleanup);
}

#[test]
fn tmux_sidebar_closes_session_cleanly_when_only_content_shell_exits() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-last-shell-exit");
    lab.tmux_ok(["switch-client", "-t", "effect-ts"]);
    lab.wait_for_client_session("effect-ts");
    let session_names = lab.session_names();
    let effect_index = session_names
        .iter()
        .position(|name| name == "effect-ts")
        .unwrap_or_else(|| panic!("effect-ts missing from display sessions: {session_names:?}"));
    let expected_fallback = effect_index
        .checked_sub(1)
        .and_then(|index| session_names.get(index))
        .or_else(|| session_names.get(effect_index + 1))
        .cloned()
        .expect("effect-ts should have a fallback display session");

    let main = lab.main_pane("effect-ts");
    lab.tmux_ok(["send-keys", "-t", main.as_str(), "exit", "Enter"]);

    lab.wait_for_session_absent_without_sidebar_expansion("effect-ts", 36);
    lab.wait_for_client_session(&expected_fallback);
    assert!(
        !lab.logs().contains("resize-pane") && !lab.logs().contains("returned 1"),
        "last content shell exit should not surface resize-hook failures; logs:\n{}",
        lab.logs(),
    );
}

#[test]
fn tmux_sidebar_width_survives_flat_three_pane_layout_churn() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-flat-pane-churn");
    let sidebar = lab.sidebar_pane("opensessions");
    let main = lab.main_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", sidebar.as_str()]);
    lab.wait_for_all_sidebar_widths(36);

    lab.tmux_ok(["split-window", "-h", "-t", main.as_str(), "sh"]);
    lab.wait_for_non_sidebar_pane_count("opensessions", 2);
    lab.tmux_ok(["select-layout", "-t", "opensessions", "even-horizontal"]);
    lab.wait_for_sidebar_width("opensessions", 36);

    let content_panes = lab.non_sidebar_panes("opensessions");
    assert_eq!(
        content_panes.len(),
        2,
        "expected sidebar | pane1 | pane2 before churn; got {content_panes:?}"
    );
    lab.tmux_ok(["kill-pane", "-t", content_panes[0].as_str()]);
    let repair_started = Instant::now();
    lab.wait_for_sidebar_width("opensessions", 36);
    let repair_elapsed = repair_started.elapsed();
    assert!(
        repair_elapsed < Duration::from_secs(2),
        "sidebar width repair after killing pane1 took {repair_elapsed:?}; panes={:?}\nwidth_option={}\nhooks:\n{}\nlogs:\n{}",
        lab.sidebar_panes(),
        lab.tmux(["show-option", "-gqv", "@opensessions_width"]),
        lab.tmux(["show-hooks", "-g"]),
        lab.logs(),
    );

    lab.wait_for_non_sidebar_pane_count("opensessions", 1);
    lab.wait_for_all_sidebar_widths(36);
}

#[test]
fn tmux_sidebar_preserves_even_content_panes_when_returning_to_stale_session() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-stale-three-pane-layout");
    let target = "effect-ts";
    let main = lab.main_pane(target);

    lab.tmux_ok(["switch-client", "-t", target]);
    lab.wait_for_client_session(target);
    lab.tmux_ok(["split-window", "-h", "-t", main.as_str(), "sh"]);
    lab.wait_for_non_sidebar_pane_count(target, 2);
    lab.tmux_ok(["select-layout", "-t", target, "even-horizontal"]);
    lab.wait_for_sidebar_width(target, 36);

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.wait_for_client_session("opensessions");
    lab.tmux_ok(["resize-window", "-t", target, "-x", "100", "-y", "40"]);
    lab.tmux_ok(["set-window-option", "-t", target, "window-size", "latest"]);

    lab.tmux_ok(["switch-client", "-t", target]);
    lab.wait_for_client_session(target);
    lab.wait_for_sidebar_width(target, 36);

    let widths = lab
        .tmux([
            "list-panes",
            "-t",
            &exact_session_target(target),
            "-f",
            "#{!=:#{pane_title},opensessions-sidebar}",
            "-F",
            "#{pane_width}",
        ])
        .lines()
        .map(|width| width.parse::<u16>().expect("content pane width"))
        .collect::<Vec<_>>();
    assert_eq!(
        widths.len(),
        2,
        "expected two content panes; got {widths:?}"
    );
    assert!(
        widths[0].abs_diff(widths[1]) <= 2,
        "returning to a stale session must preserve evenly split content panes; widths={widths:?}\nlayout={}\nlogs:\n{}",
        lab.tmux(["display-message", "-p", "-t", target, "#{window_layout}"]),
        lab.logs(),
    );
}

#[test]
fn tmux_sidebar_client_resize_never_persists_a_smaller_sidebar_width() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-client-resize-fixed-width");
    let sidebar = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", sidebar.as_str()]);
    lab.wait_for_all_sidebar_widths(36);

    lab.tmux_ok([
        "resize-window",
        "-t",
        "opensessions",
        "-x",
        "80",
        "-y",
        "40",
    ]);
    sleep(Duration::from_millis(600));
    lab.tmux_ok([
        "resize-window",
        "-t",
        "opensessions",
        "-x",
        "160",
        "-y",
        "40",
    ]);
    lab.tmux_ok([
        "set-window-option",
        "-t",
        "opensessions",
        "window-size",
        "latest",
    ]);

    lab.wait_for_sidebar_width("opensessions", 36);
    lab.wait_for_all_sidebar_widths(36);
}

#[test]
fn tmux_sidebar_switch_stays_responsive_with_100_connected_clients() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("opensessions-e2e-100-clients");
    let source = lab.sidebar_pane("opensessions");
    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    lab.wait_for_client_session("opensessions");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("build e2e tokio runtime");

    runtime.block_on(async {
        let mut clients = Vec::new();
        for index in 0..100 {
            let ws = opensessions_sidebar::client::connect_ws_path_with_token(
                "127.0.0.1",
                lab.port,
                "/",
                &lab.auth_token(),
            )
            .await
            .unwrap_or_else(|err| panic!("connect passive ws client {index}: {err}"));
            clients.push(ws);
        }

        for _ in 0..25 {
            post_refresh(lab.port, &lab.auth_token());
        }

        let started = Instant::now();
        lab.tmux_ok(["send-keys", "-t", source.as_str(), "C-i"]);
        lab.wait_for_client_session("os-demo-feat-agent-panel");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "switch took {elapsed:?} with 100 connected sidebar clients"
        );

        drop(clients);
    });
}

#[test]
fn tmux_sidebar_server_stays_responsive_while_opening_many_windows() {
    let _guard = e2e_serial_guard();
    let mut lab = Lab::new("opensessions-e2e-sidebar-stampede");
    lab.setup_repos();
    lab.setup_tmux();
    for index in 0..15 {
        let session = format!("stampede-{index}");
        lab.tmux_ok([
            "new-session",
            "-d",
            "-x",
            "160",
            "-y",
            "40",
            "-s",
            &session,
            "-c",
            lab.root.join("opensessions").to_str().unwrap(),
            "sh",
        ]);
    }
    lab.start_server();

    let port = lab.port;
    let token = lab.auth_token();
    let toggle = thread::spawn(move || post_hook(port, "/toggle", &token));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && lab.sidebar_panes().is_empty() {
        sleep(Duration::from_millis(10));
    }
    assert!(
        !lab.sidebar_panes().is_empty(),
        "toggle never began spawning sidebars; logs:\n{}",
        lab.logs()
    );

    let started = Instant::now();
    let response = get_liveness(lab.port);
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "server did not answer liveness while sidebars connected: {response:?}\nlogs:\n{}",
        lab.logs()
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "server liveness was blocked for {:?} by the sidebar connection burst\nlogs:\n{}",
        started.elapsed(),
        lab.logs()
    );

    toggle.join().expect("toggle request");
    let expected_sidebars = SIDEBAR_SESSIONS.len() + 15;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && lab.sidebar_panes().len() < expected_sidebars {
        sleep(Duration::from_millis(100));
    }
    assert_eq!(
        lab.sidebar_panes().len(),
        expected_sidebars,
        "not every window retained a live sidebar\nlogs:\n{}",
        lab.logs()
    );
}

#[test]
fn tmux_sidebar_switch_latency_during_width_repair_probe() {
    let _guard = e2e_serial_guard();
    let lab = started_lab("os-e2e-latency");
    let source = lab.sidebar_pane("opensessions");
    lab.wait_for_all_sidebar_widths(36);
    sleep(Duration::from_millis(1500));

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    let baseline = lab.measure_tab_switch("opensessions", &source);

    lab.tmux_ok(["switch-client", "-t", "opensessions"]);
    lab.wait_for_client_session("opensessions");
    lab.tmux_ok(["select-pane", "-t", source.as_str()]);
    sleep(Duration::from_millis(300));
    lab.tmux_ok(["resize-pane", "-t", source.as_str(), "-x", "42"]);
    lab.wait_for_all_sidebar_widths(36);
    let during_resize = lab.measure_tab_switch("opensessions", &source);

    eprintln!(
        "switch latency probe: sidebars={} baseline_ms={} during_resize_ms={}",
        lab.sidebar_panes().len(),
        baseline.as_millis(),
        during_resize.as_millis(),
    );

    assert!(
        during_resize <= baseline + Duration::from_millis(250),
        "switch during width repair should stay close to baseline; baseline={baseline:?} during_resize={during_resize:?} panes={:?}\nlogs:\n{}",
        lab.sidebar_panes(),
        lab.logs(),
    );
}

fn started_lab(prefix: &str) -> Lab {
    Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux is required for product E2E tests");
    Command::new("python3")
        .arg("--version")
        .output()
        .expect("python3 is required for product E2E tests");
    Command::new("git")
        .arg("--version")
        .output()
        .expect("git is required for product E2E tests");

    let mut lab = Lab::new(prefix);
    lab.setup_repos();
    lab.setup_tmux();
    lab.start_server();
    lab.spawn_sidebars();
    lab
}

fn e2e_serial_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn row_with<'a>(text: &'a str, needle: &str) -> Option<&'a str> {
    text.lines().find(|line| line.contains(needle))
}

fn row_index(text: &str, needle: &str) -> Option<usize> {
    text.lines().position(|line| line.contains(needle))
}

fn position(names: &[String], needle: &str) -> Option<usize> {
    names.iter().position(|name| name == needle)
}

fn has_non_active_focus_marker(text: &str, active_session: &str) -> bool {
    text.lines()
        .any(|line| line.contains("›") && !line.contains(active_session))
}

fn assert_worktree_group_columns(text: &str) {
    let lines = text.lines().collect::<Vec<_>>();
    let group = row_with(text, "os-demo-worktrees")
        .unwrap_or_else(|| panic!("missing worktree group row:\n{text}"));
    assert!(
        group.starts_with("  ▾ os-demo-worktrees")
            || group.starts_with("› ▾ os-demo-worktrees")
            || group.starts_with("  ▸ os-demo-worktrees")
            || group.starts_with("› ▸ os-demo-worktrees"),
        "worktree group root should align with top-level session rows and not include a leading status glyph; row={group:?}\n{text}",
    );
    assert!(
        !group.contains("▾    ")
            && !group.contains("▸    ")
            && !group.contains("○ os-demo-worktrees"),
        "worktree group root should not use the old shifted/status-prefixed layout; row={group:?}\n{text}",
    );

    let feat_idx = row_index(text, "os-demo-feat-agent-panel")
        .unwrap_or_else(|| panic!("missing feat-agent-panel child row:\n{text}"));
    let feat_branch = lines
        .get(feat_idx + 1)
        .unwrap_or_else(|| panic!("missing feat-agent-panel branch row:\n{text}"));
    assert!(
        feat_branch.starts_with("  │    feat-agent-panel"),
        "middle child branch should align under the child session name column; row={feat_branch:?}\n{text}",
    );

    let preview_idx = row_index(text, "os-demo-preview")
        .unwrap_or_else(|| panic!("missing preview child row:\n{text}"));
    let preview_branch = lines
        .get(preview_idx + 1)
        .unwrap_or_else(|| panic!("missing preview branch row:\n{text}"));
    assert!(
        preview_branch.starts_with("       preview"),
        "last child branch should align under the child session name column without a dangling rail; row={preview_branch:?}\n{text}",
    );
}

fn post_refresh(port: u16, token: &str) {
    post_hook(port, "/refresh", token);
}

fn get_liveness(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect liveness");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set liveness timeout");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .expect("write liveness");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read liveness");
    response
}

fn post_hook(port: u16, path: &str, token: &str) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect /refresh");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("write /refresh");
    let mut response = [0; 128];
    let _ = stream.read(&mut response);
}

fn post_body(port: u16, path: &str, content_type: &str, body: &str, token: &str) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect post body");
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write post body");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.1 204"),
        "unexpected response for {path}: {response}"
    );
}

fn assert_active_row(capture: &str, session: &str) {
    assert!(
        row_with(capture, session).is_some_and(|row| row.contains("▌")),
        "expected {session} to be the active row; got:\n{capture}",
    );
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral e2e port")
        .local_addr()
        .expect("read ephemeral e2e port")
        .port()
}

struct Lab {
    socket: String,
    root: PathBuf,
    port: u16,
    server: Option<Child>,
    clients: Vec<Child>,
}

impl Lab {
    fn new(prefix: &str) -> Self {
        static LAB_ID: AtomicU64 = AtomicU64::new(1);
        let id = LAB_ID.fetch_add(1, Ordering::Relaxed);
        let unique = format!("{}-{}-{}", prefix, std::process::id(), id,);
        let root = std::env::temp_dir().join(&unique);
        fs::create_dir_all(&root).expect("create e2e root");
        let config_dir = root.join("home/.config/opensessions");
        fs::create_dir_all(&config_dir).expect("create e2e config dir");
        fs::write(
            config_dir.join("config.json"),
            format!("{{\"plugins\":[],\"sidebarWidth\":{W}}}\n"),
        )
        .expect("write e2e config");
        Self {
            socket: unique,
            root,
            port: free_port(),
            server: None,
            clients: Vec::new(),
        }
    }

    fn setup_repos(&self) {
        for name in ["opensessions", "effect-ts", "lazydiff"] {
            let dir = self.root.join(name);
            fs::create_dir_all(&dir).expect("create fake repo dir");
            self.git(&dir, ["init", "-q"]);
            self.git(&dir, ["config", "user.email", "e2e@example.com"]);
            self.git(&dir, ["config", "user.name", "OpenSessions E2E"]);
            fs::write(dir.join("README.md"), format!("{name}\n")).expect("write readme");
            self.git(&dir, ["add", "README.md"]);
            self.git(&dir, ["commit", "-q", "-m", "init"]);
        }

        let base = self.root.join("os-demo-base");
        fs::create_dir_all(&base).expect("create worktree base");
        self.git(&base, ["init", "-q"]);
        self.git(&base, ["config", "user.email", "e2e@example.com"]);
        self.git(&base, ["config", "user.name", "OpenSessions E2E"]);
        fs::write(base.join("README.md"), "os-demo\n").expect("write worktree readme");
        self.git(&base, ["add", "README.md"]);
        self.git(&base, ["commit", "-q", "-m", "init"]);
        self.git(&base, ["branch", "feat-agent-panel"]);
        self.git(&base, ["branch", "preview"]);
        fs::create_dir_all(self.root.join("os-demo-worktrees")).expect("create worktrees dir");
        self.git(
            &base,
            [
                "worktree",
                "add",
                "-q",
                self.root
                    .join("os-demo-worktrees/feat-agent-panel")
                    .to_str()
                    .unwrap(),
                "feat-agent-panel",
            ],
        );
        self.git(
            &base,
            [
                "worktree",
                "add",
                "-q",
                self.root
                    .join("os-demo-worktrees/preview")
                    .to_str()
                    .unwrap(),
                "preview",
            ],
        );
    }

    fn token_file(&self) -> PathBuf {
        self.root.join("server.token")
    }

    fn auth_token(&self) -> String {
        fs::read_to_string(self.token_file())
            .expect("read e2e server token")
            .trim()
            .to_string()
    }

    fn setup_tmux(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
        for (session, dir) in [
            ("opensessions", self.root.join("opensessions")),
            ("effect-ts", self.root.join("effect-ts")),
            ("lazydiff", self.root.join("lazydiff")),
            (
                "os-demo-feat-agent-panel",
                self.root.join("os-demo-worktrees/feat-agent-panel"),
            ),
            (
                "os-demo-preview",
                self.root.join("os-demo-worktrees/preview"),
            ),
        ] {
            self.tmux_ok([
                "new-session",
                "-d",
                "-x",
                "160",
                "-y",
                "40",
                "-s",
                session,
                "-c",
                dir.to_str().unwrap(),
                "sh",
            ]);
        }

        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        self.tmux_ok([
            "set-environment",
            "-g",
            "OPENSESSIONS_DIR",
            workspace_root.to_str().unwrap(),
        ]);
        self.tmux_ok([
            "set-environment",
            "-g",
            "OPENSESSIONS_PORT",
            &self.port.to_string(),
        ]);
        self.tmux_ok([
            "set-environment",
            "-g",
            "OPENSESSIONS_PID_FILE",
            self.root.join("server.pid").to_str().unwrap(),
        ]);
        self.spawn_attached_client_for("opensessions");
        self.wait_for_client_session("opensessions");
        self.tmux_ok([
            "set-environment",
            "-g",
            "OPENSESSIONS_DIR",
            self.project_root().to_str().unwrap(),
        ]);
        for (name, value) in [
            ("OPENSESSIONS_HOST", "127.0.0.1".to_string()),
            ("OPENSESSIONS_PORT", self.port.to_string()),
            (
                "OPENSESSIONS_TOKEN_FILE",
                self.token_file().to_string_lossy().into_owned(),
            ),
            (
                "OPENSESSIONS_DEBUG_LOG",
                self.root.join("debug.log").to_string_lossy().into_owned(),
            ),
        ] {
            self.tmux_ok(["set-environment", "-g", name, &value]);
        }
        // If the Rust test process is interrupted before `Drop`, the Python
        // client notices that its parent vanished and closes its pty. Tmux can
        // then exit, and the opensessions server detects the unavailable socket.
        self.tmux_ok(["set-option", "-g", "exit-unattached", "on"]);
    }

    fn spawn_attached_client_for(&mut self, session: &str) {
        let child = self.spawn_attached_client(session);
        self.clients.push(child);
    }

    fn spawn_attached_client(&self, session: &str) -> Child {
        let script = r#"
import fcntl, os, pty, struct, sys, termios, time

socket = sys.argv[1]
session = sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp("tmux", ["tmux", "-L", socket, "attach-session", "-t", session])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
parent = os.getppid()
for _ in range(3000):
    if os.getppid() != parent:
        break
    time.sleep(0.1)
"#;
        Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&self.socket)
            .arg(session)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                File::create(self.root.join("tmux-client.stderr.log")).expect("client stderr log"),
            )
            .spawn()
            .expect("spawn attached tmux client through script")
    }

    fn start_server(&mut self) {
        let server = self.server_bin();
        let tmux_env = self.tmux_socket_env();
        let child = Command::new(server)
            .env("TMUX", tmux_env)
            .env("HOME", self.home_dir())
            .env("OPENSESSIONS_WIDTH", "35")
            .env("OPENSESSIONS_HOST", "127.0.0.1")
            .env("OPENSESSIONS_PORT", self.port.to_string())
            .env(
                "OPENSESSIONS_DEBUG_LOG",
                self.root.join("debug.log").to_str().unwrap(),
            )
            .env(
                "OPENSESSIONS_PID_FILE",
                self.root.join("server.pid").to_str().unwrap(),
            )
            .env("OPENSESSIONS_TOKEN_FILE", self.token_file())
            .stdout(File::create(self.root.join("server.stdout.log")).expect("server stdout log"))
            .stderr(File::create(self.root.join("server.stderr.log")).expect("server stderr log"))
            .spawn()
            .expect("start opensessions server");
        self.server = Some(child);
        self.wait_for_server();
    }

    fn restart_server(&mut self) {
        if self.server_is_running() {
            post_hook(self.port, "/quit", &self.auth_token());
            self.wait_for_server_exit();
        }
        self.wait_for_no_sidebar_processes();
        self.start_server();
        self.spawn_sidebars();
    }

    fn wait_for_server(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let listening = TcpStream::connect(("127.0.0.1", self.port)).is_ok();
            let token_ready =
                fs::read_to_string(self.token_file()).is_ok_and(|token| !token.trim().is_empty());
            if listening && token_ready {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!("server did not become ready; logs:\n{}", self.logs());
    }

    fn spawn_sidebars(&self) {
        post_hook(self.port, "/toggle", &self.auth_token());
        self.wait_for_sidebar_pane_count(SIDEBAR_SESSIONS.len());
        self.wait_for_sidebar_connections();
    }

    fn spawn_window_with_sidebar(&self, session: &str, window_name: &str) -> String {
        let next_index = self.next_window_index(session);
        self.tmux_ok([
            "new-window",
            "-d",
            "-t",
            &format!("{session}:{next_index}"),
            "-n",
            window_name,
            "sh",
        ]);
        let window_index = self.tmux([
            "display-message",
            "-p",
            "-t",
            &format!("{session}:{window_name}"),
            "#{window_index}",
        ]);
        let sidebar = self.sidebar_bin();
        let command = format!(
            "env OPENSESSIONS_HOST=127.0.0.1 OPENSESSIONS_PORT={} OPENSESSIONS_TOKEN_FILE={} OPENSESSIONS_DEBUG_LOG={} {} 2>{}",
            self.port,
            shell_quote(&self.token_file().to_string_lossy()),
            shell_quote(&self.root.join("debug.log").to_string_lossy()),
            sidebar.display(),
            shell_quote(
                &self
                    .root
                    .join(format!("sidebar-{session}-{window_name}.stderr.log"))
                    .to_string_lossy()
            ),
        );
        let pane = self.tmux([
            "split-window",
            "-h",
            "-b",
            "-l",
            W,
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &format!("{session}:{window_index}"),
            &command,
        ]);
        self.tmux_ok([
            "select-pane",
            "-t",
            pane.as_str(),
            "-T",
            "opensessions-sidebar",
        ]);
        self.wait_for_capture_pane(&pane, |text| text.contains("opensessions"));
        window_index
    }

    fn spawn_agent_pane(&self, session: &str, title: &str) -> String {
        let command = format!(
            "sh -c 'printf \"\\033]2;{}\\033\\\\\"; while :; do sleep 60; done'",
            title.replace('"', "")
        );
        let pane = self.tmux([
            "split-window",
            "-h",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            session,
            &command,
        ]);
        pane
    }

    fn post_agent_event(
        &self,
        session: &str,
        agent: &str,
        status: &str,
        thread_id: &str,
        pane_id: &str,
    ) {
        let body = serde_json::json!({
            "agent": agent,
            "status": status,
            "tmuxSession": session,
            "threadId": thread_id,
            "threadName": thread_id,
            "paneId": pane_id,
            "ts": 1,
        })
        .to_string();
        post_body(
            self.port,
            "/api/agent-event",
            "application/json",
            &body,
            &self.auth_token(),
        );
    }

    fn post_watcher_like_agent_event(
        &self,
        session: &str,
        agent: &str,
        status: &str,
        thread_id: &str,
        thread_name: &str,
    ) {
        let body = serde_json::json!({
            "agent": agent,
            "status": status,
            "tmuxSession": session,
            "threadId": thread_id,
            "threadName": thread_name,
            "ts": 1,
        })
        .to_string();
        post_body(
            self.port,
            "/api/agent-event",
            "application/json",
            &body,
            &self.auth_token(),
        );
    }

    fn focus_agent_pane(&self, session: &str, agent: &str, thread_id: &str, pane_id: &str) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("build e2e tokio runtime");

        runtime.block_on(async {
            let mut ws = opensessions_sidebar::client::connect_ws_path_with_token(
                "127.0.0.1",
                self.port,
                "/",
                &self.auth_token(),
            )
            .await
            .expect("connect focus-agent ws client");
            let _ = ws.next().await.expect("read ws hello").expect("ws hello");
            let _ = ws
                .next()
                .await
                .expect("read ws initial state")
                .expect("ws initial state");
            let command = serde_json::json!({
                "type": "focus-agent-pane",
                "session": session,
                "agent": agent,
                "threadId": thread_id,
                "paneId": pane_id,
            });
            ws.send(Message::text(command.to_string()))
                .await
                .expect("send focus-agent-pane command");
            ws.close().await.expect("close focus-agent ws client");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
    }

    fn wait_for_text(&self, session: &str, text: &str) {
        self.wait_for_capture(session, |capture| capture.contains(text));
    }

    fn wait_for_capture<F>(&self, session: &str, predicate: F)
    where
        F: Fn(&str) -> bool,
    {
        let pane = self.sidebar_pane(session);
        self.wait_for_capture_pane(&pane, predicate);
    }

    fn wait_for_capture_pane<F>(&self, pane: &str, predicate: F)
    where
        F: Fn(&str) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let capture = self.capture_pane(pane);
            if predicate(&capture) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for pane {pane}; last capture:\n{}\n\npanes:\n{}\n\nlogs:\n{}",
            self.capture_pane(pane),
            self.tmux(["list-panes", "-a", "-F", "#{session_name} #{pane_id} #{pane_width}x#{pane_height} command=#{pane_current_command} dead=#{pane_dead} status=#{pane_dead_status}"]),
            self.logs(),
        );
    }

    fn wait_for_client_session(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.tmux(["list-clients", "-F", "#{client_session}"]);
            if output.lines().any(|line| line.trim() == expected) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for client session {expected}; clients:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-clients",
                "-F",
                "#{client_name} #{client_tty} #{client_session}"
            ]),
            self.logs(),
        );
    }

    fn wait_for_active_window(&self, session: &str, expected_window: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let target = exact_session_target(session);
        while Instant::now() < deadline {
            let output = self.tmux([
                "list-windows",
                "-t",
                &target,
                "-F",
                "#{window_index}\t#{window_active}",
            ]);
            if output.lines().any(|line| {
                let mut parts = line.split('\t');
                matches!(
                    (parts.next(), parts.next()),
                    (Some(window_index), Some("1")) if window_index == expected_window
                )
            }) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for active window {session}:{expected_window}; clients:\n{}\n\nwindows:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-clients",
                "-F",
                "#{client_name} #{client_tty} #{client_session}"
            ]),
            self.tmux([
                "list-windows",
                "-t",
                &target,
                "-F",
                "#{window_index} #{window_name} active=#{window_active}"
            ]),
            self.logs(),
        );
    }

    fn first_capture_after_client_session(&self, expected: &str, pane: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.tmux(["list-clients", "-F", "#{client_session}"]);
            if output.lines().any(|line| line.trim() == expected) {
                return self.capture_pane(pane);
            }
            sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for client session {expected}; clients:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-clients",
                "-F",
                "#{client_name} #{client_tty} #{client_session}"
            ]),
            self.logs(),
        );
    }

    fn wait_for_client_sessions<const N: usize>(&self, expected: [&str; N]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.tmux(["list-clients", "-F", "#{client_session}"]);
            if expected
                .iter()
                .all(|expected| output.lines().any(|line| line.trim() == *expected))
            {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for client sessions {expected:?}; clients:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-clients",
                "-F",
                "#{client_name} #{client_tty} #{client_session}"
            ]),
            self.logs(),
        );
    }

    fn wait_for_all_sidebar_widths(&self, expected: u16) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let panes = self.sidebar_panes();
            if panes.len() >= SIDEBAR_SESSIONS.len()
                && panes.iter().all(|pane| pane.width == expected)
            {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for all sidebar widths to be {expected}; panes={:?}\nlogs:\n{}",
            self.sidebar_panes(),
            self.logs(),
        );
    }

    fn wait_for_config_sidebar_width(&self, expected: u16) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let config = fs::read_to_string(self.config_path()).unwrap_or_default();
            if config.contains(&format!("\"sidebarWidth\": {expected}"))
                || config.contains(&format!("\"sidebarWidth\":{expected}"))
            {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        let config = fs::read_to_string(self.config_path()).unwrap_or_default();
        panic!("timed out waiting for sidebarWidth={expected}; config={config}");
    }

    fn wait_for_sidebar_width(&self, session: &str, expected: u16) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if self.sidebar_width(session) == Some(expected) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for {session} sidebar width to be {expected}; panes={:?}\nlogs:\n{}",
            self.sidebar_panes(),
            self.logs(),
        );
    }

    fn wait_for_session_absent(&self, session: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = Vec::new();
        while Instant::now() < deadline {
            last = self.session_names();
            if !last.iter().any(|name| name == session) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for session {session} to disappear; last={last:?}\nlogs:\n{}",
            self.logs(),
        );
    }

    fn wait_for_session_absent_without_sidebar_expansion(&self, session: &str, max_width: u16) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_sessions = Vec::new();
        while Instant::now() < deadline {
            let sidebars = self.sidebar_panes();
            for pane in &sidebars {
                assert!(
                    pane.session != session || pane.width <= max_width,
                    "sidebar for {session} expanded before immediate cleanup; all sidebars={sidebars:?}\nlogs:\n{}",
                    self.logs(),
                );
            }
            last_sessions = self.session_names();
            if !last_sessions.iter().any(|name| name == session) {
                return;
            }
            sleep(Duration::from_millis(5));
        }
        panic!(
            "timed out waiting for session {session} to disappear without sidebar expansion; last={last_sessions:?}\nsidebars={:?}\npanes:\n{}\npane-died hook:\n{}\nhooks:\n{}\nlogs:\n{}",
            self.sidebar_panes(),
            self.tmux([
                "list-panes",
                "-a",
                "-F",
                "#{session_name} #{window_id} #{pane_id} title=#{pane_title} dead=#{pane_dead} width=#{pane_width} command=#{pane_current_command}"
            ]),
            self.tmux(["show-hooks", "-g", "pane-died"]),
            self.tmux(["show-hooks", "-g"]),
            self.logs(),
        );
    }

    fn move_focus_off_active(&self, pane: &str, active_session: &str) {
        for _ in 0..6 {
            self.tmux_ok(["send-keys", "-t", pane, "Down"]);
            sleep(Duration::from_millis(100));
            if has_non_active_focus_marker(&self.capture_pane(pane), active_session) {
                return;
            }
        }
    }

    fn send_sidebar_key(&self, pane: &str, key: &str) {
        self.tmux_ok(["send-keys", "-t", pane, key]);
        sleep(Duration::from_millis(100));
    }

    fn click_session_row(&self, pane: &str, session: &str) {
        let capture = self.capture_pane(pane);
        let y = row_index(&capture, session)
            .unwrap_or_else(|| panic!("no row for {session} in pane {pane}; got:\n{capture}"))
            + 1;
        let x = 6;
        let press = format!("\x1b[<0;{x};{y}M");
        let release = format!("\x1b[<0;{x};{y}m");
        self.tmux_ok(["send-keys", "-t", pane, "-l", &press]);
        self.tmux_ok(["send-keys", "-t", pane, "-l", &release]);
    }

    fn measure_tab_switch(&self, from_session: &str, source_pane: &str) -> Duration {
        let started = Instant::now();
        self.tmux_ok(["send-keys", "-t", source_pane, "C-i"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let output = self.tmux(["list-clients", "-F", "#{client_session}"]);
            if output
                .lines()
                .any(|line| !line.trim().is_empty() && line.trim() != from_session)
            {
                return started.elapsed();
            }
            sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for client to leave session {from_session}; clients:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-clients",
                "-F",
                "#{client_name} #{client_tty} #{client_session}"
            ]),
            self.logs(),
        );
    }

    fn reorder_session(&self, name: &str, delta: i8) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("build e2e tokio runtime");

        runtime.block_on(async {
            let mut ws = opensessions_sidebar::client::connect_ws_path_with_token(
                "127.0.0.1",
                self.port,
                "/",
                &self.auth_token(),
            )
            .await
            .expect("connect reorder ws client");
            let _ = ws.next().await.expect("read ws hello").expect("ws hello");
            let _ = ws
                .next()
                .await
                .expect("read ws initial state")
                .expect("ws initial state");
            let command = serde_json::json!({
                "type": "reorder-session",
                "name": name,
                "delta": delta,
            });
            ws.send(Message::text(command.to_string()))
                .await
                .expect("send reorder-session command");
            ws.close().await.expect("close reorder ws client");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
    }

    fn reorder_worktree_group(&self, delta: i8) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("build e2e tokio runtime");

        runtime.block_on(async {
            let mut ws = opensessions_sidebar::client::connect_ws_path_with_token(
                "127.0.0.1",
                self.port,
                "/",
                &self.auth_token(),
            )
            .await
            .expect("connect worktree reorder ws client");
            let _ = ws.next().await.expect("read ws hello").expect("ws hello");
            let _ = ws
                .next()
                .await
                .expect("read ws initial state")
                .expect("ws initial state");
            let command = serde_json::json!({
                "type": "reorder-worktree-group",
                "key": fs::canonicalize(self.root.join("os-demo-worktrees"))
                    .expect("canonicalize worktree group path")
                    .to_string_lossy(),
                "delta": delta,
            });
            ws.send(Message::text(command.to_string()))
                .await
                .expect("send reorder-worktree-group command");
            ws.close().await.expect("close worktree reorder ws client");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
    }

    fn wait_for_session_order<F>(&self, predicate: F)
    where
        F: Fn(&[String]) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = Vec::new();
        while Instant::now() < deadline {
            last = self.session_names();
            if predicate(&last) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for session order; last={last:?}\nlogs:\n{}",
            self.logs()
        );
    }

    fn session_names(&self) -> Vec<String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("build e2e tokio runtime");

        runtime.block_on(async {
            let mut ws = opensessions_sidebar::client::connect_ws_path_with_token(
                "127.0.0.1",
                self.port,
                "/",
                &self.auth_token(),
            )
            .await
            .expect("connect session-order ws client");
            let _ = ws.next().await.expect("read ws hello").expect("ws hello");
            let state = ws
                .next()
                .await
                .expect("read ws initial state")
                .expect("ws initial state");
            let state = String::from_utf8(state.as_payload().to_vec()).expect("state text");
            ws.close().await.expect("close session-order ws client");
            let json = serde_json::from_str::<serde_json::Value>(&state).expect("parse state json");
            json.get("sessions")
                .and_then(serde_json::Value::as_array)
                .expect("state sessions array")
                .iter()
                .filter_map(|session| session.get("name")?.as_str().map(str::to_string))
                .collect()
        })
    }

    fn wait_for_sidebar_mode(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let log = fs::read_to_string(self.root.join("debug.log")).unwrap_or_default();
            if log.contains(&format!("snapshot_json mode={expected}")) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for sidebar mode {expected}; logs:\n{}",
            self.logs()
        );
    }

    fn debug_log_occurrences(&self, needle: &str) -> usize {
        fs::read_to_string(self.root.join("debug.log"))
            .unwrap_or_default()
            .matches(needle)
            .count()
    }

    fn wait_for_sidebar_connections(&self) {
        let panes = self.sidebar_panes();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let log = fs::read_to_string(self.root.join("debug.log")).unwrap_or_default();
            if panes.iter().all(|pane| {
                log.lines().any(|line| {
                    line.contains("identify-pane")
                        && line.contains(&format!("pane=Some(\"{}\")", pane.pane))
                })
            }) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for sidebar connections; panes={panes:?}\nlogs:\n{}",
            self.logs()
        );
    }

    fn wait_for_sidebar_pane_count(&self, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.sidebar_panes().len() >= expected {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for at least {expected} sidebar panes; panes={:?}\nlogs:\n{}",
            self.sidebar_panes(),
            self.logs(),
        );
    }

    fn wait_for_no_sidebar_processes(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.sidebar_panes().is_empty() {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for all sidebar panes to exit; panes={:?}\nall panes:\n{}\nlogs:\n{}",
            self.sidebar_panes(),
            self.tmux([
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} session=#{session_name} title=#{pane_title} command=#{pane_current_command} dead=#{pane_dead} start=#{pane_start_command}"
            ]),
            self.logs(),
        );
    }

    fn assert_no_sidebar_panes(&self, reason: &str) {
        let panes = self.sidebar_panes();
        assert!(
            panes.is_empty(),
            "{reason}; panes={panes:?}\nlogs:\n{}",
            self.logs(),
        );
    }

    fn wait_for_server_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(server) = &mut self.server
                && server.try_wait().expect("poll server process").is_some()
            {
                self.server = None;
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!("server did not exit; logs:\n{}", self.logs());
    }

    fn server_is_running(&mut self) -> bool {
        self.server
            .as_mut()
            .and_then(|server| server.try_wait().expect("poll server process"))
            .is_none()
    }

    fn wait_for_non_sidebar_pane_count(&self, session: &str, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.non_sidebar_panes(session).len() == expected {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for {expected} non-sidebar panes in {session}; panes={:?}\nlogs:\n{}",
            self.non_sidebar_panes(session),
            self.logs(),
        );
    }

    fn sidebar_pane(&self, session: &str) -> String {
        let output = self.tmux([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_id}\t#{pane_current_command}\t#{pane_title}",
        ]);
        output
            .lines()
            .find_map(|line| {
                let mut parts = line.split('\t');
                let pane_session = parts.next()?;
                let pane = parts.next()?;
                let command = parts.next()?;
                let title = parts.next()?;
                (pane_session == session
                    && (title == "opensessions-sidebar" || command.starts_with("opensessions")))
                .then(|| pane.to_string())
            })
            .unwrap_or_else(|| panic!("no sidebar pane found for {session}; panes:\n{output}"))
    }

    fn wait_for_sidebar_pane(&self, session: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(pane) = self
                .sidebar_panes()
                .into_iter()
                .find(|pane| pane.session == session)
            {
                return pane.pane;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for sidebar in {session}; panes={:?}\nlogs:\n{}",
            self.sidebar_panes(),
            self.logs(),
        );
    }

    fn active_pane(&self) -> String {
        self.tmux(["display-message", "-p", "#{pane_id}"])
    }

    fn current_window_index(&self, session: &str) -> String {
        let target = exact_session_target(session);
        self.tmux(["display-message", "-p", "-t", &target, "#{window_index}"])
    }

    fn wait_for_window_absent(&self, session: &str, window_index: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let target = exact_session_target(session);
        while Instant::now() < deadline {
            let windows = self.tmux(["list-windows", "-t", &target, "-F", "#{window_index}"]);
            if !windows.lines().any(|line| line.trim() == window_index) {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        panic!(
            "timed out waiting for window {session}:{window_index} to close; windows:\n{}\n\nlogs:\n{}",
            self.tmux([
                "list-windows",
                "-t",
                &target,
                "-F",
                "#{window_index} #{window_name} panes=#{window_panes} active=#{window_active}"
            ]),
            self.logs(),
        );
    }

    fn sidebar_pane_in_window(&self, session: &str, window: &str) -> String {
        let target = format!("{session}:{window}");
        let output = self.tmux([
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_title}",
        ]);
        output
            .lines()
            .find_map(|line| {
                let mut parts = line.split('\t');
                let pane = parts.next()?;
                let command = parts.next()?;
                let title = parts.next()?;
                (title == "opensessions-sidebar" || command.starts_with("opensessions"))
                    .then(|| pane.to_string())
            })
            .unwrap_or_else(|| panic!("no sidebar pane found for {target}; panes:\n{output}"))
    }

    fn main_pane_in_window(&self, session: &str, window: &str) -> String {
        let target = format!("{session}:{window}");
        self.tmux([
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id}\t#{pane_title}",
        ])
        .lines()
        .filter_map(|line| {
            let (pane, title) = line.split_once('\t')?;
            (title != "opensessions-sidebar").then(|| pane.to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("no main pane found for {target}"))
    }

    fn next_window_index(&self, session: &str) -> u32 {
        let target = exact_session_target(session);
        self.tmux(["list-windows", "-t", &target, "-F", "#{window_index}"])
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }

    fn main_pane(&self, session: &str) -> String {
        self.non_sidebar_panes(session)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no main pane found for {session}"))
    }

    fn sidebar_width(&self, session: &str) -> Option<u16> {
        self.sidebar_panes()
            .into_iter()
            .find(|pane| pane.session == session)
            .map(|pane| pane.width)
    }

    fn non_sidebar_panes(&self, session: &str) -> Vec<String> {
        let target = exact_session_target(session);
        self.tmux([
            "list-panes",
            "-t",
            &target,
            "-F",
            "#{pane_id}\t#{pane_title}",
        ])
        .lines()
        .filter_map(|line| {
            let (pane, title) = line.split_once('\t')?;
            (title != "opensessions-sidebar").then(|| pane.to_string())
        })
        .collect()
    }

    fn sidebar_panes(&self) -> Vec<SidebarPane> {
        self.tmux([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{pane_id}\t#{pane_width}\t#{pane_current_command}\t#{pane_title}",
        ])
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let session = parts.next()?;
            let pane = parts.next()?;
            let width = parts.next()?.parse::<u16>().ok()?;
            let command = parts.next()?;
            let title = parts.next()?;
            (title == "opensessions-sidebar" || command.starts_with("opensessions")).then(|| {
                SidebarPane {
                    session: session.to_string(),
                    pane: pane.to_string(),
                    width,
                }
            })
        })
        .collect()
    }

    fn capture_pane(&self, pane: &str) -> String {
        self.tmux(["capture-pane", "-p", "-t", pane])
    }

    fn tmux_socket_env(&self) -> String {
        format!(
            "{},0,0",
            self.tmux(["display-message", "-p", "#{socket_path}"])
        )
    }

    fn assert_width_hooks_are_well_quoted(&self) {
        let hooks = self.tmux(["show-hooks", "-g"]);
        assert!(
            hooks.contains("@opensessions_width"),
            "width hooks were not installed; hooks:\n{hooks}"
        );
        assert!(
            hooks.contains("after-resize-pane")
                && hooks.contains("/repair-sidebar-width")
                && hooks.contains("#{pane_id}"),
            "width repair hook must target the resized pane through the server-owned width repair path; hooks:\n{hooks}"
        );
        assert!(
            !hooks.contains("case  in") && !hooks.contains("[ -n  ]") && !hooks.contains("-t  -x"),
            "width repair hook lost shell variables during tmux parsing; hooks:\n{hooks}"
        );
    }

    fn tmux_ok<const N: usize>(&self, args: [&str; N]) {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("run tmux");
        assert!(
            output.status.success(),
            "tmux failed: args={args:?}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn tmux<const N: usize>(&self, args: [&str; N]) -> String {
        let output = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("run tmux");
        assert!(
            output.status.success(),
            "tmux failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn git<const N: usize>(&self, dir: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn logs(&self) -> String {
        let mut logs = String::new();
        for entry in fs::read_dir(&self.root).expect("read e2e root") {
            let entry = entry.expect("read e2e log entry");
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "log") {
                logs.push_str(&format!("\n--- {} ---\n", path.display()));
                logs.push_str(&fs::read_to_string(&path).unwrap_or_else(|err| err.to_string()));
            }
        }
        logs
    }

    fn sidebar_bin(&self) -> PathBuf {
        std::env::var_os("CARGO_BIN_EXE_opensessions-sidebar")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.target_debug_bin("opensessions-sidebar"))
    }

    fn server_bin(&self) -> PathBuf {
        std::env::var_os("OPENSESSIONS_E2E_SERVER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.target_debug_bin("opensessions-server"))
    }

    fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    fn config_path(&self) -> PathBuf {
        self.home_dir().join(".config/opensessions/config.json")
    }

    fn project_root(&self) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("resolve project root")
    }

    fn target_debug_bin(&self, name: &str) -> PathBuf {
        let current = std::env::current_exe().expect("current exe");
        let deps = current.parent().expect("deps dir");
        let debug = deps.parent().expect("target debug dir");
        debug.join(name)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn exact_session_target(session: &str) -> String {
    format!("={session}:")
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
        }
        for mut client in self.clients.drain(..) {
            let _ = client.kill();
            let _ = client.wait();
        }
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SidebarPane {
    session: String,
    pane: String,
    width: u16,
}
