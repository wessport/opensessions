use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime};

use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use opensessions_runtime::agent_watchers::{
    AgentWatcherSnapshot, amp_snapshot_from_log_jsonl, amp_snapshot_from_thread_json,
    claude_code_snapshot_from_jsonl, codex_snapshot_from_jsonl, codex_thread_id_from_path,
    decode_claude_project_dir, droid_snapshot_from_jsonl, opencode_snapshot_from_row,
    parse_codex_session_index, pi_snapshot_from_jsonl,
};
use opensessions_runtime::config::{
    OpensessionsConfig, load_config_from_home, save_config_to_home,
};
use opensessions_runtime::git_info::{GitInfo, parse_git_info_output};
use opensessions_runtime::metadata_store::SessionMetadataStore;
use opensessions_runtime::mux::{ActiveWindow, MuxProvider, SidebarPosition};
use opensessions_runtime::pi_runtime_registry::{PiRuntimeRegistry, parse_pi_runtime_info};
use opensessions_runtime::port_discovery::{PortDiscoveryInput, discover_session_ports};
use opensessions_runtime::project_dir_session::{
    build_dir_session_map, resolve_session_for_project_dir,
};
use opensessions_runtime::protocol::{
    AgentEvent, AgentLiveness, AgentPanelScope, AgentStatus, MetadataTone, ServerMessage,
    SessionFilterMode, WindowData,
};
use opensessions_runtime::server_state::{ReadOnlyStateInput, build_read_only_state};
use opensessions_runtime::session_order::SessionOrder;
use opensessions_runtime::sidebar_coordinator::{SidebarCoordinator, SidebarLifecycle};
use opensessions_runtime::sidebar_width_sync::clamp_sidebar_width;
use opensessions_runtime::tmux_provider::{StdCommandRunner, TmuxProvider};
use opensessions_runtime::tracker::{AgentTracker, PanePresenceInput};
use opensessions_sidebar_core::app::App as SidebarApp;
use opensessions_sidebar_core::generated::protocol::ServerMessage as SidebarServerMessage;
use serde_json::Value;
use sha1_smol::Sha1;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, broadcast};
use tokio::task::JoinHandle;
use tokio::time::{Duration, MissedTickBehavior};
use tokio_websockets::{Message, ServerBuilder};

pub const SERVER_VERSION: &str = "0.2.0-alpha.12";
pub const PROTOCOL_VERSION: u16 = 1;
pub const HELLO_JSON: &str = r#"{"type":"hello","protocol":1,"serverVersion":"0.2.0-alpha.12"}"#;
pub const QUIT_JSON: &str = r#"{"type":"quit"}"#;

const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONCURRENT_CONNECTIONS: usize = 128;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SIDEBAR_SCRIPTS_DIR: &str = "apps/tui/scripts";
const EXPENSIVE_DATA_POLL_MS: u64 = 10_000;
const EXPENSIVE_DATA_IDLE_MAX_MS: u64 = 60_000;
const RENDERED_SIDEBAR_FRAME_MS: u64 = 16;
const AGENT_WATCHER_POLL_MS: u64 = 2_000;
const TMUX_STATE_POLL_MS: u64 = 2_000;
const AGENT_WATCHER_IDLE_MAX_MS: u64 = 10_000;
const TMUX_STATE_IDLE_MAX_MS: u64 = 30_000;
const MISSING_TMUX_POLLS_BEFORE_SHUTDOWN: u32 = 2;
const SIDEBAR_WARMUP_MS: u64 = 1_200;
const SIDEBAR_LIFECYCLE_POLL_MS: u64 = 500;
const SIDEBAR_WIDTH_REPAIR_SETTLE_MS: u64 = 50;
const SERVER_SHUTDOWN_DRAIN_MS: u64 = 120;
const AGENT_WATCHER_RECENT_MS: u64 = 5 * 60 * 1000;
const AMP_LOG_TAIL_BYTES: u64 = 1024 * 1024;
const OPENCODE_SQL_TIMEOUT_MS: u64 = 500;
const OPENCODE_SQL_SEP: char = '\u{1f}';
const DEFAULT_DETAIL_PANEL_HEIGHT: u16 = 10;
const MIN_DETAIL_PANEL_HEIGHT: u16 = 4;
const MAX_DETAIL_PANEL_HEIGHT: u16 = 60;

#[derive(Debug, Default)]
struct ShutdownAnnouncement {
    announced: AtomicBool,
}

impl ShutdownAnnouncement {
    fn is_announced(&self) -> bool {
        self.announced.load(Ordering::Acquire)
    }

    fn announce_once(
        &self,
        state_source: &Option<Arc<dyn StateSource>>,
        state_updates: &broadcast::Sender<String>,
    ) {
        if self.announced.swap(true, Ordering::SeqCst) {
            return;
        }
        announce_shutdown(state_source, state_updates);
    }
}

#[derive(Debug, Default)]
struct SidebarWidthRepairScheduler {
    pending_requests: AtomicUsize,
    notify: Notify,
}

impl SidebarWidthRepairScheduler {
    fn request(&self) {
        self.pending_requests.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_one();
    }

    fn take_pending_requests(&self) -> usize {
        self.pending_requests.swap(0, Ordering::AcqRel)
    }
}

/// Append a single debug line when explicit diagnostics are enabled.
fn debug_log(line: impl AsRef<str>) {
    use std::io::Write;
    let Ok(path) = std::env::var("OPENSESSIONS_DEBUG_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(
            file,
            "[{now}] [server pid={}] {}",
            std::process::id(),
            line.as_ref()
        );
    }
}

pub trait StateSource: Send + Sync + 'static {
    fn snapshot_json(&self) -> String;

    fn setup_mux_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {}

    fn cleanup_mux_hooks(&self) {}

    fn cleanup_sidebar_clients(&self) {}

    fn mux_namespace_available(&self) -> bool {
        true
    }

    fn start_background_tasks(
        self: Arc<Self>,
        _state_updates: broadcast::Sender<String>,
        _shutdown: broadcast::Sender<()>,
    ) -> Vec<JoinHandle<()>> {
        Vec::new()
    }

    fn handle_client_command(&self, _command: &Value) -> Option<String> {
        None
    }

    fn handle_client_command_with_context(
        &self,
        command: &Value,
        _context: Option<&ClientConnectionContext>,
    ) -> Option<String> {
        self.handle_client_command(command)
    }

    fn handle_sender_command(&self, _command: &Value) -> Option<String> {
        None
    }

    fn handle_sender_command_with_context(
        &self,
        command: &Value,
        _context: &mut ClientConnectionContext,
    ) -> Option<String> {
        self.handle_sender_command(command)
    }

    fn handle_http_json(&self, _path: &str, _body: &Value) -> Option<String> {
        None
    }

    fn handle_http_text(&self, _path: &str, _body: &str) -> Option<String> {
        None
    }

    fn handle_http_hook(&self, _path: &str, _body: &str) -> Option<String> {
        None
    }

    fn handle_switch_index(&self, _index: u32, _body: &str) -> Option<String> {
        None
    }

    fn handle_agent_event_json(&self, _body: &Value) -> Result<String, AgentEventError> {
        Err(AgentEventError::CouldNotResolveSession)
    }

    fn handle_pi_runtime_upsert(&self, _body: &Value) -> Result<(), PiRuntimeError> {
        Err(PiRuntimeError::InvalidPayload)
    }

    fn handle_pi_runtime_delete(&self, _body: &Value) -> Result<(), PiRuntimeError> {
        Err(PiRuntimeError::MissingPid)
    }

    fn begin_shutdown(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClientConnectionContext {
    client_tty: Option<String>,
    pane_id: Option<String>,
    session_name: Option<String>,
    window_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEventError {
    MissingAgent,
    InvalidStatus,
    CouldNotResolveSession,
}

impl AgentEventError {
    fn status_and_body(self) -> (&'static str, &'static str) {
        match self {
            Self::MissingAgent => ("400 Bad Request", "missing agent"),
            Self::InvalidStatus => ("400 Bad Request", "invalid status"),
            // Agent events are intentionally broadcast to every opensessions
            // server in every tmux namespace. A server that cannot map the
            // event's projectDir/tmuxSession to one of its sessions should
            // no-op with a non-error status so the plugin can publish once and
            // let each server decide folder ownership locally. Use 202 (not
            // 204) so the plugin can distinguish "ignored by this server" from
            // "applied by an owning server" when deciding whether to retry
            // during owner-server restarts.
            Self::CouldNotResolveSession => ("202 Accepted", ""),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiRuntimeError {
    InvalidPayload,
    MissingPid,
}

impl PiRuntimeError {
    fn body(self) -> &'static str {
        match self {
            Self::InvalidPayload => "invalid pi runtime payload",
            Self::MissingPid => "missing pid",
        }
    }
}

impl<F> StateSource for F
where
    F: Fn() -> String + Send + Sync + 'static,
{
    fn snapshot_json(&self) -> String {
        self()
    }
}

pub trait PortCommandRunner: Send + Sync + 'static {
    fn process_rows(&self) -> Vec<(u32, u32)>;
    fn lsof_fields(&self) -> String;
}

pub trait GitCommandRunner: Send + Sync + 'static {
    fn git_info_output(&self, dir: &str) -> String;
}

#[derive(Debug, Default)]
struct SystemPortCommandRunner;

#[derive(Debug, Default)]
struct SystemGitCommandRunner;

impl PortCommandRunner for SystemPortCommandRunner {
    fn process_rows(&self) -> Vec<(u32, u32)> {
        let Ok(output) = process::Command::new("ps")
            .args(["-eo", "pid=,ppid="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_process_row)
            .collect()
    }

    fn lsof_fields(&self) -> String {
        let Ok(output) = process::Command::new("/usr/sbin/lsof")
            .args(["-iTCP", "-sTCP:LISTEN", "-nP", "-F", "pn"])
            .output()
        else {
            return String::new();
        };
        if !output.status.success() {
            return String::new();
        }
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

impl GitCommandRunner for SystemGitCommandRunner {
    fn git_info_output(&self, dir: &str) -> String {
        if dir.is_empty() {
            return String::new();
        }

        let Ok(rev_parse) = process::Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "--abbrev-ref", "HEAD", "--git-dir"])
            .output()
        else {
            return String::new();
        };
        if !rev_parse.status.success() {
            return String::new();
        }

        let Ok(status) = process::Command::new("git")
            .current_dir(dir)
            .args(["status", "--porcelain"])
            .output()
        else {
            return String::new();
        };

        let Ok(numstat) = process::Command::new("git")
            .current_dir(dir)
            .args(["diff", "--numstat", "HEAD", "--"])
            .output()
        else {
            return String::new();
        };

        format!(
            "{}\n---\n{}\n---NUMSTAT---\n{}",
            String::from_utf8_lossy(&rev_parse.stdout).trim(),
            String::from_utf8_lossy(&status.stdout).trim(),
            String::from_utf8_lossy(&numstat.stdout).trim()
        )
    }
}

#[derive(Debug, Clone)]
struct CachedGitInfo {
    info: GitInfo,
}

#[derive(Debug, Clone)]
struct CachedPortSnapshot {
    session_names: Vec<String>,
    ports_by_session: HashMap<String, Vec<u16>>,
}

pub struct ReadOnlyMuxStateSource {
    providers: Vec<Arc<dyn MuxProvider>>,
    port_command_runner: Arc<dyn PortCommandRunner>,
    port_snapshot_cache: Mutex<Option<CachedPortSnapshot>>,
    git_command_runner: Arc<dyn GitCommandRunner>,
    git_info_cache: Mutex<HashMap<String, CachedGitInfo>>,
    // The sidebar coordinator owns the single source of truth for the current
    // width (`SidebarCoordinator::state().width`), so there is no separate
    // mirror field to drift out of sync.
    sidebar_coordinator: Mutex<SidebarCoordinator>,
    sidebar_width_repairs: Arc<SidebarWidthRepairScheduler>,
    // Presence checks and pane spawning must be atomic across concurrent tmux
    // hooks. Otherwise two ensure requests can both observe a missing sidebar
    // and split duplicate panes into the same window.
    sidebar_presence: Mutex<()>,
    detail_panel_height: Mutex<u16>,
    agent_panel_scope: Mutex<AgentPanelScope>,
    focused_session: Mutex<Option<String>>,
    focused_pane_by_session: Mutex<HashMap<String, String>>,
    focused_client_tty: Mutex<Option<String>>,
    theme: Mutex<Option<String>>,
    transparent_background: Mutex<bool>,
    session_filter: Mutex<Option<SessionFilterMode>>,
    collapsed_worktree_groups: Mutex<HashSet<String>>,
    session_order: Mutex<SessionOrder>,
    metadata_store: Mutex<SessionMetadataStore>,
    agent_tracker: Mutex<AgentTracker>,
    pi_runtime_registry: Mutex<PiRuntimeRegistry>,
    tmux_socket_path: Option<PathBuf>,
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}

pub fn default_state_source_from_env(
    env: impl Fn(&str) -> Option<String>,
) -> Option<ReadOnlyMuxStateSource> {
    if let Some(tmux) = env("TMUX") {
        let provider = Arc::new(TmuxProvider::new(Arc::new(StdCommandRunner::default())));
        let mut source = ReadOnlyMuxStateSource::new(vec![provider]);
        if let Some(socket_path) = tmux.split(',').next().filter(|path| !path.is_empty()) {
            source = source.with_tmux_socket_path(socket_path);
        }
        let config = env("HOME")
            .map(PathBuf::from)
            .map(|home| load_config_from_home(&home));
        if let Some(width) = config.as_ref().and_then(|config| config.sidebar_width) {
            source = source.with_sidebar_width(clamp_sidebar_width(width) as u32);
        }
        if let Some(theme) = config
            .as_ref()
            .and_then(|config| config.theme.as_ref())
            .and_then(Value::as_str)
        {
            if theme == "transparent" {
                source = source
                    .with_theme("catppuccin-mocha")
                    .with_transparent_background(true);
            } else {
                source = source.with_theme(theme);
            }
        }
        if let Some(transparent) = config
            .as_ref()
            .and_then(|config| config.transparent_background)
        {
            source = source.with_transparent_background(transparent);
        }
        if let Some(height) = config.and_then(|config| config.detail_panel_height) {
            source = source.with_detail_panel_height(height);
        }
        return Some(source);
    }

    None
}

impl ReadOnlyMuxStateSource {
    pub fn new(providers: Vec<Arc<dyn MuxProvider>>) -> Self {
        Self {
            providers,
            port_command_runner: Arc::new(SystemPortCommandRunner),
            port_snapshot_cache: Mutex::new(None),
            git_command_runner: Arc::new(SystemGitCommandRunner),
            git_info_cache: Mutex::new(HashMap::new()),
            sidebar_coordinator: Mutex::new(SidebarCoordinator::new(26)),
            sidebar_width_repairs: Arc::new(SidebarWidthRepairScheduler::default()),
            sidebar_presence: Mutex::new(()),
            detail_panel_height: Mutex::new(DEFAULT_DETAIL_PANEL_HEIGHT),
            agent_panel_scope: Mutex::new(AgentPanelScope::Current),
            focused_session: Mutex::new(None),
            focused_pane_by_session: Mutex::new(HashMap::new()),
            focused_client_tty: Mutex::new(None),
            theme: Mutex::new(None),
            transparent_background: Mutex::new(false),
            session_filter: Mutex::new(None),
            collapsed_worktree_groups: Mutex::new(HashSet::new()),
            session_order: Mutex::new(SessionOrder::new(None)),
            metadata_store: Mutex::new(SessionMetadataStore::new()),
            agent_tracker: Mutex::new(AgentTracker::new()),
            pi_runtime_registry: Mutex::new(PiRuntimeRegistry::with_default_ttl()),
            tmux_socket_path: None,
            now_ms: Arc::new(current_time_ms),
        }
    }

    pub fn with_sidebar_width(mut self, sidebar_width: u32) -> Self {
        self.sidebar_coordinator = Mutex::new(SidebarCoordinator::new(sidebar_width));
        self
    }

    pub fn with_detail_panel_height(mut self, height: u16) -> Self {
        self.detail_panel_height = Mutex::new(clamp_detail_panel_height(height));
        self
    }

    pub fn with_theme(mut self, theme: impl Into<String>) -> Self {
        self.theme = Mutex::new(Some(theme.into()));
        self
    }

    pub fn with_transparent_background(mut self, transparent: bool) -> Self {
        self.transparent_background = Mutex::new(transparent);
        self
    }

    pub fn with_tmux_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.tmux_socket_path = Some(path.into());
        self
    }

    /// Current sidebar width from the coordinator (single source of truth),
    /// clamped to `u16` for the tmux resize APIs.
    fn current_sidebar_width_u16(&self) -> u16 {
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .state()
            .width
            .min(u16::MAX as u32) as u16
    }

    fn is_sidebar_visible(&self) -> bool {
        self.sidebar_coordinator.lock().unwrap().state().visible
    }

    fn tmux_state_fingerprint(&self) -> Option<u64> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let fingerprints = self
            .providers
            .iter()
            .filter_map(|provider| provider.state_fingerprint())
            .collect::<Vec<_>>();
        if fingerprints.is_empty() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        fingerprints.hash(&mut hasher);
        Some(hasher.finish())
    }

    fn should_ensure_sidebar(&self) -> bool {
        let state = self.sidebar_coordinator.lock().unwrap().state();
        state.visible && state.lifecycle != SidebarLifecycle::Closing
    }

    fn persist_sidebar_width(&self, width: u16) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            debug_log("set-sidebar-width: skipped config save because HOME is unset");
            return;
        };
        if let Err(err) = save_config_to_home(
            &home,
            OpensessionsConfig {
                sidebar_width: Some(width),
                ..OpensessionsConfig::default()
            },
        ) {
            debug_log(format!(
                "set-sidebar-width: failed to save sidebarWidth={width}: {err}"
            ));
        }
    }

    fn set_sidebar_width(&self, width: u16) {
        let width = clamp_sidebar_width(width);
        self.persist_sidebar_width(width);
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .set_width(u32::from(width));
        for provider in &self.providers {
            provider.set_sidebar_width_hint(width);
        }
        self.request_sidebar_width_repair();
    }

    fn persist_detail_panel_height(&self, height: u16) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            debug_log("set-detail-panel-height: skipped config save because HOME is unset");
            return;
        };
        if let Err(err) = save_config_to_home(
            &home,
            OpensessionsConfig {
                detail_panel_height: Some(height),
                ..OpensessionsConfig::default()
            },
        ) {
            debug_log(format!(
                "set-detail-panel-height: failed to save detailPanelHeight={height}: {err}"
            ));
        }
    }

    fn persist_theme(&self, theme: &str, transparent_background: bool) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            debug_log("set-theme: skipped config save because HOME is unset");
            return;
        };
        if let Err(err) = save_config_to_home(
            &home,
            OpensessionsConfig {
                theme: Some(Value::String(theme.to_string())),
                transparent_background: Some(transparent_background),
                ..OpensessionsConfig::default()
            },
        ) {
            debug_log(format!("set-theme: failed to save config: {err}"));
        }
    }

    pub fn with_now_ms(mut self, now_ms: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now_ms = Arc::new(now_ms);
        self
    }

    pub fn with_port_command_runner(mut self, runner: Arc<dyn PortCommandRunner>) -> Self {
        self.port_command_runner = runner;
        self
    }

    pub fn with_git_command_runner(mut self, runner: Arc<dyn GitCommandRunner>) -> Self {
        self.git_command_runner = runner;
        self
    }

    fn sync_agent_pane_presence(&self) -> bool {
        let mut presence_by_session = Vec::new();
        let mut focused_agent_panes = HashMap::<String, String>::new();
        let focused_client_tty = self.focused_client_tty.lock().unwrap().clone();
        for provider in &self.providers {
            let client_focus = provider.get_client_focus(focused_client_tty.as_deref());
            for session in provider.list_sessions() {
                let pane_agents = provider
                    .list_agent_panes(&session.name)
                    .into_iter()
                    .map(|pane| {
                        if client_focus.as_ref().is_some_and(|focus| {
                            focus.session_name == session.name && focus.pane_id == pane.pane_id
                        }) {
                            focused_agent_panes.insert(session.name.clone(), pane.pane_id.clone());
                        }
                        PanePresenceInput {
                            agent: pane.agent,
                            pane_id: pane.pane_id,
                            active: pane.active,
                            thread_id: pane.thread_id,
                            thread_name: pane.thread_name,
                        }
                    })
                    .collect::<Vec<_>>();
                if !pane_agents.is_empty() {
                    debug_log(format!(
                        "agent-pane-presence session={} panes={:?}",
                        session.name, pane_agents,
                    ));
                }
                presence_by_session.push((session.name, pane_agents));
            }
        }

        let mut changed = false;
        let mut tracker = self.agent_tracker.lock().unwrap();
        for (session, pane_agents) in presence_by_session {
            changed = tracker.apply_pane_presence(&session, pane_agents) || changed;
        }
        for (session, pane_id) in focused_agent_panes {
            let previous = self
                .focused_pane_by_session
                .lock()
                .unwrap()
                .insert(session.clone(), pane_id.clone());
            let seen_changed = tracker.mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "current-agent-pane-seen session={session} pane={pane_id} previous={previous:?} changed={seen_changed}",
            ));
            changed = seen_changed || changed;
        }
        changed
    }

    fn mark_focused_agent_panes_seen(&self) -> bool {
        let focused = self.focused_pane_by_session.lock().unwrap().clone();
        if focused.is_empty() {
            return false;
        }
        let mut tracker = self.agent_tracker.lock().unwrap();
        let mut changed = false;
        for (session, pane_id) in focused {
            let pane_changed = tracker.mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "focused-pane-seen-check session={session} pane={pane_id} changed={pane_changed}",
            ));
            changed = pane_changed || changed;
        }
        changed
    }

    fn remember_focused_pane(&self, context: &HttpContext) -> bool {
        if context.pane_active == Some(false) {
            debug_log(format!(
                "focus-pane ignored inactive session={} pane={:?}",
                context.session, context.pane_id,
            ));
            return false;
        }
        let Some(pane_id) = context
            .pane_id
            .as_deref()
            .filter(|pane_id| !pane_id.is_empty())
        else {
            return false;
        };
        if context.client_tty.is_some() {
            *self.focused_client_tty.lock().unwrap() = context.client_tty.clone();
        }
        self.focused_pane_by_session
            .lock()
            .unwrap()
            .insert(context.session.clone(), pane_id.to_string());
        let changed = self
            .agent_tracker
            .lock()
            .unwrap()
            .mark_pane_seen(&context.session, pane_id);
        debug_log(format!(
            "focus-pane session={} pane={} changed={changed}",
            context.session, pane_id,
        ));
        changed
    }
}

