use clap::Parser;

use crate::runtime_config::{DEFAULT_SERVER_PORT, resolve_server_port};

const DEFAULT_SERVER_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, Parser)]
#[command(name = "opensessions-sidebar")]
pub struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    pub server_host: String,
    #[arg(long, default_value_t = DEFAULT_SERVER_PORT)]
    pub server_port: u16,
}

impl Args {
    pub fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(itr)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    pub server_host: String,
    pub server_port: u16,
    pub token_file: String,
}

pub fn resolve_endpoint_from_env<F>(env: F) -> ResolvedEndpoint
where
    F: Fn(&str) -> Option<String>,
{
    let server_key = resolve_server_key(&env);
    let explicit_port = env("OPENSESSIONS_PORT");
    let server_port = resolve_server_port(server_key.as_deref(), explicit_port.as_deref());
    let server_host = env("OPENSESSIONS_HOST")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_HOST.to_string());
    let token_file = env("OPENSESSIONS_TOKEN_FILE")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| match server_key {
            Some(key) => format!("/tmp/opensessions.{key}.token"),
            None => "/tmp/opensessions.token".to_string(),
        });

    ResolvedEndpoint {
        server_host,
        server_port,
        token_file,
    }
}

fn resolve_server_key<F>(env: &F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(explicit) = env("OPENSESSIONS_SERVER_KEY")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some(explicit);
    }

    let tmux = env("TMUX")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let socket_path = tmux.split(',').next()?;
    if socket_path.is_empty() {
        return None;
    }
    opensessions_runtime::shared::resolve_server_key(|key| env(key))
}

#[cfg(test)]
mod tests {
    use super::resolve_endpoint_from_env;
    use opensessions_runtime::shared::resolve_server_settings;

    #[test]
    fn sidebar_and_server_resolve_same_minimal_tmux_endpoint() {
        let env = |key: &str| (key == "TMUX").then(|| "/tmp/tmux-1000/review-120,1,2".into());
        let sidebar = resolve_endpoint_from_env(env);
        let server = resolve_server_settings(env);
        assert_eq!(sidebar.server_host, server.host);
        assert_eq!(sidebar.server_port, server.port);
        assert_eq!(sidebar.token_file, server.token_file);
        assert_eq!(server.server_key.as_deref(), Some("d4e887e7c9f4d63f"));
    }
}
