use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use crate::mux::{
    ActiveWindow, AgentPane, ClientFocus, MuxProvider, MuxSessionInfo, MuxWindowInfo, SidebarPane,
    SidebarPosition, ViewedPane,
};
use crate::subprocess::{TMUX_COMMAND_TIMEOUT, output_with_timeout};
use crate::tmux_scripting::{
    REMAIN_ON_EXIT_INHERITED, REMAIN_ON_EXIT_PREVIOUS_OPTION, SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION,
    hook_context_format, http_hook_command, pane_died_hook_command, pane_exited_hook_command,
    resized_pane_width_repair_command, sidebar_mouse_resize_marker_script,
    sidebar_mouse_resize_report_script,
};

const SEP: &str = "\t";
const STASH_SESSION: &str = "_os_stash";
const OPENSESSIONS_HOOK_INDEX: u16 = 909;
/// Tmux-server-scoped record of the user's last explicit sidebar show/hide
/// choice. It intentionally survives opensessions server restarts and hook
/// cleanup, and disappears with the tmux server itself.
const SIDEBAR_VISIBLE_OPTION: &str = "@opensessions_sidebar_visible";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, args: &[String]) -> CommandOutput;
}

/// Runs the mux binary as a subprocess. Each command is bounded by a timeout
/// (default [`TMUX_COMMAND_TIMEOUT`]); a command that outlives it is killed
/// and reported as a failure so a wedged tmux cannot hold callers' locks.
#[derive(Debug, Clone)]
pub struct StdCommandRunner {
    binary: String,
    timeout: Duration,
}

impl StdCommandRunner {
    pub fn new(binary: impl Into<String>) -> Self {
        Self::with_timeout(binary, TMUX_COMMAND_TIMEOUT)
    }

    pub fn with_timeout(binary: impl Into<String>, timeout: Duration) -> Self {
        Self {
            binary: binary.into(),
            timeout,
        }
    }
}

impl Default for StdCommandRunner {
    fn default() -> Self {
        Self::new("tmux")
    }
}

impl CommandRunner for StdCommandRunner {
    fn run(&self, args: &[String]) -> CommandOutput {
        match output_with_timeout(Command::new(&self.binary).args(args), self.timeout) {
            Ok(output) => CommandOutput {
                exit_code: output.status.code().unwrap_or(1),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            },
            Err(err) => CommandOutput {
                exit_code: 1,
                stdout: String::new(),
                stderr: err.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub created_at: u64,
    pub attached_clients: u32,
    pub window_count: u32,
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: String,
    pub session_id: String,
    pub session_name: String,
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub pane_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: String,
    pub session_name: String,
    pub window_id: String,
    pub window_index: u32,
    pub index: u32,
    pub active: bool,
    pub tty: String,
    pub pid: u32,
    pub cwd: String,
    pub command: String,
    pub title: String,
    pub width: u16,
    pub height: u16,
    pub left: u16,
    pub right: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub name: String,
    pub tty: String,
    pub pid: u32,
    pub session_name: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone)]
pub struct TmuxClient {
    runner: Arc<dyn CommandRunner>,
}

impl TmuxClient {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner }
    }

    pub fn run(&self, args: &[&str]) -> CommandOutput {
        let args = args
            .iter()
            .map(|arg| (*arg).to_string())
            .collect::<Vec<_>>();
        self.runner.run(&args)
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.try_list_sessions().unwrap_or_default()
    }

    /// `None` when `list-sessions` failed transiently (tmux could not be
    /// spawned, the server was too busy to accept), as opposed to listing no
    /// sessions. A tmux server that is gone or exiting has no sessions.
    pub fn try_list_sessions(&self) -> Option<Vec<SessionInfo>> {
        let output = self.run(&["list-sessions", "-F", session_format()]);
        if output.ok() {
            return Some(parse_sessions(&output.stdout));
        }
        tmux_server_is_gone(&output.stderr).then(Vec::new)
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        parse_windows(
            &self
                .run(&["list-windows", "-a", "-F", window_format()])
                .stdout,
        )
    }

    pub fn list_clients(&self) -> Vec<ClientInfo> {
        parse_clients(&self.run(&["list-clients", "-F", client_format()]).stdout)
    }

