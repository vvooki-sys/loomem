//! Test-only one-shot HTTP server for chat-completions calls.
//!
//! It listens on `127.0.0.1:0`, accepts one connection, checks the request
//! line and the `Authorization` header, and sends one fixed response.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// Start a server that answers one `POST /v1/chat/completions` with
/// `status` (for example `"200 OK"`) and `body`.
///
/// `expected_auth` is the full `Authorization` value, for example
/// `Some("Bearer fake-test-key")`. `None` asserts that the request has no
/// `Authorization` header. Returns the base URL (`http://127.0.0.1:<port>/v1`)
/// and the server task; await the task so its assertions fail the test.
pub(crate) async fn serve_once(
    status: &str,
    body: &str,
    expected_auth: Option<&str>,
) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let expected_auth = expected_auth.map(|value| format!("authorization: {value}\r\n"));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let head = read_head(&mut socket).await.to_ascii_lowercase();
        assert!(head.starts_with("post /v1/chat/completions "), "{head}");
        match expected_auth {
            Some(line) => assert!(head.contains(&line.to_ascii_lowercase()), "{head}"),
            None => assert!(!head.contains("\r\nauthorization:"), "{head}"),
        }
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    (base_url, task)
}

/// Deadline for one mock exchange. A stuck connection fails the test.
pub(crate) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait for the server task from [`serve_once`], at most [`DEADLINE`].
pub(crate) async fn finish(server: JoinHandle<()>) {
    tokio::time::timeout(DEADLINE, server)
        .await
        .expect("mock server did not finish in time")
        .unwrap();
}

/// Read until the end of the request headers (or 8 KiB).
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut input = Vec::with_capacity(8192);
    let mut chunk = [0; 1024];
    while input.len() < 8192 && !input.windows(4).any(|w| w == b"\r\n\r\n") {
        let size = socket.read(&mut chunk).await.unwrap();
        if size == 0 {
            break;
        }
        input.extend_from_slice(&chunk[..size]);
    }
    String::from_utf8_lossy(&input).into_owned()
}
