//! Sidebar startup refocus against a real tmux server on a private socket:
//! after a sidebar spawns, focus returns to the pane the user was in.

use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use opensessions_runtime::mux::{MuxProvider, SidebarPosition};
use opensessions_runtime::tmux_provider::{CommandOutput, CommandRunner, TmuxProvider};
use opensessions_sidebar::runtime_context::refocus_plan;

struct PrivateTmux {
    socket: String,
    socket_path: String,
    dir: std::path::PathBuf,
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

#[test]
fn sidebar_startup_refocus_returns_to_the_users_pane() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let socket = format!(
        "os-refocus-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let dir = std::env::temp_dir().join(&socket);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    let start = dir.join("scripts").join("start.sh");
    std::fs::write(&start, "#!/bin/sh\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&start, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut lab = PrivateTmux {
        socket: socket.clone(),
        socket_path: String::new(),
        dir: dir.clone(),
    };
    let created = lab.tmux(&["new-session", "-d", "-s", "work", "-x", "160", "-y", "40"]);
    assert!(created.status.success(), "{created:?}");
    lab.socket_path = lab.stdout(&["display-message", "-p", "#{socket_path}"]);
    lab.tmux(&[
        "set-environment",
        "-g",
        "OPENSESSIONS_DIR",
        &dir.to_string_lossy(),
    ]);
    let window = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{window_id}"]);
    let left = lab.stdout(&["display-message", "-p", "-t", "=work:", "#{pane_id}"]);
    let right = lab.stdout(&["split-window", "-h", "-t", &left, "-P", "-F", "#{pane_id}"]);
    lab.tmux(&["select-pane", "-t", &right]);

    let provider = TmuxProvider::new(Arc::new(PrivateRunner {
        socket: socket.clone(),
    }));
    let sidebar = provider
        .spawn_sidebar("work", &window, 30, SidebarPosition::Left, "scripts")
        .expect("sidebar spawned");
    let plan = refocus_plan(&sidebar, Some(&window), |args| {
        let output = lab.tmux(args);
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    });

    assert_ne!(left, right);
    assert_eq!(plan.map(|plan| plan.select_pane), Some(right));
}