    pub fn state_fingerprint(&self) -> Option<u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let output = self.run(&["list-panes", "-a", "-F", state_fingerprint_format()]);
        if !output.ok() || output.stdout.is_empty() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        output.stdout.hash(&mut hasher);
        Some(hasher.finish())
    }

    pub fn list_visible_sidebar_pane_ids(&self) -> Vec<String> {
        self.run(&[
            "list-panes",
            "-a",
            "-f",
            "#{&&:#{session_attached},#{window_active},#{==:#{pane_title},opensessions-sidebar}}",
            "-F",
            "#{pane_id}",
        ])
        .stdout
        .lines()
        .filter(|pane_id| !pane_id.is_empty())
        .map(str::to_string)
        .collect()
    }

    /// `(session, pane)` for every pane in the active window of every
    /// attached session, in one batched call.
    pub fn list_viewed_panes(&self) -> Vec<(String, String)> {
        self.run(&[
            "list-panes",
            "-a",
            "-f",
            "#{&&:#{session_attached},#{window_active}}",
            "-F",
            "#{session_name}\t#{pane_id}",
        ])
        .stdout
        .lines()
        .filter_map(|line| line.split_once(SEP))
        .filter(|(session, pane_id)| !session.is_empty() && !pane_id.is_empty())
        .map(|(session, pane_id)| (session.to_string(), pane_id.to_string()))
        .collect()
    }

    pub fn list_panes(&self, scope: PaneScope<'_>) -> Vec<PaneInfo> {
        let mut args = vec!["list-panes"];
        let session_target;
        match scope {
            PaneScope::All => args.push("-a"),
            PaneScope::Session(name) => {
                session_target = exact_session_window_target(name);
                args.push("-s");
                args.push("-t");
                args.push(&session_target);
            }
            PaneScope::Window(target) => {
                args.push("-t");
                args.push(target);
            }
        }
        args.push("-F");
        args.push(pane_format());
        parse_panes(&self.run(&args).stdout)
    }

    /// Switches to the session named exactly `session_name`.
    pub fn switch_client_to_session(&self, session_name: &str, client_tty: Option<&str>) {
        self.switch_client(&exact_session_target(session_name), client_tty);
    }

    pub fn switch_client(&self, target: &str, client_tty: Option<&str>) {
        let mut args = vec!["switch-client"];
        if let Some(client_tty) = client_tty {
            args.push("-c");
            args.push(client_tty);
        }
        args.push("-t");
        args.push(target);
        self.run(&args);
    }

    pub fn select_sidebar_pane_for_session(&self, session_name: &str) {
        let Some(window_id) = self
            .list_windows()
            .into_iter()
            .find(|window| window.session_name == session_name && window.active)
            .map(|window| window.id)
        else {
            return;
        };
        let Some(sidebar_pane) = self
            .list_panes(PaneScope::Window(&window_id))
            .into_iter()
            .find(|pane| pane.title == "opensessions-sidebar")
            .map(|pane| pane.id)
        else {
            return;
        };
        self.select_pane(&sidebar_pane);
    }

    pub fn new_session(&self, name: Option<&str>, cwd: Option<&str>) -> String {
        let mut args = vec!["new-session", "-d"];
        if let Some(name) = name {
            args.push("-s");
            args.push(name);
        }
        if let Some(cwd) = cwd {
            args.push("-c");
            args.push(cwd);
        }
        args.extend(["-P", "-F", "#{session_name}"]);
        self.run(&args).stdout
    }

    /// Kills the session named exactly `session_name`. A plain `-t name`
    /// would fall back to prefix and pattern matches and could kill another
    /// session (`api` matching `api-v2`) once `name` is already gone.
    pub fn kill_session(&self, session_name: &str) {
        self.run(&["kill-session", "-t", &exact_session_target(session_name)]);
    }

    /// Renames the session named exactly `target` and returns the resulting
    /// name. tmux sanitizes names (`.`/`:` become `_`) and format-expands
    /// them (`#{session_id}`), so the name is read back by stable session id.
    pub fn rename_session(&self, target: &str, new_name: &str) -> Option<String> {
        let session_id = self.run(&[
            "display-message",
            "-p",
            "-t",
            &exact_session_window_target(target),
            "#{session_id}",
        ]);
        let session_id = session_id.stdout.trim();
        if !session_id.starts_with('$') {
            return None;
        }
        let renamed = self.run(&[
            "rename-session",
            "-t",
            session_id,
            new_name,
            ";",
            "display-message",
            "-p",
            "-t",
            session_id,
            "#{session_name}",
        ]);
        let actual = renamed.stdout.trim();
        (renamed.ok() && !actual.is_empty()).then(|| actual.to_string())
    }

    pub fn unlink_window(&self, session_name: &str, window_id: &str) {
        self.run(&[
            "unlink-window",
            "-t",
            &format!("={session_name}:{window_id}"),
        ]);
    }

    pub fn kill_window(&self, window_id: &str) {
        self.run(&["kill-window", "-t", window_id]);
    }

    pub fn kill_pane(&self, target: &str) {
        self.run(&["kill-pane", "-t", target]);
    }

    pub fn select_window(&self, target: &str) {
        self.run(&["select-window", "-t", target]);
    }

    pub fn select_pane(&self, target: &str) {
        self.run(&["select-pane", "-t", target]);
    }

    pub fn flash_pane(&self, target: &str) {
        self.run(&["select-pane", "-t", target, "-P", "bg=colour238"]);
        let quoted = shell_quote(target);
        self.run(&[
            "run-shell",
            "-b",
            &format!("sleep 0.18; tmux select-pane -t {quoted} -P default"),
        ]);
    }

    pub fn set_pane_title(&self, target: &str, title: &str) {
        self.run(&["select-pane", "-t", target, "-T", title]);
    }

    pub fn resize_pane_width(&self, target: &str, width: u16) {
        self.run(&["resize-pane", "-t", target, "-x", &width.to_string()]);
    }

    pub fn resize_pane_widths(&self, targets: &[String], width: u16) {
        let mut pending = targets.to_vec();
        for attempt in 1..=SIDEBAR_REPAIR_ATTEMPTS {
            if pending.is_empty() {
                return;
            }
            // List layouts in the same call so each one matches its pane widths.
            let format = format!("{}{SEP}#{{window_layout}}", pane_format());
            let output = self.run(&["list-panes", "-a", "-F", &format]).stdout;
            let panes = parse_panes(&output);
            let layouts = output
                .lines()
                .filter_map(|line| {
                    let parts = split(line);
                    Some((parts.get(2)?.to_string(), parts.get(15)?.to_string()))
                })
                .collect::<HashMap<_, _>>();
            if attempt > 1 {
                // A guarded repair that found a stale layout left its window
                // untouched. Retry only sidebars that still have the wrong
                // width: content repairs are only computed for those, so the
                // fresh listing is the version-independent retry signal.
                pending.retain(|target| {
                    panes
                        .iter()
                        .any(|pane| pane.id == *target && pane.width != width)
                });
                if pending.is_empty() {
                    return;
                }
            }
            let stale_layout = if attempt == SIDEBAR_REPAIR_ATTEMPTS {
                StaleLayoutRepair::SidebarOnly
            } else {
                StaleLayoutRepair::Skip
            };
            let script = pending
                .iter()
                .map(|target| {
                    sidebar_width_repair_script(&panes, &layouts, target, width, stale_layout)
                })
                .collect::<Vec<_>>()
                .join("; ");
            self.run(&["run-shell", &script]);
            // Unguarded repairs (no listed layout) always apply, so there is
            // nothing to verify and no reason to spend another listing.
            let any_guarded = pending.iter().any(|target| {
                panes
                    .iter()
                    .find(|pane| pane.id == *target)
                    .is_some_and(|pane| layouts.contains_key(&pane.window_id))
            });
            if !any_guarded {
                return;
            }
        }
    }

    pub fn set_window_remain_on_exit(&self, target: &str, enabled: bool) {
        self.run(&[
            "set-window-option",
            "-t",
            target,
            "remain-on-exit",
            if enabled { "on" } else { "off" },
        ]);
    }

    pub fn ensure_window_remain_on_exit(&self, target: &str) {
        let local = self.run(&["show-window-options", "-t", target, "-v", "remain-on-exit"]);
        let effective = if local.stdout.is_empty() {
            self.run(&["show-window-options", "-g", "-v", "remain-on-exit"])
                .stdout
        } else {
            local.stdout.clone()
        };
        if effective == "on" {
            return;
        }
        self.set_window_remain_on_exit(target, true);
        self.run(&[
            "set-window-option",
            "-t",
            target,
            REMAIN_ON_EXIT_PREVIOUS_OPTION,
            if local.stdout.is_empty() {
                REMAIN_ON_EXIT_INHERITED
            } else {
                &local.stdout
            },
        ]);
    }

    pub fn ensure_remain_on_exit_for_sidebar_windows(&self) {
        let mut seen_windows = HashSet::new();
        for pane in self.list_panes(PaneScope::All) {
            if pane.title == "opensessions-sidebar" && seen_windows.insert(pane.window_id.clone()) {
                self.ensure_window_remain_on_exit(&pane.window_id);
            }
        }
    }

    /// Restores `remain-on-exit` on every window that opensessions changed.
    ///
    /// Windows are found by their saved-value marker rather than by their
    /// sidebar panes: during shutdown sidebar clients exit before cleanup and
    /// the `pane-died` hook removes their panes, so a sidebar-pane scan would
    /// skip windows and leave them with `remain-on-exit on`.
    pub fn restore_remain_on_exit_for_marked_windows(&self) {
        for (window_id, previous) in self.remain_on_exit_marked_windows() {
            self.restore_window_remain_on_exit(&window_id, &previous);
        }
    }

    /// Restores `remain-on-exit` on marked windows that no longer have a
    /// sidebar pane (hidden by toggle or killed by hand), and removes their
    /// dead content panes that the restored value would not have kept.
    /// Otherwise panes the user exits there linger as "Pane is dead".
    pub fn restore_remain_on_exit_for_windows_without_sidebar(&self) {
        let marked = self.remain_on_exit_marked_windows();
        if marked.is_empty() {
            return;
        }
        let listing = self.run(&[
            "list-panes",
            "-a",
            "-F",
            "#{window_id}\t#{pane_id}\t#{pane_title}\t#{pane_dead}\t#{pane_dead_status}",
        ]);
        if !listing.ok() {
            return;
        }
        let panes = listing
            .stdout
            .lines()
            .filter_map(|line| {
                let parts = split(line);
                (parts.len() >= 5)
                    .then(|| (parts[0], parts[1], parts[2], parts[3] == "1", parts[4]))
            })
            .collect::<Vec<_>>();
        let mut global = None;
        for (window_id, previous) in marked {
            let window_panes = panes
                .iter()
                .filter(|(window, ..)| *window == window_id)
                .collect::<Vec<_>>();
            if window_panes.is_empty()
                || window_panes
                    .iter()
                    .any(|(_, _, title, ..)| *title == "opensessions-sidebar")
            {
                continue;
            }
            self.restore_window_remain_on_exit(&window_id, &previous);
            let effective = if previous == REMAIN_ON_EXIT_INHERITED {
                global
                    .get_or_insert_with(|| {
                        self.run(&["show-options", "-gwv", "remain-on-exit"]).stdout
                    })
                    .clone()
            } else {
                previous
            };
            for (_, pane_id, _, dead, status) in window_panes {
                let kept = effective == "on" || (effective == "failed" && *status != "0");
                if *dead && !kept {
                    self.kill_pane(pane_id);
                }
            }
        }
    }

    /// Windows carrying the saved-value marker, found by marker rather than
    /// by sidebar panes: during shutdown sidebar clients exit before cleanup
    /// and the `pane-died` hook removes their panes.
    fn remain_on_exit_marked_windows(&self) -> Vec<(String, String)> {
        let format = format!("#{{window_id}}{SEP}#{{{REMAIN_ON_EXIT_PREVIOUS_OPTION}}}");
        let output = self.run(&["list-windows", "-a", "-F", &format]);
        let mut seen_windows = HashSet::new();
        output
            .stdout
            .lines()
            .filter_map(|line| line.split_once(SEP))
            .filter(|(window_id, previous)| !previous.is_empty() && seen_windows.insert(*window_id))
            .map(|(window_id, previous)| (window_id.to_string(), previous.to_string()))
            .collect()
    }

    fn restore_window_remain_on_exit(&self, window_id: &str, previous: &str) {
        if previous == REMAIN_ON_EXIT_INHERITED {
            self.run(&["set-window-option", "-t", window_id, "-u", "remain-on-exit"]);
        } else {
            self.run(&[
                "set-window-option",
                "-t",
                window_id,
                "remain-on-exit",
                previous,
            ]);
        }
        self.run(&[
            "set-window-option",
            "-t",
            window_id,
            "-u",
            REMAIN_ON_EXIT_PREVIOUS_OPTION,
        ]);
    }

    pub fn split_sidebar_pane(
        &self,
        target: &str,
        before: bool,
        width: u16,
        command: &str,
    ) -> Option<PaneInfo> {
        let size = width.to_string();
        let side = if before { "-hb" } else { "-h" };
        let output = self.run(&[
            "split-window",
            side,
            "-f",
            "-l",
            &size,
            "-t",
            target,
            "-P",
            "-F",
            pane_format(),
            command,
        ]);
        if !output.ok() || output.stdout.is_empty() {
            return None;
        }
        parse_panes(&output.stdout).into_iter().next()
    }

    pub fn display(&self, format: &str, target: Option<&str>) -> String {
        let mut args = vec!["display-message"];
        if let Some(target) = target {
            args.push("-t");
            args.push(target);
        }
        args.push("-p");
        args.push(format);
        self.run(&args).stdout
    }

    pub fn display_for_client(&self, format: &str, client_tty: Option<&str>) -> String {
        let mut args = vec!["display-message"];
        if let Some(client_tty) = client_tty.filter(|client_tty| !client_tty.is_empty()) {
            args.push("-c");
            args.push(client_tty);
        }
        args.push("-p");
        args.push(format);
        self.run(&args).stdout
    }

    pub fn get_current_session(&self) -> Option<String> {
        let session_name = self.display("#{session_name}", None);
        if !session_name.is_empty() && !session_name.contains('/') {
            return Some(session_name);
        }
        self.list_clients()
            .into_iter()
            .find(|client| !client.tty.is_empty())
            .and_then(|client| (!client.session_name.is_empty()).then_some(client.session_name))
    }

    pub fn get_client_tty(&self) -> String {
        self.display("#{client_tty}", None)
    }

    pub fn client_tty_for_pane(&self, pane_id: &str) -> Option<String> {
        self.run(&["list-clients", "-F", "#{client_tty}\t#{pane_id}"])
            .stdout
            .lines()
            .filter_map(|line| line.split_once(SEP))
            .find_map(|(client_tty, active_pane_id)| {
                (active_pane_id == pane_id && !client_tty.is_empty())
                    .then(|| client_tty.to_string())
            })
    }

    pub fn get_current_window_id(&self) -> Option<String> {
        let window_id = self.display("#{window_id}", None);
        (!window_id.is_empty()).then_some(window_id)
    }

    pub fn get_current_pane_id(&self) -> Option<String> {
        let pane_id = self.display("#{pane_id}", None);
        (!pane_id.is_empty()).then_some(pane_id)
    }

    pub fn get_client_focus(&self, client_tty: Option<&str>) -> Option<ClientFocus> {
        let raw = self.display_for_client(
            "#{client_tty}\t#{session_name}\t#{window_id}\t#{pane_id}",
            client_tty,
        );
        let parts = raw.split(SEP).collect::<Vec<_>>();
        if parts.len() < 4 || parts[1].is_empty() || parts[2].is_empty() || parts[3].is_empty() {
            return None;
        }
        Some(ClientFocus {
            client_tty: (!parts[0].is_empty()).then(|| parts[0].to_string()),
            session_name: parts[1].to_string(),
            window_id: parts[2].to_string(),
            pane_id: parts[3].to_string(),
        })
    }

    pub fn get_session_dir(&self, session_name: &str) -> String {
        self.display(
            "#{pane_current_path}",
            Some(&exact_session_window_target(session_name)),
        )
    }

    pub fn get_pane_count(&self, session_name: &str) -> u32 {
        self.list_panes(PaneScope::Session(session_name)).len() as u32
    }

    pub fn get_all_pane_counts(&self) -> HashMap<String, u32> {
        let mut counts = HashMap::new();
        for pane in self.list_panes(PaneScope::All) {
            *counts.entry(pane.session_name).or_insert(0) += 1;
        }
        counts
    }

    pub fn get_active_session_dirs(&self) -> HashMap<String, String> {
        let output = self.run(&[
            "list-panes",
            "-a",
            "-f",
            "#{&&:#{window_active},#{!=:#{pane_title},opensessions-sidebar}}",
            "-F",
            "#{session_name}\t#{pane_current_path}",
        ]);
        let mut dirs = HashMap::new();
        for line in output.stdout.lines() {
            let Some((session, cwd)) = line.split_once(SEP) else {
                continue;
            };
            dirs.entry(session.to_string())
                .or_insert_with(|| cwd.to_string());
        }
        dirs
    }

    pub fn set_global_hook(&self, name: &str, command: &str) {
        let owned_name = format!("{name}[{OPENSESSIONS_HOOK_INDEX}]");
        let output = self.run(&["set-hook", "-g", &owned_name, command]);
        if !output.ok() {
            eprintln!(
                "opensessions: failed to install tmux hook {name}: status={} stderr={} command={command}",
                output.exit_code, output.stderr,
            );
        }
    }

    pub fn unset_global_hook(&self, name: &str) {
        let owned_name = format!("{name}[{OPENSESSIONS_HOOK_INDEX}]");
        self.run(&["set-hook", "-gu", &owned_name]);
    }

    pub fn set_global_option(&self, name: &str, value: &str) {
        self.run(&["set-option", "-gq", name, value]);
    }

    pub fn unset_global_option(&self, name: &str) {
        self.run(&["set-option", "-gu", name]);
    }

    pub fn setup_sidebar_mouse_resize_binding(&self, server_base: &str, token_file: &str) {
        let current = self.run(&["list-keys", "-T", "root", "MouseDrag1Border"]);
        let is_default = current.ok()
            && current.stdout.ends_with("resize-pane -M")
            && !current.stdout.contains("\\;");
        let is_ours = current.stdout.contains(SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION)
            && current.stdout.contains("/set-sidebar-width");
        if !is_default && !is_ours {
            return;
        }

        let args = vec![
            "bind-key".to_string(),
            "-T".to_string(),
            "root".to_string(),
            "MouseDrag1Border".to_string(),
            "run-shell".to_string(),
            sidebar_mouse_resize_marker_script(),
            "\\;".to_string(),
            "resize-pane".to_string(),
            "-M".to_string(),
            "\\;".to_string(),
            "run-shell".to_string(),
            "-b".to_string(),
            sidebar_mouse_resize_report_script(server_base, token_file),
        ];
        let output = self.runner.run(&args);
        if !output.ok() {
            eprintln!(
                "opensessions: failed to install tmux mouse resize binding: status={} stderr={}",
                output.exit_code, output.stderr,
            );
        }
    }

    pub fn cleanup_sidebar_mouse_resize_binding(&self) {
        let current = self.run(&["list-keys", "-T", "root", "MouseDrag1Border"]);
        if current.stdout.contains(SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION)
            && current.stdout.contains("/set-sidebar-width")
        {
            self.run(&[
                "bind-key",
                "-T",
                "root",
                "MouseDrag1Border",
                "resize-pane",
                "-M",
            ]);
        }
        self.unset_global_option(SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION);
    }
}

