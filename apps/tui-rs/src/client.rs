use anyhow::Result;
use futures_util::{Sink, SinkExt};
use http::Uri;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_websockets::{ClientBuilder, MaybeTlsStream, Message, WebSocketStream};

use crate::generated::protocol::{ClientCommand, ServerMessage};

pub const EXPECTED_PROTOCOL_VERSION: u16 = 1;

pub fn validate_hello(msg: &ServerMessage) -> std::result::Result<(), String> {
    let ServerMessage::Hello(hello) = msg else {
        return Err("expected hello as first server message".to_string());
    };

    if hello.protocol != EXPECTED_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported protocol {}, expected {}",
            hello.protocol, EXPECTED_PROTOCOL_VERSION
        ));
    }

    Ok(())
}

pub fn decode_server_message(payload: &[u8]) -> serde_json::Result<ServerMessage> {
    serde_json::from_slice(payload)
}

pub fn encode_client_command(command: &ClientCommand) -> serde_json::Result<String> {
    serde_json::to_string(command)
}

/// Build the raw HTTP/1.1 request the sidebar fires at `http://host:port/quit`
/// when the user presses 'q'. This keeps quit reliable even if the websocket
/// is closing:
///   `fetch(`http://${SERVER_HOST}:${SERVER_PORT}/quit`, { method: "POST" })`
/// This is fire-and-forget — the server replies, then closes the WS, which
/// tears down the renderer.
pub fn build_quit_http_request(host: &str, port: u16, token: &str) -> String {
    format!(
        "POST /quit HTTP/1.1\r\nHost: {host}:{port}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// Fire-and-forget HTTP POST to `/quit`. Errors are intentionally swallowed:
/// this is a fallback for when the WS Quit frame might be lost while the TUI
/// is tearing down.
pub async fn fire_quit_http(host: &str, port: u16, token: &str) {
    let Ok(mut stream) = TcpStream::connect((host, port)).await else {
        return;
    };
    let _ = stream
        .write_all(build_quit_http_request(host, port, token).as_bytes())
        .await;
    let _ = stream.shutdown().await;
}

/// Upper bound on how long quitting waits for the HTTP fallback to connect
/// and write; it stays well inside the sidebar's 500 ms quit deadline.
pub const QUIT_HTTP_FALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Send `Quit` over the websocket and fire the HTTP `/quit` fallback at the
/// same time. The fallback runs whatever the websocket result is, because a
/// failing send (a closing or broken socket) is exactly when it is needed.
/// Both are awaited before returning, so the caller can exit on a websocket
/// error without cancelling the fallback. Returns the websocket send result.
pub async fn send_quit_with_http_fallback<S>(
    ws: &mut S,
    host: &str,
    port: u16,
    token: &str,
) -> Result<()>
where
    S: Sink<Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let frame = Message::text(encode_client_command(&ClientCommand::Quit)?);
    let (ws_result, _) = tokio::join!(
        ws.send(frame),
        tokio::time::timeout(
            QUIT_HTTP_FALLBACK_TIMEOUT,
            fire_quit_http(host, port, token)
        ),
    );
    ws_result.map_err(anyhow::Error::from)
}

pub async fn connect_ws(
    host: &str,
    port: u16,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let token_file = std::env::var("OPENSESSIONS_TOKEN_FILE").unwrap_or_default();
    let token = std::fs::read_to_string(token_file).unwrap_or_default();
    connect_ws_path_with_token(host, port, "/", token.trim()).await
}

pub async fn connect_ws_path(
    host: &str,
    port: u16,
    path_and_query: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let token_file = std::env::var("OPENSESSIONS_TOKEN_FILE").unwrap_or_default();
    let token = std::fs::read_to_string(token_file).unwrap_or_default();
    connect_ws_path_with_token(host, port, path_and_query, token.trim()).await
}

pub async fn connect_ws_path_with_token(
    host: &str,
    port: u16,
    path_and_query: &str,
    token: &str,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let uri: Uri = format!("ws://{host}:{port}{path_and_query}").parse()?;
    let builder = ClientBuilder::from_uri(uri).add_header(
        http::header::AUTHORIZATION,
        format!("Bearer {token}").parse()?,
    )?;
    let (ws, _) = builder.connect().await?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// A websocket sink whose connection is already closing.
    struct ClosingSink;

    impl Sink<Message> for ClosingSink {
        type Error = std::io::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }

        fn start_send(self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quit_reaches_the_server_over_http_when_the_websocket_send_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = String::new();
            stream.read_to_string(&mut request).await.unwrap();
            request
        });

        let result = send_quit_with_http_fallback(&mut ClosingSink, "127.0.0.1", port, "tok").await;

        assert!(result.is_err(), "the websocket failure is still reported");
        let request = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("the HTTP /quit fallback must reach the server")
            .unwrap();
        assert!(request.starts_with("POST /quit HTTP/1.1"), "{request}");
        assert!(request.contains("Authorization: Bearer tok"), "{request}");
    }
}