impl StateSource for ReadOnlyMuxStateSource {
    fn setup_mux_hooks(&self, server_host: &str, server_port: u16, token_file: &str) {
        let width = self.current_sidebar_width_u16();
        for provider in &self.providers {
            provider.set_sidebar_width_hint(width);
            provider.setup_hooks(server_host, server_port, token_file);
        }
        if self
            .providers
            .iter()
            .any(|provider| !provider.list_sidebar_panes(None).is_empty())
        {
            self.sidebar_coordinator.lock().unwrap().mark_ready();
            self.ensure_all_sidebars();
        }
    }

    fn cleanup_mux_hooks(&self) {
        for provider in &self.providers {
            provider.cleanup_hooks();
        }
    }

    fn cleanup_sidebar_clients(&self) {
        for provider in &self.providers {
            for pane in provider.list_sidebar_panes(None) {
                provider.kill_sidebar_pane(&pane.pane_id);
            }
        }
    }

    fn mux_namespace_available(&self) -> bool {
        self.tmux_socket_path.as_ref().is_none_or(|socket_path| {
            tmux_socket_is_live(socket_path)
                && self
                    .providers
                    .iter()
                    .any(|provider| !provider.list_sessions().is_empty())
        })
    }

    fn start_background_tasks(
        self: Arc<Self>,
        state_updates: broadcast::Sender<String>,
        shutdown: broadcast::Sender<()>,
    ) -> Vec<JoinHandle<()>> {
        let mut tasks = vec![
            tokio::spawn(run_agent_watcher_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_sidebar_lifecycle_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_sidebar_width_repair_loop(
                self.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_expensive_data_refresh_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )),
            tokio::spawn(run_tmux_state_poll_loop(
                self.clone(),
                state_updates,
                shutdown.clone(),
            )),
        ];
        if self.tmux_socket_path.is_some() {
            let liveness_source = self.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                run_tmux_socket_liveness_loop(liveness_source, shutdown)
            }));
        }
        tasks
    }

    fn snapshot_json(&self) -> String {
        self.sync_agent_pane_presence();
        self.mark_focused_agent_panes_seen();

        let providers = self
            .providers
            .iter()
            .map(|provider| provider.as_ref())
            .collect::<Vec<_>>();
        let visible_sidebar_pane_ids = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_visible_sidebar_pane_ids())
            .collect();
        let visible_session_names = self.visible_session_names();
        let metadata_by_session = visible_session_names.as_ref().map(|names| {
            names
                .iter()
                .filter_map(|name| {
                    self.metadata_store
                        .lock()
                        .unwrap()
                        .get(name)
                        .map(|metadata| (name.clone(), metadata))
                })
                .collect()
        });
        let git_by_session = self.git_info_by_session(visible_session_names.as_deref(), false);
        let (agent_state_by_session, agents_by_session, event_timestamps_by_session) =
            visible_session_names
                .as_ref()
                .map(|names| {
                    let tracker = self.agent_tracker.lock().unwrap();
                    let mut states = HashMap::new();
                    let mut agents = HashMap::new();
                    let mut timestamps = HashMap::new();
                    for name in names {
                        if let Some(state) = tracker.get_state(name) {
                            states.insert(name.clone(), state);
                        }
                        let session_agents = tracker.get_agents(name);
                        if !session_agents.is_empty() {
                            agents.insert(name.clone(), session_agents);
                        }
                        let session_timestamps = tracker.get_event_timestamps(name);
                        if !session_timestamps.is_empty() {
                            timestamps.insert(name.clone(), session_timestamps);
                        }
                    }
                    (Some(states), Some(agents), Some(timestamps))
                })
                .unwrap_or((None, None, None));
        let ports_by_session = self.discover_live_ports(visible_session_names.as_deref(), false);
        let sidebar_state = self.sidebar_coordinator.lock().unwrap().state();
        debug_log(format!(
            "snapshot_json mode={} init={} width={}",
            sidebar_state.mode, sidebar_state.initializing, sidebar_state.width,
        ));
        let state = build_read_only_state(ReadOnlyStateInput {
            providers,
            visible_session_names,
            metadata_by_session,
            git_by_session,
            agent_state_by_session,
            agents_by_session,
            event_timestamps_by_session,
            unseen_sessions: Some(self.agent_tracker.lock().unwrap().get_unseen()),
            ports_by_session,
            portless_state: None,
            focused_session: self.focused_session.lock().unwrap().clone(),
            current_session_override: None,
            visible_sidebar_pane_ids,
            theme: self.theme.lock().unwrap().clone(),
            transparent_background: *self.transparent_background.lock().unwrap(),
            session_filter: *self.session_filter.lock().unwrap(),
            agent_panel_scope: *self.agent_panel_scope.lock().unwrap(),
            collapsed_worktree_groups: self
                .collapsed_worktree_groups
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect(),
            sidebar_width: sidebar_state.width,
            detail_panel_height: u32::from(*self.detail_panel_height.lock().unwrap()),
            initializing: sidebar_state.initializing,
            init_label: (!sidebar_state.init_label.is_empty()).then_some(sidebar_state.init_label),
            now_ms: (self.now_ms)(),
        });

        serde_json::to_string(&ServerMessage::State(state)).expect("state must serialize")
    }

    fn begin_shutdown(&self) -> Option<String> {
        {
            let mut coordinator = self.sidebar_coordinator.lock().unwrap();
            coordinator.begin_closing();
        }
        Some(self.snapshot_json())
    }

    fn handle_client_command(&self, command: &Value) -> Option<String> {
        self.handle_client_command_with_context(command, None)
    }

    fn handle_client_command_with_context(
        &self,
        command: &Value,
        context: Option<&ClientConnectionContext>,
    ) -> Option<String> {
        let provider = self.providers.first()?;
        match command.get("type").and_then(Value::as_str)? {
            "new-session" => {
                provider.create_session(None, None);
                Some(self.snapshot_json())
            }
            "switch-session" => {
                let name = command.get("name")?.as_str()?;
                let client_tty = command
                    .get("clientTty")
                    .and_then(Value::as_str)
                    .or_else(|| context.and_then(|context| context.client_tty.as_deref()));
                provider.switch_session(name, client_tty);
                None
            }
            "switch-index" => {
                let index = command.get("index")?.as_u64()?.min(u32::MAX as u64) as u32;
                self.switch_visible_index(index, None)
            }
            "kill-session" => {
                let name = command.get("name")?.as_str()?;
                let client_tty = command
                    .get("clientTty")
                    .and_then(Value::as_str)
                    .or_else(|| context.and_then(|context| context.client_tty.as_deref()));
                let current_session = provider
                    .get_client_focus(client_tty)
                    .map(|focus| focus.session_name)
                    .or_else(|| provider.get_current_session());
                if current_session.as_deref() == Some(name)
                    && let Some(next) = self
                        .session_before(name)
                        .or_else(|| self.session_after(name))
                {
                    provider.switch_session(&next, client_tty);
                    *self.focused_session.lock().unwrap() = Some(next);
                }
                provider.kill_session(name);
                Some(self.snapshot_json())
            }
            "kill-windows" => {
                let session = command.get("session")?.as_str()?;
                let window_ids = command
                    .get("windowIds")?
                    .as_array()?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                provider.kill_windows(session, &window_ids);
                Some(self.snapshot_json())
            }
            "switch-window" => {
                let session = command.get("session")?.as_str()?;
                let window_id = command.get("windowId")?.as_str()?;
                let client_tty = context.and_then(|context| context.client_tty.as_deref());
                provider.switch_window(session, window_id, client_tty);
                None
            }
            "hide-session" => {
                let name = command.get("name")?.as_str()?;
                self.session_order.lock().unwrap().hide(name);
                Some(self.snapshot_json())
            }
            "show-all-sessions" => {
                self.session_order.lock().unwrap().show_all();
                Some(self.snapshot_json())
            }
            "reorder-session" => {
                let name = command.get("name")?.as_str()?;
                let delta = command.get("delta")?.as_i64()? as i8;
                if let Some(names) = self.sidebar_reordered_session_names(name, delta) {
                    self.session_order.lock().unwrap().set_visible_order(names);
                }
                Some(self.snapshot_json())
            }
            "reorder-worktree-group" => {
                let key = command.get("key")?.as_str()?;
                let delta = command.get("delta")?.as_i64()? as i8;
                if let Some(names) = self.sidebar_reordered_worktree_group_names(key, delta) {
                    self.session_order.lock().unwrap().set_visible_order(names);
                }
                Some(self.snapshot_json())
            }
            "set-theme" => {
                let theme = command.get("theme")?.as_str()?.to_string();
                let transparent_background = command
                    .get("transparentBackground")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                *self.theme.lock().unwrap() = Some(theme.clone());
                *self.transparent_background.lock().unwrap() = transparent_background;
                self.persist_theme(&theme, transparent_background);
                Some(self.snapshot_json())
            }
            "set-sidebar-width" => {
                let width = command.get("width")?.as_u64()?.min(u16::MAX as u64) as u16;
                self.set_sidebar_width(width);
                Some(self.snapshot_json())
            }
            "set-detail-panel-height" => {
                let height = command.get("height")?.as_u64()?.min(u16::MAX as u64) as u16;
                let height = clamp_detail_panel_height(height);
                *self.detail_panel_height.lock().unwrap() = height;
                self.persist_detail_panel_height(height);
                Some(self.snapshot_json())
            }
            "set-agent-panel-scope" => {
                let scope = parse_agent_panel_scope(command.get("scope")?.as_str()?)?;
                *self.agent_panel_scope.lock().unwrap() = scope;
                Some(self.snapshot_json())
            }
            "repair-width" => {
                if self.is_sidebar_visible() {
                    let width = self.current_sidebar_width_u16();
                    if !self.repair_context_sidebar_width(context, width) {
                        self.request_sidebar_width_repair();
                    }
                }
                None
            }
            "set-filter" => {
                let filter = match command.get("filter")?.as_str()? {
                    "all" => SessionFilterMode::All,
                    "active" => SessionFilterMode::Active,
                    "running" => SessionFilterMode::Running,
                    _ => return None,
                };
                *self.session_filter.lock().unwrap() = Some(filter);
                Some(self.snapshot_json())
            }
            "toggle-worktree-group" => {
                let key = command.get("key")?.as_str()?.to_string();
                let mut collapsed = self.collapsed_worktree_groups.lock().unwrap();
                if !collapsed.insert(key) {
                    collapsed.remove(command.get("key")?.as_str()?);
                }
                drop(collapsed);
                Some(self.snapshot_json())
            }
            "focus-agent-pane" => {
                let session = command.get("session")?.as_str()?;
                let agent = command.get("agent")?.as_str()?;
                let thread_id = command.get("threadId").and_then(Value::as_str);
                let thread_name = command.get("threadName").and_then(Value::as_str);
                let pane_id = command.get("paneId").and_then(Value::as_str);
                let mut seen_changed = self
                    .agent_tracker
                    .lock()
                    .unwrap()
                    .mark_agent_seen(session, agent, thread_id, pane_id);
                if let Some((provider, pane_id)) =
                    self.resolve_agent_pane(session, agent, thread_id, thread_name, pane_id)
                {
                    seen_changed = self.agent_tracker.lock().unwrap().mark_agent_seen(
                        session,
                        agent,
                        thread_id,
                        Some(&pane_id),
                    ) || seen_changed;
                    provider.focus_pane(&pane_id);
                }
                seen_changed.then(|| self.snapshot_json())
            }
            "kill-agent-pane" => {
                let session = command.get("session")?.as_str()?;
                let agent = command.get("agent")?.as_str()?;
                let thread_id = command.get("threadId").and_then(Value::as_str);
                let thread_name = command.get("threadName").and_then(Value::as_str);
                let pane_id = command.get("paneId").and_then(Value::as_str);
                if let Some((provider, pane_id)) =
                    self.resolve_agent_pane(session, agent, thread_id, thread_name, pane_id)
                {
                    provider.kill_pane(&pane_id);
                }
                None
            }
            _ => None,
        }
    }

    fn handle_sender_command(&self, command: &Value) -> Option<String> {
        self.handle_sender_command_with_context(command, &mut ClientConnectionContext::default())
    }

    fn handle_sender_command_with_context(
        &self,
        command: &Value,
        context: &mut ClientConnectionContext,
    ) -> Option<String> {
        if command.get("type").and_then(Value::as_str)? == "request-windows" {
            let session = command.get("session")?.as_str()?;
            let windows = self
                .providers
                .first()?
                .list_windows(session)
                .into_iter()
                .map(|window| WindowData {
                    id: window.id,
                    index: window.index,
                    name: window.name,
                    active: window.active,
                    pane_commands: window.pane_commands,
                })
                .collect();
            return serde_json::to_string(&ServerMessage::WindowList {
                session: session.to_string(),
                windows,
            })
            .ok();
        }
        if command.get("type").and_then(Value::as_str)? != "identify-pane" {
            return None;
        }
        let session_name = command.get("sessionName")?.as_str()?;
        if session_name == "_os_stash" {
            return None;
        }
        context.pane_id = command
            .get("paneId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        context.session_name = Some(session_name.to_string());
        context.window_id = command
            .get("windowId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        debug_log(format!(
            "identify-pane session={:?} pane={:?} window={:?} -> acknowledge_sidebar_connected",
            context.session_name, context.pane_id, context.window_id,
        ));
        let became_visible = {
            let mut coordinator = self.sidebar_coordinator.lock().unwrap();
            let was_visible = coordinator.state().visible;
            coordinator.acknowledge_sidebar_connected();
            !was_visible && coordinator.state().visible
        };
        if became_visible {
            self.ensure_all_sidebars();
        }
        if let Some(window_id) = context.window_id.as_deref() {
            for provider in &self.providers {
                provider.prepare_sidebar_window(window_id);
            }
        }
        if self.is_sidebar_visible() {
            let width = self.current_sidebar_width_u16();
            if !self.repair_context_sidebar_width(Some(context), width) {
                self.request_sidebar_width_repair();
            }
        }
        let client_tty = self.providers.first()?.get_client_tty();
        Some(format!(
            r#"{{"type":"your-session","name":{},"clientTty":{}}}"#,
            json_string_or_null(Some(session_name)),
            json_string_or_null(Some(&client_tty)),
        ))
    }

    fn handle_http_json(&self, path: &str, body: &Value) -> Option<String> {
        match path {
            "/set-status" => {
                let session = body.get("session")?.as_str()?;
                let tone = body
                    .get("tone")
                    .and_then(Value::as_str)
                    .and_then(parse_metadata_tone);
                match body.get("text") {
                    Some(Value::String(text)) => self
                        .metadata_store
                        .lock()
                        .unwrap()
                        .set_status(session, Some((text.clone(), tone))),
                    Some(Value::Null) | None => self
                        .metadata_store
                        .lock()
                        .unwrap()
                        .set_status(session, None),
                    _ => return None,
                }
            }
            "/set-progress" => {
                let session = body.get("session")?.as_str()?;
                if body.get("clear").and_then(Value::as_bool).unwrap_or(false) {
                    self.metadata_store
                        .lock()
                        .unwrap()
                        .set_progress(session, None);
                } else {
                    self.metadata_store.lock().unwrap().set_progress(
                        session,
                        Some((
                            body.get("current").and_then(Value::as_u64),
                            body.get("total").and_then(Value::as_u64),
                            body.get("percent").and_then(Value::as_f64),
                            body.get("label")
                                .and_then(Value::as_str)
                                .map(ToString::to_string),
                        )),
                    );
                }
            }
            "/log" | "/notify" => {
                let session = body.get("session")?.as_str()?;
                let message = body.get("message")?.as_str()?.to_string();
                let tone = body
                    .get("tone")
                    .and_then(Value::as_str)
                    .and_then(parse_metadata_tone);
                let source = body
                    .get("source")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                self.metadata_store
                    .lock()
                    .unwrap()
                    .append_log(session, message, tone, source);
            }
            "/clear-log" => {
                let session = body.get("session")?.as_str()?;
                self.metadata_store.lock().unwrap().clear_logs(session);
            }
            _ => return None,
        }
        Some(self.snapshot_json())
    }

    fn handle_agent_event_json(&self, body: &Value) -> Result<String, AgentEventError> {
        self.apply_agent_event(body)?;
        Ok(self.snapshot_json())
    }

    fn handle_pi_runtime_upsert(&self, body: &Value) -> Result<(), PiRuntimeError> {
        let info =
            parse_pi_runtime_info(body, (self.now_ms)()).ok_or(PiRuntimeError::InvalidPayload)?;
        self.pi_runtime_registry.lock().unwrap().upsert(info);
        Ok(())
    }

    fn handle_pi_runtime_delete(&self, body: &Value) -> Result<(), PiRuntimeError> {
        let pid = body
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|pid| *pid > 0 && *pid <= u32::MAX as u64)
            .ok_or(PiRuntimeError::MissingPid)? as u32;
        self.pi_runtime_registry.lock().unwrap().delete(pid);
        Ok(())
    }

    fn handle_http_text(&self, path: &str, body: &str) -> Option<String> {
        if path != "/focus" {
            return None;
        }
        let context = parse_context(body)?;
        let name = context.session.clone();
        *self.focused_session.lock().unwrap() = Some(name.clone());
        if self.remember_focused_pane(&context) {
            return Some(self.snapshot_json());
        }
        None
    }

    fn handle_http_hook(&self, path: &str, body: &str) -> Option<String> {
        match path {
            "/toggle" => {
                self.toggle_sidebar();
                Some(self.snapshot_json())
            }
            "/ensure-sidebar" => {
                let spawned = self.ensure_sidebar(body);
                parse_context_session(body)
                    .map(|name| activate_session_json(name, None))
                    .or_else(|| spawned.then(|| self.snapshot_json()))
            }
            "/ensure-sidebars" => {
                self.ensure_all_sidebars();
                None
            }
            "/pane-exited" => {
                let fallback_sessions = self
                    .providers
                    .iter()
                    .flat_map(|provider| provider.list_sidebar_panes(None))
                    .filter_map(|pane| {
                        let fallback = self
                            .session_before(&pane.session_name)
                            .or_else(|| self.session_after(&pane.session_name))?;
                        Some((pane.session_name, fallback))
                    })
                    .collect::<HashMap<_, _>>();
                for provider in &self.providers {
                    provider.kill_orphaned_sidebar_panes_with_fallbacks(&fallback_sessions);
                }
                if self.is_sidebar_visible() {
                    self.request_sidebar_width_repair();
                }
                None
            }
            "/pane-layout-changed" | "/client-resized" => {
                if self.is_sidebar_visible() {
                    self.request_sidebar_width_repair();
                }
                None
            }
            "/set-sidebar-width" => {
                let width = body.trim().parse::<u16>().ok()?;
                self.set_sidebar_width(width);
                Some(self.snapshot_json())
            }
            _ => None,
        }
    }

    fn handle_switch_index(&self, index: u32, body: &str) -> Option<String> {
        let client_tty = parse_context(body).and_then(|context| context.client_tty);
        self.switch_visible_index(index, client_tty.as_deref())
    }
}