pub enum PaneScope<'a> {
    All,
    /// Every pane of the session with exactly this name.
    Session(&'a str),
    Window(&'a str),
}

/// Whether a failed tmux command reported that its server does not exist or
/// is exiting, rather than a transient failure to run or reach it.
fn tmux_server_is_gone(stderr: &str) -> bool {
    stderr.contains("no server running")
        || stderr.contains("server exited")
        || stderr.contains("lost server")
        || (stderr.contains("error connecting to")
            && (stderr.contains("No such file or directory")
                || stderr.contains("Connection refused")))
}

/// tmux resolves a bare `-t name` by exact match, then prefix, then pattern,
/// so a missing `api` silently targets `api-v2`. `=` forces an exact match.
fn exact_session_target(session_name: &str) -> String {
    format!("={session_name}")
}

/// Exact session match for window- and pane-scoped targets: the trailing `:`
/// selects that session's current window (and its active pane).
fn exact_session_window_target(session_name: &str) -> String {
    format!("={session_name}:")
}

#[derive(Clone)]
pub struct TmuxProvider {
    name: String,
    client: TmuxClient,
}

impl TmuxProvider {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            name: "tmux".to_string(),
            client: TmuxClient::new(runner),
        }
    }
}

impl MuxProvider for TmuxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_sessions(&self) -> Vec<MuxSessionInfo> {
        self.try_list_sessions().unwrap_or_default()
    }

    fn try_list_sessions(&self) -> Option<Vec<MuxSessionInfo>> {
        let active_dirs = self.client.get_active_session_dirs();
        let sessions = self.client.try_list_sessions()?;
        let sessions = sessions
            .into_iter()
            .filter(|session| session.name != STASH_SESSION)
            .map(|session| MuxSessionInfo {
                name: session.name.clone(),
                created_at: session.created_at,
                dir: active_dirs
                    .get(&session.name)
                    .cloned()
                    .unwrap_or(session.dir),
                windows: session.window_count,
            })
            .collect();
        Some(sessions)
    }

    fn state_fingerprint(&self) -> Option<u64> {
        self.client.state_fingerprint()
    }

    fn switch_session(&self, name: &str, client_tty: Option<&str>) {
        self.client.switch_client_to_session(name, client_tty);
        self.client.select_sidebar_pane_for_session(name);
    }

    fn client_tty_for_pane(&self, pane_id: &str) -> Option<String> {
        self.client.client_tty_for_pane(pane_id)
    }

    fn switch_clients_from_session(
        &self,
        session_name: &str,
        fallback_session: &str,
        _preferred_client_tty: Option<&str>,
    ) -> bool {
        let fallback_target = exact_session_window_target(fallback_session);
        let mut switched = false;
        for client in self.client.list_clients() {
            if client.session_name == session_name {
                self.client
                    .switch_client(&fallback_target, Some(&client.tty));
                switched = true;
            }
        }
        switched
    }

    fn get_current_session(&self) -> Option<String> {
        self.client.get_current_session()
    }

    fn get_session_dir(&self, name: &str) -> String {
        self.client.get_session_dir(name)
    }

    fn get_session_pane_pids(&self, name: &str) -> Vec<u32> {
        self.client
            .list_panes(PaneScope::Session(name))
            .into_iter()
            .map(|pane| pane.pid)
            .filter(|pid| *pid > 0)
            .collect()
    }

    fn get_pane_pid(&self, pane_id: &str) -> Option<u32> {
        if pane_id.is_empty() {
            return None;
        }
        let output = self
            .client
            .run(&["display-message", "-t", pane_id, "-p", "#{pane_pid}"]);
        if !output.ok() {
            return None;
        }
        output
            .stdout
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 1)
    }

    fn get_pane_count(&self, name: &str) -> u32 {
        self.client.get_pane_count(name)
    }

    fn get_client_tty(&self) -> String {
        self.client.get_client_tty()
    }

    fn create_session(&self, name: Option<&str>, dir: Option<&str>) {
        self.client.new_session(name, dir);
    }

    fn rename_session(&self, name: &str, new_name: &str) -> Option<String> {
        self.client.rename_session(name, new_name)
    }

    fn kill_session(&self, name: &str) {
        self.client.kill_session(name);
    }

    fn cleanup_sidebar(&self) {
        self.client.kill_session(STASH_SESSION);
    }

    fn setup_hooks(&self, server_host: &str, server_port: u16, token_file: &str) {
        let base = format!("http://{server_host}:{server_port}");
        let hook_context = hook_context_format();
        let focus_cmd = http_hook_command(&base, "/focus", Some(hook_context), true, token_file);
        let ensure_cmd = http_hook_command(
            &base,
            "/ensure-sidebar",
            Some(hook_context),
            true,
            token_file,
        );
        let ensure_all_cmd = http_hook_command(&base, "/ensure-sidebars", None, true, token_file);
        let pane_exited_cmd = pane_exited_hook_command(&base, token_file);
        let pane_died_cmd = pane_died_hook_command(&base, token_file);
        let client_resized_cmd =
            http_hook_command(&base, "/client-resized", None, true, token_file);
        let pane_layout_changed_cmd =
            http_hook_command(&base, "/pane-layout-changed", None, true, token_file);

        self.client.set_global_hook(
            "client-session-changed",
            &format!("{focus_cmd} ; {ensure_cmd}"),
        );
        self.client.set_global_hook("after-select-pane", &focus_cmd);
        self.client
            .set_global_hook("session-created", &ensure_all_cmd);
        self.client
            .set_global_hook("after-new-session", &ensure_all_cmd);
        let refresh_cmd = http_hook_command(&base, "/refresh", None, true, token_file);
        self.client.set_global_hook("session-closed", &refresh_cmd);
        self.client
            .set_global_hook("after-select-window", &ensure_cmd);
        self.client.set_global_hook("after-new-window", &ensure_cmd);
        self.client
            .set_global_hook("client-resized", &client_resized_cmd);
        self.client
            .set_global_hook("after-kill-pane", &pane_exited_cmd);
        self.client.set_global_hook("pane-exited", &pane_exited_cmd);
        self.client.set_global_hook("pane-died", &pane_died_cmd);
        self.client.set_global_hook(
            "after-resize-pane",
            &resized_pane_width_repair_command(&base, token_file),
        );
        self.client
            .set_global_hook("after-resize-window", &pane_layout_changed_cmd);
        self.client
            .setup_sidebar_mouse_resize_binding(&base, token_file);
        self.client.ensure_remain_on_exit_for_sidebar_windows();
    }

    fn cleanup_hooks(&self) {
        self.client.restore_remain_on_exit_for_marked_windows();
        self.client.cleanup_sidebar_mouse_resize_binding();
        for hook in [
            "client-session-changed",
            "after-select-pane",
            "session-created",
            "after-new-session",
            "session-closed",
            "after-select-window",
            "after-new-window",
            "client-resized",
            "after-kill-pane",
            "pane-exited",
            "pane-died",
            "after-resize-pane",
            "after-resize-window",
        ] {
            self.client.unset_global_hook(hook);
        }
        self.client.unset_global_option("@opensessions_width");
    }

    fn set_sidebar_width_hint(&self, width: u16) {
        self.client
            .set_global_option("@opensessions_width", &width.to_string());
    }

    fn sidebar_visibility_preference(&self) -> Option<bool> {
        match self
            .client
            .run(&["show-option", "-gqv", SIDEBAR_VISIBLE_OPTION])
            .stdout
            .trim()
        {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    fn set_sidebar_visibility_preference(&self, visible: bool) {
        self.client
            .set_global_option(SIDEBAR_VISIBLE_OPTION, if visible { "on" } else { "off" });
    }

    fn is_sidebar_mouse_resize_active(&self, window_id: &str) -> bool {
        self.client
            .run(&["show-option", "-gqv", SIDEBAR_MOUSE_RESIZE_WINDOW_OPTION])
            .stdout
            == window_id
    }

    fn is_window_capable(&self) -> bool {
        true
    }

    fn is_sidebar_capable(&self) -> bool {
        true
    }

    fn is_batch_capable(&self) -> bool {
        true
    }

    fn list_active_windows(&self) -> Vec<ActiveWindow> {
        let mut windows = Vec::<ActiveWindow>::new();
        for window in self
            .client
            .list_windows()
            .into_iter()
            .filter(|window| window.session_name != STASH_SESSION)
        {
            let next = ActiveWindow {
                id: window.id,
                session_name: window.session_name,
                active: window.active,
            };
            if let Some(current) = windows.iter_mut().find(|current| current.id == next.id) {
                if !current.active && next.active {
                    *current = next;
                }
            } else {
                windows.push(next);
            }
        }

        windows
    }

    fn list_windows(&self, session_name: &str) -> Vec<MuxWindowInfo> {
        let panes = self.client.list_panes(PaneScope::Session(session_name));
        self.client
            .list_windows()
            .into_iter()
            .filter(|window| window.session_name == session_name)
            .map(|window| {
                let mut pane_commands = panes
                    .iter()
                    .filter(|pane| {
                        pane.window_id == window.id && pane.title != "opensessions-sidebar"
                    })
                    .map(|pane| pane.command.clone())
                    .filter(|command| !command.is_empty())
                    .collect::<Vec<_>>();
                pane_commands.sort();
                pane_commands.dedup();
                MuxWindowInfo {
                    id: window.id,
                    index: window.index,
                    name: window.name,
                    active: window.active,
                    pane_commands,
                }
            })
            .collect()
    }

    fn switch_window(&self, session_name: &str, window_id: &str, client_tty: Option<&str>) {
        let is_session_window = self
            .client
            .list_windows()
            .iter()
            .any(|window| window.session_name == session_name && window.id == window_id);
        if !is_session_window {
            return;
        }
        self.client
            .switch_client_to_session(session_name, client_tty);
        self.client.select_window(window_id);
    }

    fn kill_windows(&self, session_name: &str, window_ids: &[String]) {
        let all_windows = self.client.list_windows();
        let windows = all_windows
            .iter()
            .filter(|window| window.session_name == session_name)
            .collect::<Vec<_>>();
        if !windows.iter().any(|window| window.active) {
            return;
        }
        let requested = window_ids.iter().collect::<HashSet<_>>();
        for window in windows {
            if !window.active && requested.contains(&window.id) {
                if all_windows
                    .iter()
                    .filter(|candidate| candidate.id == window.id)
                    .count()
                    > 1
                {
                    self.client.unlink_window(session_name, &window.id);
                } else {
                    self.client.kill_window(&window.id);
                }
            }
        }
    }

    fn get_current_window_id(&self) -> Option<String> {
        self.client.get_current_window_id()
    }

    fn get_current_pane_id(&self) -> Option<String> {
        self.client.get_current_pane_id()
    }

    fn get_client_focus(&self, client_tty: Option<&str>) -> Option<ClientFocus> {
        self.client.get_client_focus(client_tty)
    }

    fn list_sidebar_panes(&self, session_name: Option<&str>) -> Vec<SidebarPane> {
        let panes = match session_name {
            Some(session_name) => self.client.list_panes(PaneScope::Session(session_name)),
            None => self.client.list_panes(PaneScope::All),
        };
        let mut window_widths = HashMap::new();
        for pane in &panes {
            let width = pane.right.saturating_add(1);
            window_widths
                .entry(pane.window_id.clone())
                .and_modify(|current: &mut u16| *current = (*current).max(width))
                .or_insert(width);
        }

        let mut seen_pane_ids = HashSet::new();
        panes
            .into_iter()
            .filter(|pane| {
                pane.title == "opensessions-sidebar" && pane.session_name != STASH_SESSION
            })
            .filter(|pane| seen_pane_ids.insert(pane.id.clone()))
            .map(|pane| SidebarPane {
                pane_id: pane.id,
                session_name: pane.session_name,
                window_id: pane.window_id.clone(),
                width: Some(pane.width),
                window_width: window_widths.get(&pane.window_id).copied(),
            })
            .collect()
    }

    fn list_visible_sidebar_pane_ids(&self) -> Vec<String> {
        self.client.list_visible_sidebar_pane_ids()
    }

    fn list_viewed_panes(&self) -> Vec<ViewedPane> {
        self.client
            .list_viewed_panes()
            .into_iter()
            .map(|(session_name, pane_id)| ViewedPane {
                session_name,
                pane_id,
            })
            .collect()
    }

    fn list_agent_panes(&self, session_name: &str) -> Vec<AgentPane> {
        self.client
            .list_panes(PaneScope::Session(session_name))
            .into_iter()
            .filter(|pane| pane.title != "opensessions-sidebar")
            .filter_map(|pane| agent_from_pane(&pane).map(|agent| (pane, agent)))
            .map(|(pane, agent)| AgentPane {
                thread_name: thread_name_from_pane(&pane, &agent),
                agent,
                pane_id: pane.id,
                active: pane.active,
                thread_id: None,
            })
            .collect()
    }

    fn hide_sidebar(&self, pane_id: &str) {
        self.client.kill_pane(pane_id);
    }

    fn kill_sidebar_pane(&self, pane_id: &str) {
        self.client.kill_pane(pane_id);
    }

    fn prepare_sidebar_window(&self, window_id: &str) {
        self.client.ensure_window_remain_on_exit(window_id);
    }

    fn restore_windows_without_sidebar(&self) {
        self.client
            .restore_remain_on_exit_for_windows_without_sidebar();
    }

    fn focus_pane(&self, pane_id: &str) {
        let window_id = self.client.display("#{window_id}", Some(pane_id));
        if !window_id.is_empty() {
            self.client.select_window(&window_id);
        }
        self.client.select_pane(pane_id);
        self.client.flash_pane(pane_id);
    }

    fn kill_pane(&self, pane_id: &str) {
        self.client.kill_pane(pane_id);
    }

    fn resolve_agent_pane_id(
        &self,
        session: &str,
        agent: &str,
        _thread_id: Option<&str>,
        thread_name: Option<&str>,
    ) -> Option<String> {
        // This pane may be killed, so never guess: an Amp thread must match
        // its "<thread> - amp - <dir>" title exactly, and otherwise the
        // session must have exactly one pane running the agent.
        let mut agent_panes = self
            .client
            .list_panes(PaneScope::Session(session))
            .into_iter()
            .filter(|pane| pane.title != "opensessions-sidebar")
            .filter(|pane| agent_from_pane(pane).as_deref() == Some(agent));

        if agent == "amp"
            && let Some(thread_name) = thread_name
        {
            let matches = agent_panes
                .filter(|pane| thread_name_from_pane(pane, agent).as_deref() == Some(thread_name))
                .collect::<Vec<_>>();
            return (matches.len() == 1).then(|| matches[0].id.clone());
        }

        let pane = agent_panes.next()?;
        agent_panes.next().is_none().then_some(pane.id)
    }

    fn resize_sidebar_pane(&self, pane_id: &str, width: u16) {
        self.client
            .resize_pane_widths(&[pane_id.to_string()], width);
    }

    fn resize_sidebar_panes(&self, pane_ids: &[String], width: u16) {
        self.client.resize_pane_widths(pane_ids, width);
    }

    fn kill_orphaned_sidebar_panes(&self) {
        self.kill_orphaned_sidebar_panes_with_fallbacks(&HashMap::new());
    }

    fn kill_orphaned_sidebar_panes_with_fallbacks(
        &self,
        fallback_sessions: &HashMap<String, String>,
    ) {
        let panes = self.client.list_panes(PaneScope::All);
        let windows_by_session = self
            .client
            .list_sessions()
            .into_iter()
            .map(|session| (session.name, session.window_count))
            .collect::<HashMap<_, _>>();
        let mut window_pane_counts: HashMap<String, u32> = HashMap::new();
        let mut sidebars_by_window: HashMap<String, Vec<String>> = HashMap::new();
        let mut session_by_window: HashMap<String, String> = HashMap::new();
        let mut seen_pane_ids = HashSet::new();

        for pane in panes {
            if pane.session_name == STASH_SESSION || !seen_pane_ids.insert(pane.id.clone()) {
                continue;
            }
            session_by_window
                .entry(pane.window_id.clone())
                .or_insert_with(|| pane.session_name.clone());
            *window_pane_counts
                .entry(pane.window_id.clone())
                .or_insert(0) += 1;
            if pane.title == "opensessions-sidebar" {
                sidebars_by_window
                    .entry(pane.window_id)
                    .or_default()
                    .push(pane.id);
            }
        }

        for (window_id, sidebars) in sidebars_by_window {
            if window_pane_counts.get(&window_id) == Some(&1) {
                if let Some(session_name) = session_by_window.get(&window_id)
                    && windows_by_session.get(session_name).copied().unwrap_or(1) <= 1
                    && let Some(fallback_session) = fallback_sessions.get(session_name)
                {
                    for client in self.client.list_clients() {
                        if client.session_name == *session_name {
                            self.client
                                .switch_client_to_session(fallback_session, Some(&client.tty));
                        }
                    }
                }
                for pane_id in sidebars {
                    self.client.kill_pane(&pane_id);
                }
                continue;
            }
            for pane_id in sidebars.into_iter().skip(1) {
                self.client.kill_pane(&pane_id);
            }
        }
    }

    fn spawn_sidebar(
        &self,
        session_name: &str,
        window_id: &str,
        width: u16,
        position: SidebarPosition,
        scripts_dir: &str,
    ) -> Option<String> {
        let panes = self.client.list_panes(PaneScope::Window(window_id));
        let target = match position {
            SidebarPosition::Left => panes.iter().min_by_key(|pane| pane.left),
            SidebarPosition::Right => panes.iter().max_by_key(|pane| pane.right),
        }?;
        // Resolve the script path against `$OPENSESSIONS_DIR` so the spawned
        // pane works even when the parent pane's cwd is unrelated to the
        // workspace (e.g. tmux sessions whose default cwd is `$HOME`). Falls
        // back to the literal path if the env is unset.
        let command = format!(
            "OPENSESSIONS_SESSION_NAME={} OPENSESSIONS_WINDOW_ID={} REFOCUS_WINDOW={} exec \"${{OPENSESSIONS_DIR:-.}}\"/{scripts_dir}/start.sh",
            shell_quote(session_name),
            shell_quote(window_id),
            shell_quote(window_id),
        );
        let new_pane = self.client.split_sidebar_pane(
            &target.id,
            position == SidebarPosition::Left,
            width,
            &command,
        )?;
        self.client
            .set_pane_title(&new_pane.id, "opensessions-sidebar");
        self.client.ensure_window_remain_on_exit(window_id);
        Some(new_pane.id)
    }

    fn get_all_pane_counts(&self) -> HashMap<String, u32> {
        self.client.get_all_pane_counts()
    }
}

fn session_format() -> &'static str {
    "#{session_id}\t#{session_name}\t#{session_created}\t#{session_attached}\t#{session_windows}\t#{session_path}"
}

