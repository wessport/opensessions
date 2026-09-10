pub use opensessions_runtime::shared::{DEFAULT_SERVER_PORT, hash_server_key};

pub fn resolve_server_port(server_key: Option<&str>, explicit: Option<&str>) -> u16 {
    if let Some(port) = explicit
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port > 0)
    {
        return port;
    }

    match server_key {
        Some(key) => {
            opensessions_runtime::shared::resolve_server_port_with_base(Some(key), None, 22_000)
        }
        None => DEFAULT_SERVER_PORT,
    }
}

#[cfg(test)]
mod tests {
    use super::hash_server_key;

    #[test]
    fn server_key_hashes_utf8_bytes() {
        assert_eq!(
            hash_server_key("/private/tmp/tmux-501/default"),
            "1b08f661f4b07fa9"
        );
    }
}