impl ReadOnlyMuxStateSource {
    fn apply_agent_event(&self, body: &Value) -> Result<(), AgentEventError> {
        let agent = body
            .get("agent")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or(AgentEventError::MissingAgent)?;
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .and_then(parse_agent_status)
            .ok_or(AgentEventError::InvalidStatus)?;
        let session = self
            .resolve_agent_event_session(body)
            .ok_or(AgentEventError::CouldNotResolveSession)?;
        let ts = body
            .get("ts")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| (self.now_ms)());
        let pane_id = body
            .get("paneId")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let event_pane_id = pane_id.clone();
        let event_session = session.clone();
        self.agent_tracker.lock().unwrap().apply_event(AgentEvent {
            agent,
            session,
            status,
            ts,
            thread_id: body
                .get("threadId")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            thread_name: body
                .get("threadName")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            last_user_prompt: body
                .get("lastUserPrompt")
                .or_else(|| body.get("last_user_prompt"))
                .and_then(Value::as_str)
                .map(ToString::to_string),
            unseen: None,
            liveness: pane_id.as_ref().map(|_| AgentLiveness::Alive),
            pane_id,
        });
        if let Some(pane_id) = event_pane_id
            && self
                .focused_pane_by_session
                .lock()
                .unwrap()
                .get(&event_session)
                .is_some_and(|focused_pane| focused_pane == &pane_id)
        {
            debug_log(format!(
                "agent-event-focused-pane session={} pane={} -> mark seen",
                event_session, pane_id,
            ));
            self.agent_tracker
                .lock()
                .unwrap()
                .mark_pane_seen(&event_session, &pane_id);
        }
        Ok(())
    }

    fn apply_agent_watcher_snapshot(&self, snapshot: AgentWatcherSnapshot) -> bool {
        if snapshot.status == AgentStatus::Idle {
            debug_log(format!(
                "watcher-snapshot ignored idle agent={} thread_id={:?} thread_name={:?} project_dir={:?}",
                snapshot.agent, snapshot.thread_id, snapshot.thread_name, snapshot.project_dir,
            ));
            return false;
        }
        let Some(session) = self.resolve_agent_watcher_session(&snapshot) else {
            debug_log(format!(
                "watcher-snapshot unresolved agent={} status={:?} thread_id={:?} thread_name={:?} project_dir={:?}",
                snapshot.agent,
                snapshot.status,
                snapshot.thread_id,
                snapshot.thread_name,
                snapshot.project_dir,
            ));
            return false;
        };
        let focused_pane = self
            .focused_pane_by_session
            .lock()
            .unwrap()
            .get(&session)
            .cloned();
        debug_log(format!(
            "watcher-snapshot applying session={} focused_pane={:?} agent={} status={:?} thread_id={:?} thread_name={:?} project_dir={:?}",
            session,
            focused_pane,
            snapshot.agent,
            snapshot.status,
            snapshot.thread_id,
            snapshot.thread_name,
            snapshot.project_dir,
        ));
        let event = AgentEvent {
            agent: snapshot.agent.to_string(),
            session: session.clone(),
            status: snapshot.status,
            ts: snapshot.ts,
            thread_id: snapshot.thread_id.clone(),
            thread_name: snapshot.thread_name.clone(),
            last_user_prompt: snapshot.last_user_prompt.clone(),
            unseen: None,
            pane_id: None,
            liveness: None,
        };
        self.agent_tracker.lock().unwrap().apply_event(event);
        if let Some(pane_id) = focused_pane {
            let changed = self
                .agent_tracker
                .lock()
                .unwrap()
                .mark_pane_seen(&session, &pane_id);
            debug_log(format!(
                "watcher-snapshot-focused-pane-seen session={} pane={} agent={} thread_id={:?} thread_name={:?} changed={changed}",
                session, pane_id, snapshot.agent, snapshot.thread_id, snapshot.thread_name,
            ));
        }
        true
    }

    fn resolve_agent_watcher_session(&self, snapshot: &AgentWatcherSnapshot) -> Option<String> {
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        let project_dir = snapshot.project_dir.as_deref()?;

        if let Some(encoded) = project_dir.strip_prefix("__encoded__:") {
            return sessions
                .iter()
                .find(|session| encode_agent_project_dir(&session.dir) == encoded)
                .map(|session| session.name.clone());
        }

        let dir_session_map = build_dir_session_map(
            sessions
                .into_iter()
                .map(|session| (session.name, session.dir)),
        );
        resolve_session_for_project_dir(project_dir, &dir_session_map)
    }

    fn resolve_agent_event_session(&self, body: &Value) -> Option<String> {
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();

        if let Some(project_dir) = body.get("projectDir").and_then(Value::as_str) {
            let dir_session_map = build_dir_session_map(
                sessions
                    .iter()
                    .map(|session| (session.name.clone(), session.dir.clone())),
            );
            if let Some(session) = resolve_session_for_project_dir(project_dir, &dir_session_map) {
                return Some(session);
            }
        }

        body.get("tmuxSession")
            .and_then(Value::as_str)
            .filter(|tmux_session| sessions.iter().any(|session| session.name == *tmux_session))
            .map(ToString::to_string)
    }

    fn resolve_agent_pane(
        &self,
        session: &str,
        agent: &str,
        thread_id: Option<&str>,
        thread_name: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<(Arc<dyn MuxProvider>, String)> {
        let provider = self.provider_for_session(session)?;
        if let Some(pane_id) = pane_id {
            return Some((provider, pane_id.to_string()));
        }
        self.sync_agent_pane_presence();
        if let Some(pane_id) = self.resolve_tracked_agent_pane(session, agent, thread_id) {
            return Some((provider, pane_id));
        }
        let pane_id = provider.resolve_agent_pane_id(session, agent, thread_id, thread_name)?;
        Some((provider, pane_id))
    }

    fn resolve_tracked_agent_pane(
        &self,
        session: &str,
        agent: &str,
        thread_id: Option<&str>,
    ) -> Option<String> {
        let thread_id = thread_id?;
        self.agent_tracker
            .lock()
            .unwrap()
            .get_agents(session)
            .into_iter()
            .find(|event| {
                event.agent == agent
                    && event.thread_id.as_deref() == Some(thread_id)
                    && event.liveness == Some(AgentLiveness::Alive)
                    && event.pane_id.is_some()
            })
            .and_then(|event| event.pane_id)
    }

    fn sidebar_panes_to_resize(&self, width: u16) -> Vec<String> {
        let mut pane_ids = Vec::new();
        for provider in &self.providers {
            if !provider.is_sidebar_capable() {
                continue;
            }
            for pane in provider.list_sidebar_panes(None) {
                if pane.width == Some(width) {
                    continue;
                }
                pane_ids.push(pane.pane_id);
            }
        }
        pane_ids.reverse();
        pane_ids
    }

    fn repair_context_sidebar_width(
        &self,
        context: Option<&ClientConnectionContext>,
        width: u16,
    ) -> bool {
        let window_id = context.and_then(|context| context.window_id.as_deref());
        if window_id.is_some_and(|window_id| {
            self.providers
                .iter()
                .any(|provider| provider.is_sidebar_mouse_resize_active(window_id))
        }) {
            return true;
        }
        let Some(pane_id) = context.and_then(|context| context.pane_id.as_deref()) else {
            return false;
        };
        debug_log(format!(
            "width-repair: resize context pane={pane_id} to={width}"
        ));
        for provider in &self.providers {
            provider.resize_sidebar_pane(pane_id, width);
        }
        true
    }

    fn request_sidebar_width_repair(&self) {
        self.sidebar_width_repairs.request();
    }

    fn enforce_sidebar_width(&self, width: u16) -> usize {
        let panes = self.sidebar_panes_to_resize(width);
        let pane_count = panes.len();
        for pane_id in &panes {
            debug_log(format!("width-repair: resize pane={pane_id} to={width}",));
        }
        for provider in &self.providers {
            provider.resize_sidebar_panes(&panes, width);
        }
        pane_count
    }

    fn provider_for_session(&self, session: &str) -> Option<Arc<dyn MuxProvider>> {
        self.providers
            .iter()
            .find(|provider| {
                provider
                    .list_sessions()
                    .iter()
                    .any(|mux_session| mux_session.name == session)
            })
            .cloned()
            .or_else(|| self.providers.first().cloned())
    }

    fn git_info_by_session(
        &self,
        visible_session_names: Option<&[String]>,
        force_refresh: bool,
    ) -> Option<HashMap<String, GitInfo>> {
        let visible =
            visible_session_names.map(|names| names.iter().cloned().collect::<HashSet<_>>());
        let mut git_by_session = HashMap::new();
        for provider in &self.providers {
            for session in provider.list_sessions() {
                if visible
                    .as_ref()
                    .is_some_and(|visible| !visible.contains(&session.name))
                {
                    continue;
                }
                git_by_session.insert(
                    session.name,
                    self.git_info_for_dir(&session.dir, force_refresh),
                );
            }
        }
        Some(git_by_session)
    }

    fn git_info_for_dir(&self, dir: &str, force_refresh: bool) -> GitInfo {
        if dir.is_empty() {
            return GitInfo::empty();
        }

        if let Some(cached) = self.git_info_cache.lock().unwrap().get(dir).cloned()
            && !force_refresh
        {
            return cached.info;
        }

        let output = self.git_command_runner.git_info_output(dir);
        let info = parse_git_info_output(&output);
        self.git_info_cache
            .lock()
            .unwrap()
            .insert(dir.to_string(), CachedGitInfo { info: info.clone() });
        info
    }

    fn discover_live_ports(
        &self,
        visible_session_names: Option<&[String]>,
        force_refresh: bool,
    ) -> Option<HashMap<String, Vec<u16>>> {
        let session_names = visible_session_names
            .map(|names| names.to_vec())
            .unwrap_or_else(|| self.sorted_session_names());
        // Keep cache lookup and refresh under one lock. Initial websocket
        // connections arrive in a burst when the sidebar opens in many
        // windows; without single-flight ownership every connection can miss
        // the empty cache and launch its own ps/lsof discovery.
        let mut cache = self.port_snapshot_cache.lock().unwrap();
        if let Some(cached) = cache.as_ref()
            && cached.session_names == session_names
            && !force_refresh
        {
            return Some(cached.ports_by_session.clone());
        }

        if session_names.is_empty() {
            return Some(HashMap::new());
        }

        let session_filter = session_names.iter().cloned().collect::<HashSet<_>>();
        let mut pane_pids_by_session = HashMap::new();
        for provider in &self.providers {
            for session in provider.list_sessions() {
                if !session_filter.contains(&session.name) {
                    continue;
                }
                let pids = provider.get_session_pane_pids(&session.name);
                if !pids.is_empty() {
                    pane_pids_by_session.insert(session.name, pids);
                }
            }
        }

        let ports_by_session = if pane_pids_by_session.is_empty() {
            discover_session_ports(PortDiscoveryInput {
                session_names: session_names.clone(),
                pane_pids_by_session,
                process_rows: Vec::new(),
                lsof_fields: "",
            })
        } else {
            let lsof_fields = self.port_command_runner.lsof_fields();
            discover_session_ports(PortDiscoveryInput {
                session_names: session_names.clone(),
                pane_pids_by_session,
                process_rows: self.port_command_runner.process_rows(),
                lsof_fields: &lsof_fields,
            })
        };
        cache.replace(CachedPortSnapshot {
            session_names,
            ports_by_session: ports_by_session.clone(),
        });
        Some(ports_by_session)
    }

    fn refresh_expensive_data(&self) -> bool {
        let previous_git = self
            .git_info_cache
            .lock()
            .unwrap()
            .iter()
            .map(|(dir, cached)| (dir.clone(), cached.info.clone()))
            .collect::<HashMap<_, _>>();
        let previous_ports = self
            .port_snapshot_cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|cached| cached.ports_by_session.clone());
        let visible_session_names = self.visible_session_names();
        let _ = self.git_info_by_session(visible_session_names.as_deref(), true);
        let current_ports = self.discover_live_ports(visible_session_names.as_deref(), true);
        let current_git = self
            .git_info_cache
            .lock()
            .unwrap()
            .iter()
            .map(|(dir, cached)| (dir.clone(), cached.info.clone()))
            .collect::<HashMap<_, _>>();

        previous_git != current_git || previous_ports != current_ports
    }

    fn toggle_sidebar(&self) {
        let _presence_guard = self.sidebar_presence.lock().unwrap();
        let providers = self
            .providers
            .iter()
            .filter(|provider| provider.is_full_sidebar_capable())
            .collect::<Vec<_>>();
        let panes_by_provider = providers
            .iter()
            .map(|provider| (*provider, provider.list_sidebar_panes(None)))
            .collect::<Vec<_>>();

        if panes_by_provider.iter().any(|(_, panes)| !panes.is_empty()) {
            for (provider, panes) in panes_by_provider {
                for pane in panes {
                    provider.hide_sidebar(&pane.pane_id);
                }
            }
            self.sidebar_coordinator.lock().unwrap().hide();
            return;
        }

        let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .begin_warmup_until(warmup_until);
        let width = self.current_sidebar_width_u16();
        for provider in providers {
            let mut unique_windows = Vec::<ActiveWindow>::new();
            for window in provider.list_active_windows() {
                if let Some(current) = unique_windows
                    .iter_mut()
                    .find(|current| current.id == window.id)
                {
                    if !current.active && window.active {
                        *current = window;
                    } else {
                        debug_log(format!(
                            "toggle_sidebar: skipping duplicate linked window session={} window={}",
                            window.session_name, window.id,
                        ));
                    }
                    continue;
                }
                unique_windows.push(window);
            }

            for window in unique_windows {
                debug_log(format!(
                    "toggle_sidebar: spawning in session={} window={} width={width}",
                    window.session_name, window.id,
                ));
                provider.spawn_sidebar(
                    &window.session_name,
                    &window.id,
                    width,
                    SidebarPosition::Left,
                    SIDEBAR_SCRIPTS_DIR,
                );
            }
        }
    }

    fn ensure_sidebar(&self, body: &str) -> bool {
        let _presence_guard = self.sidebar_presence.lock().unwrap();
        let context = parse_context(body);
        if !self.should_ensure_sidebar() {
            debug_log("ensure_sidebar: ignored spawn while sidebar is hidden or closing");
            return false;
        }
        // A window switch / new window can make tmux proportionally redistribute
        // panes in that window. Queue one coalesced global repair while spawning
        // missing sidebars at the configured width immediately.
        self.request_sidebar_width_repair();
        let mut spawned = false;
        for provider in &self.providers {
            if !provider.is_full_sidebar_capable() {
                continue;
            }
            let session_name = context
                .as_ref()
                .map(|context| context.session.clone())
                .or_else(|| provider.get_current_session());
            let window_id = context
                .as_ref()
                .map(|context| context.window_id.clone())
                .or_else(|| provider.get_current_window_id());
            let (Some(session_name), Some(window_id)) = (session_name, window_id) else {
                continue;
            };
            spawned |= self.ensure_sidebar_in_window(provider.as_ref(), &session_name, &window_id);
        }
        spawned
    }

    fn ensure_sidebar_in_window(
        &self,
        provider: &dyn MuxProvider,
        session_name: &str,
        window_id: &str,
    ) -> bool {
        if provider
            .list_sidebar_panes(Some(session_name))
            .iter()
            .any(|pane| pane.window_id == window_id)
        {
            return false;
        }
        let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .begin_warmup_until(warmup_until);
        provider
            .spawn_sidebar(
                session_name,
                window_id,
                self.current_sidebar_width_u16(),
                SidebarPosition::Left,
                SIDEBAR_SCRIPTS_DIR,
            )
            .is_some()
    }

    fn ensure_all_sidebars(&self) -> bool {
        let _presence_guard = self.sidebar_presence.lock().unwrap();
        if !self.should_ensure_sidebar() {
            return false;
        }
        let width = self.current_sidebar_width_u16();
        let mut spawned = false;
        for provider in &self.providers {
            if !provider.is_full_sidebar_capable() {
                continue;
            }
            let existing = provider
                .list_sidebar_panes(None)
                .into_iter()
                .map(|pane| pane.window_id)
                .collect::<HashSet<_>>();
            let mut visited = HashSet::new();
            for window in provider.list_active_windows() {
                if !visited.insert(window.id.clone()) || existing.contains(&window.id) {
                    continue;
                }
                if !spawned {
                    let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
                    self.sidebar_coordinator
                        .lock()
                        .unwrap()
                        .begin_warmup_until(warmup_until);
                }
                debug_log(format!(
                    "ensure_all_sidebars: spawning in session={} window={} width={width}",
                    window.session_name, window.id,
                ));
                if provider
                    .spawn_sidebar(
                        &window.session_name,
                        &window.id,
                        width,
                        SidebarPosition::Left,
                        SIDEBAR_SCRIPTS_DIR,
                    )
                    .is_some()
                {
                    spawned = true;
                } else {
                    debug_log(format!(
                        "ensure_all_sidebars: failed to spawn in session={} window={}",
                        window.session_name, window.id,
                    ));
                }
            }
        }
        spawned
    }

    fn switch_visible_index(&self, index: u32, client_tty: Option<&str>) -> Option<String> {
        let provider = self.providers.first()?;
        let target_index = index.checked_sub(1).map(|index| index as usize)?;
        let name = self
            .sidebar_display_session_names()
            .and_then(|names| names.get(target_index).cloned())?;
        provider.switch_session(&name, client_tty);
        None
    }

    fn session_before(&self, name: &str) -> Option<String> {
        let names = self.sidebar_display_session_names()?;
        let index = names.iter().position(|candidate| candidate == name)?;
        index
            .checked_sub(1)
            .and_then(|previous| names.get(previous).cloned())
    }

    fn session_after(&self, name: &str) -> Option<String> {
        let names = self.sidebar_display_session_names()?;
        let index = names.iter().position(|candidate| candidate == name)?;
        names.get(index + 1).cloned()
    }

    fn sidebar_display_session_names(&self) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json()).map(|app| {
            app.display_sessions()
                .into_iter()
                .map(|session| session.name.clone())
                .collect()
        })
    }

    fn sidebar_reordered_session_names(&self, name: &str, delta: i8) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json())?.reordered_session_names(name, delta)
    }

    fn sidebar_reordered_worktree_group_names(&self, key: &str, delta: i8) -> Option<Vec<String>> {
        app_from_state_json(&self.snapshot_json())?.reordered_worktree_group_names(key, delta)
    }

    fn visible_session_names(&self) -> Option<Vec<String>> {
        let names = self.sorted_session_names();
        let mut session_order = self.session_order.lock().unwrap();
        session_order.sync(names.clone());
        if let Some(current_session) = self
            .providers
            .iter()
            .find_map(|provider| provider.get_current_session())
        {
            session_order.show(&current_session);
        }
        Some(session_order.apply(names))
    }

    fn sorted_session_names(&self) -> Vec<String> {
        let mut sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        sessions.into_iter().map(|session| session.name).collect()
    }
}