fn window_format() -> &'static str {
    "#{window_id}\t#{session_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{window_active}\t#{window_panes}"
}

fn client_format() -> &'static str {
    "#{client_name}\t#{client_tty}\t#{client_pid}\t#{session_name}\t#{client_width}\t#{client_height}"
}

fn pane_format() -> &'static str {
    "#{pane_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{pane_index}\t#{pane_active}\t#{pane_tty}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_title}\t#{pane_width}\t#{pane_height}\t#{pane_left}\t#{pane_right}"
}

/// Proportional repairs whose window layout changed after listing are retried
/// with a fresh listing; the last attempt falls back to a sidebar-only resize
/// so Fixed Sidebar Width still wins under continuous layout churn.
const SIDEBAR_REPAIR_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaleLayoutRepair {
    /// Leave the window untouched; `resize_pane_widths` re-lists panes and
    /// retries sidebars that still have the wrong width. No output is used as
    /// a signal: tmux 3.4 drops `if-shell` command output under `run-shell`.
    Skip,
    /// Restore only the sidebar width.
    SidebarOnly,
}

/// Builds one sidebar's repair command for the global `run-shell` script.
///
/// Content-pane widths are absolute values computed from `panes`. Applying
/// them after the window changed (for example, a stale session expanding on
/// return) gives the reclaimed space to one content pane, and so does a
/// sidebar-only resize computed before the change; once the sidebar is
/// correct, no later pass restores the content proportions. The sidebar and
/// content resizes therefore run as one atomic tmux command guarded by the
/// window layout listed with `panes`.
fn sidebar_width_repair_script(
    panes: &[PaneInfo],
    layouts: &HashMap<String, String>,
    sidebar_id: &str,
    width: u16,
    stale_layout: StaleLayoutRepair,
) -> String {
    let resize =
        |pane: &str, width: u16| format!("resize-pane -t {} -x {width}", shell_quote(pane));
    let content = flat_content_width_repairs(panes, sidebar_id, width);
    // Guard even when no content repair is needed: a sidebar-only resize
    // applied after the window changed would also skew the content panes.
    let layout = panes
        .iter()
        .find(|pane| pane.id == sidebar_id)
        .and_then(|sidebar| layouts.get(&sidebar.window_id));
    let Some(layout) = layout else {
        return format!("tmux {} >/dev/null 2>&1 || true", resize(sidebar_id, width));
    };
    let guarded = std::iter::once(resize(sidebar_id, width))
        .chain(content.iter().map(|(pane, width)| resize(pane, *width)))
        .collect::<Vec<_>>()
        .join(" ; ");
    let otherwise = match stale_layout {
        StaleLayoutRepair::Skip => String::new(),
        StaleLayoutRepair::SidebarOnly => format!(" {}", shell_quote(&resize(sidebar_id, width))),
    };
    format!(
        "tmux if-shell -F -t {} {} {}{otherwise} >/dev/null 2>&1 || true",
        shell_quote(sidebar_id),
        shell_quote(&window_layout_guard(layout)),
        shell_quote(&guarded),
    )
}

