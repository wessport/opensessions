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
    AgentWatcherSnapshot, amp_log_pid, amp_log_thread_title, amp_snapshot_from_log_jsonl,
    amp_snapshot_from_thread_json, claude_code_snapshot_from_jsonl, codex_snapshot_from_jsonl,
    codex_thread_id_from_path, decode_claude_project_dir, droid_snapshot_from_jsonl,
    opencode_snapshot_from_row, parse_codex_session_index, pi_snapshot_from_jsonl,
};
use opensessions_runtime::config::{
    AutoHibernateSettings, OpensessionsConfig, SidebarPosition as ConfigSidebarPosition,
    load_config_from_home, save_config_to_home,
};
use opensessions_runtime::git_info::{GIT_INFO_SECTION_SEPARATOR, GitInfo, parse_git_info_output};
use opensessions_runtime::hibernate::{
    AgentProcessTarget, HIBERNATE_POLL_INTERVAL_MS, HIBERNATE_TERM_GRACE, ProcessControl,
    ProcessEntry, SystemProcessControl, find_agent_process, terminate_agent_processes_if,
};
use opensessions_runtime::metadata_store::SessionMetadataStore;
use opensessions_runtime::mux::{ActiveWindow, MuxProvider, SidebarPosition};
use opensessions_runtime::pi_runtime_registry::{PiRuntimeRegistry, parse_pi_runtime_info};
use opensessions_runtime::port_discovery::{PortDiscoveryInput, discover_session_ports};
use opensessions_runtime::project_dir_session::{
    build_dir_session_map, resolve_session_for_project_dir,
};
use opensessions_runtime::protocol::{
    AgentEvent, AgentLiveness, AgentPanelScope, AgentStatus, MetadataTone, ServerMessage,
    ServerState, SessionFilterMode, WindowData,
};
use opensessions_runtime::server_state::{ReadOnlyStateInput, build_read_only_state};
use opensessions_runtime::session_order::SessionOrder;
use opensessions_runtime::shared::resolve_server_key;
use opensessions_runtime::sidebar_coordinator::{SidebarCoordinator, SidebarLifecycle};
use opensessions_runtime::sidebar_width_sync::clamp_sidebar_width;
use opensessions_runtime::subprocess::{PROCESS_PROBE_TIMEOUT, output_with_timeout};
use opensessions_runtime::tmux_provider::{StdCommandRunner, TmuxProvider};
use opensessions_runtime::tracker::{AgentTracker, HibernationCandidate, PanePresenceInput};
use opensessions_sidebar_core::app::App as SidebarApp;
use opensessions_sidebar_core::generated::protocol::ServerMessage as SidebarServerMessage;
use serde_json::Value;
use sha1_smol::Sha1;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore, broadcast, mpsc};
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
/// Descriptors kept free for everything that is not a client connection:
/// stdio, the listener, identity and log files, and the pipes of concurrent
/// tmux, Git, `ps`, and `lsof` children.
const FD_HEADROOM: u64 = 96;
/// Startup raises the soft descriptor limit toward this (bounded by the hard
/// limit). macOS defaults to a soft limit of 256 and rejects soft limits above
/// `OPEN_MAX` (10240) when the hard limit is unlimited.
const FD_SOFT_LIMIT_TARGET: u64 = 8_192;
const MIN_CONCURRENT_CONNECTIONS: usize = 16;
const MAX_CONCURRENT_CONNECTIONS: usize = 2_048;
const MIN_RESERVED_HTTP_CONNECTIONS: usize = 8;
const MIN_PASSIVE_WEBSOCKETS: usize = 4;
/// Websocket path sidebars connect on. Every visited window keeps one sidebar
/// connection, so sidebars may use the whole websocket capacity while other
/// (passive) websocket clients are limited to a share of it.
pub const SIDEBAR_WEBSOCKET_PATH: &str = "/?client=sidebar";
const MAX_CONNECTIONS_ENV: &str = "OPENSESSIONS_MAX_CONNECTIONS";
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SIDEBAR_SCRIPTS_DIR: &str = "apps/tui/scripts";
const IDENTITY_REPAIR_INTERVAL: Duration = Duration::from_secs(1);
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
/// The startup seed pass looks back this far for idle agents' last activity.
const AGENT_WATCHER_SEED_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Newest files per agent source read by the startup seed pass.
const AGENT_WATCHER_SEED_MAX_FILES: usize = 32;
const AMP_LOG_TAIL_BYTES: u64 = 1024 * 1024;
const AMP_LOG_PID_TAIL_BYTES: u64 = 64 * 1024;
/// Amp logs a thread's generated title near the start of its log, so logs
/// whose tail has no title are named from this much of their head.
const AMP_LOG_TITLE_HEAD_BYTES: u64 = 256 * 1024;
const STUCK_RUNNING_TIMEOUT_MS: u64 = 3 * 60 * 1000;
const OPENCODE_SQL_TIMEOUT_MS: u64 = 500;
const OPENCODE_SQL_SEP: char = '\u{1f}';
const DEFAULT_DETAIL_PANEL_HEIGHT: u16 = 10;
const MIN_DETAIL_PANEL_HEIGHT: u16 = 4;
const MAX_DETAIL_PANEL_HEIGHT: u16 = 60;

/// Connection caps derived from the process descriptor limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// All concurrent connections, HTTP and websocket.
    pub total: usize,
    /// Websockets; the remainder stays available for HTTP hooks and probes.
    pub websockets: usize,
    /// Websockets that did not connect as a sidebar.
    pub passive_websockets: usize,
}

impl ConnectionLimits {
    /// Bounds connections by the soft descriptor limit less `FD_HEADROOM`,
    /// never above `MAX_CONCURRENT_CONNECTIONS`. A configured cap can lower,
    /// but not raise, that bound.
    pub fn from_fd_limit(fd_soft_limit: u64, configured: Option<usize>) -> Self {
        let fd_budget =
            usize::try_from(fd_soft_limit.saturating_sub(FD_HEADROOM)).unwrap_or(usize::MAX);
        let hard_cap = fd_budget.clamp(MIN_CONCURRENT_CONNECTIONS, MAX_CONCURRENT_CONNECTIONS);
        let total = configured.map_or(hard_cap, |configured| {
            configured.clamp(MIN_CONCURRENT_CONNECTIONS, hard_cap)
        });
        let reserved_http = (total / 16).max(MIN_RESERVED_HTTP_CONNECTIONS);
        let websockets = total - reserved_http;
        let passive_websockets = (websockets / 8)
            .max(MIN_PASSIVE_WEBSOCKETS)
            .min(websockets / 2);
        Self {
            total,
            websockets,
            passive_websockets,
        }
    }
}

/// Reads `OPENSESSIONS_MAX_CONNECTIONS`, a positive connection cap.
pub fn max_connections_from_env(env: impl Fn(&str) -> Option<String>) -> Option<usize> {
    env(MAX_CONNECTIONS_ENV)?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|limit| *limit > 0)
}

/// Returns the (soft, hard) `RLIMIT_NOFILE` descriptor limits.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `rlim_t` is not `u64` on every unix target.
fn descriptor_limits() -> Option<(u64, u64)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the provided, properly sized struct.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    Some((limit.rlim_cur as u64, limit.rlim_max as u64))
}

#[cfg(not(unix))]
fn descriptor_limits() -> Option<(u64, u64)> {
    None
}

/// Raises the soft descriptor limit toward `FD_SOFT_LIMIT_TARGET`, bounded by
/// the hard limit, and returns the resulting soft limit. Never lowers it.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `rlim_t` is not `u64` on every unix target.
pub fn raise_fd_soft_limit() -> Option<u64> {
    let (soft, hard) = descriptor_limits()?;
    let target = hard.min(FD_SOFT_LIMIT_TARGET);
    if soft >= target {
        return Some(soft);
    }
    let raised = libc::rlimit {
        rlim_cur: target as libc::rlim_t,
        rlim_max: hard as libc::rlim_t,
    };
    // SAFETY: setrlimit only reads the provided, properly sized struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
        Some(target)
    } else {
        Some(soft)
    }
}

#[cfg(not(unix))]
pub fn raise_fd_soft_limit() -> Option<u64> {
    None
}

/// Websocket admission: every websocket takes a websocket slot, and clients
/// that did not connect as sidebars also take a passive slot, so passive
/// clients can never crowd out sidebars.
#[derive(Debug, Clone)]
struct WebsocketCapacity {
    websockets: Arc<Semaphore>,
    passive: Arc<Semaphore>,
}

impl WebsocketCapacity {
    fn new(limits: ConnectionLimits) -> Self {
        Self {
            websockets: Arc::new(Semaphore::new(limits.websockets)),
            passive: Arc::new(Semaphore::new(limits.passive_websockets)),
        }
    }

    /// Permits are released when the returned guard drops, which happens as
    /// soon as the connection's handler returns.
    fn try_admit(
        &self,
        sidebar: bool,
    ) -> Option<(
        tokio::sync::OwnedSemaphorePermit,
        Option<tokio::sync::OwnedSemaphorePermit>,
    )> {
        let passive = if sidebar {
            None
        } else {
            Some(Arc::clone(&self.passive).try_acquire_owned().ok()?)
        };
        let websocket = Arc::clone(&self.websockets).try_acquire_owned().ok()?;
        Some((websocket, passive))
    }
}

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