/// Background ticker that advances sidebar lifecycle timers. This keeps
/// user-visible lifecycle states like `warming up…` stable long enough to be
/// perceived, then broadcasts the transition back to ready without relying on
/// unrelated tmux or websocket traffic.
async fn run_sidebar_lifecycle_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut interval = tokio::time::interval(Duration::from_millis(SIDEBAR_LIFECYCLE_POLL_MS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                let now = (source.now_ms)();
                let changed = {
                    let mut coordinator = source.sidebar_coordinator.lock().unwrap();
                    coordinator.tick_timers(now)
                };
                if changed {
                    debug_log("sidebar_lifecycle_loop: lifecycle changed, broadcasting fresh state");
                    let snapshot_source = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                        snapshot_source.snapshot_json()
                    }).await {
                        let _ = state_updates.send(snapshot);
                    }
                }
            }
        }
    }
}

async fn run_sidebar_width_repair_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    shutdown: broadcast::Sender<()>,
) {
    let scheduler = Arc::clone(&source.sidebar_width_repairs);
    run_coalesced_sidebar_width_repairs(
        scheduler,
        shutdown.subscribe(),
        Duration::from_millis(SIDEBAR_WIDTH_REPAIR_SETTLE_MS),
        move |request_count| {
            let source = Arc::clone(&source);
            async move {
                let _ = tokio::task::spawn_blocking(move || {
                    if !source.is_sidebar_visible() {
                        debug_log(format!(
                            "width-repair: skipped {request_count} coalesced requests while hidden"
                        ));
                        return;
                    }
                    let started = Instant::now();
                    let width = source.current_sidebar_width_u16();
                    let resized_panes = source.enforce_sidebar_width(width);
                    debug_log(format!(
                        "width-repair: completed requests={request_count} resized={resized_panes} width={width} elapsed_ms={}",
                        started.elapsed().as_millis(),
                    ));
                })
                .await;
            }
        },
    )
    .await;
}