/// An `if-shell -F` condition that holds while the window still has `layout`.
///
/// `run-shell` format-expands its script first, so `##` reaches `if-shell` as
/// `#`; the layout's `,` and `}` are escaped as `#,` and `#}` for that pass.
fn window_layout_guard(layout: &str) -> String {
    let escaped = layout.replace(',', "##,").replace('}', "##}");
    format!("##{{==:##{{window_layout}},{escaped}}}")
}

fn flat_content_width_repairs(
    panes: &[PaneInfo],
    sidebar_id: &str,
    sidebar_width: u16,
) -> Vec<(String, u16)> {
    let Some(sidebar) = panes.iter().find(|pane| pane.id == sidebar_id) else {
        return Vec::new();
    };
    if sidebar.width == sidebar_width {
        return Vec::new();
    }

    let mut window_panes = panes
        .iter()
        .filter(|pane| pane.window_id == sidebar.window_id)
        .collect::<Vec<_>>();
    window_panes.sort_by_key(|pane| pane.left);
    if window_panes.len() < 3
        || window_panes
            .iter()
            .any(|pane| pane.height != sidebar.height)
    {
        return Vec::new();
    }

    let content = window_panes
        .into_iter()
        .filter(|pane| pane.id != sidebar_id)
        .collect::<Vec<_>>();
    let old_content_width = content
        .iter()
        .map(|pane| u32::from(pane.width))
        .sum::<u32>();
    let new_content_width =
        old_content_width as i32 + i32::from(sidebar.width) - i32::from(sidebar_width);
    if old_content_width == 0 || new_content_width < content.len() as i32 {
        return Vec::new();
    }

    let mut remaining = new_content_width as u32;
    let mut repairs = Vec::new();
    for (index, pane) in content.iter().enumerate().take(content.len() - 1) {
        let panes_after = (content.len() - index - 1) as u32;
        let proportional = ((new_content_width as u32 * u32::from(pane.width))
            + old_content_width / 2)
            / old_content_width;
        let pane_width = proportional.clamp(1, remaining.saturating_sub(panes_after));
        repairs.push((pane.id.clone(), pane_width as u16));
        remaining = remaining.saturating_sub(pane_width);
    }
    repairs
}

fn state_fingerprint_format() -> &'static str {
    "#{session_id}\t#{session_name}\t#{session_created}\t#{session_attached}\t#{session_windows}\t#{session_path}\t#{window_id}\t#{window_index}\t#{window_name}\t#{window_active}\t#{window_panes}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_pid}\t#{pane_current_path}\t#{pane_current_command}\t#{pane_title}\t#{pane_width}\t#{pane_height}\t#{pane_left}\t#{pane_right}"
}

fn agent_from_pane(pane: &PaneInfo) -> Option<String> {
    let title = pane.title.to_lowercase();
    let command = pane.command.to_lowercase();
    if title == "pi" || title.starts_with("pi ") || title.starts_with('π') || command == "pi" {
        return Some("pi".to_string());
    }
    // Amp titles its pane "<thread> - amp - <dir>"; the thread name may
    // mention other agents, so the structured form wins.
    if title.contains(AMP_TITLE_SEPARATOR) {
        return Some("amp".to_string());
    }
    // Whole words only: "sample.rs" or "timestamp" must not read as Amp.
    // The running command is more reliable than free-form title text.
    for text in [&command, &title] {
        let words = agent_words(text);
        for (agent, aliases) in AGENT_ALIASES {
            if aliases.iter().any(|alias| words.contains(alias)) {
                return Some((*agent).to_string());
            }
        }
    }
    None
}

const AMP_TITLE_SEPARATOR: &str = " - amp - ";

/// Words of a pane title or command; hyphens and underscores stay inside a
/// word so names like `amp-local` and `claude-code` are matched whole.
fn agent_words(text: &str) -> Vec<&str> {
    text.split(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == '_'))
        .filter(|word| !word.is_empty())
        .collect()
}