/// Append a single debug line when explicit diagnostics are enabled. Shares
/// the sidebars' size cap and whole-line writes, so leaving
/// `OPENSESSIONS_DEBUG_LOG` set can't grow the file without bound.
fn debug_log(line: impl AsRef<str>) {
    let Some(path) =
        opensessions_runtime::debug_log::debug_log_path_from_env(|key| std::env::var(key).ok())
    else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    opensessions_runtime::debug_log::append_bounded(
        &path,
        &format!(
            "[{now}] [server pid={}] {}",
            std::process::id(),
            line.as_ref()
        ),
        opensessions_runtime::debug_log::DEBUG_LOG_MAX_BYTES,
    );
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

    fn handle_agent_event_json(&self, _body: &Value) -> Result<(), AgentEventError> {
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

/// `lsof` can take a second or more on hosts with many open files.
const LSOF_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// `git status` in a large worktree can be slow; still bound it.
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct SystemPortCommandRunner;

#[derive(Debug, Default)]
struct SystemGitCommandRunner;

impl PortCommandRunner for SystemPortCommandRunner {
    fn process_rows(&self) -> Vec<(u32, u32)> {
        let Ok(output) = output_with_timeout(
            process::Command::new("ps").args(["-eo", "pid=,ppid="]),
            PROCESS_PROBE_TIMEOUT,
        ) else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_process_row)
            .collect()
    }

    fn lsof_fields(&self) -> String {
        let Ok(output) = output_with_timeout(
            process::Command::new("lsof").args(["-iTCP", "-sTCP:LISTEN", "-nP", "-F", "pn"]),
            LSOF_COMMAND_TIMEOUT,
        ) else {
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

        let Ok(rev_parse) = output_with_timeout(
            process::Command::new("git").current_dir(dir).args([
                "rev-parse",
                "--abbrev-ref",
                "HEAD",
                "--git-dir",
            ]),
            GIT_COMMAND_TIMEOUT,
        ) else {
            return String::new();
        };
        if !rev_parse.status.success() {
            return String::new();
        }

        let Ok(status) = output_with_timeout(
            process::Command::new("git")
                .current_dir(dir)
                .args(["status", "--porcelain"]),
            GIT_COMMAND_TIMEOUT,
        ) else {
            return String::new();
        };

        let Ok(numstat) = output_with_timeout(
            process::Command::new("git")
                .current_dir(dir)
                .args(["diff", "--numstat", "HEAD", "--"]),
            GIT_COMMAND_TIMEOUT,
        ) else {
            return String::new();
        };

        let separator = GIT_INFO_SECTION_SEPARATOR;
        format!(
            "{}{separator}{}{separator}{}",
            String::from_utf8_lossy(&rev_parse.stdout).trim(),
            String::from_utf8_lossy(&status.stdout),
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
    sidebar_position: SidebarPosition,
    detail_panel_height: Mutex<u16>,
    agent_panel_scope: Mutex<AgentPanelScope>,
    focused_session: Mutex<Option<String>>,
    focused_pane_by_session: Mutex<HashMap<String, String>>,
    focused_client_tty: Mutex<Option<String>>,
    theme: Mutex<Option<String>>,
    transparent_background: Mutex<bool>,
    // Serializes each shared-settings mutation with snapshot capture so a
    // snapshot can never pair old values with a newer revision.
    settings_revision: Mutex<u64>,
    session_filter: Mutex<Option<SessionFilterMode>>,
    collapsed_worktree_groups: Mutex<HashSet<String>>,
    session_order: Mutex<SessionOrder>,
    metadata_store: Mutex<SessionMetadataStore>,
    agent_tracker: Mutex<AgentTracker>,
    pi_runtime_registry: Mutex<PiRuntimeRegistry>,
    tmux_socket_path: Option<PathBuf>,
    auto_hibernate: AutoHibernateSettings,
    process_control: Arc<dyn ProcessControl>,
    /// Home directory holding agents' own durable state (transcripts, logs).
    agent_state_home: Option<PathBuf>,
    /// Agent panes found for writer pids recorded in agent state files.
    agent_pane_routes: Mutex<AgentPaneRouteCache>,
    /// Last full state built, so shutdown can announce `closing…` without
    /// running tmux, Git, or port discovery.
    last_state: Mutex<Option<ServerState>>,
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
        let home = env("HOME").map(PathBuf::from);
        let config = home.as_ref().map(|home| load_config_from_home(home));
        if let Some(home) = home {
            let config_dir = home.join(".config").join("opensessions");
            let legacy_path = config_dir.join("session-order.json");
            let server_key = resolve_server_key(&env);
            let persist_path = server_key
                .map(|key| config_dir.join(format!("session-order.{key}.json")))
                .unwrap_or_else(|| legacy_path.clone());
            // Copy the legacy global order once per socket. Keeping the old
            // file allows every existing tmux namespace to migrate without
            // one namespace stealing the user's prior settings from another.
            if persist_path != legacy_path && !persist_path.exists() && legacy_path.exists() {
                let _ = fs::copy(&legacy_path, &persist_path);
            }
            source = source.with_session_order_path(persist_path);
        }
        if let Some(width) = config.as_ref().and_then(|config| config.sidebar_width) {
            source = source.with_sidebar_width(clamp_sidebar_width(width) as u32);
        }
        if let Some(position) = config.as_ref().and_then(|config| config.sidebar_position) {
            source = source.with_sidebar_position(match position {
                ConfigSidebarPosition::Left => SidebarPosition::Left,
                ConfigSidebarPosition::Right => SidebarPosition::Right,
            });
        }
        if let Some(filter) = config.as_ref().and_then(|config| config.session_filter) {
            source = source.with_session_filter(filter);
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
        if let Some(config) = config.as_ref() {
            source = source.with_auto_hibernate(config.auto_hibernate_settings());
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
            sidebar_position: SidebarPosition::Left,
            detail_panel_height: Mutex::new(DEFAULT_DETAIL_PANEL_HEIGHT),
            agent_panel_scope: Mutex::new(AgentPanelScope::Current),
            focused_session: Mutex::new(None),
            focused_pane_by_session: Mutex::new(HashMap::new()),
            focused_client_tty: Mutex::new(None),
            theme: Mutex::new(None),
            transparent_background: Mutex::new(false),
            settings_revision: Mutex::new(0),
            session_filter: Mutex::new(None),
            collapsed_worktree_groups: Mutex::new(HashSet::new()),
            session_order: Mutex::new(SessionOrder::new(None)),
            metadata_store: Mutex::new(SessionMetadataStore::new()),
            agent_tracker: Mutex::new(AgentTracker::new()),
            pi_runtime_registry: Mutex::new(PiRuntimeRegistry::with_default_ttl()),
            tmux_socket_path: None,
            auto_hibernate: AutoHibernateSettings::default(),
            process_control: Arc::new(SystemProcessControl),
            agent_state_home: std::env::var_os("HOME").map(PathBuf::from),
            agent_pane_routes: Mutex::new(AgentPaneRouteCache::default()),
            last_state: Mutex::new(None),
            now_ms: Arc::new(current_time_ms),
        }
    }

    pub fn with_sidebar_width(mut self, sidebar_width: u32) -> Self {
        self.sidebar_coordinator = Mutex::new(SidebarCoordinator::new(sidebar_width));
        self
    }

    pub fn with_sidebar_position(mut self, position: SidebarPosition) -> Self {
        self.sidebar_position = position;
        self
    }

    pub fn with_session_filter(mut self, filter: SessionFilterMode) -> Self {
        self.session_filter = Mutex::new(Some(filter));
        self
    }

    pub fn with_session_order_path(mut self, path: PathBuf) -> Self {
        self.session_order = Mutex::new(SessionOrder::new(Some(path)));
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

    fn recorded_sidebar_visibility(&self) -> Option<bool> {
        self.providers
            .iter()
            .filter(|provider| provider.is_full_sidebar_capable())
            .find_map(|provider| provider.sidebar_visibility_preference())
    }

    /// Persist an explicit user show/hide choice in the mux namespace so the
    /// next server generation can restore it. Only user-driven transitions
    /// call this; shutdown (`begin_closing`, hook/pane cleanup) never does.
    fn record_sidebar_visibility(&self, visible: bool) {
        for provider in &self.providers {
            if provider.is_full_sidebar_capable() {
                provider.set_sidebar_visibility_preference(visible);
            }
        }
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

    pub fn with_auto_hibernate(mut self, settings: AutoHibernateSettings) -> Self {
        self.auto_hibernate = settings;
        self
    }

    pub fn with_process_control(mut self, control: Arc<dyn ProcessControl>) -> Self {
        self.process_control = control;
        self
    }

    pub fn with_agent_state_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.agent_state_home = Some(home.into());
        self
    }

    /// Restores agents the previous server instance knew about, so their idle
    /// clocks reflect real last activity instead of restarting with the
    /// server. Runs once at startup over a bounded set of recent files.
    fn seed_agents_from_durable_state(&self) -> bool {
        let Some(home) = self.agent_state_home.as_deref() else {
            return false;
        };
        let observations =
            scan_agent_watcher_observations(home, current_time_ms(), WatcherScanWindow::SEED);
        self.seed_agent_watcher_observations(observations)
    }

    /// Seeds tracker rows from durable agent state. Status comes from the
    /// agent's own files; panes are used only for routing:
    /// - files that record their writer process (Amp logs) are bound to the
    ///   exact pane running that process, or ignored when it is not running in
    ///   an agent pane;
    /// - other files resolve by project dir, taking the newest activity that
    ///   could belong to that session's agent (including activity that matches
    ///   no session), so a seed is never older than the agent it stands for.
    ///
    /// Only quiet statuses are seeded; working agents keep today's behavior.
    fn seed_agent_watcher_observations(&self, observations: Vec<AgentWatcherObservation>) -> bool {
        if observations.is_empty() {
            return false;
        }
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        let pane_by_agent_pid = if observations
            .iter()
            .any(|observation| observation.pid.is_some())
        {
            self.agent_panes_by_pid(&self.process_control.process_table())
        } else {
            HashMap::new()
        };

        let resolve_project = self.watcher_project_resolver(&sessions);
        // (agent, session, pane) -> (newest observation, newest activity)
        let mut groups =
            HashMap::<(&str, String, Option<String>), (AgentWatcherObservation, u64)>::new();
        let mut unattributed_activity = HashMap::<&str, u64>::new();
        for observation in observations {
            let route = match observation.pid {
                Some(pid) => match pane_by_agent_pid.get(&(observation.agent.to_string(), pid)) {
                    Some(route) => Some((route.session.clone(), Some(route.pane_id.clone()))),
                    None => continue,
                },
                None => observation
                    .project_dir
                    .as_deref()
                    .and_then(&resolve_project)
                    .map(|session| (session, None)),
            };
            let Some((session, pane_id)) = route else {
                let latest = unattributed_activity.entry(observation.agent).or_default();
                *latest = (*latest).max(observation.mtime_ms);
                continue;
            };
            let mtime_ms = observation.mtime_ms;
            groups
                .entry((observation.agent, session, pane_id))
                .and_modify(|(newest, activity)| {
                    *activity = (*activity).max(mtime_ms);
                    if mtime_ms > newest.mtime_ms {
                        *newest = observation.clone();
                    }
                })
                .or_insert((observation, mtime_ms));
        }

        // Project-dir seeds could be attached to any of the session's panes,
        // so they also yield to activity routed to those panes.
        let mut session_activity = unattributed_activity
            .iter()
            .map(|(agent, ts)| ((*agent, None), *ts))
            .collect::<HashMap<(&str, Option<String>), u64>>();
        for ((agent, session, _), (_, activity)) in &groups {
            let latest = session_activity
                .entry((*agent, Some(session.clone())))
                .or_default();
            *latest = (*latest).max(*activity);
        }

        let mut groups = groups.into_iter().collect::<Vec<_>>();
        // Pane-bound seeds first: they are exact, and project-dir seeds yield
        // to any row the agent already has in the session.
        groups.sort_by_key(|((_, _, pane_id), _)| pane_id.is_none());
        let mut tracker = self.agent_tracker.lock().unwrap();
        let mut changed = false;
        for ((agent, session, pane_id), (newest, activity)) in groups {
            let Some(snapshot) = newest.snapshot else {
                continue;
            };
            if !matches!(
                snapshot.status,
                AgentStatus::Done
                    | AgentStatus::Error
                    | AgentStatus::Interrupted
                    | AgentStatus::Stale
            ) {
                continue;
            }
            let ts = if pane_id.is_some() {
                activity
            } else {
                [(agent, None), (agent, Some(session.clone()))]
                    .iter()
                    .filter_map(|key| session_activity.get(key))
                    .fold(activity, |ts, other| ts.max(*other))
            };
            debug_log(format!(
                "watcher-seed session={session} pane={pane_id:?} agent={agent} thread={:?} status={:?} ts={ts}",
                snapshot.thread_id, snapshot.status,
            ));
            changed = tracker.apply_seed_event(AgentEvent {
                agent: agent.to_string(),
                session,
                status: snapshot.status,
                ts,
                thread_id: snapshot.thread_id,
                thread_name: snapshot.thread_name,
                last_user_prompt: snapshot.last_user_prompt,
                unseen: None,
                liveness: pane_id.as_ref().map(|_| AgentLiveness::Alive),
                pane_id,
            }) || changed;
        }
        changed
    }

    /// One routine watcher pass over recently modified agent state: returns
    /// the snapshots to apply and whether any seeded row was released.
    fn scan_live_agent_watchers(&self, now_ms: u64) -> (Vec<LiveAgentSnapshot>, bool) {
        let Some(home) = self.agent_state_home.as_deref() else {
            return (Vec::new(), false);
        };
        let observations = scan_agent_watcher_observations(home, now_ms, WatcherScanWindow::LIVE);
        let released = self.release_seeds_for_activity(&observations);
        let routes = self.route_unattributed_observations(&observations);
        let mut snapshots = observations
            .into_iter()
            .filter_map(|observation| {
                let pane = routes
                    .get(&(observation.agent, observation.thread_id.clone()))
                    .cloned();
                observation
                    .snapshot
                    .map(|snapshot| LiveAgentSnapshot { snapshot, pane })
            })
            .collect::<Vec<_>>();
        let mut opencode = Vec::new();
        scan_opencode_sessions(home, now_ms, &mut opencode);
        snapshots.extend(opencode.into_iter().map(|snapshot| LiveAgentSnapshot {
            snapshot,
            pane: None,
        }));
        (snapshots, released)
    }

    /// Routes live snapshots their project dir cannot place (most current
    /// Amp logs record no workdir) to the agent pane running the process that
    /// wrote them, the same way startup seeding does. Only reads the process
    /// table when such snapshots exist and their pids are not already known
    /// for the current tmux layout.
    fn route_unattributed_observations(
        &self,
        observations: &[AgentWatcherObservation],
    ) -> HashMap<(&'static str, String), AgentPaneRoute> {
        let unattributed = observations
            .iter()
            .filter(|observation| {
                observation.pid.is_some()
                    && observation
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.status != AgentStatus::Idle)
            })
            .collect::<Vec<_>>();
        if unattributed.is_empty() {
            return HashMap::new();
        }
        let sessions = if unattributed
            .iter()
            .any(|observation| observation.project_dir.is_some())
        {
            self.providers
                .iter()
                .flat_map(|provider| provider.list_sessions())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let resolve_project = self.watcher_project_resolver(&sessions);
        let unattributed = unattributed
            .into_iter()
            .filter(|observation| {
                observation
                    .project_dir
                    .as_deref()
                    .and_then(&resolve_project)
                    .is_none()
            })
            .filter_map(|observation| {
                let pid_key = (observation.agent.to_string(), observation.pid?);
                Some(((observation.agent, observation.thread_id.clone()), pid_key))
            })
            .collect::<Vec<_>>();
        let wanted = unattributed
            .iter()
            .map(|(_, pid_key)| pid_key.clone())
            .collect::<HashSet<_>>();
        if wanted.is_empty() {
            return HashMap::new();
        }

        let fingerprint = self.tmux_state_fingerprint();
        let mut cache = self.agent_pane_routes.lock().unwrap();
        if fingerprint.is_none() || cache.tmux_fingerprint != fingerprint {
            *cache = AgentPaneRouteCache {
                tmux_fingerprint: fingerprint,
                routes: HashMap::new(),
            };
        }
        if wanted.iter().any(|key| !cache.routes.contains_key(key)) {
            // A pid missing from a fresh table belongs to a process that has
            // exited; remembering it as unroutable is safe for this layout.
            // An empty table means `ps` failed, which proves nothing.
            let process_table = self.process_control.process_table();
            if !process_table.is_empty() {
                let found = self.agent_panes_by_pid(&process_table);
                for key in &wanted {
                    cache.routes.insert(key.clone(), None);
                }
                cache
                    .routes
                    .extend(found.into_iter().map(|(key, route)| (key, Some(route))));
            }
        }
        unattributed
            .into_iter()
            .filter_map(|(thread_key, pid_key)| {
                let route = cache.routes.get(&pid_key)?.clone()?;
                Some((thread_key, route))
            })
            .collect()
    }

    /// Every agent process running in an agent pane, keyed by agent and
    /// pid. Panes only route state; status still comes from the agent.
    fn agent_panes_by_pid(
        &self,
        process_table: &[ProcessEntry],
    ) -> HashMap<(String, u32), AgentPaneRoute> {
        let mut routes = HashMap::new();
        if process_table.is_empty() {
            return routes;
        }
        for provider in &self.providers {
            for session in provider.list_sessions() {
                for pane in provider.list_agent_panes(&session.name) {
                    let Some(target) = provider.get_pane_pid(&pane.pane_id).and_then(|pane_pid| {
                        find_agent_process(pane_pid, &pane.agent, process_table)
                    }) else {
                        continue;
                    };
                    for process in target.processes {
                        routes.insert(
                            (pane.agent.clone(), process.pid),
                            AgentPaneRoute {
                                session: session.name.clone(),
                                pane_id: pane.pane_id.clone(),
                            },
                        );
                    }
                }
            }
        }
        routes
    }

    /// Drops seeded rows whose source files show newer activity, including
    /// activity the live watchers cannot attribute to a session.
    fn release_seeds_for_activity(&self, observations: &[AgentWatcherObservation]) -> bool {
        let mut tracker = self.agent_tracker.lock().unwrap();
        let mut released = false;
        for observation in observations {
            released = tracker.release_seeds_with_newer_activity(
                observation.agent,
                &observation.thread_id,
                observation.mtime_ms,
            ) || released;
        }
        released
    }

    fn watcher_project_resolver(
        &self,
        sessions: &[opensessions_runtime::mux::MuxSessionInfo],
    ) -> impl Fn(&str) -> Option<String> {
        let encoded_sessions = sessions
            .iter()
            .map(|session| (encode_agent_project_dir(&session.dir), session.name.clone()))
            .collect::<Vec<_>>();
        let dir_session_map = build_dir_session_map(
            sessions
                .iter()
                .map(|session| (session.name.clone(), session.dir.clone())),
        );
        move |project_dir: &str| {
            if let Some(encoded) = project_dir.strip_prefix("__encoded__:") {
                return encoded_sessions
                    .iter()
                    .find(|(session_encoded, _)| session_encoded == encoded)
                    .map(|(_, name)| name.clone());
            }
            resolve_session_for_project_dir(project_dir, &dir_session_map)
        }
    }

    /// Stops the agent process in panes whose agent has been idle longer
    /// than the configured threshold, keeping each row as `hibernated`.
    /// Status still comes from the tracker; panes are only used to find the
    /// process to stop. Every session an attached client is viewing, and
    /// every pane a client can see, is skipped.
    fn hibernate_idle_agent_panes(&self) -> bool {
        if !self.auto_hibernate.enabled {
            return false;
        }
        let (protected_sessions, protected_panes) = self.viewed_sessions_and_panes();
        let candidates = self
            .agent_tracker
            .lock()
            .unwrap()
            .find_hibernation_candidates(
                (self.now_ms)(),
                self.auto_hibernate.idle_after_ms,
                &protected_sessions,
            );
        if candidates.is_empty() {
            return false;
        }

        let process_table = self.process_control.process_table();
        let amp_activity_by_pid = if candidates.iter().any(|candidate| candidate.agent == "amp") {
            self.agent_state_home
                .as_deref()
                .map(|home| {
                    recent_amp_activity_by_pid(
                        home,
                        (self.now_ms)(),
                        self.auto_hibernate.idle_after_ms,
                    )
                })
                .unwrap_or_default()
        } else {
            HashMap::new()
        };
        let busy_amp_pids = if candidates.iter().any(|candidate| candidate.agent == "amp") {
            self.busy_unplaced_amp_pids()
        } else {
            HashSet::new()
        };
        let mut targets = Vec::<AgentProcessTarget>::new();
        let mut planned = Vec::new();
        for candidate in candidates {
            let Some(provider) = self.provider_for_session(&candidate.session) else {
                continue;
            };
            if protected_panes.contains(&candidate.pane_id)
                || provider.client_tty_for_pane(&candidate.pane_id).is_some()
            {
                continue;
            }
            let Some(pane_pid) = provider.get_pane_pid(&candidate.pane_id) else {
                debug_log(format!(
                    "auto-hibernate: pane missing session={} pane={} agent={}",
                    candidate.session, candidate.pane_id, candidate.agent,
                ));
                continue;
            };
            let Some(target) = find_agent_process(pane_pid, &candidate.agent, &process_table)
            else {
                debug_log(format!(
                    "auto-hibernate: no {} process under pane={} pid={pane_pid}",
                    candidate.agent, candidate.pane_id,
                ));
                continue;
            };
            // The agent's own durable state can show activity the tracker
            // could not attribute (for example a new thread in the same
            // process); never stop a process that was active recently.
            if candidate.agent == "amp"
                && let Some(activity) = target
                    .processes
                    .iter()
                    .find_map(|process| amp_activity_by_pid.get(&process.pid))
            {
                debug_log(format!(
                    "auto-hibernate: recent durable activity session={} pane={} pid={} activity={activity}",
                    candidate.session, candidate.pane_id, target.pid,
                ));
                continue;
            }
            // Another thread served by the same process may be busy (for
            // example awaiting approval) in a row the tracker could not
            // bind to this pane.
            if candidate.agent == "amp"
                && target
                    .processes
                    .iter()
                    .any(|process| busy_amp_pids.contains(&process.pid))
            {
                debug_log(format!(
                    "auto-hibernate: process serves a busy thread session={} pane={} pid={}",
                    candidate.session, candidate.pane_id, target.pid,
                ));
                continue;
            }
            let index = targets
                .iter()
                .position(|existing| existing.pid == target.pid)
                .unwrap_or_else(|| {
                    targets.push(target);
                    targets.len() - 1
                });
            planned.push((candidate, index));
        }
        if targets.is_empty() {
            return false;
        }

        // An event or a focus change during the SIGTERM grace period can make
        // a candidate active again; such agents are neither SIGKILLed nor
        // marked hibernated.
        let still_hibernatable = std::cell::OnceCell::new();
        let still_wanted = |index: usize| {
            let still = still_hibernatable.get_or_init(|| {
                let viewed = self.viewed_sessions_and_panes();
                let tracker = self.agent_tracker.lock().unwrap();
                self.unviewed_hibernation_candidates(&tracker, &viewed)
            });
            planned
                .iter()
                .filter(|(_, planned_index)| *planned_index == index)
                .all(|(candidate, _)| still.contains(candidate))
        };
        let outcomes = terminate_agent_processes_if(
            self.process_control.as_ref(),
            &targets,
            HIBERNATE_TERM_GRACE,
            &still_wanted,
        );
        let viewed = self.viewed_sessions_and_panes();
        let hibernated_at = (self.now_ms)();
        let mut tracker = self.agent_tracker.lock().unwrap();
        let still = self.unviewed_hibernation_candidates(&tracker, &viewed);
        let mut changed = false;
        for (candidate, index) in planned {
            let outcome = outcomes[index];
            let still_quiet = still.contains(&candidate);
            debug_log(format!(
                "auto-hibernate: session={} pane={} agent={} thread={:?} pid={} terminated={} escalated={} stopped={} still_quiet={still_quiet}",
                candidate.session,
                candidate.pane_id,
                candidate.agent,
                candidate.thread_id,
                outcome.pid,
                outcome.terminated,
                outcome.escalated,
                outcome.stopped,
            ));
            if outcome.stopped && still_quiet {
                changed = tracker.mark_hibernated(&candidate, hibernated_at) || changed;
            }
        }
        changed
    }

    /// Hibernation candidates as of now, minus every session and pane in
    /// `viewed` (from [`Self::viewed_sessions_and_panes`]).
    fn unviewed_hibernation_candidates(
        &self,
        tracker: &AgentTracker,
        (protected_sessions, protected_panes): &(HashSet<String>, HashSet<String>),
    ) -> Vec<HibernationCandidate> {
        tracker
            .find_hibernation_candidates(
                (self.now_ms)(),
                self.auto_hibernate.idle_after_ms,
                protected_sessions,
            )
            .into_iter()
            .filter(|candidate| !protected_panes.contains(&candidate.pane_id))
            .collect()
    }

    /// Sessions and panes the user may be looking at: the focused session,
    /// each provider's current session, and every session and pane shown
    /// to an attached client.
    fn viewed_sessions_and_panes(&self) -> (HashSet<String>, HashSet<String>) {
        let mut sessions = HashSet::new();
        let mut panes = HashSet::new();
        if let Some(session) = self.focused_session.lock().unwrap().clone() {
            sessions.insert(session);
        }
        for provider in &self.providers {
            if let Some(session) = provider.get_current_session() {
                sessions.insert(session);
            }
            for viewed in provider.list_viewed_panes() {
                sessions.insert(viewed.session_name);
                panes.insert(viewed.pane_id);
            }
        }
        (sessions, panes)
    }

    /// Pids of the Amp processes that last wrote busy threads (not quiet
    /// past the idle threshold) whose rows have no known pane. Pane-bound
    /// rows are already grouped by pane in the tracker.
    fn busy_unplaced_amp_pids(&self) -> HashSet<u32> {
        let Some(home) = self.agent_state_home.as_deref() else {
            return HashSet::new();
        };
        let now = (self.now_ms)();
        let idle_after_ms = self.auto_hibernate.idle_after_ms;
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .map(|session| session.name)
            .collect::<HashSet<_>>();
        let busy_threads = {
            let tracker = self.agent_tracker.lock().unwrap();
            sessions
                .iter()
                .flat_map(|session| tracker.get_agents(session))
                .filter(|event| {
                    event.agent == "amp"
                        && event.pane_id.is_none()
                        && event.liveness != Some(AgentLiveness::Exited)
                        && event.status != AgentStatus::Hibernated
                        && (!matches!(
                            event.status,
                            AgentStatus::Idle
                                | AgentStatus::Done
                                | AgentStatus::Error
                                | AgentStatus::Interrupted
                        ) || now.saturating_sub(event.ts) <= idle_after_ms)
                })
                .filter_map(|event| event.thread_id)
                .collect::<HashSet<_>>()
        };
        let logs_dir = home.join(".cache/amp/logs/threads");
        busy_threads
            .into_iter()
            // Thread ids come from external events; never let one escape the
            // log directory.
            .filter(|thread_id| {
                !thread_id.is_empty()
                    && thread_id
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            })
            .filter_map(|thread_id| {
                read_file_tail(
                    &logs_dir.join(format!("{thread_id}.log")),
                    AMP_LOG_PID_TAIL_BYTES,
                )
                .as_deref()
                .and_then(amp_log_pid)
            })
            .collect()
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
        for provider in &self.providers {
            for session in provider.list_sessions() {
                let pane_agents = provider
                    .list_agent_panes(&session.name)
                    .into_iter()
                    .map(|pane| {
                        if provider.client_tty_for_pane(&pane.pane_id).is_some() {
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
        } else if self.recorded_sidebar_visibility() == Some(true) {
            // A previous server generation exited (update, crash, SIGTERM)
            // and its sidebar clients exited with it. Restore the user's last
            // explicit choice exactly like toggle-on: every window, warming.
            debug_log("setup_mux_hooks: restoring recorded visible sidebar");
            let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
            self.sidebar_coordinator
                .lock()
                .unwrap()
                .begin_warmup_until(warmup_until);
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
        // A failed listing (fork pressure, a slow server) says nothing about
        // the namespace; only a successful empty listing means it is gone.
        self.tmux_socket_path.as_ref().is_none_or(|socket_path| {
            tmux_socket_is_live(socket_path)
                && self.providers.iter().any(|provider| {
                    provider
                        .try_list_sessions()
                        .is_none_or(|sessions| !sessions.is_empty())
                })
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
                state_updates.clone(),
                shutdown.clone(),
            )),
        ];
        if self.auto_hibernate.enabled {
            tasks.push(tokio::spawn(run_auto_hibernate_loop(
                self.clone(),
                state_updates.clone(),
                shutdown.clone(),
            )));
        }
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
        self.agent_tracker.lock().unwrap().prune_terminal();

        // Never prune on a failed listing: that would drop every session's
        // metadata for one transient tmux error.
        if let Some(valid_session_names) = self.try_sorted_session_names() {
            self.metadata_store
                .lock()
                .unwrap()
                .prune_sessions(valid_session_names);
        }

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
        // Copy tracker state out under one short lock; tmux commands run
        // later must never hold it (agent events and watchers need it).
        let tracker = self.agent_tracker.lock().unwrap();
        let unseen_sessions = Some(tracker.get_unseen());
        let (agent_state_by_session, agents_by_session, event_timestamps_by_session) =
            visible_session_names
                .as_ref()
                .map(|names| {
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
        drop(tracker);
        let ports_by_session = self.discover_live_ports(visible_session_names.as_deref(), false);
        // Capture settings with their revision under the revision lock, then
        // release every lock before `build_read_only_state` runs tmux
        // commands; a slow or panicking tmux call must not stall or poison
        // the server's shared state.
        let settings_revision = self.settings_revision.lock().unwrap();
        let revision = *settings_revision;
        let sidebar_state = self.sidebar_coordinator.lock().unwrap().state();
        let theme = self.theme.lock().unwrap().clone();
        let transparent_background = *self.transparent_background.lock().unwrap();
        let session_filter = *self.session_filter.lock().unwrap();
        let agent_panel_scope = *self.agent_panel_scope.lock().unwrap();
        let collapsed_worktree_groups = self
            .collapsed_worktree_groups
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        let detail_panel_height = u32::from(*self.detail_panel_height.lock().unwrap());
        drop(settings_revision);
        let focused_session = self.focused_session.lock().unwrap().clone();
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
            unseen_sessions,
            ports_by_session,
            portless_state: None,
            focused_session,
            current_session_override: None,
            visible_sidebar_pane_ids,
            theme,
            transparent_background,
            session_filter,
            agent_panel_scope,
            collapsed_worktree_groups,
            sidebar_width: sidebar_state.width,
            detail_panel_height,
            settings_revision: revision,
            initializing: sidebar_state.initializing,
            init_label: (!sidebar_state.init_label.is_empty()).then_some(sidebar_state.init_label),
            now_ms: (self.now_ms)(),
        });

        let payload = serde_json::to_string(&ServerMessage::State(state.clone()))
            .expect("state must serialize");
        *self.last_state.lock().unwrap() = Some(state);
        payload
    }

    /// Announces `closing…` from the last built state. Shutdown runs on the
    /// runtime thread and must not wait for tmux, Git, or port discovery.
    fn begin_shutdown(&self) -> Option<String> {
        let sidebar_state = {
            let mut coordinator = self.sidebar_coordinator.lock().unwrap();
            coordinator.begin_closing();
            coordinator.state()
        };
        let mut state = self.last_state.lock().unwrap().clone()?;
        state.initializing = sidebar_state.initializing;
        state.init_label =
            (!sidebar_state.init_label.is_empty()).then_some(sidebar_state.init_label);
        state.sidebar_width = sidebar_state.width;
        serde_json::to_string(&ServerMessage::State(state)).ok()
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
            "rename-session" => {
                let name = command.get("name")?.as_str()?;
                let new_name = command.get("newName")?.as_str()?.trim();
                if new_name.is_empty() || new_name == name {
                    return None;
                }
                // The mux may sanitize the requested name; every reference
                // must follow the name the session actually has now.
                let Some(new_name) = provider
                    .rename_session(name, new_name)
                    .filter(|actual| actual != name)
                else {
                    return Some(self.snapshot_json());
                };
                self.rename_session_references(name, &new_name);
                serde_json::to_string(&ServerMessage::ReIdentify {
                    old_name: name.to_string(),
                    new_name,
                })
                .ok()
            }
            "switch-session" => {
                let name = command.get("name")?.as_str()?;
                let client_tty = live_client_tty(provider.as_ref(), context);
                provider.switch_session(name, client_tty.as_deref());
                None
            }
            "switch-index" => {
                let index = command.get("index")?.as_u64()?.min(u32::MAX as u64) as u32;
                let client_tty = live_client_tty(provider.as_ref(), context);
                self.switch_visible_index(index, client_tty.as_deref())
            }
            "kill-session" => {
                let name = command.get("name")?.as_str()?;
                let client_tty = live_client_tty(provider.as_ref(), context);
                if let Some(next) = self
                    .session_before(name)
                    .or_else(|| self.session_after(name))
                    && provider.switch_clients_from_session(name, &next, client_tty.as_deref())
                {
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
                let client_tty = live_client_tty(provider.as_ref(), context);
                provider.switch_window(session, window_id, client_tty.as_deref());
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
                let mut settings_revision = self.settings_revision.lock().unwrap();
                *self.theme.lock().unwrap() = Some(theme.clone());
                *self.transparent_background.lock().unwrap() = transparent_background;
                self.persist_theme(&theme, transparent_background);
                *settings_revision += 1;
                drop(settings_revision);
                Some(self.snapshot_json())
            }
            "set-sidebar-width" => {
                let width = command.get("width")?.as_u64()?.min(u16::MAX as u64) as u16;
                let mut settings_revision = self.settings_revision.lock().unwrap();
                self.set_sidebar_width(width);
                *settings_revision += 1;
                drop(settings_revision);
                Some(self.snapshot_json())
            }
            "set-detail-panel-height" => {
                let height = command.get("height")?.as_u64()?.min(u16::MAX as u64) as u16;
                let height = clamp_detail_panel_height(height);
                let mut settings_revision = self.settings_revision.lock().unwrap();
                *self.detail_panel_height.lock().unwrap() = height;
                self.persist_detail_panel_height(height);
                *settings_revision += 1;
                drop(settings_revision);
                Some(self.snapshot_json())
            }
            "set-agent-panel-scope" => {
                let scope = parse_agent_panel_scope(command.get("scope")?.as_str()?)?;
                let mut settings_revision = self.settings_revision.lock().unwrap();
                *self.agent_panel_scope.lock().unwrap() = scope;
                *settings_revision += 1;
                drop(settings_revision);
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
                let mut settings_revision = self.settings_revision.lock().unwrap();
                *self.session_filter.lock().unwrap() = Some(filter);
                *settings_revision += 1;
                drop(settings_revision);
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
        context.client_tty = context
            .pane_id
            .as_deref()
            .and_then(|pane_id| self.providers.first()?.client_tty_for_pane(pane_id));
        debug_log(format!(
            "identify-pane session={:?} pane={:?} window={:?} client_tty={:?} -> acknowledge_sidebar_connected",
            context.session_name, context.pane_id, context.window_id, context.client_tty,
        ));
        let became_visible = {
            let mut coordinator = self.sidebar_coordinator.lock().unwrap();
            let was_visible = coordinator.state().visible;
            coordinator.acknowledge_sidebar_connected();
            !was_visible && coordinator.state().visible
        };
        if became_visible {
            self.record_sidebar_visibility(true);
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
        Some(format!(
            r#"{{"type":"your-session","name":{},"clientTty":{}}}"#,
            json_string_or_null(Some(session_name)),
            json_string_or_null(context.client_tty.as_deref()),
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

    fn handle_agent_event_json(&self, body: &Value) -> Result<(), AgentEventError> {
        self.apply_agent_event(body)
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
                // Queue fixed-width repair before orphan cleanup. The repair
                // worker runs outside the state-operation lock, so tmux's
                // redistributed sidebar width is restored without waiting for
                // the snapshot-backed fallback computation below.
                if self.is_sidebar_visible() {
                    self.request_sidebar_width_repair();
                }
                // One snapshot serves every sidebar; computing it per pane
                // held the state-operation lock for seconds under load.
                let display_names = self.sidebar_display_session_names().unwrap_or_default();
                let fallback_sessions = self
                    .providers
                    .iter()
                    .flat_map(|provider| provider.list_sidebar_panes(None))
                    .filter_map(|pane| {
                        let fallback = session_before_in(&display_names, &pane.session_name)
                            .or_else(|| session_after_in(&display_names, &pane.session_name))?;
                        Some((pane.session_name, fallback))
                    })
                    .collect::<HashMap<_, _>>();
                for provider in &self.providers {
                    provider.kill_orphaned_sidebar_panes_with_fallbacks(&fallback_sessions);
                }
                // A sidebar pane that exited or was killed leaves its window
                // with forced `remain-on-exit`; serialize with spawning so a
                // sidebar being created is never mistaken for a missing one.
                let _presence_guard = self.sidebar_presence.lock().unwrap();
                for provider in &self.providers {
                    provider.restore_windows_without_sidebar();
                }
                None
            }
            "/pane-layout-changed" | "/client-resized" => {
                if self.is_sidebar_visible() {
                    self.request_sidebar_width_repair();
                }
                None
            }
            "/repair-sidebar-width" => {
                if self.is_sidebar_visible()
                    && let Some(context) = parse_context(body)
                    && let Some(pane_id) = context.pane_id
                    && !self
                        .providers
                        .iter()
                        .any(|provider| provider.is_sidebar_mouse_resize_active(&context.window_id))
                {
                    let width = self.current_sidebar_width_u16();
                    for provider in &self.providers {
                        provider.resize_sidebar_pane(&pane_id, width);
                    }
                }
                None
            }
            "/set-sidebar-width" => {
                let width = body.trim().parse::<u16>().ok()?;
                let mut settings_revision = self.settings_revision.lock().unwrap();
                self.set_sidebar_width(width);
                *settings_revision += 1;
                drop(settings_revision);
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
    fn rename_session_references(&self, name: &str, new_name: &str) {
        self.session_order.lock().unwrap().rename(name, new_name);
        self.metadata_store
            .lock()
            .unwrap()
            .rename_session(name, new_name);
        self.agent_tracker
            .lock()
            .unwrap()
            .rename_session(name, new_name);

        let mut focused_session = self.focused_session.lock().unwrap();
        if focused_session.as_deref() == Some(name) {
            *focused_session = Some(new_name.to_string());
        }
        drop(focused_session);

        let mut focused_panes = self.focused_pane_by_session.lock().unwrap();
        if let Some(pane_id) = focused_panes.remove(name) {
            focused_panes.insert(new_name.to_string(), pane_id);
        }
    }

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
                .providers
                .iter()
                .any(|provider| provider.client_tty_for_pane(&pane_id).is_some())
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

    fn apply_agent_watcher_snapshot(&self, live: LiveAgentSnapshot) -> bool {
        let LiveAgentSnapshot { snapshot, pane } = live;
        if snapshot.status == AgentStatus::Idle {
            debug_log(format!(
                "watcher-snapshot ignored idle agent={} thread_id={:?} thread_name={:?} project_dir={:?}",
                snapshot.agent, snapshot.thread_id, snapshot.thread_name, snapshot.project_dir,
            ));
            return false;
        }
        let session = match &pane {
            Some(route) => Some(route.session.clone()),
            None => self.resolve_agent_watcher_session(&snapshot),
        };
        let Some(session) = session else {
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
        let existing = self
            .agent_tracker
            .lock()
            .unwrap()
            .get_agents(&session)
            .into_iter()
            .find(|event| {
                event.agent == snapshot.agent
                    && event.thread_id.as_deref() == snapshot.thread_id.as_deref()
            });
        // Amp snapshots are stamped with the log's mtime; a row with newer
        // activity came from a live event (such as the Amp plugin) that the
        // log has not caught up with, so the older snapshot must not win.
        // That row may live in another session (a plugin event resolved by
        // project dir), where the snapshot would otherwise add a duplicate.
        if snapshot.agent == "amp"
            && let Some(thread_id) = snapshot.thread_id.as_deref()
            && let Some(row_ts) = self
                .agent_tracker
                .lock()
                .unwrap()
                .newest_thread_activity(snapshot.agent, thread_id)
            && row_ts > snapshot.ts
        {
            debug_log(format!(
                "watcher-snapshot older than tracked row session={} agent={} thread_id={:?} snapshot_ts={} row_ts={row_ts}",
                session, snapshot.agent, snapshot.thread_id, snapshot.ts,
            ));
            return false;
        }
        let pane_id = pane.map(|route| route.pane_id);
        let focused_pane = pane_id
            .clone()
            .or_else(|| existing.and_then(|event| event.pane_id))
            .filter(|pane_id| {
                self.providers
                    .iter()
                    .any(|provider| provider.client_tty_for_pane(pane_id).is_some())
            });
        debug_log(format!(
            "watcher-snapshot applying session={} pane={:?} focused_pane={:?} agent={} status={:?} thread_id={:?} thread_name={:?} project_dir={:?}",
            session,
            pane_id,
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
            liveness: pane_id.as_ref().map(|_| AgentLiveness::Alive),
            pane_id,
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
        let project_dir = snapshot.project_dir.as_deref()?;
        let sessions = self
            .providers
            .iter()
            .flat_map(|provider| provider.list_sessions())
            .collect::<Vec<_>>();
        self.watcher_project_resolver(&sessions)(project_dir)
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
                if pane.width == Some(width)
                    || provider.is_sidebar_mouse_resize_active(&pane.window_id)
                {
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
        // A toggle queued behind the state-operation lock can run after
        // shutdown began; it must neither spawn sidebars nor record a choice.
        if self.sidebar_coordinator.lock().unwrap().state().lifecycle == SidebarLifecycle::Closing {
            debug_log("toggle_sidebar: ignored while the server is closing");
            return;
        }
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
                // Hidden windows must exit panes normally again.
                provider.restore_windows_without_sidebar();
            }
            self.sidebar_coordinator.lock().unwrap().hide();
            self.record_sidebar_visibility(false);
            return;
        }

        let warmup_until = (self.now_ms)().saturating_add(SIDEBAR_WARMUP_MS);
        self.sidebar_coordinator
            .lock()
            .unwrap()
            .begin_warmup_until(warmup_until);
        self.record_sidebar_visibility(true);
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
                    self.sidebar_position,
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
                self.sidebar_position,
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
                        self.sidebar_position,
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
        session_before_in(&self.sidebar_display_session_names()?, name)
    }

    fn session_after(&self, name: &str) -> Option<String> {
        session_after_in(&self.sidebar_display_session_names()?, name)
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
        let names = self.try_sorted_session_names();
        let current_session = self
            .providers
            .iter()
            .find_map(|provider| provider.get_current_session());
        let mut session_order = self.session_order.lock().unwrap();
        // A failed listing must not prune the order or hidden list; fall
        // back to the last known sessions until tmux answers again.
        let names = match names {
            Some(names) => {
                session_order.sync(names.clone());
                names
            }
            None => session_order.known_names(),
        };
        if let Some(current_session) = current_session {
            session_order.show(&current_session);
        }
        Some(session_order.apply(names))
    }

    fn sorted_session_names(&self) -> Vec<String> {
        self.try_sorted_session_names().unwrap_or_default()
    }

    /// Session names in creation order, or `None` if any provider failed to
    /// list its sessions.
    fn try_sorted_session_names(&self) -> Option<Vec<String>> {
        let mut sessions = Vec::new();
        for provider in &self.providers {
            sessions.extend(provider.try_list_sessions()?);
        }
        // `created_at` has whole-second resolution, so ties keep the
        // provider's creation order. A name tiebreak would make the order of
        // sessions created together depend on whether a second ticked.
        sessions.sort_by_key(|session| session.created_at);
        Some(sessions.into_iter().map(|session| session.name).collect())
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

/// Every few minutes, stop agent processes that have idled past the
/// configured threshold so idle agents do not hold memory for weeks.
async fn run_auto_hibernate_loop(
    source: Arc<ReadOnlyMuxStateSource>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown.subscribe();
    let period = Duration::from_millis(HIBERNATE_POLL_INTERVAL_MS);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = interval.tick() => {
                let hibernate_source = source.clone();
                let changed = tokio::task::spawn_blocking(move || {
                    hibernate_source.hibernate_idle_agent_panes()
                })
                .await
                .unwrap_or(false);
                if changed {
                    debug_log("auto_hibernate_loop: hibernated idle agents, broadcasting");
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

fn session_before_in(names: &[String], name: &str) -> Option<String> {
    let index = names.iter().position(|candidate| candidate == name)?;
    index
        .checked_sub(1)
        .and_then(|previous| names.get(previous).cloned())
}

fn session_after_in(names: &[String], name: &str) -> Option<String> {
    let index = names.iter().position(|candidate| candidate == name)?;
    names.get(index + 1).cloned()
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
                let fingerprint_source = source.clone();
                let fingerprint = tokio::task::spawn_blocking(move || {
                    fingerprint_source.tmux_state_fingerprint()
                }).await.unwrap_or(None);
                let Some(fingerprint) = fingerprint else {
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
    let poll = Arc::new(Mutex::new(AgentWatcherPoll::default()));
    let mut unchanged_polls = 0;

    let seed_source = source.clone();
    let seeded = tokio::task::spawn_blocking(move || seed_source.seed_agents_from_durable_state())
        .await
        .unwrap_or(false);
    if seeded {
        debug_log("agent_watcher_loop: restored idle agents from durable state, broadcasting");
        let snapshot_source = source.clone();
        if let Ok(snapshot) =
            tokio::task::spawn_blocking(move || snapshot_source.snapshot_json()).await
        {
            let _ = state_updates.send(snapshot);
        }
    }

    loop {
        let delay = adaptive_poll_delay_ms(
            unchanged_polls,
            AGENT_WATCHER_POLL_MS,
            AGENT_WATCHER_IDLE_MAX_MS,
        );
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {
                // The tracker lock can be held by blocking-pool work; waiting
                // for it here would stall every task on the runtime thread.
                let prune_source = source.clone();
                let pruned = tokio::task::spawn_blocking(move || {
                    prune_source
                        .agent_tracker
                        .lock()
                        .unwrap()
                        .prune_stuck(STUCK_RUNNING_TIMEOUT_MS)
                })
                .await
                .unwrap_or(false);
                if pruned {
                    debug_log("agent_watcher_loop: stale agent state changed, broadcasting");
                    let snapshot_source = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                        snapshot_source.snapshot_json()
                    }).await {
                        let _ = state_updates.send(snapshot);
                    }
                }
                let now = current_time_ms();
                let poll_source = source.clone();
                let poll_state = poll.clone();
                let AgentWatcherPollOutcome { changed, has_active_agents } =
                    tokio::task::spawn_blocking(move || {
                        poll_state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .run(&poll_source, now)
                    })
                    .await
                    .unwrap_or_default();
                if changed {
                    let snapshot_source = source.clone();
                    if let Ok(snapshot) = tokio::task::spawn_blocking(move || {
                        snapshot_source.snapshot_json()
                    }).await {
                        let _ = state_updates.send(snapshot);
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

/// Watcher state kept across polls: the last applied fingerprint per
/// thread, so unchanged agent state is not reapplied.
#[derive(Debug, Default)]
struct AgentWatcherPoll {
    last_seen: HashMap<String, AgentWatcherFingerprint>,
}

#[derive(Debug, Default, Clone, Copy)]
struct AgentWatcherPollOutcome {
    changed: bool,
    has_active_agents: bool,
}

impl AgentWatcherPoll {
    /// Scans recently modified agent state once and applies what changed.
    fn run(&mut self, source: &ReadOnlyMuxStateSource, now_ms: u64) -> AgentWatcherPollOutcome {
        let (snapshots, released) = source.scan_live_agent_watchers(now_ms);
        let has_active_agents = snapshots
            .iter()
            .any(|snapshot| agent_status_needs_fast_polling(snapshot.status));
        let mut changed = released;
        for snapshot in snapshots {
            if snapshot.status == AgentStatus::Idle {
                continue;
            }
            let key = agent_watcher_key(&snapshot);
            let fingerprint = AgentWatcherFingerprint::from(&snapshot);
            if self.last_seen.get(&key) == Some(&fingerprint) {
                continue;
            }
            let agent = snapshot.agent;
            let status = snapshot.status;
            let thread_name = snapshot.thread_name.clone();
            if source.apply_agent_watcher_snapshot(snapshot) {
                debug_log(format!(
                    "agent_watcher_loop: applied snapshot agent={agent} status={status:?} thread={thread_name:?}",
                ));
                self.last_seen.insert(key, fingerprint);
                changed = true;
            } else {
                debug_log(format!(
                    "agent_watcher_loop: dropped snapshot agent={agent} status={status:?} (no matching session or newer row)",
                ));
            }
        }
        AgentWatcherPollOutcome {
            changed,
            has_active_agents,
        }
    }
}

/// A live watcher snapshot, plus the agent pane it was routed to through its
/// writer process when its project dir could not place it.
#[derive(Debug, Clone)]
struct LiveAgentSnapshot {
    snapshot: AgentWatcherSnapshot,
    pane: Option<AgentPaneRoute>,
}

impl std::ops::Deref for LiveAgentSnapshot {
    type Target = AgentWatcherSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPaneRoute {
    session: String,
    pane_id: String,
}

/// Writer pid routes for one tmux layout. Unroutable pids are remembered
/// too, so steady activity from processes outside agent panes does not
/// reread the process table every poll. Cleared when the layout changes;
/// never cached without a layout fingerprint.
#[derive(Debug, Default)]
struct AgentPaneRouteCache {
    tmux_fingerprint: Option<u64>,
    routes: HashMap<(String, u32), Option<AgentPaneRoute>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentWatcherFingerprint {
    status: AgentStatus,
    thread_name: Option<String>,
    last_user_prompt: Option<String>,
    project_dir: Option<String>,
    pane: Option<AgentPaneRoute>,
    ts: u64,
}

impl From<&LiveAgentSnapshot> for AgentWatcherFingerprint {
    fn from(live: &LiveAgentSnapshot) -> Self {
        Self {
            status: live.status,
            thread_name: live.thread_name.clone(),
            last_user_prompt: live.last_user_prompt.clone(),
            project_dir: live.project_dir.clone(),
            pane: live.pane.clone(),
            ts: live.ts,
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

/// Which agent state files a scan reads: files modified within `max_age_ms`,
/// newest first, at most `max_files` per source.
#[derive(Debug, Clone, Copy)]
struct WatcherScanWindow {
    max_age_ms: u64,
    max_files: usize,
}

impl WatcherScanWindow {
    /// Routine polling only rereads recently modified files.
    const LIVE: Self = Self {
        max_age_ms: AGENT_WATCHER_RECENT_MS,
        max_files: usize::MAX,
    };
    /// One bounded startup pass that restores idle agents' last activity.
    const SEED: Self = Self {
        max_age_ms: AGENT_WATCHER_SEED_MAX_AGE_MS,
        max_files: AGENT_WATCHER_SEED_MAX_FILES,
    };

    fn select(self, paths: impl IntoIterator<Item = PathBuf>, now_ms: u64) -> Vec<(PathBuf, u64)> {
        let mut files = paths
            .into_iter()
            .filter_map(|path| {
                let mtime_ms = file_mtime_ms(&path)?;
                (now_ms.saturating_sub(mtime_ms) <= self.max_age_ms).then_some((path, mtime_ms))
            })
            .collect::<Vec<_>>();
        files.sort_by_key(|(_, mtime_ms)| std::cmp::Reverse(*mtime_ms));
        files.truncate(self.max_files);
        files
    }
}

/// One agent state file: when it last changed, where it can be routed, and
/// the parsed snapshot when the file was readable.
#[derive(Debug, Clone)]
struct AgentWatcherObservation {
    agent: &'static str,
    thread_id: String,
    mtime_ms: u64,
    project_dir: Option<String>,
    /// The agent process that last wrote the file, when the format records it.
    pid: Option<u32>,
    snapshot: Option<AgentWatcherSnapshot>,
}

impl AgentWatcherObservation {
    fn new(
        agent: &'static str,
        thread_id: &str,
        mtime_ms: u64,
        snapshot: Option<AgentWatcherSnapshot>,
    ) -> Self {
        Self {
            agent,
            thread_id: snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.thread_id.clone())
                .unwrap_or_else(|| thread_id.to_string()),
            mtime_ms,
            project_dir: snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.project_dir.clone()),
            pid: None,
            snapshot,
        }
    }
}

fn scan_agent_watcher_observations(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
) -> Vec<AgentWatcherObservation> {
    let mut observations = Vec::new();
    scan_amp_threads(home, now_ms, window, &mut observations);
    scan_amp_logs(home, now_ms, window, &mut observations);
    scan_claude_code_projects(home, now_ms, window, &mut observations);
    scan_codex_sessions(home, now_ms, window, &mut observations);
    scan_pi_sessions(home, now_ms, window, &mut observations);
    scan_droid_sessions(home, now_ms, window, &mut observations);
    observations
}

fn files_with_extension(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some(extension))
        .collect()
}

fn file_stem(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|stem| stem.to_str())
}

fn scan_amp_threads(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let threads_dir = home.join(".local/share/amp/threads");
    for (path, mtime_ms) in window.select(files_with_extension(&threads_dir, "json"), now_ms) {
        let Some(thread_id) = file_stem(&path) else {
            continue;
        };
        let snapshot = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| amp_snapshot_from_thread_json(&raw, mtime_ms));
        observations.push(AgentWatcherObservation::new(
            "amp", thread_id, mtime_ms, snapshot,
        ));
    }
}

fn scan_amp_logs(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let logs_dir = home.join(".cache/amp/logs/threads");
    for (path, mtime_ms) in window.select(files_with_extension(&logs_dir, "log"), now_ms) {
        let Some(thread_id) = file_stem(&path) else {
            continue;
        };
        let raw = read_file_tail(&path, AMP_LOG_TAIL_BYTES);
        let mut snapshot = raw
            .as_deref()
            .and_then(|raw| amp_snapshot_from_log_jsonl(thread_id, raw, mtime_ms));
        if let Some(snapshot) = snapshot.as_mut().filter(|snapshot| {
            snapshot.thread_name.is_none()
                && file_len(&path).is_some_and(|len| len > AMP_LOG_TAIL_BYTES)
        }) {
            snapshot.thread_name = read_file_head(&path, AMP_LOG_TITLE_HEAD_BYTES)
                .and_then(|head| amp_log_thread_title(&head));
        }
        let mut observation = AgentWatcherObservation::new("amp", thread_id, mtime_ms, snapshot);
        observation.pid = raw.as_deref().and_then(amp_log_pid);
        // Legacy thread files and current logs can describe the same thread;
        // the newer readable snapshot wins, keeping the legacy title if the
        // log has none.
        if let Some(existing) = observations
            .iter_mut()
            .find(|existing| existing.agent == "amp" && existing.thread_id == observation.thread_id)
        {
            if observation.snapshot.is_some() && mtime_ms > existing.mtime_ms {
                let legacy_name = existing
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.thread_name.clone());
                if let Some(snapshot) = observation
                    .snapshot
                    .as_mut()
                    .filter(|snapshot| snapshot.thread_name.is_none())
                {
                    snapshot.thread_name = legacy_name;
                }
                *existing = observation;
            } else {
                existing.mtime_ms = existing.mtime_ms.max(mtime_ms);
                existing.pid = observation.pid.or(existing.pid);
            }
        } else {
            observations.push(observation);
        }
    }
}

/// Latest log activity per Amp process for logs modified within
/// `max_age_ms`. Reads only a small tail per file unless its last line is
/// longer than that.
fn recent_amp_activity_by_pid(home: &Path, now_ms: u64, max_age_ms: u64) -> HashMap<u32, u64> {
    let window = WatcherScanWindow {
        max_age_ms,
        max_files: usize::MAX,
    };
    let logs_dir = home.join(".cache/amp/logs/threads");
    let mut activity = HashMap::<u32, u64>::new();
    for (path, mtime_ms) in window.select(files_with_extension(&logs_dir, "log"), now_ms) {
        let pid = read_file_tail(&path, AMP_LOG_PID_TAIL_BYTES)
            .as_deref()
            .and_then(amp_log_pid)
            .or_else(|| {
                read_file_tail(&path, AMP_LOG_TAIL_BYTES)
                    .as_deref()
                    .and_then(amp_log_pid)
            });
        if let Some(pid) = pid {
            let latest = activity.entry(pid).or_default();
            *latest = (*latest).max(mtime_ms);
        }
    }
    activity
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

/// The first `max_bytes` of a file, without a trailing partial line.
fn read_file_head(path: &Path, max_bytes: u64) -> Option<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .ok()?;
    if let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') {
        bytes.truncate(end + 1);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
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

fn scan_claude_code_projects(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let projects_dir = home.join(".claude/projects");
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return;
    };
    let files = projects
        .flatten()
        .map(|project| project.path())
        .filter(|project_path| project_path.is_dir())
        .flat_map(|project_path| files_with_extension(&project_path, "jsonl"));

    for (path, mtime_ms) in window.select(files, now_ms) {
        let (Some(thread_id), Some(encoded)) = (
            file_stem(&path),
            path.parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
        ) else {
            continue;
        };
        let project_dir = decode_claude_project_dir(encoded, |path| Path::new(path).is_dir());
        let snapshot = fs::read_to_string(&path).ok().and_then(|raw| {
            claude_code_snapshot_from_jsonl(thread_id, &project_dir, &raw, mtime_ms, now_ms)
        });
        let mut observation =
            AgentWatcherObservation::new("claude-code", thread_id, mtime_ms, snapshot);
        observation.project_dir = Some(project_dir);
        observations.push(observation);
    }
}

fn scan_codex_sessions(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let sessions_dir = codex_home.join("sessions");
    let files = window.select(collect_jsonl_files(&sessions_dir), now_ms);
    if files.is_empty() {
        return;
    }
    let names = fs::read_to_string(codex_home.join("session_index.jsonl"))
        .ok()
        .map(|raw| {
            parse_codex_session_index(&raw)
                .into_iter()
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    for (path, mtime_ms) in files {
        let Some(path_text) = path.to_str() else {
            continue;
        };
        let thread_id = codex_thread_id_from_path(path_text);
        let snapshot = fs::read_to_string(&path).ok().and_then(|raw| {
            codex_snapshot_from_jsonl(
                &thread_id,
                &raw,
                names.get(&thread_id).map(String::as_str),
                mtime_ms,
                now_ms,
            )
        });
        observations.push(AgentWatcherObservation::new(
            "codex", &thread_id, mtime_ms, snapshot,
        ));
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

fn scan_pi_sessions(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let sessions_dir = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("PI_CODING_AGENT_DIR")
                .map(PathBuf::from)
                .map(|dir| dir.join("sessions"))
        })
        .unwrap_or_else(|| home.join(".pi/agent/sessions"));

    for (path, mtime_ms) in window.select(collect_jsonl_files(&sessions_dir), now_ms) {
        let Some(thread_id) = file_stem(&path) else {
            continue;
        };
        let snapshot = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| pi_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms));
        observations.push(AgentWatcherObservation::new(
            "pi", thread_id, mtime_ms, snapshot,
        ));
    }
}

fn scan_droid_sessions(
    home: &Path,
    now_ms: u64,
    window: WatcherScanWindow,
    observations: &mut Vec<AgentWatcherObservation>,
) {
    let projects_dir = std::env::var_os("FACTORY_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".factory/projects"));

    for (path, mtime_ms) in window.select(collect_jsonl_files(&projects_dir), now_ms) {
        let Some(thread_id) = file_stem(&path) else {
            continue;
        };
        let snapshot = fs::read_to_string(&path)
            .ok()
            .and_then(|raw| droid_snapshot_from_jsonl(thread_id, &raw, mtime_ms, now_ms));
        observations.push(AgentWatcherObservation::new(
            "droid", thread_id, mtime_ms, snapshot,
        ));
    }
}

fn run_process_with_timeout(
    mut command: process::Command,
    timeout: Duration,
) -> Option<process::Output> {
    output_with_timeout(&mut command, timeout).ok()
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
        "hibernated" => Some(AgentStatus::Hibernated),
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
    if let Some(context) = parse_pipe_context(trimmed) {
        return Some(context);
    }
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

/// Parses `client_tty|session|window_id[|pane_id[|pane_active]]`.
///
/// tmux allows `|` in session names, so the separator count cannot pick the
/// format. Every other field is structured: the tty is a path before the
/// first `|`, and the trailing fields are tmux ids (`@N`, `%N`) and a 0/1
/// flag, so they are taken from the end and the session is what remains.
fn parse_pipe_context(trimmed: &str) -> Option<HttpContext> {
    let (client_tty, rest) = trimmed.split_once('|')?;
    let client_tty = (!client_tty.is_empty()).then(|| client_tty.to_string());
    let is_window = |value: &str| value.len() > 1 && value.starts_with('@');
    let is_pane = |value: &str| value.len() > 1 && value.starts_with('%');
    let context = |session: &str, window_id: &str, pane_id: Option<&str>, active: Option<&str>| {
        (!session.is_empty()).then(|| HttpContext {
            client_tty: client_tty.clone(),
            session: session.to_string(),
            window_id: window_id.to_string(),
            pane_id: pane_id.map(str::to_string),
            pane_active: active.map(|active| active == "1"),
        })
    };
    if let [active, pane_id, window_id, session] = rest.rsplitn(4, '|').collect::<Vec<_>>()[..]
        && matches!(active, "0" | "1")
        && is_pane(pane_id)
        && is_window(window_id)
    {
        return context(session, window_id, Some(pane_id), Some(active));
    }
    if let [pane_id, window_id, session] = rest.rsplitn(3, '|').collect::<Vec<_>>()[..]
        && is_pane(pane_id)
        && is_window(window_id)
    {
        return context(session, window_id, Some(pane_id), None);
    }
    if let [window_id, session] = rest.rsplitn(2, '|').collect::<Vec<_>>()[..]
        && is_window(window_id)
    {
        return context(session, window_id, None, None);
    }
    None
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
    pub server_identity: Option<String>,
    pub max_connections: Option<usize>,
    state_source: Option<Arc<dyn StateSource>>,
}

impl ServerConfig {
    pub fn new(host: impl Into<String>, port: u16, pid_file: impl Into<PathBuf>) -> Self {
        Self {
            host: host.into(),
            port,
            pid_file: pid_file.into(),
            token_file: PathBuf::new(),
            server_identity: None,
            max_connections: None,
            state_source: None,
        }
    }

    /// Lowers the connection cap below the one derived from the descriptor
    /// limit; it can never exceed that bound.
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = Some(max_connections);
        self
    }

    pub fn with_token_file(mut self, token_file: impl Into<PathBuf>) -> Self {
        self.token_file = token_file.into();
        self
    }

    pub fn with_server_identity(mut self, identity: impl Into<String>) -> Self {
        self.server_identity = Some(identity.into());
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

    pub fn shutdown_sender(&self) -> broadcast::Sender<()> {
        self.shutdown.clone()
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

fn lock_identity(pid_file: &Path) -> std::io::Result<fs::File> {
    let lock_file = pid_file.with_extension("identity.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_file)?;
    file.lock()?;
    Ok(file)
}

fn publish_identity_locked(pid_file: &Path, token_file: &Path, token: &str) -> std::io::Result<()> {
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

fn publish_identity(pid_file: &Path, token_file: &Path, token: &str) -> std::io::Result<()> {
    let _identity_lock = lock_identity(pid_file)?;
    publish_identity_locked(pid_file, token_file, token)
}

/// Removes this generation's mux hooks, sidebar clients, and identity files.
///
/// The identity lock is held from the ownership check through the last
/// cleanup command: a successor generation publishes its identity under the
/// same lock before it installs hooks or spawns sidebars, so it can neither
/// slip in between the check and cleanup nor have its own hooks unset.
fn cleanup_owned_generation(
    pid_file: &Path,
    token_file: &Path,
    token: &str,
    state_source: Option<&dyn StateSource>,
) -> std::io::Result<()> {
    let _identity_lock = lock_identity(pid_file)?;
    if !owns_identity_generation(pid_file, token_file, token) {
        debug_log("shutdown: a newer generation owns the identity; skipping mux cleanup");
        return Ok(());
    }
    if let Some(source) = state_source
        && source.mux_namespace_available()
    {
        source.cleanup_mux_hooks();
        source.cleanup_sidebar_clients();
    }
    remove_identity_files(pid_file, token_file)
}

fn remove_identity_files(pid_file: &Path, token_file: &Path) -> std::io::Result<()> {
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

fn repair_identity_if_missing(
    pid_file: &Path,
    token_file: &Path,
    token: &str,
) -> std::io::Result<bool> {
    let _identity_lock = lock_identity(pid_file)?;
    if owns_identity_generation(pid_file, token_file, token) {
        return Ok(false);
    }

    let published_pid = fs::read_to_string(pid_file).ok();
    let published_token = fs::read_to_string(token_file).ok();
    let foreign_generation = published_pid
        .as_deref()
        .is_some_and(|pid| pid.trim() != process::id().to_string())
        || published_token
            .as_deref()
            .is_some_and(|published| published.trim() != token);
    if foreign_generation {
        return Ok(false);
    }

    publish_identity_locked(pid_file, token_file, token)?;
    Ok(true)
}

async fn maintain_server_identity(
    pid_file: PathBuf,
    token_file: PathBuf,
    token: String,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = tokio::time::interval(IDENTITY_REPAIR_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            _ = interval.tick() => {
                match repair_identity_if_missing(&pid_file, &token_file, &token) {
                    Ok(true) => debug_log("server identity files were missing or stale; republished active generation"),
                    Ok(false) => {}
                    Err(error) => debug_log(format!("failed to repair server identity files: {error}")),
                }
            }
        }
    }
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
    let server_identity = config.server_identity.clone();
    publish_identity(&config.pid_file, &token_file, &token)?;

    let (shutdown, shutdown_rx) = broadcast::channel(1);
    let identity_task = tokio::spawn(maintain_server_identity(
        config.pid_file.clone(),
        token_file.clone(),
        token.clone(),
        shutdown.subscribe(),
    ));
    let (state_updates, _) = broadcast::channel(16);
    let latest_state = Arc::new(RwLock::new(None));
    let state_cache_task = tokio::spawn(cache_latest_state(
        state_updates.subscribe(),
        shutdown.subscribe(),
        Arc::clone(&latest_state),
    ));
    let shutdown_announcement = Arc::new(ShutdownAnnouncement::default());
    let connection_limits = ConnectionLimits::from_fd_limit(
        descriptor_limits().map_or(256, |(soft, _)| soft),
        config.max_connections,
    );
    debug_log(format!("connection limits: {connection_limits:?}"));
    let state_operation_lock = Arc::new(AsyncMutex::new(()));
    let startup_ready = Arc::new(AtomicBool::new(config.state_source.is_none()));
    // Serve from here on: hook setup and the first snapshot (Git per session,
    // system-wide ps and lsof) run on the blocking pool. The startup task
    // holds the state operation lock from before the first accept, so every
    // state request queues behind it exactly as it used to queue in the
    // listen backlog.
    let startup_task = config.state_source.clone().map(|source| {
        let startup_guard = Arc::clone(&state_operation_lock)
            .try_lock_owned()
            .expect("a new state operation lock is uncontended");
        tokio::spawn(initialize_state_source(StartupContext {
            source,
            startup_guard,
            host: config.host.clone(),
            port: addr.port(),
            token_file: token_file.to_string_lossy().into_owned(),
            startup_ready: Arc::clone(&startup_ready),
            latest_state: Arc::clone(&latest_state),
            state_updates: state_updates.clone(),
            shutdown: shutdown.clone(),
            shutdown_announcement: Arc::clone(&shutdown_announcement),
        }))
    });
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
            server_identity,
            state_operation_lock,
            startup_ready,
            connection_limits,
        )
        .await;
        // Never remove hooks while startup may still be installing them.
        if let Some(startup_task) = startup_task {
            let _ = startup_task.await;
        }
        identity_task.abort();
        let _ = identity_task.await;
        state_cache_task.abort();
        // Dozens of tmux commands: keep them off the runtime thread.
        let cleanup_result = tokio::task::spawn_blocking(move || {
            cleanup_owned_generation(
                &config.pid_file,
                &token_file,
                &token,
                cleanup_state_source.as_deref(),
            )
        })
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error)));
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

struct StartupContext {
    source: Arc<dyn StateSource>,
    startup_guard: tokio::sync::OwnedMutexGuard<()>,
    host: String,
    port: u16,
    token_file: String,
    startup_ready: Arc<AtomicBool>,
    latest_state: Arc<RwLock<Option<String>>>,
    state_updates: broadcast::Sender<String>,
    shutdown: broadcast::Sender<()>,
    shutdown_announcement: Arc<ShutdownAnnouncement>,
}

/// Installs mux hooks (restoring recorded sidebars), then builds and publishes
/// the one initial snapshot, then starts the background loops, all while the
/// accept loop is already serving liveness and ingestion.
///
/// The state operation lock is held throughout, so requests that need state
/// run after the initial snapshot and their newer payloads are published after
/// it. Background loops, which publish snapshots without that lock, start only
/// once the initial snapshot is published, so it can never overwrite them.
async fn initialize_state_source(context: StartupContext) {
    let StartupContext {
        source,
        startup_guard,
        host,
        port,
        token_file,
        startup_ready,
        latest_state,
        state_updates,
        shutdown,
        shutdown_announcement,
    } = context;
    let hook_source = Arc::clone(&source);
    if let Err(error) =
        tokio::task::spawn_blocking(move || hook_source.setup_mux_hooks(&host, port, &token_file))
            .await
    {
        debug_log(format!("startup: mux hook setup failed: {error}"));
    }
    startup_ready.store(true, Ordering::Release);
    if shutdown_announcement.is_announced() {
        return;
    }
    let snapshot_source = Arc::clone(&source);
    match tokio::task::spawn_blocking(move || snapshot_source.snapshot_json()).await {
        Ok(snapshot) if !shutdown_announcement.is_announced() => {
            *latest_state.write().unwrap() = Some(snapshot.clone());
            let _ = state_updates.send(snapshot);
        }
        Ok(_) => return,
        Err(error) => debug_log(format!("startup: initial snapshot failed: {error}")),
    }
    drop(startup_guard);
    let _background_tasks = source.start_background_tasks(state_updates, shutdown);
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
    server_identity: Option<String>,
    state_operation_lock: Arc<AsyncMutex<()>>,
    startup_ready: Arc<AtomicBool>,
    connection_limits: ConnectionLimits,
) -> Result<(), ServerError> {
    let connection_limit = Arc::new(Semaphore::new(connection_limits.total));
    let websocket_capacity = WebsocketCapacity::new(connection_limits);
    let ingestion_lock = Arc::new(AsyncMutex::new(()));
    let (state_refreshes, refresh_requests) = mpsc::channel(1);
    tokio::spawn(run_coalesced_state_refreshes(
        state_source.clone(),
        state_updates.clone(),
        Arc::clone(&state_operation_lock),
        refresh_requests,
        shutdown.subscribe(),
    ));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                shutdown_announcement.announce_once(&state_source, &state_updates);
                tokio::time::sleep(Duration::from_millis(SERVER_SHUTDOWN_DRAIN_MS)).await;
                return Ok(());
            }
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(error) if accept_error_action(&error) == AcceptErrorAction::Retry => {
                        // Out of descriptors or an aborted handshake: the
                        // listener is fine, so back off instead of exiting.
                        debug_log(format!("accept failed transiently: {error}; retrying"));
                        tokio::time::sleep(ACCEPT_RETRY_BACKOFF).await;
                        continue;
                    }
                    Err(error) => {
                        debug_log(format!("accept failed fatally: {error}; shutting down"));
                        request_shutdown(
                            &state_source,
                            &state_updates,
                            &shutdown,
                            &shutdown_announcement,
                        );
                        tokio::time::sleep(Duration::from_millis(SERVER_SHUTDOWN_DRAIN_MS)).await;
                        return Err(error.into());
                    }
                };
                let Ok(connection_permit) = Arc::clone(&connection_limit).try_acquire_owned() else {
                    // Refuse without blocking the accept loop; the reply is
                    // best effort and the descriptor is closed right away.
                    let _ = stream.try_write(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 19\r\n\r\nconnection capacity",
                    );
                    continue;
                };
                let connection_shutdown = shutdown.clone();
                let connection_state_source = state_source.clone();
                let connection_state_updates = state_updates.clone();
                let connection_latest_state = Arc::clone(&latest_state);
                let connection_shutdown_announcement = Arc::clone(&shutdown_announcement);
                let connection_auth_token = auth_token.clone();
                let connection_state_operation_lock = Arc::clone(&state_operation_lock);
                let connection_ingestion_lock = Arc::clone(&ingestion_lock);
                let connection_state_refreshes = state_refreshes.clone();
                let connection_server_identity = server_identity.clone();
                let connection_websocket_capacity = websocket_capacity.clone();
                let connection_startup_ready = Arc::clone(&startup_ready);
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
                        connection_ingestion_lock,
                        connection_state_refreshes,
                        connection_server_identity,
                        connection_websocket_capacity,
                        connection_startup_ready,
                    )
                    .await;
                });
            }

        }
    }
}

const ACCEPT_RETRY_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptErrorAction {
    Retry,
    Fatal,
}

/// `accept` errno values that describe one connection or temporary resource
/// exhaustion rather than a broken listener (see accept(2)).
#[cfg(target_os = "linux")]
const TRANSIENT_ACCEPT_ERRNOS: &[i32] = &[
    1,   // EPERM: firewall rules forbid the connection
    12,  // ENOMEM
    23,  // ENFILE
    24,  // EMFILE
    71,  // EPROTO
    100, // ENETDOWN
    101, // ENETUNREACH
    105, // ENOBUFS
    113, // EHOSTUNREACH
];
#[cfg(not(target_os = "linux"))]
const TRANSIENT_ACCEPT_ERRNOS: &[i32] = &[
    12,  // ENOMEM
    23,  // ENFILE
    24,  // EMFILE
    50,  // ENETDOWN
    51,  // ENETUNREACH
    55,  // ENOBUFS
    65,  // EHOSTUNREACH
    100, // EPROTO
];

fn accept_error_action(error: &std::io::Error) -> AcceptErrorAction {
    use std::io::ErrorKind;
    let transient_kind = matches!(
        error.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::TimedOut
            | ErrorKind::OutOfMemory
    );
    let transient_errno = error
        .raw_os_error()
        .is_some_and(|errno| TRANSIENT_ACCEPT_ERRNOS.contains(&errno));
    if transient_kind || transient_errno {
        AcceptErrorAction::Retry
    } else {
        AcceptErrorAction::Fatal
    }
}

async fn run_coalesced_state_refreshes(
    state_source: Option<Arc<dyn StateSource>>,
    state_updates: broadcast::Sender<String>,
    state_operation_lock: Arc<AsyncMutex<()>>,
    mut requests: mpsc::Receiver<()>,
    mut shutdown: broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            request = requests.recv() => {
                if request.is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(RENDERED_SIDEBAR_FRAME_MS)).await;
                while requests.try_recv().is_ok() {}
                if let Ok(Some(snapshot)) = run_state_source_blocking(
                    &state_source,
                    &state_operation_lock,
                    StateSource::snapshot_json,
                ).await {
                    let _ = state_updates.send(snapshot);
                }
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

/// Runs agent-event and Pi runtime ingestion on the blocking pool without the
/// state operation lock. Ingestion only updates the server's own registries
/// (plus a short session lookup), so it must not queue behind a full snapshot
/// or a tmux mutation that holds that lock for seconds: senders such as the
/// Amp plugin give up after 750 ms. `ingestion_lock` applies these requests one
/// at a time in arrival order; the resulting sidebar state is published by the
/// coalesced refresh worker rather than per event.
async fn run_state_ingestion_blocking<R, F>(
    state_source: &Option<Arc<dyn StateSource>>,
    ingestion_lock: &AsyncMutex<()>,
    operation: F,
) -> Result<Option<R>, ServerError>
where
    R: Send + 'static,
    F: FnOnce(&dyn StateSource) -> R + Send + 'static,
{
    let Some(state_source) = state_source.clone() else {
        return Ok(None);
    };
    let _ingestion_guard = ingestion_lock.lock().await;
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
    ingestion_lock: Arc<AsyncMutex<()>>,
    state_refreshes: mpsc::Sender<()>,
    server_identity: Option<String>,
    websocket_capacity: WebsocketCapacity,
    startup_ready: Arc<AtomicBool>,
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
        let result = run_state_ingestion_blocking(&state_source, &ingestion_lock, move |source| {
            source.handle_agent_event_json(&body)
        })
        .await?
        .unwrap_or(Err(AgentEventError::CouldNotResolveSession));
        match result {
            Ok(()) => {
                let _ = state_refreshes.try_send(());
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
            run_state_ingestion_blocking(&state_source, &ingestion_lock, move |source| {
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
            run_state_ingestion_blocking(&state_source, &ingestion_lock, move |source| {
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
        let sidebar = parsed.query_param("client") == Some("sidebar");
        let Some(websocket_permit) = websocket_capacity.try_admit(sidebar) else {
            write_http_response(
                &mut stream,
                "503 Service Unavailable",
                "websocket capacity reached",
            )
            .await?;
            return Ok(());
        };
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
        let _websocket_permit = websocket_permit;
        debug_log("ws: client connected, sending hello + initial state");
        websocket.send(Message::text(HELLO_JSON)).await?;
        // Subscribe before reading the cached state: startup stores its
        // snapshot before broadcasting it, so a client that finds no cached
        // state is guaranteed to receive that broadcast.
        let mut connection_shutdown = shutdown.subscribe();
        let mut state_rx = state_updates.subscribe();
        let initial_state = latest_state.read().unwrap().clone();
        let initial_state = match initial_state {
            Some(state) => Some(state),
            // Early clients share the single startup snapshot instead of each
            // building their own while it is still in flight.
            None if state_source.is_some() => loop {
                tokio::select! {
                    _ = connection_shutdown.recv() => {
                        let _ = websocket.send(Message::text(QUIT_JSON)).await;
                        return Ok(());
                    }
                    update = state_rx.recv() => match update {
                        Ok(update) if update == QUIT_JSON => {
                            let _ = websocket.send(Message::text(QUIT_JSON)).await;
                            return Ok(());
                        }
                        Ok(update) if is_immediate_server_message(&update) => {}
                        Ok(update) => break Some(update),
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    },
                }
            },
            None => None,
        };
        if let Some(initial_state) = initial_state {
            websocket.send(Message::text(initial_state)).await?;
        }

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
                                if let Some(name) = switch_session_target(&command) {
                                    let _ = state_updates.send(activate_session_json(
                                        name,
                                        client_context.pane_id.as_deref(),
                                    ));
                                    // Let destination sidebars render the activation before
                                    // tmux makes one visible. Switching itself intentionally
                                    // bypasses the full-state operation lock: snapshots may
                                    // perform slow discovery, while tmux switch-client is a
                                    // narrow interactive operation that must remain responsive.
                                    tokio::time::sleep(Duration::from_millis(
                                        RENDERED_SIDEBAR_FRAME_MS,
                                    ))
                                    .await;
                                    if let Some(switch_source) = state_source.clone() {
                                        let switch_command = command.clone();
                                        let switch_context = client_context.clone();
                                        if let Some(payload) = tokio::task::spawn_blocking(move || {
                                            switch_source.handle_client_command_with_context(
                                                &switch_command,
                                                Some(&switch_context),
                                            )
                                        })
                                        .await?
                                        {
                                            websocket.send(Message::text(payload)).await?;
                                        }
                                    }

                                    // A settled full state clears the source sidebar's pending
                                    // marker. Coalesce it with other refresh requests so rapid
                                    // switches and agent events cannot build a snapshot queue.
                                    let _ = state_refreshes.try_send(());
                                    continue;
                                }
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
                                        if let Some(acknowledgement) =
                                            settings_acknowledgement(&command, &payload)
                                        {
                                            websocket.send(Message::text(acknowledgement)).await?;
                                        }
                                        let _ = state_updates.send(payload);
                                    }
                                }
                                if command.get("type").and_then(Value::as_str)
                                    == Some("rename-session")
                                    && let Some(snapshot) = run_state_source_blocking(
                                        &state_source,
                                        &state_operation_lock,
                                        StateSource::snapshot_json,
                                    ).await?
                                {
                                    let _ = state_updates.send(snapshot);
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

    if parsed.method == "GET" && parsed.path == "/" && !startup_ready.load(Ordering::Acquire) {
        // Launchers treat a live server as one whose hooks are installed and
        // whose recorded sidebars are restored; answer, but not as live yet.
        write_http_response(
            &mut stream,
            "503 Service Unavailable",
            "opensessions server initializing",
        )
        .await?;
    } else if parsed.method == "GET" && parsed.path == "/" {
        let body = server_identity
            .map(|identity| format!("opensessions server {identity}"))
            .unwrap_or_else(|| "opensessions server".to_string());
        write_http_response(&mut stream, "200 OK", &body).await?;
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
            | "/repair-sidebar-width"
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

fn settings_acknowledgement(command: &Value, state_payload: &str) -> Option<String> {
    if !matches!(
        command.get("type").and_then(Value::as_str),
        Some(
            "set-theme" | "set-sidebar-width" | "set-detail-panel-height" | "set-agent-panel-scope"
        )
    ) {
        return None;
    }
    let request_id = command.get("requestId")?.as_u64()?;
    let settings_revision = serde_json::from_str::<Value>(state_payload)
        .ok()?
        .get("settingsRevision")?
        .as_u64()?;
    serde_json::to_string(&ServerMessage::SettingsApplied {
        request_id,
        settings_revision,
    })
    .ok()
}

fn live_client_tty(
    provider: &dyn MuxProvider,
    context: Option<&ClientConnectionContext>,
) -> Option<String> {
    context
        .and_then(|context| context.pane_id.as_deref())
        .and_then(|pane_id| provider.client_tty_for_pane(pane_id))
        .or_else(|| {
            context
                .filter(|context| context.pane_id.is_none())
                .and_then(|context| context.client_tty.clone())
        })
}

fn switch_session_target(command: &Value) -> Option<String> {
    (command.get("type").and_then(Value::as_str) == Some("switch-session"))
        .then(|| command.get("name")?.as_str().map(str::to_string))?
}

fn is_immediate_server_message(payload: &str) -> bool {
    payload.contains(r#""type":"activate-session""#) || payload.contains(r#""type":"re-identify""#)
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
    fn settings_commands_receive_revisioned_acknowledgements() {
        let command = serde_json::json!({
            "type": "set-sidebar-width",
            "width": 42,
            "requestId": 7,
        });
        let payload = serde_json::json!({
            "type": "state",
            "settingsRevision": 3,
        })
        .to_string();

        let acknowledgement = settings_acknowledgement(&command, &payload).unwrap();

        assert_eq!(
            serde_json::from_str::<ServerMessage>(&acknowledgement).unwrap(),
            ServerMessage::SettingsApplied {
                request_id: 7,
                settings_revision: 3,
            }
        );
    }

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
    fn state_source_loads_sidebar_position_filter_and_session_order() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-runtime-config-test-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let config_dir = home.join(".config/opensessions");
        fs::create_dir_all(&config_dir).expect("create config directory");
        fs::write(
            config_dir.join("config.json"),
            r#"{"sidebarPosition":"right","sessionFilter":"running"}"#,
        )
        .expect("write config");
        fs::write(
            config_dir.join("session-order.json"),
            r#"{"order":["beta","alpha"],"hidden":["alpha"]}"#,
        )
        .expect("write session order");

        let source = default_state_source_from_env(|key| match key {
            "TMUX" => Some("/tmp/opensessions-runtime-config-test,1,1".to_string()),
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("tmux state source");

        assert_eq!(source.sidebar_position, SidebarPosition::Right);
        assert_eq!(
            *source.session_filter.lock().unwrap(),
            Some(SessionFilterMode::Running)
        );
        assert_eq!(
            source
                .session_order
                .lock()
                .unwrap()
                .apply(["alpha".to_string(), "beta".to_string()]),
            vec!["beta".to_string()]
        );
        let key =
            opensessions_runtime::shared::hash_server_key("/tmp/opensessions-runtime-config-test");
        assert!(
            config_dir
                .join(format!("session-order.{key}.json"))
                .is_file()
        );
        fs::remove_dir_all(home).expect("remove config directory");
    }

    #[test]
    fn session_order_is_isolated_by_tmux_socket() {
        let home = std::env::temp_dir().join(format!(
            "opensessions-session-order-isolation-test-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&home).expect("create home");

        let source_a = default_state_source_from_env(|key| match key {
            "TMUX" => Some("/tmp/opensessions-order-a,1,1".to_string()),
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("first tmux state source");
        let source_b = default_state_source_from_env(|key| match key {
            "TMUX" => Some("/tmp/opensessions-order-b,1,1".to_string()),
            "HOME" => Some(home.to_string_lossy().into_owned()),
            _ => None,
        })
        .expect("second tmux state source");

        source_a
            .session_order
            .lock()
            .unwrap()
            .sync(["alpha".to_string()]);
        source_b
            .session_order
            .lock()
            .unwrap()
            .sync(["alpha".to_string()]);
        source_a.session_order.lock().unwrap().hide("alpha");
        assert!(
            source_a
                .session_order
                .lock()
                .unwrap()
                .apply(["alpha".to_string()])
                .is_empty()
        );
        assert_eq!(
            source_b
                .session_order
                .lock()
                .unwrap()
                .apply(["alpha".to_string()]),
            vec!["alpha".to_string()]
        );

        fs::remove_dir_all(home).expect("remove home");
    }

    #[test]
    fn watcher_fingerprint_tracks_fresh_activity_without_visible_changes() {
        let first = AgentWatcherSnapshot {
            agent: "amp",
            thread_id: Some("thread".to_string()),
            thread_name: Some("Task".to_string()),
            last_user_prompt: Some("continue".to_string()),
            project_dir: Some("/repo".to_string()),
            status: AgentStatus::Running,
            ts: 100,
        };
        let mut second = first.clone();
        second.ts = 200;
        let live = |snapshot| LiveAgentSnapshot {
            snapshot,
            pane: None,
        };

        assert_ne!(
            AgentWatcherFingerprint::from(&live(first)),
            AgentWatcherFingerprint::from(&live(second))
        );
    }

    #[test]
    fn client_actions_resolve_the_current_viewer_instead_of_cached_tty() {
        struct ViewerProvider;

        impl MuxProvider for ViewerProvider {
            fn name(&self) -> &str {
                "viewer"
            }
            fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
                Vec::new()
            }
            fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
            fn get_current_session(&self) -> Option<String> {
                None
            }
            fn get_session_dir(&self, _name: &str) -> String {
                String::new()
            }
            fn get_pane_count(&self, _name: &str) -> u32 {
                0
            }
            fn get_client_tty(&self) -> String {
                String::new()
            }
            fn client_tty_for_pane(&self, pane_id: &str) -> Option<String> {
                (pane_id == "%sidebar").then(|| "/dev/current-viewer".to_string())
            }
            fn create_session(&self, _name: Option<&str>, _dir: Option<&str>) {}
            fn kill_session(&self, _name: &str) {}
            fn setup_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {}
            fn cleanup_hooks(&self) {}
        }

        let context = ClientConnectionContext {
            client_tty: Some("/dev/stale-viewer".to_string()),
            pane_id: Some("%sidebar".to_string()),
            ..ClientConnectionContext::default()
        };

        assert_eq!(
            live_client_tty(&ViewerProvider, Some(&context)).as_deref(),
            Some("/dev/current-viewer")
        );
    }

    #[test]
    fn subprocess_timeout_drains_large_stdout_before_waiting_for_exit() {
        let mut command = process::Command::new("sh");
        command.args(["-c", "head -c 131072 /dev/zero"]);

        let output = run_process_with_timeout(command, Duration::from_secs(2))
            .expect("large output should not deadlock");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131_072);
    }

    #[test]
    fn historical_focus_does_not_mark_a_background_completion_seen() {
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(PortTestProvider)]);
        source
            .focused_pane_by_session
            .lock()
            .unwrap()
            .insert("session".to_string(), "%old".to_string());

        source
            .apply_agent_event(&serde_json::json!({
                "agent": "amp",
                "tmuxSession": "session",
                "status": "done",
                "paneId": "%old",
            }))
            .expect("apply completion");

        assert!(source.agent_tracker.lock().unwrap().is_unseen("session"));
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

        let (snapshots, _) = ReadOnlyMuxStateSource::new(Vec::new())
            .with_agent_state_home(home.clone())
            .scan_live_agent_watchers(current_time_ms());

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

    #[derive(Clone)]
    struct ContendedSwitchSource {
        snapshot_count: Arc<AtomicUsize>,
        snapshot_started: Arc<AtomicBool>,
        snapshot_release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        switch_count: Arc<AtomicUsize>,
    }

    impl StateSource for ContendedSwitchSource {
        fn snapshot_json(&self) -> String {
            let previous = self.snapshot_count.fetch_add(1, Ordering::SeqCst);
            if previous > 0 {
                self.snapshot_started.store(true, Ordering::SeqCst);
                let (released, wake) = self.snapshot_release.as_ref();
                let mut released = released.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            "{}".to_string()
        }

        fn handle_client_command(&self, command: &Value) -> Option<String> {
            if command.get("type").and_then(Value::as_str) == Some("switch-session") {
                self.switch_count.fetch_add(1, Ordering::SeqCst);
            }
            None
        }
    }

    struct HibernateTestProvider;

    impl MuxProvider for HibernateTestProvider {
        fn name(&self) -> &str {
            "hibernate-test"
        }

        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            ["focused", "background", "viewed"]
                .into_iter()
                .map(|name| opensessions_runtime::mux::MuxSessionInfo {
                    name: name.to_string(),
                    created_at: 0,
                    dir: String::new(),
                    windows: 1,
                })
                .collect()
        }

        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("focused".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
        }
        fn get_pane_pid(&self, pane_id: &str) -> Option<u32> {
            match pane_id {
                "%1" => Some(100),
                "%2" => Some(200),
                "%3" => Some(300),
                "%visible" => Some(400),
                "%5" => Some(500),
                "%6" => Some(600),
                _ => None,
            }
        }
        fn client_tty_for_pane(&self, pane_id: &str) -> Option<String> {
            (pane_id == "%visible").then(|| "/dev/ttys009".to_string())
        }
        /// A second client shows session `viewed`, whose active window holds
        /// `%5` and a window linked from `background` holding `%6`; neither
        /// is that client's active pane.
        fn list_viewed_panes(&self) -> Vec<opensessions_runtime::mux::ViewedPane> {
            ["%5", "%6"]
                .into_iter()
                .map(|pane_id| opensessions_runtime::mux::ViewedPane {
                    session_name: "viewed".to_string(),
                    pane_id: pane_id.to_string(),
                })
                .collect()
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

    /// Every process ignores SIGTERM, like Amp.
    #[derive(Default)]
    struct TermIgnoringProcesses {
        signals: Mutex<Vec<(u32, opensessions_runtime::hibernate::Signal)>>,
        table_reads: AtomicUsize,
        /// How many of the first process-table reads fail (return nothing),
        /// like a `ps` that could not run.
        failed_table_reads: usize,
        /// Runs during the SIGTERM grace period.
        during_grace: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl ProcessControl for TermIgnoringProcesses {
        fn process_table(&self) -> Vec<opensessions_runtime::hibernate::ProcessEntry> {
            if self.table_reads.fetch_add(1, Ordering::SeqCst) < self.failed_table_reads {
                return Vec::new();
            }
            opensessions_runtime::hibernate::parse_process_table(
                "100 1 -zsh\n\
                 101 100 /Users/me/.amp/bin/amp threads continue T-bg\n\
                 102 101 /Users/me/.amp/bin/amp run plugin-runtime.ts\n\
                 200 1 -zsh\n\
                 201 200 /Users/me/.amp/bin/amp\n\
                 300 1 -zsh\n\
                 301 300 vim notes.md\n\
                 400 1 -zsh\n\
                 401 400 /Users/me/.amp/bin/amp\n\
                 500 1 -zsh\n\
                 501 500 /Users/me/.amp/bin/amp\n\
                 600 1 -zsh\n\
                 601 600 /Users/me/.amp/bin/amp\n",
            )
        }

        fn signal(&self, pid: u32, signal: opensessions_runtime::hibernate::Signal) -> bool {
            self.signals.lock().unwrap().push((pid, signal));
            true
        }

        fn sleep(&self, _duration: Duration) {
            if let Some(during_grace) = self.during_grace.lock().unwrap().as_ref() {
                during_grace();
            }
        }
    }

    const HIBERNATE_TEST_NOW: u64 = 1_000 + 6 * 60 * 60 * 1000 + 1;

    fn hibernate_test_source(
        settings: AutoHibernateSettings,
    ) -> (ReadOnlyMuxStateSource, Arc<TermIgnoringProcesses>) {
        let processes = Arc::new(TermIgnoringProcesses::default());
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(HibernateTestProvider)])
            .with_auto_hibernate(settings)
            .with_process_control(processes.clone())
            .with_now_ms(|| HIBERNATE_TEST_NOW);
        for (session, thread_id, status, pane_id) in [
            ("background", "T-bg", "done", "%1"),
            ("focused", "T-focused", "idle", "%2"),
            ("background", "T-no-agent", "idle", "%3"),
            ("background", "T-visible", "idle", "%visible"),
            ("background", "T-working", "running", "%4"),
            ("viewed", "T-viewed", "done", "%5"),
            ("background", "T-linked", "done", "%6"),
        ] {
            source
                .apply_agent_event(&serde_json::json!({
                    "agent": "amp",
                    "tmuxSession": session,
                    "threadId": thread_id,
                    "threadName": thread_id,
                    "status": status,
                    "paneId": pane_id,
                    "ts": 1_000,
                }))
                .expect("apply agent event");
        }
        (source, processes)
    }

    fn agent_status(source: &ReadOnlyMuxStateSource, session: &str, thread_id: &str) -> AgentEvent {
        source
            .agent_tracker
            .lock()
            .unwrap()
            .get_agents(session)
            .into_iter()
            .find(|agent| agent.thread_id.as_deref() == Some(thread_id))
            .expect("tracked agent")
    }

    #[test]
    fn auto_hibernate_kills_only_the_idle_background_agent_process() {
        use opensessions_runtime::hibernate::Signal;
        let (source, processes) = hibernate_test_source(AutoHibernateSettings::default());

        assert!(source.hibernate_idle_agent_panes());

        assert_eq!(
            *processes.signals.lock().unwrap(),
            vec![
                (101, Signal::Term),
                (101, Signal::Kill),
                (102, Signal::Kill)
            ],
            "SIGTERM-ignoring agent is escalated; shells and other panes are untouched"
        );
        let hibernated = agent_status(&source, "background", "T-bg");
        assert_eq!(hibernated.status, AgentStatus::Hibernated);
        assert_eq!(hibernated.liveness, Some(AgentLiveness::Exited));
        assert_eq!(hibernated.pane_id, None);
        assert_eq!(hibernated.ts, HIBERNATE_TEST_NOW);
        for (session, thread_id, status) in [
            ("focused", "T-focused", AgentStatus::Idle),
            ("background", "T-no-agent", AgentStatus::Idle),
            ("background", "T-visible", AgentStatus::Idle),
            ("background", "T-working", AgentStatus::Running),
        ] {
            assert_eq!(agent_status(&source, session, thread_id).status, status);
        }

        let snapshot = source.snapshot_json();
        assert!(snapshot.contains(r#""status":"hibernated""#), "{snapshot}");

        processes.signals.lock().unwrap().clear();
        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_hibernate_protects_every_session_and_pane_a_client_is_viewing() {
        let (source, processes) = hibernate_test_source(AutoHibernateSettings::default());

        assert!(source.hibernate_idle_agent_panes());

        let signalled = processes
            .signals
            .lock()
            .unwrap()
            .iter()
            .map(|(pid, _)| *pid)
            .collect::<HashSet<_>>();
        assert!(!signalled.contains(&501), "{signalled:?}");
        assert!(!signalled.contains(&601), "{signalled:?}");
        for (session, thread_id) in [("viewed", "T-viewed"), ("background", "T-linked")] {
            assert_eq!(
                agent_status(&source, session, thread_id).status,
                AgentStatus::Done
            );
        }
    }

    #[test]
    fn auto_hibernate_spares_an_agent_that_becomes_active_during_the_grace_period() {
        use opensessions_runtime::hibernate::Signal;
        let (source, processes) = hibernate_test_source(AutoHibernateSettings::default());
        let source = Arc::new(source);
        let resumed = Arc::downgrade(&source);
        *processes.during_grace.lock().unwrap() = Some(Box::new(move || {
            resumed
                .upgrade()
                .expect("source")
                .apply_agent_event(&serde_json::json!({
                    "agent": "amp",
                    "tmuxSession": "background",
                    "threadId": "T-bg",
                    "status": "running",
                    "paneId": "%1",
                    "ts": HIBERNATE_TEST_NOW,
                }))
                .expect("apply running event");
        }));

        assert!(!source.hibernate_idle_agent_panes());

        assert_eq!(
            *processes.signals.lock().unwrap(),
            vec![(101, Signal::Term)],
            "the agent that resumed during the grace period is not killed"
        );
        let resumed = agent_status(&source, "background", "T-bg");
        assert_eq!(resumed.status, AgentStatus::Running);
        assert_eq!(resumed.pane_id.as_deref(), Some("%1"));
    }

    #[test]
    fn auto_hibernate_does_nothing_when_disabled() {
        let (source, processes) = hibernate_test_source(AutoHibernateSettings {
            enabled: false,
            idle_after_ms: 1,
        });

        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
        assert_eq!(
            agent_status(&source, "background", "T-bg").status,
            AgentStatus::Done
        );
    }

    #[test]
    fn auto_hibernate_waits_for_the_configured_idle_threshold() {
        let (source, processes) = hibernate_test_source(AutoHibernateSettings {
            enabled: true,
            idle_after_ms: HIBERNATE_TEST_NOW,
        });

        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    /// A freshly restarted server: tmux still has Amp running in a background
    /// pane (`%1`, process 101) and the focused pane (`%2`, process 201), but
    /// the in-memory tracker is empty.
    struct RestartTestProvider;

    impl MuxProvider for RestartTestProvider {
        fn name(&self) -> &str {
            "restart-test"
        }

        /// The tmux layout never changes during these tests.
        fn state_fingerprint(&self) -> Option<u64> {
            Some(1)
        }

        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            ["focused", "background"]
                .into_iter()
                .map(|name| opensessions_runtime::mux::MuxSessionInfo {
                    name: name.to_string(),
                    created_at: 0,
                    dir: format!("/restart-test/{name}"),
                    windows: 1,
                })
                .collect()
        }

        fn list_agent_panes(
            &self,
            session_name: &str,
        ) -> Vec<opensessions_runtime::mux::AgentPane> {
            let (agent, pane_id) = match session_name {
                "background" => ("amp", "%1"),
                "focused" => ("amp", "%2"),
                _ => return Vec::new(),
            };
            vec![opensessions_runtime::mux::AgentPane {
                agent: agent.to_string(),
                pane_id: pane_id.to_string(),
                active: true,
                thread_id: None,
                thread_name: None,
            }]
        }

        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("focused".to_string())
        }
        fn get_session_dir(&self, name: &str) -> String {
            format!("/restart-test/{name}")
        }
        fn get_pane_pid(&self, pane_id: &str) -> Option<u32> {
            match pane_id {
                "%1" => Some(100),
                "%2" => Some(200),
                _ => None,
            }
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

    const HOUR_MS: u64 = 60 * 60 * 1000;

    struct AgentStateHome(PathBuf);

    impl AgentStateHome {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "opensessions-restart-test-{}-{}",
                process::id(),
                NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
            )))
        }

        fn write(&self, relative: &str, contents: &str, age_ms: u64) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).expect("create agent state dir");
            fs::write(&path, contents).expect("write agent state file");
            let modified = SystemTime::now() - Duration::from_millis(age_ms);
            fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(modified))
                .expect("set agent state mtime");
            path
        }

        fn amp_log(&self, thread_id: &str, pid: u32, subtype: &str, age_ms: u64) -> PathBuf {
            self.write(
                &format!(".cache/amp/logs/threads/{thread_id}.log"),
                &format!(
                    "{{\"type\":\"agent_state\",\"direction\":\"receive\",\"subtype\":\"{subtype}\",\"threadId\":\"{thread_id}\",\"pid\":\"{pid}\"}}\n"
                ),
                age_ms,
            )
        }
    }

    impl Drop for AgentStateHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn restarted_source(
        home: &AgentStateHome,
    ) -> (ReadOnlyMuxStateSource, Arc<TermIgnoringProcesses>) {
        let processes = Arc::new(TermIgnoringProcesses::default());
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(RestartTestProvider)])
            .with_process_control(processes.clone())
            .with_agent_state_home(home.0.clone());
        (source, processes)
    }

    fn file_mtime(path: &Path) -> u64 {
        file_mtime_ms(path).expect("mtime")
    }

    #[test]
    fn restarted_server_hibernates_agents_idle_since_before_the_restart() {
        use opensessions_runtime::hibernate::Signal;
        let home = AgentStateHome::new();
        let idle_log = home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        home.amp_log("T-focused", 201, "idle", 48 * HOUR_MS);
        let (source, processes) = restarted_source(&home);

        assert!(source.seed_agents_from_durable_state());
        source.snapshot_json();

        let seeded = agent_status(&source, "background", "T-bg");
        assert_eq!(
            seeded.ts,
            file_mtime(&idle_log),
            "idle clock is the thread's real last activity"
        );
        assert_eq!(seeded.pane_id.as_deref(), Some("%1"));
        assert_eq!(seeded.unseen, None);
        assert!(!source.agent_tracker.lock().unwrap().is_unseen("background"));

        assert!(source.hibernate_idle_agent_panes());
        assert_eq!(
            *processes.signals.lock().unwrap(),
            vec![
                (101, Signal::Term),
                (101, Signal::Kill),
                (102, Signal::Kill)
            ],
            "only the background agent process is stopped; the focused session is protected"
        );
        assert_eq!(
            agent_status(&source, "background", "T-bg").status,
            AgentStatus::Hibernated
        );
        assert_eq!(
            agent_status(&source, "focused", "T-focused").status,
            AgentStatus::Done
        );
    }

    #[test]
    fn restarted_server_keeps_recently_active_agents() {
        let home = AgentStateHome::new();
        let recent_log = home.amp_log("T-bg", 101, "idle", HOUR_MS);
        let (source, processes) = restarted_source(&home);

        assert!(source.seed_agents_from_durable_state());

        assert_eq!(
            agent_status(&source, "background", "T-bg").ts,
            file_mtime(&recent_log)
        );
        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn restarted_server_does_not_seed_agents_without_known_last_activity() {
        let home = AgentStateHome::new();
        // A working thread, a thread whose process is gone, and a log with no
        // writer process: none of them prove when a pane's agent was last used.
        home.amp_log("T-working", 101, "working", 48 * HOUR_MS);
        home.amp_log("T-gone", 999, "idle", 48 * HOUR_MS);
        home.write(
            ".cache/amp/logs/threads/T-anonymous.log",
            "{\"type\":\"agent_state\",\"direction\":\"receive\",\"subtype\":\"idle\"}\n",
            48 * HOUR_MS,
        );
        let (source, processes) = restarted_source(&home);

        assert!(!source.seed_agents_from_durable_state());
        source.snapshot_json();

        assert!(
            source
                .agent_tracker
                .lock()
                .unwrap()
                .get_agents("background")
                .is_empty()
        );
        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn seeds_resolved_by_project_dir_take_the_newest_activity_that_could_be_theirs() {
        let home = AgentStateHome::new();
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stop_reason":"end_turn"}}"#;
        home.write(
            ".claude/projects/-restart-test-background/old.jsonl",
            line,
            48 * HOUR_MS,
        );
        let unattributed = home.write(
            ".claude/projects/-somewhere-else/recent.jsonl",
            line,
            HOUR_MS,
        );
        let (source, _) = restarted_source(&home);

        assert!(source.seed_agents_from_durable_state());

        let seeded = agent_status(&source, "background", "old");
        assert_eq!(
            seeded.ts,
            file_mtime(&unattributed),
            "activity that matches no session could be this pane's, so it keeps the clock recent"
        );
        assert_eq!(seeded.pane_id, None);
    }

    #[test]
    fn hibernation_skips_agent_processes_with_recent_durable_activity() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        let (source, processes) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());
        source.snapshot_json();

        // The same Amp process moved on to a new thread the live watcher
        // cannot attribute to a session.
        home.amp_log("T-new", 101, "working", 0);

        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn hibernation_skips_a_process_serving_a_busy_thread_tracked_elsewhere() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        // The same Amp process has another thread awaiting approval, tracked
        // in a different session (for example from a plugin event resolved
        // by project dir) and quiet for longer than the idle threshold.
        home.amp_log("T-approval", 101, "awaiting_approval", 50 * HOUR_MS);
        let (source, processes) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());
        source
            .apply_agent_event(&serde_json::json!({
                "agent": "amp",
                "tmuxSession": "focused",
                "threadId": "T-approval",
                "status": "waiting",
                "ts": current_time_ms() - 50 * HOUR_MS,
            }))
            .expect("apply agent event");

        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn newer_activity_on_a_seeded_thread_releases_its_restored_idle_clock() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        let (source, processes) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());

        home.amp_log("T-bg", 101, "working", 0);
        let (_, released) = source.scan_live_agent_watchers(current_time_ms());
        assert!(released);

        assert!(
            source
                .agent_tracker
                .lock()
                .unwrap()
                .get_agents("background")
                .is_empty()
        );
        assert!(!source.hibernate_idle_agent_panes());
        assert!(processes.signals.lock().unwrap().is_empty());
    }

    #[test]
    fn live_scan_skips_threads_idle_past_the_recent_window() {
        let home = AgentStateHome::new();
        home.write(
            ".cache/amp/logs/threads/T-old.log",
            "{\"data\":{\"args\":{\"workdir\":\"/repo\"}}}\n{\"type\":\"agent_state\",\"direction\":\"receive\",\"subtype\":\"idle\",\"pid\":\"101\"}\n",
            AGENT_WATCHER_RECENT_MS + 60_000,
        );
        home.write(
            ".cache/amp/logs/threads/T-fresh.log",
            "{\"data\":{\"args\":{\"workdir\":\"/repo\"}}}\n{\"type\":\"agent_state\",\"direction\":\"receive\",\"subtype\":\"idle\",\"pid\":\"101\"}\n",
            0,
        );

        let (source, _) = restarted_source(&home);
        let (snapshots, _) = source.scan_live_agent_watchers(current_time_ms());

        assert_eq!(
            snapshots
                .iter()
                .filter_map(|snapshot| snapshot.thread_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["T-fresh"]
        );
    }

    fn amp_state_line(thread_id: &str, pid: u32, subtype: &str) -> String {
        format!(
            "{{\"type\":\"agent_state\",\"direction\":\"receive\",\"subtype\":\"{subtype}\",\"threadId\":\"{thread_id}\",\"pid\":\"{pid}\"}}\n"
        )
    }

    fn amp_title_line(thread_id: &str, pid: u32, title: &str) -> String {
        format!(
            "{{\"message\":\"[observer] onThreadTitle\",\"threadId\":\"{thread_id}\",\"data\":{{\"type\":\"thread_title\",\"title\":\"{title}\",\"source\":\"generated\"}},\"pid\":{pid}}}\n"
        )
    }

    fn background_threads(source: &ReadOnlyMuxStateSource) -> Vec<String> {
        let mut threads = source
            .agent_tracker
            .lock()
            .unwrap()
            .get_agents("background")
            .into_iter()
            .filter_map(|agent| agent.thread_id)
            .collect::<Vec<_>>();
        threads.sort();
        threads
    }

    #[test]
    fn live_amp_activity_routed_by_writer_pid_updates_the_seeded_row() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        let (source, _) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());

        // The log has no workdir, so only its writer process can route it.
        let working = home.amp_log("T-bg", 101, "working", 0);
        let mut poll = AgentWatcherPoll::default();
        assert!(poll.run(&source, current_time_ms()).changed);

        let row = agent_status(&source, "background", "T-bg");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.ts, file_mtime(&working));
        assert_eq!(row.pane_id.as_deref(), Some("%1"));
        assert_eq!(row.liveness, Some(AgentLiveness::Alive));

        let done = home.amp_log("T-bg", 101, "idle", 0);
        assert!(poll.run(&source, current_time_ms()).changed);
        let row = agent_status(&source, "background", "T-bg");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.ts, file_mtime(&done));
        assert!(
            source.agent_tracker.lock().unwrap().is_unseen("background"),
            "a live completion is unseen like any other"
        );
    }

    #[test]
    fn a_new_thread_in_a_seeded_amp_process_replaces_its_seed() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "idle", 48 * HOUR_MS);
        let (source, _) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());

        home.amp_log("T-new", 101, "working", 0);
        AgentWatcherPoll::default().run(&source, current_time_ms());

        assert_eq!(background_threads(&source), vec!["T-new"]);
        let row = agent_status(&source, "background", "T-new");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.pane_id.as_deref(), Some("%1"));
    }

    #[test]
    fn first_live_rescan_of_a_seeded_log_keeps_the_seed_quiet() {
        let home = AgentStateHome::new();
        let idle_log = home.amp_log("T-bg", 101, "idle", 60_000);
        let (source, _) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());

        AgentWatcherPoll::default().run(&source, current_time_ms());

        let row = agent_status(&source, "background", "T-bg");
        assert_eq!(row.status, AgentStatus::Done);
        assert_eq!(row.ts, file_mtime(&idle_log));
        assert_eq!(row.unseen, None);
        assert!(!source.agent_tracker.lock().unwrap().is_unseen("background"));
    }

    #[test]
    fn pid_routed_amp_rows_carry_the_logged_thread_title() {
        let home = AgentStateHome::new();
        home.write(
            ".cache/amp/logs/threads/T-bg.log",
            &format!(
                "{}{}",
                amp_title_line("T-bg", 101, "Restore idle clocks"),
                amp_state_line("T-bg", 101, "idle"),
            ),
            48 * HOUR_MS,
        );
        let (source, _) = restarted_source(&home);
        assert!(source.seed_agents_from_durable_state());
        assert_eq!(
            agent_status(&source, "background", "T-bg")
                .thread_name
                .as_deref(),
            Some("Restore idle clocks")
        );

        // Long threads log their title near the start, outside the tail the
        // watcher parses.
        let filler = "{\"message\":\"websocket message\",\"pid\":\"101\"}\n"
            .repeat((AMP_LOG_TAIL_BYTES as usize / 40) + 1_000);
        home.write(
            ".cache/amp/logs/threads/T-long.log",
            &format!(
                "{}{filler}{}",
                amp_title_line("T-long", 101, "Long running thread"),
                amp_state_line("T-long", 101, "working"),
            ),
            0,
        );
        AgentWatcherPoll::default().run(&source, current_time_ms());

        let row = agent_status(&source, "background", "T-long");
        assert_eq!(row.status, AgentStatus::Running);
        assert_eq!(row.thread_name.as_deref(), Some("Long running thread"));
    }

    #[test]
    fn amp_activity_from_processes_outside_agent_panes_is_ignored() {
        let home = AgentStateHome::new();
        home.amp_log("T-gone", 999, "working", 0);
        home.amp_log("T-shell", 401, "working", 0);
        let (source, _) = restarted_source(&home);

        let outcome = AgentWatcherPoll::default().run(&source, current_time_ms());

        assert!(!outcome.changed);
        for session in ["background", "focused"] {
            assert!(
                source
                    .agent_tracker
                    .lock()
                    .unwrap()
                    .get_agents(session)
                    .is_empty()
            );
        }
    }

    #[test]
    fn plugin_events_keep_precedence_over_older_pid_routed_log_snapshots() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "working", 10_000);
        let (source, _) = restarted_source(&home);
        source
            .apply_agent_event(&serde_json::json!({
                "agent": "amp",
                "tmuxSession": "background",
                "threadId": "T-bg",
                "threadName": "Plugin title",
                "status": "done",
                "paneId": "%1",
            }))
            .expect("apply plugin event");
        let mut poll = AgentWatcherPoll::default();

        poll.run(&source, current_time_ms());
        let row = agent_status(&source, "background", "T-bg");
        assert_eq!(
            row.status,
            AgentStatus::Done,
            "older log activity never wins"
        );
        assert_eq!(row.thread_name.as_deref(), Some("Plugin title"));

        std::thread::sleep(Duration::from_millis(5));
        home.amp_log("T-bg", 101, "working", 0);
        poll.run(&source, current_time_ms());
        let row = agent_status(&source, "background", "T-bg");
        assert_eq!(
            row.status,
            AgentStatus::Running,
            "newer log activity applies"
        );
        assert_eq!(row.thread_name.as_deref(), Some("Plugin title"));
        assert_eq!(row.pane_id.as_deref(), Some("%1"));
    }

    #[test]
    fn watcher_reads_processes_only_for_unattributed_amp_activity() {
        let home = AgentStateHome::new();
        home.write(
            ".cache/amp/logs/threads/T-repo.log",
            &format!(
                "{{\"data\":{{\"args\":{{\"workdir\":\"/restart-test/background\"}}}}}}\n{}",
                amp_state_line("T-repo", 101, "working"),
            ),
            0,
        );
        let (source, processes) = restarted_source(&home);
        let mut poll = AgentWatcherPoll::default();
        let reads = || processes.table_reads.load(Ordering::SeqCst);

        poll.run(&source, current_time_ms());
        assert_eq!(background_threads(&source), vec!["T-repo"]);
        assert_eq!(reads(), 0, "project-dir routing needs no process table");

        home.amp_log("T-bg", 101, "working", 0);
        poll.run(&source, current_time_ms());
        assert_eq!(background_threads(&source), vec!["T-bg", "T-repo"]);
        assert_eq!(reads(), 1);

        home.amp_log("T-bg", 101, "idle", 0);
        poll.run(&source, current_time_ms());
        assert_eq!(reads(), 1, "routes are reused while tmux is unchanged");

        home.amp_log("T-gone", 999, "working", 0);
        poll.run(&source, current_time_ms());
        poll.run(&source, current_time_ms());
        assert_eq!(reads(), 2, "processes outside agent panes are remembered");
    }

    #[test]
    fn older_log_activity_does_not_duplicate_a_thread_tracked_in_another_session() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "working", 60_000);
        let (source, _) = restarted_source(&home);
        source
            .apply_agent_event(&serde_json::json!({
                "agent": "amp",
                "tmuxSession": "focused",
                "threadId": "T-bg",
                "status": "done",
                "ts": current_time_ms(),
            }))
            .expect("apply plugin event");

        AgentWatcherPoll::default().run(&source, current_time_ms());

        assert!(
            background_threads(&source).is_empty(),
            "the pid-routed older snapshot must not add a second T-bg row"
        );
        assert_eq!(
            agent_status(&source, "focused", "T-bg").status,
            AgentStatus::Done
        );
    }

    #[test]
    fn a_failed_process_table_read_is_not_remembered_as_unroutable() {
        let home = AgentStateHome::new();
        home.amp_log("T-bg", 101, "working", 0);
        let processes = Arc::new(TermIgnoringProcesses {
            failed_table_reads: 1,
            ..Default::default()
        });
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(RestartTestProvider)])
            .with_process_control(processes.clone())
            .with_agent_state_home(home.0.clone());
        let mut poll = AgentWatcherPoll::default();

        poll.run(&source, current_time_ms());
        assert!(background_threads(&source).is_empty(), "ps failed");

        poll.run(&source, current_time_ms());
        assert_eq!(background_threads(&source), vec!["T-bg"]);
    }

    #[test]
    fn hibernated_is_an_accepted_external_status() {
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(HibernateTestProvider)]);
        source
            .apply_agent_event(&serde_json::json!({
                "agent": "my-agent",
                "tmuxSession": "background",
                "status": "hibernated",
            }))
            .expect("hibernated status accepted");
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

    #[derive(Default)]
    struct RenameTestProvider {
        calls: Mutex<Vec<(String, String)>>,
    }

    impl MuxProvider for RenameTestProvider {
        fn name(&self) -> &str {
            "rename-test"
        }

        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            Vec::new()
        }

        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("draft".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
        }
        fn get_pane_count(&self, _name: &str) -> u32 {
            1
        }
        fn get_client_tty(&self) -> String {
            String::new()
        }
        fn create_session(&self, _name: Option<&str>, _dir: Option<&str>) {}
        fn rename_session(&self, name: &str, new_name: &str) -> Option<String> {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), new_name.to_string()));
            // Like tmux, replace characters that are invalid in names.
            Some(new_name.replace(['.', ':'], "_"))
        }
        fn kill_session(&self, _name: &str) {}
        fn setup_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {}
        fn cleanup_hooks(&self) {}
    }

    #[derive(Default)]
    struct PaneExitTestProvider {
        list_sessions_calls: AtomicUsize,
        repair_scheduler: std::sync::OnceLock<Arc<SidebarWidthRepairScheduler>>,
        cleanup_observations: Mutex<Vec<(HashMap<String, String>, usize)>>,
    }

    impl MuxProvider for PaneExitTestProvider {
        fn name(&self) -> &str {
            "pane-exit-test"
        }

        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            self.list_sessions_calls.fetch_add(1, Ordering::SeqCst);
            ["alpha", "beta", "gamma"]
                .into_iter()
                .map(|name| opensessions_runtime::mux::MuxSessionInfo {
                    name: name.to_string(),
                    created_at: 0,
                    dir: String::new(),
                    windows: 1,
                })
                .collect()
        }

        fn list_sidebar_panes(
            &self,
            _session_name: Option<&str>,
        ) -> Vec<opensessions_runtime::mux::SidebarPane> {
            ["alpha", "beta", "gamma"]
                .into_iter()
                .enumerate()
                .map(|(index, session)| opensessions_runtime::mux::SidebarPane {
                    pane_id: format!("%{index}"),
                    session_name: session.to_string(),
                    window_id: format!("@{index}"),
                    width: Some(36),
                    window_width: Some(160),
                })
                .collect()
        }

        fn kill_orphaned_sidebar_panes_with_fallbacks(
            &self,
            fallback_sessions: &HashMap<String, String>,
        ) {
            let pending_repairs = self
                .repair_scheduler
                .get()
                .map(|scheduler| scheduler.pending_requests.load(Ordering::SeqCst))
                .unwrap_or_default();
            self.cleanup_observations
                .lock()
                .unwrap()
                .push((fallback_sessions.clone(), pending_repairs));
        }

        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("alpha".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
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

    #[test]
    fn pane_exit_queues_width_repair_before_one_snapshot_backed_orphan_cleanup() {
        let provider = Arc::new(PaneExitTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        provider
            .repair_scheduler
            .set(Arc::clone(&source.sidebar_width_repairs))
            .expect("scheduler set once");
        source
            .sidebar_coordinator
            .lock()
            .unwrap()
            .acknowledge_sidebar_connected();

        // The first snapshot also warms caches; measure a steady-state one.
        source.snapshot_json();
        let before_snapshot = provider.list_sessions_calls.load(Ordering::SeqCst);
        source.snapshot_json();
        let calls_per_snapshot =
            provider.list_sessions_calls.load(Ordering::SeqCst) - before_snapshot;

        let before_hook = provider.list_sessions_calls.load(Ordering::SeqCst);
        source.handle_http_hook("/pane-exited", "");
        let hook_calls = provider.list_sessions_calls.load(Ordering::SeqCst) - before_hook;

        assert_eq!(
            hook_calls, calls_per_snapshot,
            "pane exit must build one display-order snapshot, not one per sidebar pane"
        );
        let observations = provider.cleanup_observations.lock().unwrap();
        let (fallbacks, pending_repairs) = observations.first().expect("orphan cleanup ran");
        assert_eq!(
            *pending_repairs, 1,
            "fixed-width repair must be queued before slow orphan cleanup"
        );
        assert_eq!(
            fallbacks,
            &HashMap::from([
                ("alpha".to_string(), "beta".to_string()),
                ("beta".to_string(), "alpha".to_string()),
                ("gamma".to_string(), "beta".to_string()),
            ])
        );
    }

    #[test]
    fn rename_command_updates_tmux_and_requests_live_client_reidentification() {
        let provider = Arc::new(RenameTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        *source.focused_session.lock().unwrap() = Some("draft".to_string());
        source
            .focused_pane_by_session
            .lock()
            .unwrap()
            .insert("draft".to_string(), "%1".to_string());

        let response = source.handle_client_command(&serde_json::json!({
            "type": "rename-session",
            "name": "draft",
            "newName": "descriptive-name",
        }));

        assert_eq!(
            provider.calls.lock().unwrap().as_slice(),
            &[("draft".to_string(), "descriptive-name".to_string())]
        );
        assert_eq!(
            response,
            serde_json::to_string(&ServerMessage::ReIdentify {
                old_name: "draft".to_string(),
                new_name: "descriptive-name".to_string(),
            })
            .ok()
        );
        assert_eq!(
            source.focused_session.lock().unwrap().as_deref(),
            Some("descriptive-name")
        );
        assert_eq!(
            source
                .focused_pane_by_session
                .lock()
                .unwrap()
                .get("descriptive-name")
                .map(String::as_str),
            Some("%1")
        );
    }

    #[test]
    fn rename_follows_the_name_the_mux_actually_assigned() {
        let provider = Arc::new(RenameTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        *source.focused_session.lock().unwrap() = Some("draft".to_string());

        let response = source.handle_client_command(&serde_json::json!({
            "type": "rename-session",
            "name": "draft",
            "newName": "v1.2:x",
        }));

        assert_eq!(
            response,
            serde_json::to_string(&ServerMessage::ReIdentify {
                old_name: "draft".to_string(),
                new_name: "v1_2_x".to_string(),
            })
            .ok()
        );
        assert_eq!(
            source.focused_session.lock().unwrap().as_deref(),
            Some("v1_2_x")
        );
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
    async fn root_liveness_identifies_the_server_namespace() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-root-id-{}-{id}", process::id()));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, root.with_extension("pid"))
                .with_token_file(root.with_extension("token"))
                .with_server_identity("d4e887e7c9f4d63f"),
        )
        .await
        .expect("start server");
        let response = request_at(
            server.addr(),
            "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
        )
        .await;
        assert!(response.ends_with(b"opensessions server d4e887e7c9f4d63f"));
        server.shutdown().await.expect("shutdown");
        let _ = fs::remove_file(root.with_extension("identity.lock"));
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
    async fn burst_state_refresh_requests_build_one_snapshot() {
        let snapshot_count = Arc::new(AtomicUsize::new(0));
        let source_count = Arc::clone(&snapshot_count);
        let source: Arc<dyn StateSource> = Arc::new(move || {
            source_count.fetch_add(1, Ordering::SeqCst);
            "{}".to_string()
        });
        let (state_updates, mut updates) = broadcast::channel(4);
        let (shutdown, _) = broadcast::channel(1);
        let (requests, receiver) = mpsc::channel(1);
        let worker = tokio::spawn(run_coalesced_state_refreshes(
            Some(source),
            state_updates,
            Arc::new(AsyncMutex::new(())),
            receiver,
            shutdown.subscribe(),
        ));

        assert!(requests.try_send(()).is_ok());
        for _ in 0..20 {
            let _ = requests.try_send(());
        }
        tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("coalesced snapshot should be broadcast")
            .expect("state update channel should remain open");

        assert_eq!(snapshot_count.load(Ordering::SeqCst), 1);
        let _ = shutdown.send(());
        worker.await.expect("refresh worker should stop");
    }

    #[tokio::test]
    async fn websocket_switch_does_not_queue_behind_a_full_state_snapshot() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "opensessions-switch-contention-{}-{id}",
            process::id()
        ));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let snapshot_count = Arc::new(AtomicUsize::new(0));
        let snapshot_started = Arc::new(AtomicBool::new(false));
        let snapshot_release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let switch_count = Arc::new(AtomicUsize::new(0));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(ContendedSwitchSource {
                    snapshot_count: Arc::clone(&snapshot_count),
                    snapshot_started: Arc::clone(&snapshot_started),
                    snapshot_release: Arc::clone(&snapshot_release),
                    switch_count: Arc::clone(&switch_count),
                }),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let uri = format!("ws://{}", server.addr()).parse().expect("ws uri");
        let authorization = format!("Bearer {}", token.trim())
            .parse()
            .expect("authorization header");
        let (mut websocket, _) = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(http::header::AUTHORIZATION, authorization)
            .expect("add authorization header")
            .connect()
            .await
            .expect("connect websocket");
        let _ = websocket.next().await.expect("hello").expect("hello frame");
        let _ = websocket
            .next()
            .await
            .expect("initial state")
            .expect("initial state frame");

        let refresh = tokio::spawn(request_at(
            server.addr(),
            format!(
                "POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
                token.trim()
            ),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !snapshot_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("refresh snapshot should start");

        websocket
            .send(Message::text(
                r#"{"type":"switch-session","name":"destination"}"#,
            ))
            .await
            .expect("send switch command");
        let switched_before_release = tokio::time::timeout(Duration::from_millis(150), async {
            while switch_count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();

        let (released, wake) = snapshot_release.as_ref();
        *released.lock().unwrap() = true;
        wake.notify_all();
        let _ = refresh.await;
        server.shutdown().await.expect("stop server");

        assert!(
            switched_before_release,
            "interactive switching must bypass unrelated full-state work"
        );
    }

    /// A test server's identity files, removed (with the identity lock) on drop.
    struct TestServerFiles {
        pid_file: PathBuf,
        token_file: PathBuf,
    }

    impl TestServerFiles {
        fn paths(&self) -> (PathBuf, PathBuf) {
            (self.pid_file.clone(), self.token_file.clone())
        }
    }

    impl Drop for TestServerFiles {
        fn drop(&mut self) {
            for path in [
                self.pid_file.with_extension("identity.lock"),
                self.pid_file.clone(),
                self.token_file.clone(),
            ] {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn startup_test_paths(label: &str) -> TestServerFiles {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-{label}-{}-{id}", process::id()));
        TestServerFiles {
            pid_file: root.with_extension("pid"),
            token_file: root.with_extension("token"),
        }
    }

    async fn connect_test_websocket(
        addr: SocketAddr,
        token: &str,
    ) -> tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<TcpStream>> {
        let uri = format!("ws://{addr}").parse().expect("ws uri");
        let (websocket, _) = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(
                http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().expect("authorization"),
            )
            .expect("add authorization header")
            .connect()
            .await
            .expect("connect websocket");
        websocket
    }

    async fn next_text<S>(websocket: &mut S) -> String
    where
        S: futures_util::Stream<Item = Result<Message, tokio_websockets::Error>> + Unpin,
    {
        let message = tokio::time::timeout(Duration::from_secs(5), websocket.next())
            .await
            .expect("websocket frame in time")
            .expect("websocket open")
            .expect("websocket frame");
        message.as_text().expect("text frame").to_string()
    }

    #[tokio::test]
    async fn liveness_answers_while_the_initial_snapshot_is_slow() {
        let files = startup_test_paths("slow-initial-snapshot");
        let (pid_file, token_file) = files.paths();
        let snapshot_count = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(SlowSnapshotSource {
                    snapshot_count: Arc::clone(&snapshot_count),
                    delay: Duration::from_millis(2_000),
                }),
        )
        .await
        .expect("start server");
        // Hook setup is instant here; only the snapshot is slow.
        let liveness = tokio::time::timeout(Duration::from_millis(500), async {
            loop {
                let response = http_exchange(
                    server.addr(),
                    "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
                )
                .await;
                if !response.starts_with(b"HTTP/1.1 503") {
                    return response;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let answered_after = started.elapsed();
        server.shutdown().await.expect("stop server");

        let liveness = liveness.expect("liveness must not wait for the initial snapshot");
        assert!(
            liveness.starts_with(b"HTTP/1.1 200 OK"),
            "unexpected liveness response: {}",
            String::from_utf8_lossy(&liveness)
        );
        assert!(
            answered_after < Duration::from_millis(500),
            "liveness answered after {answered_after:?}"
        );
    }

    /// Hook setup blocks until released, like tmux installing hooks and
    /// restoring sidebars on a loaded machine.
    #[derive(Clone)]
    struct GatedHookSource {
        hooks_release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        hooks_installed: Arc<AtomicBool>,
    }

    impl StateSource for GatedHookSource {
        fn snapshot_json(&self) -> String {
            "{}".to_string()
        }

        fn setup_mux_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {
            let (released, wake) = self.hooks_release.as_ref();
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.hooks_installed.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn liveness_reports_initializing_until_hooks_are_installed() {
        let files = startup_test_paths("hook-gate");
        let (pid_file, token_file) = files.paths();
        let hooks_release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let hooks_installed = Arc::new(AtomicBool::new(false));
        // Safety net so a server that installs hooks on the runtime thread
        // fails this test instead of hanging it.
        let fallback_release = Arc::clone(&hooks_release);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            let (released, wake) = fallback_release.as_ref();
            *released.lock().unwrap() = true;
            wake.notify_all();
        });
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(GatedHookSource {
                    hooks_release: Arc::clone(&hooks_release),
                    hooks_installed: Arc::clone(&hooks_installed),
                }),
        )
        .await
        .expect("start server");
        let liveness = "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string();
        let initializing = tokio::time::timeout(
            Duration::from_millis(500),
            http_exchange(server.addr(), liveness.clone()),
        )
        .await
        .expect("liveness answers during hook setup");

        let (released, wake) = hooks_release.as_ref();
        *released.lock().unwrap() = true;
        wake.notify_all();
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = http_exchange(server.addr(), liveness.clone()).await;
                if response.starts_with(b"HTTP/1.1 200 OK") {
                    return hooks_installed.load(Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        server.shutdown().await.expect("stop server");

        // Launchers treat a live server as one whose hooks and restored
        // sidebars are in place, so it must not claim liveness earlier.
        assert!(
            initializing.starts_with(b"HTTP/1.1 503 Service Unavailable"),
            "unexpected response during hook setup: {}",
            String::from_utf8_lossy(&initializing)
        );
        assert!(!initializing.ends_with(b"opensessions server"));
        assert_eq!(ready, Ok(true), "liveness becomes ready only after hooks");
    }

    #[tokio::test]
    async fn early_sidebar_connections_share_one_initial_snapshot() {
        let files = startup_test_paths("early-sidebars");
        let (pid_file, token_file) = files.paths();
        let snapshot_count = Arc::new(AtomicUsize::new(0));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(SlowSnapshotSource {
                    snapshot_count: Arc::clone(&snapshot_count),
                    delay: Duration::from_millis(400),
                }),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let token = token.trim().to_string();
        let addr = server.addr();
        let sidebars = (0..8)
            .map(|_| {
                let token = token.clone();
                tokio::spawn(async move {
                    let mut websocket = connect_test_websocket(addr, &token).await;
                    let hello = next_text(&mut websocket).await;
                    let state = next_text(&mut websocket).await;
                    (hello, state)
                })
            })
            .collect::<Vec<_>>();
        let mut received = Vec::new();
        for sidebar in sidebars {
            received.push(sidebar.await.expect("sidebar task"));
        }
        server.shutdown().await.expect("stop server");

        for (hello, state) in received {
            assert_eq!(hello, HELLO_JSON);
            assert_eq!(state, "{}");
        }
        assert_eq!(snapshot_count.load(Ordering::SeqCst), 1);
    }

    /// The initial snapshot is slow and marked `initial`; a metadata update
    /// produces a newer, marked state.
    #[derive(Clone)]
    struct InitialThenNewerSource {
        snapshot_started: Arc<AtomicBool>,
    }

    impl StateSource for InitialThenNewerSource {
        fn snapshot_json(&self) -> String {
            self.snapshot_started.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            r#"{"type":"state","marker":"initial"}"#.to_string()
        }

        fn handle_http_json(&self, _path: &str, _body: &Value) -> Option<String> {
            Some(r#"{"type":"state","marker":"newer"}"#.to_string())
        }
    }

    #[tokio::test]
    async fn the_initial_snapshot_never_overwrites_newer_state() {
        let files = startup_test_paths("initial-overwrite");
        let (pid_file, token_file) = files.paths();
        let snapshot_started = Arc::new(AtomicBool::new(false));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(InitialThenNewerSource {
                    snapshot_started: Arc::clone(&snapshot_started),
                }),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let token = token.trim().to_string();
        let addr = server.addr();
        let mut early = connect_test_websocket(addr, &token).await;
        assert_eq!(next_text(&mut early).await, HELLO_JSON);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !snapshot_started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("initial snapshot starts");
        let body = r#"{"session":"work","text":"busy"}"#;
        let update = http_exchange(
            addr,
            format!(
                "POST /set-status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(update.starts_with(b"HTTP/1.1 204 No Content"));
        let first = next_text(&mut early).await;
        let second = next_text(&mut early).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut late = connect_test_websocket(addr, &token).await;
        assert_eq!(next_text(&mut late).await, HELLO_JSON);
        let late_state = next_text(&mut late).await;
        server.shutdown().await.expect("stop server");

        assert!(first.contains("initial"), "first state: {first}");
        assert!(second.contains("newer"), "second state: {second}");
        assert!(
            late_state.contains("newer"),
            "late client state: {late_state}"
        );
    }

    #[test]
    fn connection_limits_are_derived_from_the_descriptor_limit() {
        // macOS's default soft limit when raising it is not possible.
        let constrained = ConnectionLimits::from_fd_limit(256, None);
        assert_eq!(constrained.total, 160);
        assert_eq!(constrained.websockets, 150);
        assert_eq!(constrained.passive_websockets, 18);

        // A raised limit allows far more sidebars than the old fixed 120,
        // but the hard cap still bounds the server's descriptors.
        let raised = ConnectionLimits::from_fd_limit(8_192, None);
        assert_eq!(raised.total, 2_048);
        assert_eq!(raised.websockets, 1_920);
        assert_eq!(raised.passive_websockets, 240);
        assert_eq!(ConnectionLimits::from_fd_limit(u64::MAX, None), raised);

        // A configured cap may lower the limit but never exceed the
        // descriptor budget or drop below a usable floor.
        assert_eq!(
            ConnectionLimits::from_fd_limit(256, Some(10_000)),
            constrained
        );
        let configured = ConnectionLimits::from_fd_limit(8_192, Some(500));
        assert_eq!(configured.total, 500);
        assert_eq!(configured.websockets, 469);
        assert_eq!(ConnectionLimits::from_fd_limit(8_192, Some(1)).total, 16);
        assert_eq!(ConnectionLimits::from_fd_limit(32, None).total, 16);
        for limits in [constrained, raised, configured] {
            assert!(limits.websockets < limits.total);
            assert!(limits.passive_websockets < limits.websockets);
        }
    }

    #[test]
    fn configured_connection_cap_is_read_from_the_environment() {
        let env = |value: &'static str| {
            move |key: &str| (key == "OPENSESSIONS_MAX_CONNECTIONS").then(|| value.to_string())
        };
        assert_eq!(max_connections_from_env(env("600")), Some(600));
        assert_eq!(max_connections_from_env(env(" 600 ")), Some(600));
        assert_eq!(max_connections_from_env(env("lots")), None);
        assert_eq!(max_connections_from_env(env("0")), None);
        assert_eq!(max_connections_from_env(|_| None), None);
    }

    #[cfg(unix)]
    #[test]
    fn raising_the_descriptor_limit_never_lowers_it() {
        let before = descriptor_limits().expect("read RLIMIT_NOFILE");
        let raised = raise_fd_soft_limit().expect("raise RLIMIT_NOFILE");
        let after = descriptor_limits().expect("read RLIMIT_NOFILE");
        assert_eq!(raised, after.0);
        assert!(after.0 >= before.0);
        assert!(after.0 >= before.1.min(FD_SOFT_LIMIT_TARGET));
        assert_eq!(after.1, before.1, "the hard limit is left alone");
    }

    async fn try_connect_websocket(
        addr: SocketAddr,
        token: &str,
        path: &str,
    ) -> Option<tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<TcpStream>>>
    {
        let uri = format!("ws://{addr}{path}").parse().expect("ws uri");
        let (mut websocket, _) = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(
                http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().expect("authorization"),
            )
            .expect("add authorization header")
            .connect()
            .await
            .ok()?;
        assert_eq!(next_text(&mut websocket).await, HELLO_JSON);
        let _ = next_text(&mut websocket).await;
        Some(websocket)
    }

    async fn connect_websocket_when_released(
        addr: SocketAddr,
        token: &str,
        path: &str,
    ) -> Option<tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<TcpStream>>>
    {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(websocket) = try_connect_websocket(addr, token, path).await {
                return Some(websocket);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn websocket_slots_are_reserved_for_sidebars_and_released_on_close() {
        let files = startup_test_paths("websocket-capacity");
        let (pid_file, token_file) = files.paths();
        let limits = ConnectionLimits::from_fd_limit(u64::MAX, Some(32));
        assert_eq!((limits.websockets, limits.passive_websockets), (24, 4));
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_max_connections(32)
                .with_state_source(|| "{}".to_string()),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let token = token.trim().to_string();
        let addr = server.addr();
        let sidebar = SIDEBAR_WEBSOCKET_PATH;

        let mut passive = Vec::new();
        for _ in 0..4 {
            passive.push(try_connect_websocket(addr, &token, "/").await);
        }
        let passive_over_share = try_connect_websocket(addr, &token, "/").await.is_some();
        let mut sidebars = Vec::new();
        for _ in 0..20 {
            sidebars.push(try_connect_websocket(addr, &token, sidebar).await);
        }
        let sidebar_over_cap = try_connect_websocket(addr, &token, sidebar).await.is_some();

        // Closing a client must free its slot promptly, for either kind.
        let mut closed_passive = passive.pop().flatten().expect("passive client");
        closed_passive.close().await.expect("close passive client");
        drop(closed_passive);
        let passive_after_close = connect_websocket_when_released(addr, &token, "/").await;
        drop(sidebars.pop().flatten().expect("sidebar client"));
        let sidebar_after_close = connect_websocket_when_released(addr, &token, sidebar).await;
        // HTTP keeps its reserved connections while websockets are full.
        let liveness =
            http_exchange(addr, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".into()).await;
        server.shutdown().await.expect("stop server");

        assert!(
            passive.iter().all(Option::is_some),
            "passive share admitted"
        );
        assert!(
            !passive_over_share,
            "passive clients stay within their share"
        );
        assert!(
            sidebars.iter().all(Option::is_some),
            "sidebars use the reserve"
        );
        assert!(!sidebar_over_cap, "the websocket cap still holds");
        assert!(
            passive_after_close.is_some(),
            "closed passive slot released"
        );
        assert!(
            sidebar_after_close.is_some(),
            "closed sidebar slot released"
        );
        assert!(liveness.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[tokio::test]
    async fn default_websocket_capacity_admits_more_sidebars_than_the_old_fixed_cap() {
        if raise_fd_soft_limit().is_none_or(|limit| limit < 1_024) {
            eprintln!("skipping: descriptor limit too low for 150 in-process sidebars");
            return;
        }
        let files = startup_test_paths("websocket-default-capacity");
        let (pid_file, token_file) = files.paths();
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(|| "{}".to_string()),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let token = token.trim().to_string();
        let mut sidebars = Vec::new();
        for _ in 0..150 {
            match try_connect_websocket(server.addr(), &token, SIDEBAR_WEBSOCKET_PATH).await {
                Some(websocket) => sidebars.push(websocket),
                None => break,
            }
        }
        let admitted = sidebars.len();
        drop(sidebars);
        server.shutdown().await.expect("stop server");
        assert_eq!(admitted, 150, "a sidebar was refused after {admitted}");
    }

    /// A one-session mux whose sidebar pane listing, part of every full
    /// snapshot, stalls while `slow` is set, like tmux under fork pressure.
    struct StallingSnapshotMux {
        slow: Arc<AtomicBool>,
        stalled_calls: Arc<AtomicUsize>,
    }

    impl MuxProvider for StallingSnapshotMux {
        fn name(&self) -> &str {
            "stalling-snapshot"
        }
        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            vec![opensessions_runtime::mux::MuxSessionInfo {
                name: "work".to_string(),
                created_at: 0,
                dir: String::new(),
                windows: 1,
            }]
        }
        fn list_visible_sidebar_pane_ids(&self) -> Vec<String> {
            if self.slow.load(Ordering::SeqCst) {
                self.stalled_calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(1_500));
            }
            Vec::new()
        }
        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("work".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
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

    async fn http_exchange(addr: SocketAddr, request: String) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("server should answer")
            .expect("read response");
        response
    }

    fn agent_event_request(token: &str, thread: &str, status: &str) -> String {
        let body = format!(
            r#"{{"agent":"amp","status":"{status}","tmuxSession":"work","threadId":"{thread}"}}"#
        );
        format!(
            "POST /api/agent-event HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn agent_event_burst_is_answered_while_a_slow_snapshot_is_in_flight() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-event-burst-{}-{id}", process::id()));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let _files = TestServerFiles {
            pid_file: pid_file.clone(),
            token_file: token_file.clone(),
        };
        let agent_home = root.with_extension("home");
        fs::create_dir_all(&agent_home).expect("agent home");
        let slow = Arc::new(AtomicBool::new(false));
        let stalled_calls = Arc::new(AtomicUsize::new(0));
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(StallingSnapshotMux {
            slow: Arc::clone(&slow),
            stalled_calls: Arc::clone(&stalled_calls),
        })])
        .with_agent_state_home(&agent_home)
        .with_auto_hibernate(AutoHibernateSettings {
            enabled: false,
            idle_after_ms: 0,
        });
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, &pid_file)
                .with_token_file(&token_file)
                .with_state_source(source),
        )
        .await
        .expect("start server");
        let token = fs::read_to_string(&token_file).expect("token");
        let token = token.trim().to_string();
        let addr = server.addr();

        slow.store(true, Ordering::SeqCst);
        let refresh = tokio::spawn(http_exchange(
            addr,
            format!(
                "POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while stalled_calls.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the slow snapshot should start");

        // Ten Amp plugins reloading at once, each with the plugin's 750 ms budget.
        let burst = (0..10)
            .map(|index| {
                let request = agent_event_request(&token, &format!("T-{index}"), "running");
                tokio::spawn(async move {
                    let started = Instant::now();
                    let response = http_exchange(addr, request).await;
                    (started.elapsed(), response)
                })
            })
            .collect::<Vec<_>>();
        let mut slowest = Duration::ZERO;
        for request in burst {
            let (elapsed, response) = request.await.expect("burst request");
            assert!(
                response.starts_with(b"HTTP/1.1 204 No Content"),
                "unexpected response: {}",
                String::from_utf8_lossy(&response)
            );
            slowest = slowest.max(elapsed);
        }
        // One sender's later event must still win over its earlier one.
        let sequential_started = Instant::now();
        for status in ["running", "waiting"] {
            let response = http_exchange(addr, agent_event_request(&token, "T-0", status)).await;
            assert!(response.starts_with(b"HTTP/1.1 204 No Content"));
        }
        let sequential = sequential_started.elapsed();

        let _ = refresh.await;
        slow.store(false, Ordering::SeqCst);
        let refreshed = http_exchange(
            addr,
            format!(
                "POST /refresh HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
            ),
        )
        .await;
        assert!(refreshed.starts_with(b"HTTP/1.1 200 OK"));
        let uri = format!("ws://{addr}").parse().expect("ws uri");
        let (mut websocket, _) = tokio_websockets::ClientBuilder::from_uri(uri)
            .add_header(
                http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().expect("authorization"),
            )
            .expect("add authorization header")
            .connect()
            .await
            .expect("connect websocket");
        let _ = websocket.next().await.expect("hello").expect("hello frame");
        let state = websocket
            .next()
            .await
            .expect("initial state")
            .expect("initial state frame");
        let state: Value = serde_json::from_str(state.as_text().expect("text state")).unwrap();
        let agents = state["sessions"][0]["agents"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        server.shutdown().await.expect("stop server");
        let _ = fs::remove_dir_all(&agent_home);

        assert!(
            slowest < Duration::from_millis(750),
            "agent events waited {slowest:?} behind an unrelated snapshot"
        );
        assert!(
            sequential < Duration::from_millis(750),
            "sequential agent events waited {sequential:?}"
        );
        assert_eq!(agents.len(), 10, "every burst event is applied: {agents:?}");
        let latest = agents
            .iter()
            .find(|agent| agent["threadId"] == "T-0")
            .expect("thread T-0");
        assert_eq!(latest["status"], "waiting");
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

    #[test]
    fn stale_generation_cleanup_waits_for_identity_publication() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "opensessions-identity-handoff-{}-{id}",
            process::id()
        ));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let lock_file = root.with_extension("identity.lock");
        let old_token = "a".repeat(64);
        let new_token = "b".repeat(64);
        publish_identity(&pid_file, &token_file, &old_token).expect("publish old identity");

        let publication_lock = lock_identity(&pid_file).expect("hold publication lock");
        let cleanup_pid = pid_file.clone();
        let cleanup_token = token_file.clone();
        let cleanup_old_token = old_token.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            started_tx.send(()).expect("signal cleanup start");
            cleanup_owned_generation(&cleanup_pid, &cleanup_token, &cleanup_old_token, None)
        });
        started_rx.recv().expect("cleanup started");
        std::thread::sleep(Duration::from_millis(25));
        assert!(
            !cleanup.is_finished(),
            "cleanup must not race identity publication"
        );

        write_private_file(&token_file, &new_token, &new_token[..16])
            .expect("publish replacement token");
        write_private_file(&pid_file, &process::id().to_string(), &new_token[..16])
            .expect("publish replacement pid");
        drop(publication_lock);
        cleanup
            .join()
            .expect("join stale cleanup")
            .expect("stale cleanup succeeds");

        assert_eq!(
            fs::read_to_string(&pid_file).expect("replacement pid"),
            process::id().to_string()
        );
        assert_eq!(
            fs::read_to_string(&token_file).expect("replacement token"),
            new_token
        );
        assert!(
            !repair_identity_if_missing(&pid_file, &token_file, &old_token)
                .expect("preserve replacement identity"),
            "stale generation must not overwrite its replacement"
        );
        assert_eq!(
            fs::read_to_string(&token_file).expect("replacement token remains"),
            new_token
        );
        let _ = fs::remove_file(pid_file);
        let _ = fs::remove_file(token_file);
        let _ = fs::remove_file(lock_file);
    }

    #[tokio::test]
    async fn active_generation_repairs_missing_identity_files() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "opensessions-identity-repair-{}-{id}",
            process::id()
        ));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let lock_file = root.with_extension("identity.lock");
        let server =
            start_server(ServerConfig::new("127.0.0.1", 0, &pid_file).with_token_file(&token_file))
                .await
                .expect("start server");
        let expected_pid = fs::read_to_string(&pid_file).expect("initial pid");
        let expected_token = fs::read_to_string(&token_file).expect("initial token");

        async fn wait_for_identity(
            pid_file: &Path,
            token_file: &Path,
            expected_pid: &str,
            expected_token: &str,
        ) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if fs::read_to_string(pid_file).is_ok_and(|pid| pid == expected_pid)
                        && fs::read_to_string(token_file).is_ok_and(|token| token == expected_token)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("active generation should restore identity files");
        }

        fs::remove_file(&pid_file).expect("remove pid file");
        fs::remove_file(&token_file).expect("remove token file");
        wait_for_identity(&pid_file, &token_file, &expected_pid, &expected_token).await;

        fs::remove_file(&pid_file).expect("remove only pid file");
        wait_for_identity(&pid_file, &token_file, &expected_pid, &expected_token).await;

        fs::remove_file(&token_file).expect("remove only token file");
        wait_for_identity(&pid_file, &token_file, &expected_pid, &expected_token).await;

        let response = request_at(
            server.addr(),
            format!(
                "POST /ensure-sidebars HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\n\r\n",
                fs::read_to_string(&token_file)
                    .expect("re-read repaired token")
                    .trim()
            ),
        )
        .await;
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK"),
            "repaired token should authenticate a newly launched hook"
        );
        server.shutdown().await.expect("stop server");
        let _ = fs::remove_file(lock_file);
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

    /// Two windows, sidebar panes created by `spawn_sidebar`, and a recorded
    /// show/hide preference standing in for the tmux global user option.
    struct SidebarVisibilityTestProvider {
        preference: Mutex<Option<bool>>,
        panes: Mutex<Vec<opensessions_runtime::mux::SidebarPane>>,
        spawned_windows: Mutex<Vec<String>>,
        /// Sidebar pane count seen by each `restore_windows_without_sidebar`.
        restores: Mutex<Vec<usize>>,
    }

    impl SidebarVisibilityTestProvider {
        fn with_preference(preference: Option<bool>) -> Arc<Self> {
            Arc::new(Self {
                preference: Mutex::new(preference),
                panes: Mutex::new(Vec::new()),
                spawned_windows: Mutex::new(Vec::new()),
                restores: Mutex::new(Vec::new()),
            })
        }

        fn spawned_windows(&self) -> Vec<String> {
            self.spawned_windows.lock().unwrap().clone()
        }
    }

    impl MuxProvider for SidebarVisibilityTestProvider {
        fn name(&self) -> &str {
            "sidebar-visibility-test"
        }
        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            Vec::new()
        }
        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            Some("main".to_string())
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
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
        fn sidebar_visibility_preference(&self) -> Option<bool> {
            *self.preference.lock().unwrap()
        }
        fn set_sidebar_visibility_preference(&self, visible: bool) {
            *self.preference.lock().unwrap() = Some(visible);
        }
        fn is_window_capable(&self) -> bool {
            true
        }
        fn is_sidebar_capable(&self) -> bool {
            true
        }
        fn get_current_window_id(&self) -> Option<String> {
            Some("@1".to_string())
        }
        fn list_active_windows(&self) -> Vec<ActiveWindow> {
            ["@1", "@2"]
                .into_iter()
                .map(|id| ActiveWindow {
                    id: id.to_string(),
                    session_name: "main".to_string(),
                    active: id == "@1",
                })
                .collect()
        }
        fn list_sidebar_panes(
            &self,
            _session_name: Option<&str>,
        ) -> Vec<opensessions_runtime::mux::SidebarPane> {
            self.panes.lock().unwrap().clone()
        }
        fn spawn_sidebar(
            &self,
            session_name: &str,
            window_id: &str,
            width: u16,
            _position: SidebarPosition,
            _scripts_dir: &str,
        ) -> Option<String> {
            let mut panes = self.panes.lock().unwrap();
            let pane_id = format!("%{}", panes.len() + 100);
            panes.push(opensessions_runtime::mux::SidebarPane {
                pane_id: pane_id.clone(),
                session_name: session_name.to_string(),
                window_id: window_id.to_string(),
                width: Some(width),
                window_width: Some(120),
            });
            self.spawned_windows
                .lock()
                .unwrap()
                .push(window_id.to_string());
            Some(pane_id)
        }
        fn hide_sidebar(&self, pane_id: &str) {
            self.panes
                .lock()
                .unwrap()
                .retain(|pane| pane.pane_id != pane_id);
        }
        fn kill_sidebar_pane(&self, pane_id: &str) {
            self.hide_sidebar(pane_id);
        }
        fn restore_windows_without_sidebar(&self) {
            let sidebars = self.panes.lock().unwrap().len();
            self.restores.lock().unwrap().push(sidebars);
        }
    }

    const ENSURE_CONTEXT: &str = "/dev/ttys001|main|@1|%1|1";

    #[test]
    fn restarted_server_restores_a_visible_sidebar_in_every_window() {
        let provider = SidebarVisibilityTestProvider::with_preference(Some(true));
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);

        source.setup_mux_hooks("127.0.0.1", 0, "");

        assert!(source.is_sidebar_visible());
        assert_eq!(provider.spawned_windows(), ["@1", "@2"]);
    }

    #[test]
    fn restarted_server_respawns_a_visible_sidebar_on_ensure() {
        let provider = SidebarVisibilityTestProvider::with_preference(Some(true));
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.setup_mux_hooks("127.0.0.1", 0, "");
        provider.panes.lock().unwrap().clear();

        source.handle_http_hook("/ensure-sidebar", ENSURE_CONTEXT);

        assert_eq!(provider.spawned_windows(), ["@1", "@2", "@1"]);
    }

    #[test]
    fn restarted_server_keeps_a_hidden_sidebar_hidden() {
        let provider = SidebarVisibilityTestProvider::with_preference(Some(false));
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);

        source.setup_mux_hooks("127.0.0.1", 0, "");
        source.handle_http_hook("/ensure-sidebar", ENSURE_CONTEXT);
        source.handle_http_hook("/ensure-sidebars", "");

        assert!(!source.is_sidebar_visible());
        assert!(provider.spawned_windows().is_empty());
    }

    #[test]
    fn first_server_start_without_a_recorded_choice_stays_hidden() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);

        source.setup_mux_hooks("127.0.0.1", 0, "");
        source.handle_http_hook("/ensure-sidebar", ENSURE_CONTEXT);

        assert!(!source.is_sidebar_visible());
        assert!(provider.spawned_windows().is_empty());
        assert_eq!(provider.sidebar_visibility_preference(), None);
    }

    #[test]
    fn toggle_records_the_users_visibility_choice() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);

        source.handle_http_hook("/toggle", ENSURE_CONTEXT);
        assert_eq!(provider.sidebar_visibility_preference(), Some(true));

        source.handle_http_hook("/toggle", ENSURE_CONTEXT);
        assert_eq!(provider.sidebar_visibility_preference(), Some(false));
    }

    #[test]
    fn sidebar_connection_on_a_fresh_server_records_a_visible_choice() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        let mut context = ClientConnectionContext::default();

        source.handle_sender_command_with_context(
            &serde_json::json!({
                "type": "identify-pane",
                "paneId": "%1",
                "sessionName": "main",
                "windowId": "@1",
            }),
            &mut context,
        );

        assert!(source.is_sidebar_visible());
        assert_eq!(provider.sidebar_visibility_preference(), Some(true));
    }

    #[test]
    fn server_shutdown_does_not_record_the_sidebar_as_hidden() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.handle_http_hook("/toggle", ENSURE_CONTEXT);

        source.begin_shutdown();
        source.cleanup_mux_hooks();
        source.cleanup_sidebar_clients();

        assert!(provider.panes.lock().unwrap().is_empty());
        assert_eq!(provider.sidebar_visibility_preference(), Some(true));
    }

    #[test]
    fn hiding_the_sidebar_restores_windows_without_a_sidebar() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.handle_http_hook("/toggle", ENSURE_CONTEXT);
        assert!(provider.restores.lock().unwrap().is_empty());

        source.handle_http_hook("/toggle", ENSURE_CONTEXT);

        assert_eq!(*provider.restores.lock().unwrap(), vec![0]);
    }

    #[test]
    fn an_exited_sidebar_pane_restores_its_window() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.handle_http_hook("/toggle", ENSURE_CONTEXT);
        provider.panes.lock().unwrap().pop();

        source.handle_http_hook("/pane-exited", "");

        assert_eq!(*provider.restores.lock().unwrap(), vec![1]);
    }

    /// Lists `alpha` and `beta` until told to fail like a `tmux list-sessions`
    /// that could not run.
    #[derive(Default)]
    struct FlakySessionsTestProvider {
        failing: AtomicBool,
    }

    impl MuxProvider for FlakySessionsTestProvider {
        fn name(&self) -> &str {
            "flaky-sessions-test"
        }
        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            self.try_list_sessions().unwrap_or_default()
        }
        fn try_list_sessions(&self) -> Option<Vec<opensessions_runtime::mux::MuxSessionInfo>> {
            if self.failing.load(Ordering::SeqCst) {
                return None;
            }
            Some(
                ["alpha", "beta"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, name)| opensessions_runtime::mux::MuxSessionInfo {
                        name: name.to_string(),
                        created_at: index as u64,
                        dir: String::new(),
                        windows: 1,
                    })
                    .collect(),
            )
        }
        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            None
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
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

    #[test]
    fn a_failed_session_listing_keeps_metadata_order_and_hidden_sessions() {
        let order_path = std::env::temp_dir().join(format!(
            "opensessions-flaky-order-{}-{}.json",
            process::id(),
            current_time_ms()
        ));
        let provider = Arc::new(FlakySessionsTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()])
            .with_session_order_path(order_path.clone());
        source.snapshot_json();
        source
            .metadata_store
            .lock()
            .unwrap()
            .set_status("alpha", Some(("building".to_string(), None)));
        source.handle_client_command(&serde_json::json!({
            "type": "hide-session",
            "name": "beta",
        }));

        provider.failing.store(true, Ordering::SeqCst);
        source.snapshot_json();
        provider.failing.store(false, Ordering::SeqCst);

        assert!(source.metadata_store.lock().unwrap().get("alpha").is_some());
        assert_eq!(
            source.visible_session_names(),
            Some(vec!["alpha".to_string()])
        );
        let persisted = fs::read_to_string(&order_path).unwrap_or_default();
        let _ = fs::remove_file(&order_path);
        assert!(persisted.contains("beta"), "{persisted}");
    }

    #[test]
    fn a_failed_session_listing_does_not_mean_the_namespace_is_gone() {
        let socket_path = std::env::temp_dir().join(format!(
            "os-flaky-{}-{}.sock",
            process::id(),
            current_time_ms() % 100_000
        ));
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let provider = Arc::new(FlakySessionsTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()])
            .with_tmux_socket_path(socket_path.clone());

        provider.failing.store(true, Ordering::SeqCst);
        let available = source.mux_namespace_available();
        let _ = fs::remove_file(&socket_path);

        assert!(available);
    }

    /// Parks inside `build_read_only_state`'s tmux work until released.
    struct BlockingSnapshotProvider {
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl MuxProvider for BlockingSnapshotProvider {
        fn name(&self) -> &str {
            "blocking-snapshot-test"
        }
        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            Vec::new()
        }
        fn is_batch_capable(&self) -> bool {
            true
        }
        fn get_all_pane_counts(&self) -> HashMap<String, u32> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
                let release = self.release.lock().unwrap().take();
                if let Some(release) = release {
                    let _ = release.recv_timeout(Duration::from_secs(5));
                }
            }
            HashMap::new()
        }
        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            None
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
        }
        fn get_pane_count(&self, _name: &str) -> u32 {
            0
        }
        fn get_client_tty(&self) -> String {
            String::new()
        }
        fn create_session(&self, _name: Option<&str>, _dir: Option<&str>) {}
        fn kill_session(&self, _name: &str) {}
        fn setup_hooks(&self, _server_host: &str, _server_port: u16, _token_file: &str) {}
        fn cleanup_hooks(&self) {}
    }

    #[test]
    fn snapshot_tmux_work_holds_no_shared_state_locks() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let provider = Arc::new(BlockingSnapshotProvider {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        });
        let source = Arc::new(ReadOnlyMuxStateSource::new(vec![provider]));
        let snapshot_source = Arc::clone(&source);
        let snapshot = std::thread::spawn(move || snapshot_source.snapshot_json());
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("snapshot reached tmux work");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let lock_source = Arc::clone(&source);
        let contender = std::thread::spawn(move || {
            lock_source
                .agent_tracker
                .lock()
                .unwrap()
                .prune_stuck(STUCK_RUNNING_TIMEOUT_MS);
            drop(lock_source.settings_revision.lock().unwrap());
            drop(lock_source.sidebar_coordinator.lock().unwrap());
            drop(lock_source.theme.lock().unwrap());
            drop(lock_source.focused_session.lock().unwrap());
            drop(lock_source.collapsed_worktree_groups.lock().unwrap());
            let _ = done_tx.send(());
        });
        let locks_available = done_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        let _ = release_tx.send(());
        snapshot.join().expect("snapshot finished");
        contender.join().expect("contender finished");

        assert!(
            locks_available,
            "shared state stayed locked while the snapshot ran tmux commands"
        );
    }

    #[test]
    fn shutdown_announces_closing_without_running_mux_commands() {
        let provider = Arc::new(FlakySessionsTestProvider::default());
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.snapshot_json();
        provider.failing.store(true, Ordering::SeqCst);

        let payload = source.begin_shutdown().expect("closing state");

        let state: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(state["initLabel"], "closing…");
        // The announcement reuses the last state; a fresh snapshot would
        // have seen the now-failing listing and dropped both sessions.
        assert_eq!(state["sessions"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn a_toggle_queued_behind_shutdown_changes_nothing() {
        let provider = SidebarVisibilityTestProvider::with_preference(None);
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.begin_shutdown();

        source.handle_http_hook("/toggle", ENSURE_CONTEXT);

        assert!(provider.spawned_windows().is_empty());
        assert_eq!(provider.sidebar_visibility_preference(), None);
    }

    /// Parks in `cleanup_mux_hooks` until released.
    struct BlockingCleanupSource {
        entered: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl StateSource for BlockingCleanupSource {
        fn snapshot_json(&self) -> String {
            "{}".to_string()
        }

        fn cleanup_mux_hooks(&self) {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = release.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    #[tokio::test]
    async fn a_successor_cannot_publish_until_shutdown_cleanup_finishes() {
        let id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("opensessions-successor-{}-{id}", process::id()));
        let pid_file = root.with_extension("pid");
        let token_file = root.with_extension("token");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = start_server(
            ServerConfig::new("127.0.0.1", 0, pid_file.clone())
                .with_token_file(token_file.clone())
                .with_state_source(BlockingCleanupSource {
                    entered: Mutex::new(Some(entered_tx)),
                    release: Mutex::new(Some(release_rx)),
                }),
        )
        .await
        .expect("start server");
        // A successor generation starting while cleanup runs publishes its
        // identity first. Everything below runs off the runtime thread, so
        // it also observes a cleanup that blocks that thread.
        let successor_token = "c".repeat(64);
        let successor_pid = pid_file.clone();
        let successor_token_file = token_file.clone();
        let successor_token_value = successor_token.clone();
        let successor = std::thread::spawn(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("shutdown cleanup started");
            let (published_tx, published_rx) = std::sync::mpsc::channel();
            let publisher = std::thread::spawn(move || {
                publish_identity(
                    &successor_pid,
                    &successor_token_file,
                    &successor_token_value,
                )
                .expect("publish successor");
                let _ = published_tx.send(());
            });
            let published_during_cleanup = published_rx
                .recv_timeout(Duration::from_millis(300))
                .is_ok();
            let _ = release_tx.send(());
            publisher.join().expect("successor published");
            published_during_cleanup
        });
        server.shutdown().await.expect("shutdown");
        let published_during_cleanup = successor.join().expect("successor thread");

        let successor_kept = fs::read_to_string(&token_file).ok() == Some(successor_token);
        let _ = fs::remove_file(&pid_file);
        let _ = fs::remove_file(&token_file);
        let _ = fs::remove_file(root.with_extension("identity.lock"));
        assert!(
            !published_during_cleanup,
            "a successor published while the old generation was still cleaning up"
        );
        assert!(
            successor_kept,
            "old generation removed the successor identity"
        );
    }

    #[test]
    fn accept_retries_resource_exhaustion_and_aborted_connections() {
        use std::io::{Error, ErrorKind};
        for retry in [
            Error::from_raw_os_error(24), // EMFILE
            Error::from_raw_os_error(23), // ENFILE
            Error::from(ErrorKind::ConnectionAborted),
            Error::from(ErrorKind::Interrupted),
        ] {
            assert_eq!(
                accept_error_action(&retry),
                AcceptErrorAction::Retry,
                "{retry}"
            );
        }
        for fatal in [
            Error::from_raw_os_error(9),  // EBADF
            Error::from_raw_os_error(22), // EINVAL
        ] {
            assert_eq!(
                accept_error_action(&fatal),
                AcceptErrorAction::Fatal,
                "{fatal}"
            );
        }
    }

    #[test]
    fn git_info_survives_branch_names_containing_dashes() {
        let repo = std::env::temp_dir().join(format!(
            "opensessions-git-dashes-{}-{}",
            process::id(),
            NEXT_SERVER_ID.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            process::Command::new("git")
                .current_dir(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .expect("run git")
        };
        if !git(&["init", "-q", "-b", "fix---races"]).status.success() {
            let _ = fs::remove_dir_all(&repo);
            return;
        }
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "init"]);
        fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();

        let info =
            parse_git_info_output(&SystemGitCommandRunner.git_info_output(&repo.to_string_lossy()));
        let _ = fs::remove_dir_all(&repo);

        assert_eq!(info.branch, "fix---races");
        assert_eq!(info.changed_files, 1);
        assert_eq!((info.insertions, info.deletions), (1, 0));
    }

    #[test]
    fn hook_context_accepts_session_names_containing_pipes() {
        for (body, session, pane_id, active) in [
            ("/dev/ttys001|a|b|@3|%7|1", "a|b", Some("%7"), Some(true)),
            ("|x|0|@3|%7|0", "x|0", Some("%7"), Some(false)),
            ("/dev/ttys001|a|@b|@3|%7", "a|@b", Some("%7"), None),
            ("|pipe|end|@3", "pipe|end", None, None),
            ("/dev/ttys001|main|@3|%1|1", "main", Some("%1"), Some(true)),
        ] {
            let context = parse_context(body).expect(body);
            assert_eq!(context.session, session, "{body}");
            assert_eq!(context.window_id, "@3", "{body}");
            assert_eq!(context.pane_id.as_deref(), pane_id, "{body}");
            assert_eq!(context.pane_active, active, "{body}");
        }

        let provider = SidebarVisibilityTestProvider::with_preference(Some(true));
        let source = ReadOnlyMuxStateSource::new(vec![provider.clone()]);
        source.setup_mux_hooks("127.0.0.1", 0, "");
        provider.panes.lock().unwrap().clear();
        source.handle_http_hook("/ensure-sidebar", "/dev/ttys001|a|b|@7|%1|1");
        assert_eq!(
            provider.spawned_windows().last().map(String::as_str),
            Some("@7")
        );
    }

    /// Lists sessions created within one second, in creation order, the way
    /// tmux reports `#{session_created}` with whole-second resolution.
    struct SameSecondSessionsTestProvider;

    impl MuxProvider for SameSecondSessionsTestProvider {
        fn name(&self) -> &str {
            "same-second-sessions-test"
        }
        fn list_sessions(&self) -> Vec<opensessions_runtime::mux::MuxSessionInfo> {
            ["opensessions", "effect-ts", "lazydiff"]
                .into_iter()
                .map(|name| opensessions_runtime::mux::MuxSessionInfo {
                    name: name.to_string(),
                    created_at: 1_791_401_685,
                    dir: String::new(),
                    windows: 1,
                })
                .collect()
        }
        fn switch_session(&self, _name: &str, _client_tty: Option<&str>) {}
        fn get_current_session(&self) -> Option<String> {
            None
        }
        fn get_session_dir(&self, _name: &str) -> String {
            String::new()
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

    /// Whether sessions created within one tmux second are listed in name
    /// order must not depend on when the clock ticks, so `Tab` from the first
    /// created session always reaches the second created session.
    #[test]
    fn tab_follows_creation_order_for_sessions_created_in_the_same_second() {
        let source = ReadOnlyMuxStateSource::new(vec![Arc::new(SameSecondSessionsTestProvider)]);
        let mut sidebar = app_from_state_json(&source.snapshot_json()).expect("state snapshot");
        sidebar.apply_server_message(
            opensessions_sidebar_core::generated::protocol::ServerMessage::YourSession {
                name: "opensessions".to_string(),
                client_tty: None,
            },
        );

        sidebar.handle_tab(false);

        assert_eq!(
            sidebar.drain_commands(),
            vec![
                opensessions_sidebar_core::generated::protocol::ClientCommand::SwitchSession {
                    name: "effect-ts".to_string(),
                    client_tty: None,
                }
            ]
        );
    }
}
