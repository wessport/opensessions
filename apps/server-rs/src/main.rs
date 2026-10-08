use opensessions_runtime::shared::resolve_server_settings;
use opensessions_server::{
    ServerConfig, default_state_source_from_env, max_connections_from_env, raise_fd_soft_limit,
    start_server,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Each visited window keeps a sidebar connection; lift the soft
    // descriptor limit (256 by default on macOS) before deriving the
    // connection caps from it.
    raise_fd_soft_limit();
    let settings = resolve_server_settings(|key| std::env::var(key).ok());
    let mut config = ServerConfig::new(settings.host, settings.port, settings.pid_file)
        .with_token_file(settings.token_file);
    if let Some(max_connections) = max_connections_from_env(|key| std::env::var(key).ok()) {
        config = config.with_max_connections(max_connections);
    }
    if let Some(server_key) = settings.server_key {
        config = config.with_server_identity(server_key);
    }
    if let Some(source) = default_state_source_from_env(|key| std::env::var(key).ok()) {
        config = config.with_state_source(source);
    }
    let server = start_server(config).await?;
    let shutdown = server.shutdown_sender();
    let mut wait_shutdown = Box::pin(server.wait_shutdown());
    tokio::select! {
        result = &mut wait_shutdown => result?,
        signal = shutdown_signal() => {
            signal?;
            let _ = shutdown.send(());
            wait_shutdown.await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