// Keep this broad and process/title based for zero-config agent
// awareness. Transcript/file watchers still provide richer status where we
// have native integrations; this path makes panes from other popular CLIs show
// up immediately instead of disappearing from the sidebar.
const AGENT_ALIASES: &[(&str, &[&str])] = &[
    ("amp", &["amp", "amp-local"]),
    ("claude-code", &["claude", "claude-code"]),
    ("codex", &["codex"]),
    ("gemini", &["gemini"]),
    ("cursor", &["cursor", "cursor-agent"]),
    ("antigravity", &["agy", "antigravity", "antigravity-cli"]),
    ("cline", &["cline"]),
    ("opencode", &["opencode", "open-code"]),
    ("github-copilot", &["copilot", "github-copilot", "ghcs"]),
    ("kimi", &["kimi", "kimi-code"]),
    ("kiro", &["kiro", "kiro-cli"]),
    ("droid", &["droid"]),
    ("grok", &["grok", "grok-build"]),
    ("hermes", &["hermes", "hermes-agent"]),
    ("qodercli", &["qodercli", "qoderclicn", "qoder", "qodercn"]),
];

fn thread_name_from_pane(pane: &PaneInfo, agent: &str) -> Option<String> {
    let title = pane.title.trim();
    if agent == "amp"
        && let Some((thread_name, _)) = title.split_once(" - amp - ")
    {
        let thread_name = thread_name.trim();
        if !thread_name.is_empty() {
            return Some(thread_name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod agent_pane_tests {
    use super::*;

    /// Serves one fixed `list-panes` table: `(pane id, command, title)`.
    struct PaneTableRunner(Vec<(&'static str, &'static str, &'static str)>);

    impl CommandRunner for PaneTableRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            let stdout = if args.first().map(String::as_str) == Some("list-panes") {
                self.0
                    .iter()
                    .map(|(id, command, title)| {
                        format!(
                            "{id}\twork\t@1\t0\t0\t0\t/dev/ttys1\t10\t/tmp\t{command}\t{title}\t80\t24\t0\t79"
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            };
            CommandOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            }
        }
    }

    fn provider(panes: Vec<(&'static str, &'static str, &'static str)>) -> TmuxProvider {
        TmuxProvider::new(Arc::new(PaneTableRunner(panes)))
    }

    #[test]
    fn agent_panes_are_detected_by_whole_words_not_substrings() {
        let provider = provider(vec![
            ("%1", "nvim", "sample.rs"),
            ("%2", "zsh", "timestamp"),
            ("%3", "zsh", "campaign - notes"),
            ("%4", "node", "Fix focus - amp - repo"),
            ("%5", "amp", "zsh"),
            ("%6", "node", "✳ Claude Code"),
            ("%7", "claude", "host.local"),
            ("%8", "codex", "repo"),
            ("%9", "amp-local", "repo"),
            ("%10", "node", "Debug cursor - amp - repo"),
            ("%11", "zsh", "my-codex-notes"),
        ]);

        let agents = provider
            .list_agent_panes("work")
            .into_iter()
            .map(|pane| (pane.pane_id, pane.agent, pane.thread_name))
            .collect::<Vec<_>>();

        let pane = |id: &str, agent: &str, thread: Option<&str>| {
            (
                id.to_string(),
                agent.to_string(),
                thread.map(str::to_string),
            )
        };
        assert_eq!(
            agents,
            vec![
                pane("%4", "amp", Some("Fix focus")),
                pane("%5", "amp", None),
                pane("%6", "claude-code", None),
                pane("%7", "claude-code", None),
                pane("%8", "codex", None),
                pane("%9", "amp", None),
                pane("%10", "amp", Some("Debug cursor")),
            ]
        );
    }

    #[test]
    fn viewed_panes_are_the_active_windows_of_attached_sessions() {
        struct ViewedRunner(std::sync::Mutex<Vec<Vec<String>>>);
        impl CommandRunner for ViewedRunner {
            fn run(&self, args: &[String]) -> CommandOutput {
                self.0.lock().unwrap().push(args.to_vec());
                CommandOutput {
                    exit_code: 0,
                    stdout: "work\t%1\nwork\t%2\nreview\t%2\n".to_string(),
                    stderr: String::new(),
                }
            }
        }
        let runner = Arc::new(ViewedRunner(Default::default()));
        let provider = TmuxProvider::new(runner.clone());

        let viewed = provider
            .list_viewed_panes()
            .into_iter()
            .map(|pane| (pane.session_name, pane.pane_id))
            .collect::<Vec<_>>();

        let pair = |session: &str, pane: &str| (session.to_string(), pane.to_string());
        assert_eq!(
            viewed,
            vec![pair("work", "%1"), pair("work", "%2"), pair("review", "%2")]
        );
        assert_eq!(
            runner.0.lock().unwrap().as_slice(),
            [[
                "list-panes",
                "-a",
                "-f",
                "#{&&:#{session_attached},#{window_active}}",
                "-F",
                "#{session_name}\t#{pane_id}",
            ]
            .map(str::to_string)
            .to_vec()]
        );
    }

    #[test]
    fn resolving_an_amp_pane_matches_the_real_title_format_or_fails() {
        let two_amp_panes = provider(vec![
            ("%1", "zsh", "sample.rs"),
            ("%2", "node", "Fix focus - amp - repo"),
            ("%3", "node", "Fix focus later - amp - repo"),
        ]);
        let resolve =
            |thread_name| two_amp_panes.resolve_agent_pane_id("work", "amp", None, thread_name);

        assert_eq!(resolve(Some("Fix focus")).as_deref(), Some("%2"));
        assert_eq!(resolve(Some("Fix focus later")).as_deref(), Some("%3"));
        assert_eq!(
            resolve(Some("Unknown thread")),
            None,
            "a thread with no matching pane is not guessed"
        );
        assert_eq!(resolve(None), None, "two Amp panes are ambiguous");

        let single = provider(vec![
            ("%1", "zsh", "sample.rs"),
            ("%2", "node", "Fix focus - amp - repo"),
        ]);
        assert_eq!(
            single
                .resolve_agent_pane_id("work", "amp", None, None)
                .as_deref(),
            Some("%2")
        );
        assert_eq!(
            single.resolve_agent_pane_id("work", "codex", None, None),
            None
        );
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_sessions(raw: &str) -> Vec<SessionInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            SessionInfo {
                id: part(&parts, 0),
                name: part(&parts, 1),
                created_at: parse_u64(&parts, 2),
                attached_clients: parse_u32(&parts, 3),
                window_count: parse_u32(&parts, 4),
                dir: part(&parts, 5),
            }
        })
        .collect()
}

fn parse_windows(raw: &str) -> Vec<WindowInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            WindowInfo {
                id: part(&parts, 0),
                session_id: part(&parts, 1),
                session_name: part(&parts, 2),
                index: parse_u32(&parts, 3),
                name: part(&parts, 4),
                active: part(&parts, 5) == "1",
                pane_count: parse_u32(&parts, 6),
            }
        })
        .collect()
}

fn parse_clients(raw: &str) -> Vec<ClientInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts = split(line);
            ClientInfo {
                name: part(&parts, 0),
                tty: part(&parts, 1),
                pid: parse_u32(&parts, 2),
                session_name: part(&parts, 3),
                width: parse_u16(&parts, 4),
                height: parse_u16(&parts, 5),
            }
        })
        .collect()
}

fn parse_panes(raw: &str) -> Vec<PaneInfo> {
    raw.lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let parts = split(line);
            if parts.len() < 15 {
                return None;
            }
            Some(PaneInfo {
                id: part(&parts, 0),
                session_name: part(&parts, 1),
                window_id: part(&parts, 2),
                window_index: parse_u32(&parts, 3),
                index: parse_u32(&parts, 4),
                active: part(&parts, 5) == "1",
                tty: part(&parts, 6),
                pid: parse_u32(&parts, 7),
                cwd: part(&parts, 8),
                command: part(&parts, 9),
                title: part(&parts, 10),
                width: parse_u16(&parts, 11),
                height: parse_u16(&parts, 12),
                left: parse_u16(&parts, 13),
                right: parse_u16(&parts, 14),
            })
        })
        .collect()
}

fn split(line: &str) -> Vec<&str> {
    line.split(SEP).collect()
}

fn part(parts: &[&str], index: usize) -> String {
    parts.get(index).copied().unwrap_or_default().to_string()
}

fn parse_u16(parts: &[&str], index: usize) -> u16 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or_default()
}

fn parse_u32(parts: &[&str], index: usize) -> u32 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or_default()
}