async fn run_coalesced_sidebar_width_repairs<F, Fut>(
    scheduler: Arc<SidebarWidthRepairScheduler>,
    mut shutdown_rx: broadcast::Receiver<()>,
    settle_delay: Duration,
    mut repair: F,
) where
    F: FnMut(usize) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = scheduler.notify.notified() => {}
        }
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tokio::time::sleep(settle_delay) => {}
        }
        loop {
            let request_count = scheduler.take_pending_requests();
            if request_count == 0 {
                break;
            }
            repair(request_count).await;
        }
    }
}

fn adaptive_poll_delay_ms(unchanged_polls: u32, base_ms: u64, max_ms: u64) -> u64 {
    let shift = unchanged_polls.min(10);
    base_ms.saturating_mul(1_u64 << shift).min(max_ms)
}

fn agent_status_needs_fast_polling(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Running | AgentStatus::ToolRunning | AgentStatus::Waiting
    )
}

fn tmux_socket_is_live(socket_path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket_path).is_ok()
}

fn run_tmux_socket_liveness_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut missing_polls = 0;
    let socket_path = source
        .tmux_socket_path
        .as_deref()
        .expect("tmux liveness watcher requires a socket path");
    debug_log(format!(
        "tmux socket liveness watcher started for {}",
        socket_path.display(),
    ));
    loop {
        if !matches!(
            shutdown_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ) {
            return;
        }
        if source.mux_namespace_available() {
            missing_polls = 0;
        } else {
            missing_polls += 1;
            debug_log(format!(
                "tmux socket {} is not accepting connections ({missing_polls}/{MISSING_TMUX_POLLS_BEFORE_SHUTDOWN})",
                socket_path.display(),
            ));
            if missing_polls >= MISSING_TMUX_POLLS_BEFORE_SHUTDOWN {
                debug_log(format!(
                    "tmux namespace at {} is unavailable; shutting down server",
                    socket_path.display(),
                ));
                let _ = shutdown.send(());
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(TMUX_STATE_POLL_MS));
    }
}

async fn run_expensive_data_refresh_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut unchanged_polls = 0;
    loop {
        let delay = adaptive_poll_delay_ms(
            unchanged_polls,
            EXPENSIVE_DATA_POLL_MS,
            EXPENSIVE_DATA_IDLE_MAX_MS,
        );
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {
                let refresh_source = source.clone();
                let changed = tokio::task::spawn_blocking(move || {
                    refresh_source.refresh_expensive_data()
                })
                .await
                .unwrap_or(false);
                if changed {
                    unchanged_polls = 0;
                    let snapshot_source = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                        snapshot_source.snapshot_json()
                    }).await {
                        let _ = state_updates.send(snapshot);
                    }
                } else {
                    unchanged_polls = unchanged_polls.saturating_add(1);
                }
            }
        }
    }
}

/// Poll only cheap tmux topology/focus data, backing off while it is stable.
/// Full snapshots (including git and port discovery) are built only after the
/// fingerprint changes, so routine polling cannot repeatedly launch those
/// subprocesses. Hooks remain the immediate path for known tmux changes.
async fn run_tmux_state_poll_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut last_fingerprint = None;
    let mut unchanged_polls = 0;
    loop {
        let delay =
            adaptive_poll_delay_ms(unchanged_polls, TMUX_STATE_POLL_MS, TMUX_STATE_IDLE_MAX_MS);
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {
                let Some(fingerprint) = source.tmux_state_fingerprint() else {
                    unchanged_polls = unchanged_polls.saturating_add(1);
                    continue;
                };
                if last_fingerprint == Some(fingerprint) {
                    unchanged_polls = unchanged_polls.saturating_add(1);
                    continue;
                }
                last_fingerprint = Some(fingerprint);
                unchanged_polls = 0;
                debug_log("tmux_state_poll_loop: state changed, broadcasting");
                let snapshot_source = source.clone();
                if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                    snapshot_source.snapshot_json()
                }).await {
                    let _ = state_updates.send(snapshot);
                }
            }
        }
    }
}

async fn run_agent_watcher_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let mut last_seen = HashMap::<String, AgentWatcherFingerprint>::new();
    let mut unchanged_polls = 0;

    loop {
        let delay = adaptive_poll_delay_ms(
            unchanged_polls,
            AGENT_WATCHER_POLL_MS,
            AGENT_WATCHER_IDLE_MAX_MS,
        );
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {
                let now = current_time_ms();
                let snapshots = tokio::task::spawn_blocking(move || scan_agent_watcher_snapshots(now))
                    .await
                    .unwrap_or_default();
                let has_active_agents = snapshots
                    .iter()
                    .any(|snapshot| agent_status_needs_fast_polling(snapshot.status));
                let mut changed = false;
                for snapshot in snapshots {
                    if snapshot.status == AgentStatus::Idle {
                        continue;
                    }
                    let key = agent_watcher_key(&snapshot);
                    let fingerprint = AgentWatcherFingerprint::from(&snapshot);
                    if last_seen.get(&key) == Some(&fingerprint) {
                        continue;
                    }
                    let agent = snapshot.agent.to_string();
                    let status = snapshot.status;
                    let thread_name = snapshot.thread_name.clone();
                    if source.apply_agent_watcher_snapshot(snapshot) {
                        debug_log(format!(
                            "agent_watcher_loop: applied snapshot agent={agent} status={status:?} thread={thread_name:?}",
                        ));
                        last_seen.insert(key, fingerprint);
                        let snapshot_source = source.clone();
                        if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                            snapshot_source.snapshot_json()
                        }).await {
                            let _ = state_updates.send(snapshot);
                        }
                        changed = true;
                    } else {
                        debug_log(format!(
                            "agent_watcher_loop: dropped snapshot agent={agent} status={status:?} (no matching session)",
                        ));
                    }
                }
                if changed || has_active_agents {
                    unchanged_polls = 0;
                } else {
                    unchanged_polls = unchanged_polls.saturating_add(1);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentWatcherFingerprint {
    status: AgentStatus,
    thread_name: Option<String>,
    last_user_prompt: Option<String>,
    project_dir: Option<String>,
}

impl From<&AgentWatcherSnapshot> for AgentWatcherFingerprint {
    fn from(snapshot: &AgentWatcherSnapshot) -> Self {
        Self {
            status: snapshot.status,
            thread_name: snapshot.thread_name.clone(),
            last_user_prompt: snapshot.last_user_prompt.clone(),
            project_dir: snapshot.project_dir.clone(),
        }
    }
}

fn agent_watcher_key(snapshot: &AgentWatcherSnapshot) -> String {
    format!(
        "{}\0{}",
        snapshot.agent,
        snapshot
            .thread_id
            .as_deref()
            .or(snapshot.project_dir.as_deref())
            .unwrap_or_default(),
    )
}

fn scan_agent_watcher_snapshots(now_ms: u64) -> Vec<AgentWatcherSnapshot> {
    let mut snapshots = Vec::new();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return snapshots;
    };

    scan_amp_threads(&home, now_ms, &mut snapshots);
    scan_amp_logs(&home, now_ms, &mut snapshots);
    scan_claude_code_projects(&home, now_ms, &mut snapshots);
    scan_codex_sessions(&home, now_ms, &mut snapshots);
    scan_opencode_sessions(&home, now_ms, &mut snapshots);
    scan_pi_sessions(&home, now_ms, &mut snapshots);
    scan_droid_sessions(&home, now_ms, &mut snapshots);
    snapshots
}

fn scan_amp_threads(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let threads_dir = home.join(".local/share/amp/threads");
    let Ok(entries) = fs::read_dir(threads_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = amp_snapshot_from_thread_json(&raw, mtime_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_amp_logs(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let logs_dir = home.join(".cache/amp/logs/threads");
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(raw) = read_file_tail(&path, AMP_LOG_TAIL_BYTES) else {
            continue;
        };
        let Some(snapshot) = amp_snapshot_from_log_jsonl(thread_id, &raw, mtime_ms) else {
            continue;
        };
        if let Some(existing) = snapshots.iter_mut().find(|existing| {
            existing.agent == "amp" && existing.thread_id.as_deref() == Some(thread_id)
        }) {
            if snapshot.ts > existing.ts {
                *existing = snapshot;
            }
        } else {
            snapshots.push(snapshot);
        }
    }
}

fn read_file_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes)).ok()?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn scan_claude_code_projects(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let projects_dir = home.join(".claude/projects");
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return;
    };

    for project in projects.flatten() {
        let project_path = project.path();
        if !project_path.is_dir() {
            continue;
        }
        let encoded = project.file_name().to_string_lossy().to_string();
        let project_dir = decode_claude_project_dir(&encoded, |path| Path::new(path).is_dir());
        let Ok(files) = fs::read_dir(project_path) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(mtime_ms) = file_mtime_ms(&path) else {
                continue;
            };
            if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
                continue;
            }
            let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some(snapshot) =
                claude_code_snapshot_from_jsonl(thread_id, &project_dir, &raw, mtime_ms, now_ms)
            {
                snapshots.push(snapshot);
            }
        }
    }
}

