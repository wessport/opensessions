use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const DEFAULT_SERVER_PORT: u16 = 7_391;
pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    pub server_key: Option<String>,
    pub host: String,
    pub port: u16,
    pub pid_file: String,
    pub token_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSocketPath(PathBuf);

impl TmuxSocketPath {
    pub fn from_tmux_env(tmux: &str) -> Option<Self> {
        let socket = tmux.trim().split(',').next()?.trim();
        (!socket.is_empty()).then(|| Self(PathBuf::from(socket)))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn server_key(&self) -> ServerKey {
        let canonical = std::fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
        ServerKey(hash_server_key(&canonical.to_string_lossy()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerKey(pub String);

impl ServerKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpensessionsEndpoint {
    pub server_key: Option<ServerKey>,
    pub host: String,
    pub port: u16,
    pub pid_file: PathBuf,
    pub token_file: PathBuf,
}

impl OpensessionsEndpoint {
    pub fn from_env(env: impl Fn(&str) -> Option<String>) -> Self {
        let socket = env("TMUX").and_then(|tmux| TmuxSocketPath::from_tmux_env(&tmux));
        let explicit_key = env("OPENSESSIONS_SERVER_KEY")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(ServerKey);
        let server_key = explicit_key.or_else(|| socket.as_ref().map(TmuxSocketPath::server_key));
        let host = resolve_server_host(env("OPENSESSIONS_HOST").as_deref());
        let server_key_string = server_key.as_ref().map(|key| key.0.clone());
        let port = resolve_server_port_with_base(
            server_key_string.as_deref(),
            env("OPENSESSIONS_PORT").as_deref(),
            22_000,
        );
        let pid_file = PathBuf::from(resolve_pid_file(
            server_key_string.as_deref(),
            env("OPENSESSIONS_PID_FILE").as_deref(),
        ));
        let token_file = PathBuf::from(resolve_token_file(
            server_key_string.as_deref(),
            env("OPENSESSIONS_TOKEN_FILE").as_deref(),
        ));
        Self {
            server_key,
            host,
            port,
            pid_file,
            token_file,
        }
    }
}

pub fn hash_server_key(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn resolve_server_key(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(explicit) = env("OPENSESSIONS_SERVER_KEY")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(explicit);
    }

    let tmux = env("TMUX")?;
    let socket_path = tmux.trim().split(',').next()?.trim();
    if socket_path.is_empty() {
        return None;
    }

    let canonical =
        std::fs::canonicalize(socket_path).unwrap_or_else(|_| PathBuf::from(socket_path));
    Some(hash_server_key(&canonical.to_string_lossy()))
}

pub fn resolve_server_port(server_key: Option<&str>, explicit: Option<&str>) -> u16 {
    resolve_server_port_with_base(server_key, explicit, 17_000)
}

/// Compute the port like [`resolve_server_port`] but with a configurable base.
/// Mirrors the `PORT_BASE` branch in
/// `integrations/tmux-plugin/scripts/server-common.sh` so the Rust server can
/// pin 22000+server_key when `OPENSESSIONS_RUST=1` and coexist with the TS
/// legacy server (17000+server_key) on the same tmux socket.
pub fn resolve_server_port_with_base(
    server_key: Option<&str>,
    explicit: Option<&str>,
    base: u32,
) -> u16 {
    if let Some(port) = explicit
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
    {
        return port;
    }

    let Some(server_key) = server_key else {
        return DEFAULT_SERVER_PORT;
    };

    match server_port_offset(server_key) {
        Some(offset) => (base + offset) as u16,
        None => DEFAULT_SERVER_PORT,
    }
}

/// Map a server key to a port offset in `0..20_000`, or `None` for an empty key.
///
/// The rule is shared by `integrations/tmux-plugin/scripts/server-common.sh`
/// (`server_port_offset`) and the Amp and Pi integrations (`portForServerKey`);
/// keep all four implementations identical. After trimming whitespace:
///
/// 1. 1–15 ASCII digits: legacy numeric key, decimal value.
/// 2. Only hexadecimal digits: socket-derived SHA key, first 8 hex digits.
///    Canonical keys are always 16 hex characters, so length—not
///    contents—distinguishes them from legacy numeric keys.
/// 3. Anything else: first 8 hex digits of `SHA-256(key)`, so arbitrary
///    explicit keys such as `work` still get a stable per-key port.
pub fn server_port_offset(server_key: &str) -> Option<u32> {
    let key = server_key.trim();
    if key.is_empty() {
        return None;
    }
    if key.len() < 16 && key.bytes().all(|byte| byte.is_ascii_digit()) {
        let value = key.parse::<u64>().ok()?;
        return Some((value % 20_000) as u32);
    }
    let hashed;
    let hex = if key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        key
    } else {
        hashed = hash_server_key(key);
        &hashed
    };
    let prefix = &hex[..hex.len().min(8)];
    u32::from_str_radix(prefix, 16)
        .ok()
        .map(|value| value % 20_000)
}

pub fn resolve_server_host(explicit: Option<&str>) -> String {
    explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SERVER_HOST)
        .to_string()
}

pub fn resolve_pid_file(server_key: Option<&str>, explicit: Option<&str>) -> String {
    if let Some(path) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return path.to_string();
    }

    match server_key {
        Some(key) => format!("/tmp/opensessions.{key}.pid"),
        None => "/tmp/opensessions.pid".to_string(),
    }
}

pub fn resolve_token_file(server_key: Option<&str>, explicit: Option<&str>) -> String {
    if let Some(path) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return path.to_string();
    }
    match server_key {
        Some(key) => format!("/tmp/opensessions.{key}.token"),
        None => "/tmp/opensessions.token".to_string(),
    }
}

pub fn resolve_server_settings(env: impl Fn(&str) -> Option<String>) -> ServerSettings {
    let endpoint = OpensessionsEndpoint::from_env(env);

    ServerSettings {
        server_key: endpoint.server_key.map(|key| key.0),
        host: endpoint.host,
        port: endpoint.port,
        pid_file: endpoint.pid_file.to_string_lossy().to_string(),
        token_file: endpoint.token_file.to_string_lossy().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{OpensessionsEndpoint, hash_server_key, resolve_server_port_with_base};

    #[test]
    fn server_key_hashes_utf8_bytes() {
        assert_eq!(
            hash_server_key("/private/tmp/tmux-501/default"),
            "1b08f661f4b07fa9"
        );
        assert_ne!(
            hash_server_key("/tmp/tmux-1000/review-120"),
            hash_server_key("/tmp/tmux-1000/review-201")
        );
    }

    #[test]
    fn derived_port_accepts_new_and_legacy_explicit_keys() {
        assert_ne!(
            resolve_server_port_with_base(
                Some(&hash_server_key("/tmp/tmux-1000/review-120")),
                None,
                22_000
            ),
            resolve_server_port_with_base(
                Some(&hash_server_key("/tmp/tmux-1000/review-201")),
                None,
                22_000
            )
        );
        assert_eq!(
            resolve_server_port_with_base(Some("123"), None, 22_000),
            22_123
        );
        assert_eq!(
            resolve_server_port_with_base(Some("12345678abcdef00"), None, 22_000),
            41_896
        );
    }

    /// Shared vectors; keep in sync with scripts/server-key.test.ts, which
    /// checks the shell, Amp, and Pi implementations against the same table.
    #[test]
    fn explicit_keys_map_to_the_same_ports_as_shell_and_typescript() {
        for (key, port) in [
            ("123", 22_123),
            ("00123", 22_123),
            ("  123\n", 22_123),
            ("123456", 25_456),
            ("999999999999999", 41_999),
            ("1234567890123456", 41_896),
            ("12345678abcdef00", 41_896),
            ("DEADBEEF", 30_559),
            ("work", 23_687),
            ("deadbeefzz", 29_937),
        ] {
            assert_eq!(
                resolve_server_port_with_base(Some(key), None, 22_000),
                port,
                "key {key:?}"
            );
        }
    }

    #[test]
    fn empty_key_and_explicit_port_keep_their_precedence() {
        assert_eq!(
            resolve_server_port_with_base(Some("  "), None, 22_000),
            super::DEFAULT_SERVER_PORT
        );
        assert_eq!(
            resolve_server_port_with_base(Some("work"), Some(" 4242 "), 22_000),
            4242
        );
    }

    #[test]
    fn endpoint_trims_explicit_key_for_port_and_files() {
        let endpoint = OpensessionsEndpoint::from_env(|name| match name {
            "OPENSESSIONS_SERVER_KEY" => Some(" work ".to_string()),
            _ => None,
        });
        assert_eq!(
            endpoint.server_key.as_ref().map(|key| key.as_str()),
            Some("work")
        );
        assert_eq!(endpoint.port, 23_687);
        assert_eq!(
            endpoint.token_file,
            std::path::PathBuf::from("/tmp/opensessions.work.token")
        );
    }
}