fn parse_u64(parts: &[&str], index: usize) -> u64 {
    parts
        .get(index)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            CommandOutput {
                exit_code: 0,
                stdout: "/dev/ttys001\topensessions\t@0\t%186".to_string(),
                stderr: String::new(),
            }
        }
    }

    #[derive(Default)]
    struct VisibilityRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[derive(Default)]
    struct WindowRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[derive(Default)]
    struct ClientRoutingRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    struct MouseBindingRunner {
        calls: Mutex<Vec<Vec<String>>>,
        binding: String,
    }

    impl MouseBindingRunner {
        fn new(binding: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                binding: binding.to_string(),
            }
        }
    }

    impl CommandRunner for MouseBindingRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            CommandOutput {
                exit_code: 0,
                stdout: (args.first().map(String::as_str) == Some("list-keys"))
                    .then(|| self.binding.clone())
                    .unwrap_or_default(),
                stderr: String::new(),
            }
        }
    }

    impl CommandRunner for WindowRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            let stdout = match args.first().map(String::as_str) {
                Some("list-windows") => concat!(
                    "@1\t$1\tproject\t0\tcvm\t1\t2\n",
                    "@2\t$1\tproject\t1\tregression\t0\t2\n",
                    "@4\t$1\tproject\t2\tnotes\t0\t1\n",
                    "@2\t$2\tother\t0\tshared-regression\t1\t2\n",
                    "@3\t$2\tother\t1\tzsh\t0\t1"
                ),
                Some("list-panes") => concat!(
                    "%1\tproject\t@1\t0\t0\t1\t/dev/ttys1\t10\t/tmp\tamp\tAgent\t80\t24\t0\t79\n",
                    "%2\tproject\t@1\t0\t1\t0\t/dev/ttys2\t11\t/tmp\tbun\topensessions-sidebar\t36\t24\t0\t35\n",
                    "%3\tproject\t@2\t1\t0\t1\t/dev/ttys3\t12\t/tmp\tzsh\tzsh\t80\t24\t0\t79"
                ),
                _ => "",
            };
            CommandOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        }
    }

    impl CommandRunner for ClientRoutingRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            CommandOutput {
                exit_code: 0,
                stdout: (args.first().map(String::as_str) == Some("list-clients"))
                    .then_some(concat!(
                        "client-a\t/dev/ttys001\t10\ttarget\t160\t40\n",
                        "client-b\t/dev/ttys002\t11\tother\t160\t40"
                    ))
                    .unwrap_or_default()
                    .to_string(),
                stderr: String::new(),
            }
        }
    }

    impl CommandRunner for VisibilityRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            CommandOutput {
                exit_code: 0,
                stdout: (args.first().map(String::as_str) == Some("list-panes"))
                    .then_some("%1")
                    .unwrap_or_default()
                    .to_string(),
                stderr: String::new(),
            }
        }
    }

    #[test]
    fn cleanup_restores_remain_on_exit_after_sidebar_panes_are_gone() {
        struct RestoreRunner {
            calls: Mutex<Vec<Vec<String>>>,
        }

        impl CommandRunner for RestoreRunner {
            fn run(&self, args: &[String]) -> CommandOutput {
                self.calls.lock().unwrap().push(args.to_vec());
                // Sidebar clients already exited and `pane-died` removed their
                // panes, so only the per-window markers remain.
                let stdout = match args.first().map(String::as_str) {
                    Some("list-windows") => concat!(
                        "@1\toff\n",
                        "@2\t\n",
                        "@3\tfailed\n",
                        "@4\t__inherited__\n",
                        "@1\toff",
                    ),
                    _ => "",
                };
                CommandOutput {
                    exit_code: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }
            }
        }

        let runner = Arc::new(RestoreRunner {
            calls: Mutex::new(Vec::new()),
        });
        let provider = TmuxProvider::new(runner.clone());

        provider.cleanup_hooks();

        let calls = runner.calls.lock().unwrap();
        let option_calls = calls
            .iter()
            .filter(|call| call.first().map(String::as_str) == Some("set-window-option"))
            .map(|call| call[1..].join(" "))
            .collect::<Vec<_>>();
        assert_eq!(
            option_calls,
            vec![
                "-t @1 remain-on-exit off",
                "-t @1 -u @opensessions_remain_on_exit_previous",
                "-t @3 remain-on-exit failed",
                "-t @3 -u @opensessions_remain_on_exit_previous",
                "-t @4 -u remain-on-exit",
                "-t @4 -u @opensessions_remain_on_exit_previous",
            ],
        );
        assert_eq!(
            calls[0],
            vec![
                "list-windows".to_string(),
                "-a".to_string(),
                "-F".to_string(),
                "#{window_id}\t#{@opensessions_remain_on_exit_previous}".to_string(),
            ],
        );
    }

    #[test]
    fn client_focus_uses_tmux_target_client_not_window_active_state() {
        let runner = Arc::new(RecordingRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        let focus = provider
            .get_client_focus(Some("/dev/ttys001"))
            .expect("client focus");

        assert_eq!(focus.client_tty.as_deref(), Some("/dev/ttys001"));
        assert_eq!(focus.session_name, "opensessions");
        assert_eq!(focus.window_id, "@0");
        assert_eq!(focus.pane_id, "%186");
        assert_eq!(
            runner.calls.lock().unwrap()[0],
            vec![
                "display-message".to_string(),
                "-c".to_string(),
                "/dev/ttys001".to_string(),
                "-p".to_string(),
                "#{client_tty}\t#{session_name}\t#{window_id}\t#{pane_id}".to_string(),
            ],
        );
    }

    #[test]
    fn session_close_rehomes_the_client_actually_attached_to_the_target() {
        let runner = Arc::new(ClientRoutingRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        assert!(provider.switch_clients_from_session("target", "fallback", Some("/dev/stale")));

        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            &[
                vec![
                    "list-clients".to_string(),
                    "-F".to_string(),
                    client_format().to_string(),
                ],
                vec![
                    "switch-client".to_string(),
                    "-c".to_string(),
                    "/dev/ttys001".to_string(),
                    "-t".to_string(),
                    "=fallback:".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn visible_sidebars_require_an_attached_client_and_active_window() {
        let runner = Arc::new(VisibilityRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        assert_eq!(provider.list_visible_sidebar_pane_ids(), vec!["%1"]);
        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            &[vec![
                "list-panes".to_string(),
                "-a".to_string(),
                "-f".to_string(),
                "#{&&:#{session_attached},#{window_active},#{==:#{pane_title},opensessions-sidebar}}"
                    .to_string(),
                "-F".to_string(),
                "#{pane_id}".to_string(),
            ]],
        );
    }

    #[test]
    fn default_mouse_border_drag_reports_explicit_sidebar_width_intent() {
        let runner = Arc::new(MouseBindingRunner::new(
            "bind-key -T root MouseDrag1Border resize-pane -M",
        ));
        let client = TmuxClient::new(runner.clone());

        client
            .setup_sidebar_mouse_resize_binding("http://127.0.0.1:1234", "/tmp/opensessions.token");

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        let binding = &calls[1];
        assert_eq!(
            &binding[..6],
            [
                "bind-key",
                "-T",
                "root",
                "MouseDrag1Border",
                "run-shell",
                "tmux -S #{socket_path} set-option -gq @opensessions_mouse_resize_window '#{mouse_window}'",
            ]
        );
        assert!(binding.contains(&"resize-pane".to_string()));
        assert!(binding.contains(&"-M".to_string()));
        assert!(binding.last().is_some_and(|script| {
            script.contains("-X POST http://127.0.0.1:1234/set-sidebar-width")
                && script.contains("-d \"$width\"")
        }));
    }

    #[test]
    fn custom_mouse_border_binding_is_not_overridden() {
        let runner = Arc::new(MouseBindingRunner::new(
            "bind-key -T root MouseDrag1Border display-message custom",
        ));
        let client = TmuxClient::new(runner.clone());

        client
            .setup_sidebar_mouse_resize_binding("http://127.0.0.1:1234", "/tmp/opensessions.token");

        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn window_manager_switches_by_stable_id_and_never_kills_active_window() {
        let runner = Arc::new(WindowRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        assert_eq!(
            provider.list_windows("project"),
            vec![
                MuxWindowInfo {
                    id: "@1".to_string(),
                    index: 0,
                    name: "cvm".to_string(),
                    active: true,
                    pane_commands: vec!["amp".to_string()],
                },
                MuxWindowInfo {
                    id: "@2".to_string(),
                    index: 1,
                    name: "regression".to_string(),
                    active: false,
                    pane_commands: vec!["zsh".to_string()],
                },
                MuxWindowInfo {
                    id: "@4".to_string(),
                    index: 2,
                    name: "notes".to_string(),
                    active: false,
                    pane_commands: Vec::new(),
                },
            ]
        );

        provider.switch_window("project", "@2", Some("/dev/ttys001"));
        provider.kill_windows(
            "project",
            &[
                "@1".to_string(),
                "@2".to_string(),
                "@3".to_string(),
                "@4".to_string(),
            ],
        );

        let calls = runner.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|call| { call == &["switch-client", "-c", "/dev/ttys001", "-t", "=project"] })
        );
        assert!(
            calls
                .iter()
                .any(|call| call == &["select-window", "-t", "@2"])
        );
        assert!(
            calls
                .iter()
                .any(|call| call == &["unlink-window", "-t", "=project:@2"])
        );
        assert!(
            calls
                .iter()
                .any(|call| call == &["kill-window", "-t", "@4"])
        );
        assert!(!calls.iter().any(|call| {
            matches!(
                call.first().map(String::as_str),
                Some("unlink-window" | "kill-window")
            ) && !matches!(call.last().map(String::as_str), Some("=project:@2" | "@4"))
        }));
    }

    #[test]
    fn state_fingerprint_uses_one_tmux_snapshot() {
        let runner = Arc::new(RecordingRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        assert!(provider.state_fingerprint().is_some());
        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            &[vec![
                "list-panes".to_string(),
                "-a".to_string(),
                "-F".to_string(),
                state_fingerprint_format().to_string(),
            ]],
        );
    }

    #[test]
    fn sidebar_width_repairs_are_sent_in_one_tmux_invocation() {
        let runner = Arc::new(RecordingRunner::default());
        let provider = TmuxProvider::new(runner.clone());

        provider.resize_sidebar_panes(&["%1".to_string(), "%2".to_string()], 36);

        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            &[
                vec![
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    format!("{}\t#{{window_layout}}", pane_format()),
                ],
                vec![
                    "run-shell".to_string(),
                    "tmux resize-pane -t '%1' -x 36 >/dev/null 2>&1 || true; tmux resize-pane -t '%2' -x 36 >/dev/null 2>&1 || true".to_string(),
                ],
            ],
        );
    }

    #[test]
    fn proportional_sidebar_repair_is_guarded_by_the_listed_window_layout() {
        let panes = parse_panes(concat!(
            "%1\tproject\t@1\t0\t0\t0\t/dev/ttys1\t10\t/tmp\topensessions\topensessions-sidebar\t53\t39\t0\t52\n",
            "%2\tproject\t@1\t0\t1\t1\t/dev/ttys2\t11\t/tmp\tamp\tAgent 1\t53\t39\t54\t106\n",
            "%3\tproject\t@1\t0\t2\t0\t/dev/ttys3\t12\t/tmp\tamp\tAgent 2\t52\t39\t107\t158",
        ));
        let layouts = HashMap::from([(
            "@1".to_string(),
            "a1b2,159x39,0,0{53x39,0,0,1,53x39,54,0,2,52x39,107,0,3}".to_string(),
        )]);

        let guard = "'##{==:##{window_layout},a1b2##,159x39##,0##,0{53x39##,0##,0##,1##,53x39##,54##,0##,2##,52x39##,107##,0##,3##}}'";
        let guarded = r"'resize-pane -t '\''%1'\'' -x 36 ; resize-pane -t '\''%2'\'' -x 62'";
        assert_eq!(
            sidebar_width_repair_script(&panes, &layouts, "%1", 36, StaleLayoutRepair::Skip),
            format!("tmux if-shell -F -t '%1' {guard} {guarded} >/dev/null 2>&1 || true"),
        );
        assert_eq!(
            sidebar_width_repair_script(&panes, &layouts, "%1", 36, StaleLayoutRepair::SidebarOnly),
            format!(
                "tmux if-shell -F -t '%1' {guard} {guarded} {} >/dev/null 2>&1 || true",
                r"'resize-pane -t '\''%1'\'' -x 36'",
            ),
        );

        // An already-correct sidebar is guarded too, so its no-op resize
        // cannot shrink a neighbor after the window changes size.
        let panes = parse_panes(concat!(
            "%1\tproject\t@1\t0\t0\t0\t/dev/ttys1\t10\t/tmp\topensessions\topensessions-sidebar\t36\t39\t0\t35\n",
            "%2\tproject\t@1\t0\t1\t1\t/dev/ttys2\t11\t/tmp\tamp\tAgent 1\t123\t39\t37\t159",
        ));
        let layouts = HashMap::from([(
            "@1".to_string(),
            "c3d4,160x39,0,0{36x39,0,0,1,123x39,37,0,2}".to_string(),
        )]);
        assert_eq!(
            sidebar_width_repair_script(&panes, &layouts, "%1", 36, StaleLayoutRepair::Skip),
            concat!(
                "tmux if-shell -F -t '%1' ",
                "'##{==:##{window_layout},c3d4##,160x39##,0##,0{36x39##,0##,0##,1##,123x39##,37##,0##,2##}}' ",
                r"'resize-pane -t '\''%1'\'' -x 36' ",
                ">/dev/null 2>&1 || true",
            ),
        );
    }

    #[test]
    fn flat_content_width_repairs_preserve_content_proportions() {
        let panes = parse_panes(concat!(
            "%1\tproject\t@1\t0\t0\t0\t/dev/ttys1\t10\t/tmp\topensessions\topensessions-sidebar\t53\t39\t0\t52\n",
            "%2\tproject\t@1\t0\t1\t1\t/dev/ttys2\t11\t/tmp\tamp\tAgent 1\t53\t39\t54\t106\n",
            "%3\tproject\t@1\t0\t2\t0\t/dev/ttys3\t12\t/tmp\tamp\tAgent 2\t52\t39\t107\t158",
        ));

        assert_eq!(
            flat_content_width_repairs(&panes, "%1", 36),
            vec![("%2".to_string(), 62)],
        );
    }

    #[test]
    fn client_tty_for_pane_uses_client_active_pane_context() {
        struct ClientPaneRunner;

        impl CommandRunner for ClientPaneRunner {
            fn run(&self, _args: &[String]) -> CommandOutput {
                CommandOutput {
                    exit_code: 0,
                    stdout: "/dev/ttys001\t%186\n/dev/ttys002\t%22\n".to_string(),
                    stderr: String::new(),
                }
            }
        }

        let client = TmuxClient::new(Arc::new(ClientPaneRunner));

        assert_eq!(
            client.client_tty_for_pane("%186").as_deref(),
            Some("/dev/ttys001")
        );
        assert_eq!(client.client_tty_for_pane("%999"), None);
    }

    #[test]
    fn switching_sessions_focuses_the_destination_sidebar() {
        struct SwitchRunner {
            calls: Mutex<Vec<Vec<String>>>,
        }

        impl CommandRunner for SwitchRunner {
            fn run(&self, args: &[String]) -> CommandOutput {
                self.calls.lock().unwrap().push(args.to_vec());
                let stdout = match args.first().map(String::as_str) {
                    Some("list-windows") => "@2\t$2\tbeta\t0\tbeta\t1\t2",
                    Some("list-panes") => concat!(
                        "%2\tbeta\t@2\t0\t0\t1\t/dev/ttys2\t11\t/tmp\tzsh\tzsh\t80\t24\t0\t79\n",
                        "%3\tbeta\t@2\t0\t1\t0\t/dev/ttys3\t12\t/tmp\topensessions-sidebar\topensessions-sidebar\t36\t24\t0\t35"
                    ),
                    _ => "",
                };
                CommandOutput {
                    exit_code: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }
            }
        }

        let runner = Arc::new(SwitchRunner {
            calls: Mutex::new(Vec::new()),
        });
        let provider = TmuxProvider::new(runner.clone());

        provider.switch_session("beta", Some("/dev/ttys001"));

        assert!(runner.calls.lock().unwrap().contains(&vec![
            "select-pane".to_string(),
            "-t".to_string(),
            "%3".to_string(),
        ]));
    }

    struct PanePidRunner {
        output: CommandOutput,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for PanePidRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            self.calls.lock().unwrap().push(args.to_vec());
            self.output.clone()
        }
    }

    fn pane_pid_provider(exit_code: i32, stdout: &str) -> (Arc<PanePidRunner>, TmuxProvider) {
        let runner = Arc::new(PanePidRunner {
            output: CommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
            calls: Mutex::new(Vec::new()),
        });
        (runner.clone(), TmuxProvider::new(runner))
    }

    #[test]
    fn a_failed_session_listing_is_not_an_empty_one() {
        let failing = |stderr: &str| {
            TmuxProvider::new(Arc::new(PanePidRunner {
                output: CommandOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
                calls: Mutex::new(Vec::new()),
            }))
        };
        assert_eq!(
            pane_pid_provider(0, "").1.try_list_sessions(),
            Some(Vec::new())
        );
        for transient in [
            "No such file or directory (os error 2)",
            "error connecting to /tmp/tmux-501/default (Resource temporarily unavailable)",
            "",
        ] {
            assert_eq!(failing(transient).try_list_sessions(), None, "{transient}");
        }
        for gone in [
            "no server running on /tmp/tmux-501/default",
            "error connecting to /tmp/tmux-501/default (No such file or directory)",
            "server exited unexpectedly",
        ] {
            assert_eq!(
                failing(gone).try_list_sessions(),
                Some(Vec::new()),
                "{gone}"
            );
        }
        assert!(failing("").list_sessions().is_empty());
    }

    /// A stand-in tmux binary running `script`, removed on drop.
    struct FakeTmux(std::path::PathBuf);

    impl FakeTmux {
        fn new(name: &str, script: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let path = std::env::temp_dir().join(format!(
                "opensessions-fake-tmux-{name}-{}",
                std::process::id()
            ));
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self(path)
        }

        fn client(&self, timeout: Duration) -> TmuxClient {
            TmuxClient::new(Arc::new(StdCommandRunner::with_timeout(
                self.0.to_string_lossy(),
                timeout,
            )))
        }
    }

    impl Drop for FakeTmux {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_hung_tmux_command_fails_within_its_timeout() {
        let hung = FakeTmux::new("hung", "exec sleep 10");
        let started = std::time::Instant::now();

        let sessions = hung.client(Duration::from_millis(300)).try_list_sessions();

        assert_eq!(
            sessions, None,
            "a timed-out listing is a failure, not empty"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_prompt_tmux_command_is_unaffected_by_the_timeout() {
        let prompt = FakeTmux::new("prompt", "printf '$1\\twork\\t0\\t1\\t2\\t/tmp\\n'");

        let sessions = prompt
            .client(Duration::from_secs(5))
            .try_list_sessions()
            .expect("listing succeeds");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "work");
    }

    #[test]
    fn pane_pid_targets_the_exact_pane() {
        let (runner, provider) = pane_pid_provider(0, "4242");

        assert_eq!(provider.get_pane_pid("%7"), Some(4242));
        assert_eq!(
            runner.calls.lock().unwrap()[0],
            ["display-message", "-t", "%7", "-p", "#{pane_pid}"]
                .map(str::to_string)
                .to_vec(),
        );
    }

    #[test]
    fn pane_pid_is_unknown_for_missing_panes_or_bogus_output() {
        assert_eq!(pane_pid_provider(1, "").1.get_pane_pid("%7"), None);
        assert_eq!(pane_pid_provider(0, "").1.get_pane_pid("%7"), None);
        assert_eq!(pane_pid_provider(0, "1").1.get_pane_pid("%7"), None);
        assert_eq!(pane_pid_provider(0, "4242").1.get_pane_pid(""), None);
    }

    /// Emulates tmux global user options so visibility persistence can be
    /// exercised through the provider's public interface.
    #[derive(Default)]
    struct GlobalOptionRunner {
        options: Mutex<HashMap<String, String>>,
    }

    impl CommandRunner for GlobalOptionRunner {
        fn run(&self, args: &[String]) -> CommandOutput {
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            let mut options = self.options.lock().unwrap();
            let stdout = match args.as_slice() {
                ["set-option", "-gq", name, value] => {
                    options.insert(name.to_string(), value.to_string());
                    String::new()
                }
                ["set-option", "-gu", name] => {
                    options.remove(*name);
                    String::new()
                }
                ["show-option", "-gqv", name] => options.get(*name).cloned().unwrap_or_default(),
                _ => String::new(),
            };
            CommandOutput {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            }
        }
    }

    #[test]
    fn sidebar_visibility_preference_round_trips_through_tmux_global_option() {
        let provider = TmuxProvider::new(Arc::new(GlobalOptionRunner::default()));

        assert_eq!(provider.sidebar_visibility_preference(), None);
        provider.set_sidebar_visibility_preference(true);
        assert_eq!(provider.sidebar_visibility_preference(), Some(true));
        provider.set_sidebar_visibility_preference(false);
        assert_eq!(provider.sidebar_visibility_preference(), Some(false));
    }

    #[test]
    fn hook_cleanup_preserves_the_sidebar_visibility_preference() {
        let provider = TmuxProvider::new(Arc::new(GlobalOptionRunner::default()));
        provider.set_sidebar_visibility_preference(true);

        provider.cleanup_hooks();

        assert_eq!(provider.sidebar_visibility_preference(), Some(true));
    }
}
