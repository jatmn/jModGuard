//! Tiny process-local HTTP health listener.
//!
//! The bot binary does not enable the `web` feature, so Magic Containers
//! health checks cannot use axum. This listener answers `GET /health` (and
//! `GET /`) with 200 after the database is reachable, without cookies or
//! routing. The API exposes the same path on its public bind via axum.

use anyhow::{Context as _, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::db::Db;

/// Binds `addr` and serves `/health` until the process exits.
pub async fn serve(addr: &str, db: Db) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health listener on {addr}"))?;
    tracing::info!("health listener on http://{addr}/health");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let db = db.clone();
                tokio::spawn(async move {
                    if let Err(error) = answer(stream, db).await {
                        tracing::debug!("health connection ended: {error:#}");
                    }
                });
            }
            Err(error) => tracing::warn!("health accept failed: {error}"),
        }
    }
}

async fn answer(mut stream: TcpStream, db: Db) -> Result<()> {
    let mut buf = [0u8; 2048];
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await;
    let ok = db.healthcheck().await.is_ok();
    let (status, body) = if ok {
        ("200 OK", "ok")
    } else {
        ("503 Service Unavailable", "database unavailable")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::db::Db;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[tokio::test]
    async fn health_listener_returns_ok_for_open_database() {
        let tmp = std::env::temp_dir().join(format!(
            "jmodguard_health_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = Db::open(&tmp, 2).expect("temp db");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bind = addr.to_string();
        let server_db = db.clone();
        let server = tokio::spawn(async move { serve(&bind, server_db).await });
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
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(format!("{}-wal", tmp.display()));
        let _ = std::fs::remove_file(format!("{}-shm", tmp.display()));
    }
}
