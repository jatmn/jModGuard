//! Tiny process-local HTTP health listener.
//!
//! The bot binary does not enable the `web` feature, so Magic Containers
//! health checks cannot use axum. This listener answers `GET /health` (and
//! `GET /`) with 200 once the process has bound the port, without cookies,
//! routing, or a database round-trip. The API exposes the same path on its
//! public bind via axum, and answers probes on that socket while the
//! database is still opening. `/health` is liveness, not database
//! readiness: a pooled remote `SELECT 1` can wait tens of seconds and
//! false-fail a healthy process under load.
//!
//! Bind and accept before opening the database. A closed port during
//! remote first-boot retries is a failed liveness probe.

use anyhow::{Context as _, Result};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Binds `addr` and returns the listener. Call this during startup so a
/// occupied health port fails the process before Discord connects.
pub async fn bind(addr: &str) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health listener on {addr}"))
}

/// Serves `/health` on an already-bound listener until the process exits.
pub async fn serve_listener(listener: TcpListener) -> Result<()> {
    tracing::info!(
        "health listener on http://{}/health",
        listener.local_addr()?
    );
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // Handle one probe at a time. Health is tiny; unbounded
                // spawn would let a connection flood create tasks.
                if let Err(error) = answer_connection(stream).await {
                    tracing::debug!("health connection ended: {error:#}");
                }
            }
            Err(error) => tracing::warn!("health accept failed: {error}"),
        }
    }
}

/// Binds `addr` and serves `/health` until the process exits.
pub async fn serve(addr: &str) -> Result<()> {
    serve_listener(bind(addr).await?).await
}

/// Accepts liveness probes on `listener` until `work` completes, then
/// returns that result so the same socket can be handed to axum.
pub async fn serve_while<F, T>(listener: &TcpListener, work: F) -> T
where
    F: Future<Output = T>,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result,
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        if let Err(error) = answer_connection(stream).await {
                            tracing::debug!("health connection ended: {error:#}");
                        }
                    });
                }
                Err(error) => tracing::warn!("health accept failed: {error}"),
            },
        }
    }
}

/// Answers one probe with `200 ok`. Used by the bot listener and by the
/// API while it is still opening the database on the same bind.
pub async fn answer_connection(mut stream: TcpStream) -> Result<()> {
    let mut buf = [0u8; 2048];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    let body = "ok";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::serve;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn health_listener_returns_ok() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bind = addr.to_string();
        let server = tokio::spawn(async move { serve(&bind).await });
        let mut last_err = None;
        let mut stream = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => {
                    last_err = Some(error);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        let mut stream =
            stream.unwrap_or_else(|| panic!("health listener never accepted: {last_err:?}"));
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        assert!(text.contains("\r\n\r\nok"), "unexpected: {text}");

        server.abort();
    }

    #[tokio::test]
    async fn serve_while_answers_health_until_work_completes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let mut stream = None;
            let mut last_err = None;
            for _ in 0..50 {
                match TcpStream::connect(addr).await {
                    Ok(connected) => {
                        stream = Some(connected);
                        break;
                    }
                    Err(error) => {
                        last_err = Some(error);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            let mut stream =
                stream.unwrap_or_else(|| panic!("health listener never accepted: {last_err:?}"));
            stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut buf = vec![0u8; 512];
            let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
            7u8
        };
        let result = super::serve_while(&listener, work).await;
        assert_eq!(result, 7);
    }
}