fn scan_codex_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let sessions_dir = codex_home.join("sessions");
    let names = fs::read_to_string(codex_home.join("session_index.jsonl"))
        .ok()
        .map(|raw| {
            parse_codex_session_index(&raw)
                .into_iter()
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    for path in collect_jsonl_files(&sessions_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(path_text) = path.to_str() else {
            continue;
        };
        let thread_id = codex_thread_id_from_path(path_text);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = codex_snapshot_from_jsonl(
            &thread_id,
            &raw,
            names.get(&thread_id).map(String::as_str),
            mtime_ms,
            now_ms,
        ) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_opencode_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let db_path = std::env::var_os("OPENCODE_DB_PATH")
        .or_else(|| std::env::var_os("OPENCODE_DB"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/opencode/opencode.db"));
    if !db_path.exists() {
        return;
    }

    let stale_threshold = now_ms.saturating_sub(AGENT_WATCHER_RECENT_MS);
    let query = format!(
        "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),''), ifnull((SELECT sm.data FROM session_message sm WHERE sm.session_id = r.id AND sm.type = 'user' ORDER BY sm.seq DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
    );
    let run_query = |query: String| {
        let mut command = process::Command::new("sqlite3");
        command
            .arg("-readonly")
            .arg("-separator")
            .arg(OPENCODE_SQL_SEP.to_string())
            .arg(&db_path)
            .arg(query);
        run_process_with_timeout(command, Duration::from_millis(OPENCODE_SQL_TIMEOUT_MS))
    };
    let output = run_query(query).or_else(|| {
        let legacy_query = format!(
            "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
        );
        run_query(legacy_query)
    });
    let Some(mut output) = output else {
        return;
    };
    if !output.status.success() {
        let legacy_query = format!(
            "WITH recent AS MATERIALIZED (SELECT id, title, directory, time_updated FROM session WHERE time_updated > {stale_threshold} ORDER BY time_updated DESC LIMIT 50) SELECT r.id, ifnull(r.title,''), r.directory, r.time_updated, ifnull((SELECT m.data FROM message m WHERE m.session_id = r.id ORDER BY m.time_created DESC LIMIT 1),'') FROM recent r ORDER BY r.time_updated DESC;"
        );
        let Some(legacy_output) = run_query(legacy_query) else {
            return;
        };
        if !legacy_output.status.success() {
            return;
        }
        output = legacy_output;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts = line.split(OPENCODE_SQL_SEP).collect::<Vec<_>>();
        if parts.len() < 5 || parts[4].is_empty() {
            continue;
        }
        let time_updated = parts[3].parse::<u64>().unwrap_or(now_ms);
        if let Some(snapshot) = opencode_snapshot_from_row(
            parts[0],
            (!parts[1].is_empty()).then_some(parts[1]),
            parts[2],
            time_updated,
            parts[4],
            parts.get(5).copied().filter(|value| !value.is_empty()),
            now_ms,
        ) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_pi_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let sessions_dir = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .map(|dir| dir.join("sessions"))
        })
        .unwrap_or_else(|| home.join(".pi/agent/sessions"));

    for path in collect_jsonl_files(&sessions_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = pi_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn scan_droid_sessions(home: &Path, now_ms: u64, snapshots: &mut Vec<AgentWatcherSnapshot>) {
    let projects_dir = std::env::var_os("FACTORY_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".factory/projects"));

    for path in collect_jsonl_files(&projects_dir) {
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if now_ms.saturating_sub(mtime_ms) > AGENT_WATCHER_RECENT_MS {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(snapshot) = droid_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms) {
            snapshots.push(snapshot);
        }
    }
}

fn run_process_with_timeout(
    mut command: process::Command,
    timeout: Duration,
) -> Option<process::Output> {
    let mut child = command
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .ok()?;
    let started = Instant::now();

    loop {
        if child.try_wait().ok()?.is_some() {
            return child.wait_with_output().ok();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn collect_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(collect_jsonl_files(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    files
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn encode_agent_project_dir(path: &str) -> String {
    path.chars()
        .map(|ch| match ch {
            '/' | '.' | '_' => '-',
            ch => ch,
        })
        .collect()
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn json_string_or_null(value: Option<&str>) -> String {
    value
        .map(|value| serde_json::to_string(value).expect("string must serialize"))
        .unwrap_or_else(|| "null".to_string())
}

fn activate_session_json(name: String, source_pane_id: Option<&str>) -> String {
    serde_json::to_string(&SidebarServerMessage::ActivateSession {
        name,
        source_pane_id: source_pane_id.map(str::to_string),
    })
    .expect("activate-session must serialize")
}

fn parse_metadata_tone(value: &str) -> Option<MetadataTone> {
    match value {
        "neutral" => Some(MetadataTone::Neutral),
        "info" => Some(MetadataTone::Info),
        "success" => Some(MetadataTone::Success),
        "warn" => Some(MetadataTone::Warn),
        "error" => Some(MetadataTone::Error),
        _ => None,
    }
}

fn parse_agent_status(value: &str) -> Option<AgentStatus> {
    match value {
        "idle" => Some(AgentStatus::Idle),
        "running" => Some(AgentStatus::Running),
        "tool-running" => Some(AgentStatus::ToolRunning),
        "done" => Some(AgentStatus::Done),
        "error" => Some(AgentStatus::Error),
        "waiting" => Some(AgentStatus::Waiting),
        "interrupted" => Some(AgentStatus::Interrupted),
        "stale" => Some(AgentStatus::Stale),
        _ => None,
    }
}

fn parse_agent_panel_scope(value: &str) -> Option<AgentPanelScope> {
    match value {
        "current" => Some(AgentPanelScope::Current),
        "all" => Some(AgentPanelScope::All),
        _ => None,
    }
}

fn parse_process_row(line: &str) -> Option<(u32, u32)> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    Some((pid, ppid))
}

struct HttpContext {
    client_tty: Option<String>,
    session: String,
    window_id: String,
    pane_id: Option<String>,
    pane_active: Option<bool>,
}

fn parse_context(body: &str) -> Option<HttpContext> {
    let trimmed = trim_context_quotes(body);
    let pipe_parts = trimmed.split('|').collect::<Vec<_>>();
    if pipe_parts.len() == 5 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: Some(pipe_parts[3].to_string()),
            pane_active: Some(pipe_parts[4] == "1"),
        });
    }
    if pipe_parts.len() == 4 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: Some(pipe_parts[3].to_string()),
            pane_active: None,
        });
    }
    if pipe_parts.len() == 3 && !pipe_parts[1].is_empty() && !pipe_parts[2].is_empty() {
        return Some(HttpContext {
            client_tty: (!pipe_parts[0].is_empty()).then(|| pipe_parts[0].to_string()),
            session: pipe_parts[1].to_string(),
            window_id: pipe_parts[2].to_string(),
            pane_id: None,
            pane_active: None,
        });
    }

    let colon_idx = trimmed.find(':')?;
    if colon_idx < 1 {
        return None;
    }
    let session = &trimmed[..colon_idx];
    let window_id = &trimmed[colon_idx + 1..];
    (!session.is_empty() && !window_id.is_empty()).then(|| HttpContext {
        client_tty: None,
        session: session.to_string(),
        window_id: window_id.to_string(),
        pane_id: None,
        pane_active: None,
    })
}

fn parse_context_session(body: &str) -> Option<String> {
    parse_context(body).map(|context| context.session)
}

fn trim_context_quotes(value: &str) -> &str {
    trim_single_quotes(trim_double_quotes(value.trim()))
}

fn trim_double_quotes(value: &str) -> &str {
    value.trim_matches('"')
}

fn trim_single_quotes(value: &str) -> &str {
    value.trim_matches('\'')
}

#[derive(Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub pid_file: PathBuf,
    pub token_file: PathBuf,
    state_source: Option<Arc<dyn StateSource>>,
}

impl ServerConfig {
    pub fn new(host: impl Into<String>, port: u16, pid_file: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            port,
            pid_file: pid_file.into(),
            token_file: PathBuf::new(),
            state_source: None,
        }
    }

    pub fn with_token_file(mut self, token_file: impl Into<PathBuf>) -> Self {
        self.token_file = token_file.into();
        self
    }

    pub fn with_state_source(mut self, source: impl StateSource) -> Self {
        self.state_source = Some(Arc::new(source));
        self
    }
}

#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    shutdown: broadcast::Sender<()>,
    task: JoinHandle<Result<(), ServerError>>,
}

impl ServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn shutdown(self) -> Result<(), ServerError> {
        let _ = self.shutdown.send(());
        self.wait_shutdown().await
    }

    pub async fn wait_shutdown(self) -> Result<(), ServerError> {
        self.task.await.map_err(ServerError::from)?
    }
}

#[derive(Debug, Clone)]
pub struct ServerError {
    message: String,
}

impl ServerError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ServerError {}

impl From<std::io::Error> for ServerError {
    fn from(value: std::io::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<tokio_websockets::Error> for ServerError {
    fn from(value: tokio_websockets::Error) -> Self {
        Self::new(value.to_string())
    }
}

impl From<tokio::task::JoinError> for ServerError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::new(value.to_string())
    }
}

fn generate_auth_token() -> Result<String, ServerError> {
    let mut bytes = [0_u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_private_file(path: &Path, contents: &str, generation: &str) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}.{generation}", process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)
}

fn publish_identity(pid_file: &Path, token_file: &Path, token: &str) -> std::io::Result<()> {
    // Token first, pid last: discovery treats the pid file as the publication
    // marker and can never observe a generation without its credential.
    let generation = &token[..16];
    write_private_file(token_file, token, generation)?;
    if let Err(error) = write_private_file(pid_file, &process::id().to_string(), generation) {
        let _ = fs::remove_file(token_file);
        return Err(error);
    }
    Ok(())
}

fn cleanup_owned_identity(pid_file: &Path, token_file: &Path, token: &str) -> std::io::Result<()> {
    if !owns_identity_generation(pid_file, token_file, token) {
        return Ok(());
    }
    match fs::remove_file(pid_file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match fs::remove_file(token_file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn owns_identity_generation(pid_file: &Path, token_file: &Path, token: &str) -> bool {
    fs::read_to_string(pid_file).is_ok_and(|pid| pid.trim() == process::id().to_string())
        && fs::read_to_string(token_file).is_ok_and(|current| current.trim() == token)
}

async fn cache_latest_state(
    mut state_updates: broadcast::Receiver<String>,
    mut shutdown: broadcast::Receiver<()>,
    latest_state: Arc<RwLock<Option<String>>>,
) {
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            update = state_updates.recv() => match update {
                Ok(update) => {
                    if update != QUIT_JSON && !is_immediate_server_message(&update) {
                        *latest_state.write().unwrap() = Some(update);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
            },
        }
    }
}

pub async fn start_server(config: ServerConfig) -> Result<ServerHandle, ServerError> {
    let bind_addr = (config.host.as_str(), config.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| ServerError::new("server bind address did not resolve"))?;
    let listener = TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;
    let token_file = if config.token_file.as_os_str().is_empty() {
        config.pid_file.with_extension("token")
    } else {
        config.token_file.clone()
    };
    let token = generate_auth_token()?;
    publish_identity(&config.pid_file, &token_file, &token)?;

    let (shutdown, shutdown_rx) = broadcast::channel(1);
    let (state_updates, _) = broadcast::channel(16);
    let latest_state = Arc::new(RwLock::new(None));
    let state_cache_task = tokio::spawn(cache_latest_state(
        state_updates.subscribe(),
        shutdown.subscribe(),
        Arc::clone(&latest_state),
    ));
    let shutdown_announcement = Arc::new(ShutdownAnnouncement::default());
    if let Some(source) = config.state_source.clone() {
        *latest_state.write().unwrap() = Some(source.snapshot_json());
        let _background_tasks = source
            .clone()
            .start_background_tasks(state_updates.clone(), shutdown.clone());
        source.setup_mux_hooks(&config.host, addr.port(), &token_file.to_string_lossy());
    }
    let task_shutdown = shutdown.clone();
    let state_source = config.state_source.clone();
    let cleanup_state_source = state_source.clone();
    let loop_shutdown_announcement = Arc::clone(&shutdown_announcement);
    let task = tokio::spawn(async move {
        let result = run_accept_loop(
            listener,
            task_shutdown,
            shutdown_rx,
            state_source,
            state_updates,
            latest_state,
            loop_shutdown_announcement,
            token.clone(),
        )
        .await;
        state_cache_task.abort();
        if owns_identity_generation(&config.pid_file, &token_file, &token)
            && let Some(source) = cleanup_state_source.as_ref()
            && source.mux_namespace_available()
        {
            source.cleanup_mux_hooks();
            source.cleanup_sidebar_clients();
        }
        let cleanup_result = cleanup_owned_identity(&config.pid_file, &token_file, &token);
        match (result, cleanup_result) {
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    });

    Ok(ServerHandle {
        addr,
        shutdown,
        task,
    })
}

async fn run_accept_loop(
    listener: TcpListener,
    shutdown: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
    state_source: Option<Arc<dyn StateSource>>,
    state_updates: broadcast::Sender<String>,
    latest_state: Arc<RwLock<Option<String>>>,
    shutdown_announcement: Arc<ShutdownAnnouncement>,
    auth_token: String,
) -> Result<(), ServerError> {
    let connection_limit = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    let state_operation_lock = Arc::new(AsyncMutex::new(()));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                shutdown_announcement.announce_once(&state_source, &state_updates);
                tokio::time::sleep(Duration::from_millis(SERVER_SHUTDOWN_DRAIN_MS)).await;
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(connection_permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                    continue;
                };
                let connection_shutdown = shutdown.clone();
                let connection_state_source = state_source.clone();
                let connection_state_updates = state_updates.clone();
                let connection_latest_state = Arc::clone(&latest_state);
                let connection_shutdown_announcement = Arc::clone(&shutdown_announcement);
                let connection_auth_token = auth_token.clone();
                let connection_state_operation_lock = Arc::clone(&state_operation_lock);
                tokio::spawn(async move {
                    let _connection_permit = connection_permit;
                    let _ = handle_connection(
                        stream,
                        connection_shutdown,
                        connection_state_source,
                        connection_state_updates,
                        connection_latest_state,
                        connection_shutdown_announcement,
                        connection_auth_token,
                        connection_state_operation_lock,
                    )
                    .await;
                });
            }

        }
    }
}

fn announce_shutdown(
    state_source: &Option<Arc<dyn StateSource>>,
    state_updates: &broadcast::Sender<String>,
) {
    if let Some(payload) = state_source
        .as_ref()
        .and_then(|source| source.begin_shutdown())
    {
        let _ = state_updates.send(payload);
    }
    let _ = state_updates.send(QUIT_JSON.to_string());
}

fn request_shutdown(
    state_source: &Option<Arc<dyn StateSource>>,
    state_updates: &broadcast::Sender<String>,
    shutdown: &broadcast::Sender<()>,
    shutdown_announcement: &ShutdownAnnouncement,
) {
    shutdown_announcement.announce_once(state_source, state_updates);
    let _ = shutdown.send(());
}

async fn run_state_source_blocking<R, F>(
    state_source: &Option<Arc<dyn StateSource>>,
    state_operation_lock: &AsyncMutex<()>,
    operation: F,
) -> Result<Option<R>, ServerError>
where
    R: Send + 'static,
    F: FnOnce(&dyn StateSource) -> R + Send + 'static,
{
    let Some(state_source) = state_source.clone() else {
        return Ok(None);
    };
    // StateSource was intentionally synchronous and connection handling used
    // to serialize its tmux mutations on the runtime thread. Preserve that
    // ordering while moving the work itself to the blocking pool.
    let _operation_guard = state_operation_lock.lock().await;
    tokio::task::spawn_blocking(move || operation(state_source.as_ref()))
        .await
        .map(Some)
        .map_err(ServerError::from)
}

async fn handle_connection(
    mut stream: TcpStream,
    shutdown: broadcast::Sender<()>,
    state_source: Option<Arc<dyn StateSource>>,
    state_updates: broadcast::Sender<String>,
    latest_state: Arc<RwLock<Option<String>>>,
    shutdown_announcement: Arc<ShutdownAnnouncement>,
    auth_token: String,
    state_operation_lock: Arc<AsyncMutex<()>>,
) -> Result<(), ServerError> {
    let mut request = tokio::time::timeout(HTTP_READ_TIMEOUT, read_http_header(&mut stream))
        .await
        .map_err(|_| ServerError::new("timed out reading http request headers"))??;
    let parsed = parse_http_request(&request)?;
    let content_length = match parsed.content_length() {
        Ok(content_length) if content_length <= MAX_HTTP_BODY_BYTES => content_length,
        Ok(_) => {
            write_http_response(&mut stream, "413 Payload Too Large", "payload too large").await?;
            return Ok(());
        }
        Err(_) => {
            write_http_response(&mut stream, "400 Bad Request", "invalid content-length").await?;
            return Ok(());
        }
    };
    tokio::time::timeout(
        HTTP_READ_TIMEOUT,
        read_remaining_http_body(&mut stream, &mut request, content_length),
    )
    .await
    .map_err(|_| ServerError::new("timed out reading http request body"))??;

    // The root GET is the only unauthenticated liveness probe. Everything
    // capable of observing or mutating an instance, including WS upgrades,
    // must prove possession of that instance's token.
    if !(parsed.method == "GET" && parsed.path == "/" && !parsed.is_websocket_upgrade())
        && parsed.header("authorization") != Some(&format!("Bearer {auth_token}"))
    {
        write_http_response(&mut stream, "401 Unauthorized", "unauthorized").await?;
        return Ok(());
    }

    if shutdown_announcement.is_announced() {
        write_http_response(&mut stream, "503 Service Unavailable", "server is closing").await?;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/refresh" {
        if let Some(snapshot) = run_state_source_blocking(
            &state_source,
            &state_operation_lock,
            StateSource::snapshot_json,
        )
        .await?
        {
            let _ = state_updates.send(snapshot);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/focus" {
        let path = parsed.path.clone();
        let body = String::from_utf8_lossy(http_body(&request)).into_owned();
        if let Some(Some(payload)) =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_http_text(&path, &body)
            })
            .await?
        {
            let _ = state_updates.send(payload);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/switch-index" {
        let Some(index) = parsed
            .query_param("index")
            .and_then(|index| index.parse::<u32>().ok())
        else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 13\r\n\r\nmissing index")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        let body = String::from_utf8_lossy(http_body(&request)).into_owned();
        let _ = run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
            source.handle_switch_index(index, &body)
        })
        .await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && is_ok_hook_path(&parsed.path) {
        let path = parsed.path.clone();
        let body = String::from_utf8_lossy(http_body(&request)).into_owned();
        if let Some(Some(payload)) =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_http_hook(&path, &body)
            })
            .await?
        {
            let _ = state_updates.send(payload);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/agent-event" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        let result =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_agent_event_json(&body)
            })
            .await?
            .unwrap_or(Err(AgentEventError::CouldNotResolveSession));
        match result {
            Ok(payload) => {
                let _ = state_updates.send(payload);
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await?;
            }
            Err(err) => {
                let (status, body) = err.status_and_body();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await?;
            }
        }
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/runtime/pi/upsert" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        if let Some(Err(err)) =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_pi_runtime_upsert(&body)
            })
            .await?
        {
            let body = err.body();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/api/runtime/pi/delete" {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 12\r\n\r\ninvalid json")
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        };
        if let Some(Err(err)) =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_pi_runtime_delete(&body)
            })
            .await?
        {
            let body = err.body();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await?;
        let _ = stream.shutdown().await;
        return Ok(());
    }

    if parsed.method == "POST" && is_metadata_path(&parsed.path) {
        let Ok(body) = serde_json::from_slice::<Value>(http_body(&request)) else {
            write_http_response(&mut stream, "400 Bad Request", "invalid json").await?;
            return Ok(());
        };
        if !body.get("session").is_some_and(Value::is_string) {
            write_http_response(&mut stream, "400 Bad Request", "missing session").await?;
            return Ok(());
        }
        let path = parsed.path.clone();
        let Some(Some(payload)) =
            run_state_source_blocking(&state_source, &state_operation_lock, move |source| {
                source.handle_http_json(&path, &body)
            })
            .await?
        else {
            write_http_response(&mut stream, "400 Bad Request", "invalid payload").await?;
            return Ok(());
        };
        let _ = state_updates.send(payload);
        write_http_response(&mut stream, "204 No Content", "").await?;
        return Ok(());
    }

    if is_metadata_path(&parsed.path) {
        write_http_response(&mut stream, "405 Method Not Allowed", "method not allowed").await?;
        return Ok(());
    }

    if parsed.method == "POST" && parsed.path == "/quit" {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .await?;
        let _ = stream.shutdown().await;
        request_shutdown(
            &state_source,
            &state_updates,
            &shutdown,
            &shutdown_announcement,
        );
        return Ok(());
    }

    if parsed.is_websocket_upgrade() {
        let Some(key) = parsed.header("sec-websocket-key") else {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await?;
            return Ok(());
        };
        let accept = websocket_accept(key);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await?;

        let mut websocket = ServerBuilder::new().serve(stream);
        debug_log("ws: client connected, sending hello + initial state");
        websocket.send(Message::text(HELLO_JSON)).await?;
        let initial_state = latest_state.read().unwrap().clone();
        let initial_state = if initial_state.is_some() {
            initial_state
        } else {
            run_state_source_blocking(
                &state_source,
                &state_operation_lock,
                StateSource::snapshot_json,
            )
            .await?
        };
        if let Some(initial_state) = initial_state {
            websocket.send(Message::text(initial_state)).await?;
        }

        let mut connection_shutdown = shutdown.subscribe();
        let mut state_rx = state_updates.subscribe();
        let mut client_context = ClientConnectionContext::default();
        let mut pending_state: Option<String> = None;
        let mut state_flush =
            tokio::time::interval(Duration::from_millis(RENDERED_SIDEBAR_FRAME_MS));
        state_flush.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;

                _ = connection_shutdown.recv() => {
                    let _ = websocket.send(Message::text(QUIT_JSON)).await;
                    return Ok(());
                }
                message = websocket.next() => {
                    match message {
                        Some(Ok(message)) if message.is_close() => return Ok(()),
                        Some(Ok(message)) => {
                            if is_quit_command(&message) {
                                request_shutdown(
                                    &state_source,
                                    &state_updates,
                                    &shutdown,
                                    &shutdown_announcement,
                                );
                                return Ok(());
                            }
                            if is_command_type(&message, "refresh")
                                && let Some(snapshot) = run_state_source_blocking(
                                    &state_source,
                                    &state_operation_lock,
                                    StateSource::snapshot_json,
                                ).await?
                            {
                                let _ = state_updates.send(snapshot);
                            }
                            if let Some(command) = parse_command(&message) {
                                let sender_command = command.clone();
                                let sender_context = client_context.clone();
                                if let Some((reply, updated_context)) = run_state_source_blocking(
                                    &state_source,
                                    &state_operation_lock,
                                    move |source| {
                                        let mut context = sender_context;
                                        let reply = source.handle_sender_command_with_context(
                                            &sender_command,
                                            &mut context,
                                        );
                                        (reply, context)
                                    },
                                ).await?
                                {
                                    client_context = updated_context;
                                    if let Some(reply) = reply {
                                        websocket.send(Message::text(reply)).await?;
                                    }
                                }
                                if let Some(name) = switch_session_target(&command) {
                                    let _ = state_updates.send(activate_session_json(
                                        name,
                                        client_context.pane_id.as_deref(),
                                    ));
                                    tokio::task::yield_now().await;
                                }
                                let command_for_handler = command.clone();
                                let context_for_handler = client_context.clone();
                                if let Some(Some(payload)) = run_state_source_blocking(
                                    &state_source,
                                    &state_operation_lock,
                                    move |source| source.handle_client_command_with_context(
                                        &command_for_handler,
                                        Some(&context_for_handler),
                                    ),
                                ).await?
                                {
                                    if is_client_view_command(&command) {
                                        websocket.send(Message::text(payload)).await?;
                                    } else {
                                        let _ = state_updates.send(payload);
                                    }
                                }
                            }
                        }
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
                _ = state_flush.tick(), if pending_state.is_some() => {
                    let state = pending_state.take().expect("pending state checked above");
                    debug_log(format!(
                        "ws: flushing latest broadcast state ({} bytes) to client",
                        state.len()
                    ));
                    websocket.send(Message::text(state)).await?;
                }
                state = state_rx.recv() => {
                    match state {
                        Ok(state) => {
                            if state == QUIT_JSON {
                                let _ = websocket.send(Message::text(QUIT_JSON)).await;
                                return Ok(());
                            }
                            if is_immediate_server_message(&state) {
                                websocket.send(Message::text(state)).await?;
                                continue;
                            }
                            pending_state = Some(state);
                        }
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            debug_log(format!("ws: state_rx lagged by {n} messages"));
                        }
                    }
                }
            }
        }
    }

    if parsed.method == "GET" && parsed.path == "/" {
        write_http_response(&mut stream, "200 OK", "opensessions server").await?;
    } else {
        write_http_response(&mut stream, "404 Not Found", "not found").await?;
    }
    Ok(())
}

async fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
) -> Result<(), ServerError> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn app_from_state_json(state_json: &str) -> Option<SidebarApp> {
    let SidebarServerMessage::State(state) =
        serde_json::from_str::<SidebarServerMessage>(state_json).ok()?
    else {
        return None;
    };
    Some(SidebarApp::from_state(state))
}

async fn read_http_header(stream: &mut TcpStream) -> Result<Vec<u8>, ServerError> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];

    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(ServerError::new("client closed before sending request"));
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(request);
        }
        if request.len() > MAX_HTTP_HEADER_BYTES {
            return Err(ServerError::new("http request headers exceeded limit"));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct HttpRequest {
    method: String,
    path: String,
    query: Option<String>,
    headers: Vec<(String, String)>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }

    fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            && self
                .header("connection")
                .is_some_and(|value| contains_token_ignore_ascii_case(value, "upgrade"))
    }

    fn content_length(&self) -> Result<usize, ServerError> {
        self.header("content-length")
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| ServerError::new("invalid content-length"))
            })
            .unwrap_or(Ok(0))
    }

    fn query_param(&self, name: &str) -> Option<&str> {
        self.query.as_deref()?.split('&').find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name).then_some(value)
        })
    }
}

fn parse_http_request(bytes: &[u8]) -> Result<HttpRequest, ServerError> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| ServerError::new("http request missing header terminator"))?;
    let text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| ServerError::new("http request headers were not utf-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| ServerError::new("http request missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| ServerError::new("http request missing method"))?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| ServerError::new("http request missing target"))?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target.to_string(), None),
    };

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();

    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
    })
}

fn contains_token_ignore_ascii_case(value: &str, needle: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case(needle))
}

fn is_metadata_path(path: &str) -> bool {
    matches!(
        path,
        "/set-status" | "/set-progress" | "/log" | "/notify" | "/clear-log"
    )
}

fn is_ok_hook_path(path: &str) -> bool {
    matches!(
        path,
        "/pane-exited"
            | "/pane-layout-changed"
            | "/client-resized"
            | "/ensure-sidebar"
            | "/ensure-sidebars"
            | "/set-sidebar-width"
            | "/toggle"
    )
}

async fn read_remaining_http_body(
    stream: &mut TcpStream,
    request: &mut Vec<u8>,
    content_length: usize,
) -> Result<(), ServerError> {
    let remaining = content_length.saturating_sub(http_body(request).len());
    if remaining == 0 {
        return Ok(());
    }

    let start_len = request.len();
    let end_len = start_len
        .checked_add(remaining)
        .ok_or_else(|| ServerError::new("http request body length overflowed"))?;
    request.resize(end_len, 0);
    stream.read_exact(&mut request[start_len..]).await?;
    Ok(())
}

fn http_body(request: &[u8]) -> &[u8] {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return &[];
    };
    &request[header_end + 4..]
}

fn websocket_accept(key: &str) -> String {
    let mut sha1 = Sha1::new();
    sha1.update(key.as_bytes());
    sha1.update(WEBSOCKET_GUID.as_bytes());
    STANDARD.encode(sha1.digest().bytes())
}

fn is_quit_command(message: &Message) -> bool {
    is_command_type(message, "quit")
}

