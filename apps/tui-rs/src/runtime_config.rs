pub use opensessions_runtime::shared::{DEFAULT_SERVER_PORT, hash_server_key};

/// Resolve the server port exactly like the server does (trimmed explicit
/// `OPENSESSIONS_PORT`, else derived from the server key, else the default).
pub fn resolve_server_port(server_key: Option<&str>, explicit: Option<&str>) -> u16 {
    opensessions_runtime::shared::resolve_server_port_with_base(server_key, explicit, 22_000)
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