fn is_command_type(message: &Message, command_type: &str) -> bool {
    parse_command(message)
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .as_deref()
        == Some(command_type)
}

fn is_client_view_command(command: &Value) -> bool {
    matches!(
        command.get("type").and_then(Value::as_str),
        Some("switch-session" | "switch-index")
    )
}

fn switch_session_target(command: &Value) -> Option<String> {
    (command.get("type").and_then(Value::as_str) == Some("switch-session"))
        .then(|| command.get("name")?.as_str().map(str::to_string))?
}

fn is_immediate_server_message(payload: &str) -> bool {
    payload.contains(r#""type":"activate-session""#)
}

fn clamp_detail_panel_height(height: u16) -> u16 {
    height.clamp(MIN_DETAIL_PANEL_HEIGHT, MAX_DETAIL_PANEL_HEIGHT)
}

fn parse_command(message: &Message) -> Option<Value> {
    serde_json::from_str::<Value>(message.as_text()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_SERVER_ID: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn state_source_loads_persisted_theme() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-theme-config-test-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let config_dir = home.join(".config/opensessions");
        fs::create_dir_all(&config_dir).expect("create config directory");
        fs::write(
            config_dir.join("config.json"),
            r#"{"theme":"electric-fusion","transparentBackground":true}"#,
        )
        .expect("write config");

        let source = default_state_source_from_env(|key| match key {
            "TMUX" => Some("/tmp/opensessions-theme-test,1,1".to_string()),
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("tmux state source");

        assert_eq!(
            source.theme.lock().unwrap().as_deref(),
            Some("electric-fusion")
        );
        assert!(*source.transparent_background.lock().unwrap());
        fs::remove_dir_all(home).expect("remove config directory");
    }

    #[test]
    fn legacy_transparent_theme_becomes_a_background_option() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-transparent-theme-config-test-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let config_dir = home.join(".config/opensessions");
        fs::create_dir_all(&config_dir).expect("create config directory");
        fs::write(config_dir.join("config.json"), r#"{"theme":"transparent"}"#)
            .expect("write config");

        let source = default_state_source_from_env(|key| match key {
            "TMUX" => Some("/tmp/opensessions-transparent-theme-test,1,1".to_string()),
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("tmux state source");

        assert_eq!(
            source.theme.lock().unwrap().as_deref(),
            Some("catppuccin-mocha")
        );
        assert!(*source.transparent_background.lock().unwrap());
        fs::remove_dir_all(home).expect("remove config directory");
    }

    #[test]
    fn amp_log_scanner_reads_current_cloud_thread_logs() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-amp-log-test-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let logs = home.join(".cache/amp/logs/threads");
        fs::create_dir_all(&logs).expect("create Amp log directory");
        fs::write(
            logs.join("T-current.log"),
            r#"{"message":"onToolLease","data":{"args":{"workdir":"/repo"}}}
{"type":"agent_state","direction":"receive","subtype":"idle"}
"#,
        )
        .expect("write Amp log");

        let mut snapshots = Vec::new();
        scan_amp_logs(&home, current_time_ms(), &mut snapshots);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].thread_id.as_deref(), Some("T-current"));
        assert_eq!(snapshots[0].project_dir.as_deref(), Some("/repo"));
        assert_eq!(snapshots[0].status, AgentStatus::Done);
        fs::remove_dir_all(home).expect("remove Amp log directory");
    }

    #[derive(Clone)]
    struct SlowSnapshotSource {
        snapshot_count: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl StateSource for SlowSnapshotSource {
        fn snapshot_json(&self) -> String {
            self.snapshot_count.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            "{}".to_string()
        }
    }

    struct PortTestProvider;

    impl MuxProvider for PortTestProvider {
        fn name(&self) -> &str {
            "port-test"
        }

        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            vec![opensessions_runtime::mux::MuxSessionInfo {
                name: "session".to_string(),
                created_at: 0,
                dir: String::new(),
                windows: 1,
            }]
        }

        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("session".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
        }
        fn get_session_pane_pids(&self, _name: &str) -> Vec<u32> {
            vec![10]
        }
        fn get_pane_count(&self, _name: &str) -> u32 {
            1
        }
        fn get_client_tty(&self) -> String {
            String::new()
        }
        fn create_session(&self, _name: Option<&str>, _dir: Option<&str>) {}
        fn kill_session(&self, _name: &str) {}
        fn setup_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {}
        fn cleanup_hooks(&self) {}
    }

    struct CountingPortRunner {
        calls: Arc<AtomicUsize>,
    }

    impl PortCommandRunner for CountingPortRunner {
        fn process_rows(&self) -> Vec<(u32, u32)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(25));
            vec![(10, 1)]
        }

        fn lsof_fields(&self) -> String {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(25));
            "p10\nn8080\n".to_string()
        }
    }

    #[test]
    fn idle_polling_backs_off_and_resets_after_activity() {
        assert_eq!(adaptive_poll_delay_ms(0, 2_000, 30_000), 2_000);
        assert_eq!(adaptive_poll_delay_ms(1, 2_000, 30_000), 4_000);
        assert_eq!(adaptive_poll_delay_ms(4, 2_000, 30_000), 30_000);
        assert_eq!(adaptive_poll_delay_ms(20, 2_000, 30_000), 30_000);
        assert!(agent_status_needs_fast_polling(AgentStatus::Running));
        assert!(agent_status_needs_fast_polling(AgentStatus::ToolRunning));
        assert!(agent_status_needs_fast_polling(AgentStatus::Waiting));
        assert!(!agent_status_needs_fast_polling(AgentStatus::Done));
        assert!(!agent_status_needs_fast_polling(AgentStatus::Stale));
    }

    #[test]
    fn concurrent_port_snapshots_share_one_discovery() {
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(
            ReadOnlyMuxStateSource::new(vec![Arc::new(PortTestProvider)]).with_port_command_runner(
                Arc::new(CountingPortRunner {
                    calls: Arc::clone(&calls),
                }),
            ),
        );
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let source = Arc::clone(&source);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    source.discover_live_ports(Some(&["session".to_string()]), false)
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            assert_eq!(
                worker.join().expect("port discovery worker"),
                Some(HashMap::from([("session".to_string(), Vec::new())]))
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    async fn send_raw_request(request: &[u8]) -> Vec<u8> {
        send_raw_request_with_auth(request, true).await
    }

    async fn send_raw_request_with_auth(request: &[u8], authorized: bool) -> Vec<u8> {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let pid_file = std::env::temp_dir().join(format!(
            "opensessions-server-test-{}-{id}.pid",
            process::id()
        ));
        let token_file = pid_file.with_extension("token");
        let server = start_server(ServerConfig::new("127.0.0.1", 0, &pid_file))
            .await
            .expect("start test server");
        let token = fs::read_to_string(token_file).expect("read test token");
        let mut stream = TcpStream::connect(server.addr())
            .await
            .expect("connect to test server");
        let request = if authorized {
            let split = request.windows(2).position(|window| window == b"\r\n");
            split.map_or_else(
                || request.to_vec(),
                |index| {
                    let mut authenticated = request[..index + 2].to_vec();
                    authenticated.extend_from_slice(
                        format!("Authorization: Bearer {}\r\n", token.trim()).as_bytes(),
                    );
                    authenticated.extend_from_slice(&request[index + 2..]);
                    authenticated
                },
            )
        } else {
            request.to_vec()
        };
        stream
            .write_all(&request)
            .await
            .expect("write test request");

        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
            .await
            .expect("server should close the request")
            .expect("read server response");
        server.shutdown().await.expect("stop test server");
        response
    }

    #[tokio::test]
    async fn non_liveness_http_routes_require_instance_token() {
        let unauthorized = send_raw_request_with_auth(
            b"POST /refresh HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n",
            false,
        )
        .await;
        assert!(unauthorized.starts_with(b"HTTP/1.1 401 Unauthorized"));

        let liveness =
            send_raw_request_with_auth(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n", false).await;
        assert!(liveness.starts_with(b"HTTP/1.1 200 OK"));
    }

    async fn request_at(addr: SocketAddr, request: String) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = Vec::new();
        if let Ok(result) = tokio::time::timeout(
            Duration::from_millis(100),
            stream.read_to_end(&mut response),
        )
        .await
        {
            result.expect("read response");
        }
        response
    }

    #[tokio::test]
    async fn slow_state_snapshot_does_not_block_server_liveness() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-slow-snapshot-{}-{id}", process::id()));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let snapshot_count = Arc::new(AtomicUsize::new(0));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(SlowSnapshotSource {
                    snapshot_count: Arc::clone(&snapshot_count),
                    delay: Duration::from_millis(300),
                }),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let addr = server.addr();
        let started = Instant::now();
        let refresh = tokio::spawn(request_at(
            addr,
            format!(
                "POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
                token.trim()
            ),
        ));

        while snapshot_count.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the runtime was blocked by synchronous snapshot work"
        );

        let liveness = request_at(
            addr,
            "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
        )
        .await;
        assert!(liveness.starts_with(b"HTTP/1.1 200 OK"));

        let _ = refresh.await;
        server.shutdown().await.expect("stop server");
    }

    #[tokio::test]
    async fn websocket_upgrade_requires_the_matching_instance_token() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-ws-auth-{}-{id}", process::id()));
        let pid = root.with_extension("pid");
        let token_path = root.with_extension("token");
        let server =
            start_server(ServerConfig::new("127.0.0.1", 0, &pid).with_token_file(&token_path))
                .await
                .expect("start");
        let token = fs::read_to_string(&token_path).expect("token");
        let upgrade = |authorization: &str| {
            format!(
                "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{authorization}\r\n"
            )
        };
        let denied = request_at(server.addr(), upgrade("")).await;
        assert!(denied.starts_with(b"HTTP/1.1 401 Unauthorized"));
        let accepted = request_at(
            server.addr(),
            upgrade(&format!("Authorization: Bearer {}\r\n", token.trim())),
        )
        .await;
        assert!(accepted.starts_with(b"HTTP/1.1 101 Switching Protocols"));
        server.shutdown().await.expect("stop");
    }

    #[tokio::test]
    async fn tokens_are_isolated_and_rotate_on_restart() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-isolation-{}-{id}", process::id()));
        let pid_a = root.with_extension("a.pid");
        let token_a = root.with_extension("a.token");
        let pid_b = root.with_extension("b.pid");
        let token_b = root.with_extension("b.token");
        let first =
            start_server(ServerConfig::new("127.0.0.1", 0, &pid_a).with_token_file(&token_a))
                .await
                .expect("first");
        let second =
            start_server(ServerConfig::new("127.0.0.1", 0, &pid_b).with_token_file(&token_b))
                .await
                .expect("second");
        let first_token = fs::read_to_string(&token_a).expect("first token");
        let second_token = fs::read_to_string(&token_b).expect("second token");
        assert_ne!(first_token, second_token);
        let wrong = request_at(second.addr(), format!("POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n", first_token.trim())).await;
        assert!(wrong.starts_with(b"HTTP/1.1 401 Unauthorized"));
        let first_addr = first.addr();
        first.shutdown().await.expect("stop first");
        let restarted = start_server(
            ServerConfig::new("127.0.0.1", first_addr.port(), &pid_a).with_token_file(&token_a),
        )
        .await
        .expect("restart");
        let rotated = fs::read_to_string(&token_a).expect("rotated token");
        assert_ne!(first_token, rotated);
        let stale = request_at(restarted.addr(), format!("POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n", first_token.trim())).await;
        assert!(stale.starts_with(b"HTTP/1.1 401 Unauthorized"));
        restarted.shutdown().await.expect("stop restart");
        second.shutdown().await.expect("stop second");
    }

    #[tokio::test]
    async fn oversized_http_body_is_rejected_before_it_is_read() {
        let response = send_raw_request(
            format!(
                "POST /api/agent-event HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
                MAX_HTTP_BODY_BYTES + 1
            )
            .as_bytes(),
        )
        .await;

        assert!(response.starts_with(b"HTTP/1.1 413 Payload Too Large"));
    }

    #[tokio::test]
    async fn malformed_metadata_and_unknown_routes_return_errors() {
        let malformed = send_raw_request(
            b"POST /set-status HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\n\r\n{",
        )
        .await;
        assert!(malformed.starts_with(b"HTTP/1.1 400 Bad Request"));

        let unknown = send_raw_request(
            b"POST /unknown HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(unknown.starts_with(b"HTTP/1.1 404 Not Found"));

        let invalid_length = send_raw_request(
            b"POST /log HTTP/1.1\r\nHost: localhost\r\nContent-Length: nope\r\n\r\n",
        )
        .await;
        assert!(invalid_length.starts_with(b"HTTP/1.1 400 Bad Request"));
    }

    #[tokio::test]
    async fn incomplete_http_requests_are_closed_after_the_read_deadline() {
        let partial_header =
            send_raw_request(b"POST /api/agent-event HTTP/1.1\r\nHost: localhost\r\n").await;
        assert!(partial_header.is_empty());

        let partial_body = send_raw_request(
            b"POST /api/agent-event HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n{",
        )
        .await;
        assert!(partial_body.is_empty());
    }

    #[tokio::test]
    async fn sidebar_width_repair_requests_are_coalesced_without_losing_active_requests() {
        let scheduler = Arc::new(SidebarWidthRepairScheduler::default());
        let (shutdown, _) = broadcast::channel(1);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let worker_scheduler = Arc::clone(&scheduler);
        let callback_scheduler = Arc::clone(&scheduler);
        let callback_shutdown = shutdown.clone();
        let callback_batches = Arc::clone(&batches);
        let worker = tokio::spawn(run_coalesced_sidebar_width_repairs(
            worker_scheduler,
            shutdown.subscribe(),
            Duration::from_millis(1),
            move |request_count| {
                let callback_scheduler = Arc::clone(&callback_scheduler);
                let callback_shutdown = callback_shutdown.clone();
                let callback_batches = Arc::clone(&callback_batches);
                async move {
                    let mut batches = callback_batches.lock().unwrap();
                    batches.push(request_count);
                    if batches.len() == 1 {
                        callback_scheduler.request();
                    } else {
                        let _ = callback_shutdown.send(());
                    }
                }
            },
        ));

        scheduler.request();
        scheduler.request();
        scheduler.request();

        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("repair worker should stop")
            .expect("repair worker should not panic");
        assert_eq!(*batches.lock().unwrap(), vec![3, 1]);
    }
}
