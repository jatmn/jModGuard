//! Tiny process-local HTTP health listener.
//!
//! The bot binary does not enable the `web` feature, so Magic Containers
//! health checks cannot use axum. This listener answers `GET`/`HEAD`
//! `/health` and `/health/` with 200 once the process has bound the port,
//! without cookies, routing, or a database round-trip. `/` is not liveness.
//! The API exposes the same paths on its public bind via axum, and answers
//! probes on that socket until axum takes over (database open, API state
//! primary-version prime, router build). Application routes must not look
//! successful during that window: they get `503`. `/api` uses the JSON
//! `{"error":"not ready"}` shape so the SPA can retry. Browser navigations
//! (`/login`, `/oauth/callback`, …) get HTML that refreshes in place so an
//! OAuth redirect is not replaced by a JSON error page. `/health` is
//! liveness, not database readiness: a pooled remote `SELECT 1` can wait
//! tens of seconds and false-fail a healthy process under load.
//!
//! Bind and accept before opening the database. A closed port during
//! remote first-boot retries is a failed liveness probe.

use anyhow::{Context as _, Result};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

const MAX_HEALTH_CONNECTIONS: usize = 16;
const LIVENESS_BODY: &str = "ok";
/// Same JSON error shape as axum `ApiError`, so the CDN SPA can retry warmup
/// instead of treating a plain-text 503 as a fatal load failure.
const NOT_READY_JSON: &str = r#"{"error":"not ready"}"#;
/// Waiting copy shared with `frontend/index.html` startup-view. `Refresh` /
/// meta-refresh reloads the current URL (including OAuth query strings)
/// without reflecting the request-target into HTML. This is a document
/// retry page, not the styled SPA shell.
const NOT_READY_HTML: &str = concat!(
    "<!DOCTYPE html><html lang=\"en\"><head>",
    "<meta charset=\"utf-8\">",
    "<meta http-equiv=\"refresh\" content=\"1\">",
    "<title>jModGuard</title></head><body>",
    "<p>Waiting for the API to finish starting…</p>",
    "</body></html>"
);

/// Binds `addr` and returns the listener. Call this during startup so a
/// occupied health port fails the process before Discord connects.
pub async fn bind(addr: &str) -> Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health listener on {addr}"))
}

/// Serves `/health` on an already-bound listener until the process exits.
///
/// Concurrency is the same bounded-accept loop as API startup (`serve_while`):
/// a slow probe must not stall the next one, and excess sockets stay in the
/// listen backlog instead of being accepted and dropped.
pub async fn serve_listener(listener: TcpListener) -> Result<()> {
    tracing::info!(
        "health listener on http://{}/health",
        listener.local_addr()?
    );
    serve_while(&listener, std::future::pending::<Result<()>>()).await
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
    let permits = Arc::new(Semaphore::new(MAX_HEALTH_CONNECTIONS));
    loop {
        // Acquire is the wait primitive. `available_permits() > 0` as a
        // `select!` guard never wakes when a handler finishes, so a full
        // set of in-flight probes would freeze accept until `work` ended.
        let permit = tokio::select! {
            result = &mut work => return result,
            permit = Arc::clone(&permits).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return (&mut work).await,
            },
        };
        tokio::select! {
            result = &mut work => return result,
            accept = listener.accept() => match accept {
                Ok((stream, _)) => {
                    tokio::spawn(async move {
                        let _permit = permit;
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

/// Answers one HTTP connection. `GET`/`HEAD` `/health` return `200 ok`
/// (process liveness). `/api` returns JSON `503` so `fetch` can retry.
/// Other `GET`/`HEAD` paths return HTML `503` so a browser sitting on
/// `/login` or `/oauth/callback` retries the same URL. `/` is not
/// liveness: Bunny serves the admin UI from Storage, and the API image
/// has no `/` route after axum takes over.
pub async fn answer_connection(mut stream: TcpStream) -> Result<()> {
    let mut buf = [0u8; 2048];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(0);
    let probe = parse_probe(&buf[..n]);
    let response = probe_response(probe);
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn probe_response(probe: Probe) -> String {
    let (status, content_type, extra, body) = if probe.liveness {
        ("200 OK", "text/plain; charset=utf-8", "", LIVENESS_BODY)
    } else if probe.json_error {
        (
            "503 Service Unavailable",
            "application/json",
            "retry-after: 1\r\n",
            NOT_READY_JSON,
        )
    } else {
        (
            "503 Service Unavailable",
            "text/html; charset=utf-8",
            "retry-after: 1\r\nrefresh: 1\r\n",
            NOT_READY_HTML,
        )
    };
    let body_out = if probe.head { "" } else { body };
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra}content-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{body_out}",
        body.len()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    liveness: bool,
    /// `/api` and non-GET/HEAD methods: machine clients (SPA `fetch`).
    json_error: bool,
    head: bool,
}

fn parse_probe(buf: &[u8]) -> Probe {
    let text = std::str::from_utf8(buf).unwrap_or("");
    let line = text.split(['\r', '\n']).next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let path = request_target_path(target);
    let head = method == "HEAD";
    let get_or_head = matches!(method, "GET" | "HEAD");
    Probe {
        liveness: get_or_head && is_liveness_path(path),
        json_error: is_api_path(path) || !get_or_head,
        head,
    }
}

/// Path of an HTTP/1.1 request-target (origin-form or absolute-form).
fn request_target_path(target: &str) -> &str {
    let target = target.split(['?', '#']).next().unwrap_or("");
    if target.starts_with('/') {
        return target;
    }
    if let Some(scheme) = target.find("://") {
        let after_authority = &target[scheme + 3..];
        return after_authority
            .find('/')
            .map(|i| &after_authority[i..])
            .unwrap_or("/");
    }
    target
}

fn is_liveness_path(path: &str) -> bool {
    path == "/health" || path == "/health/"
}

fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

#[cfg(test)]
mod tests {
    use super::{NOT_READY_HTML, NOT_READY_JSON, Probe, parse_probe, request_target_path, serve};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn connect(addr: std::net::SocketAddr) -> TcpStream {
        let mut last_err = None;
        for _ in 0..50 {
            match TcpStream::connect(addr).await {
                Ok(connected) => return connected,
                Err(error) => {
                    last_err = Some(error);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        panic!("health listener never accepted: {last_err:?}");
    }

    async fn exchange(addr: std::net::SocketAddr, request: &[u8]) -> String {
        let mut stream = connect(addr).await;
        stream.write_all(request).await.unwrap();
        let mut buf = vec![0u8; 512];
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn request_target_path_origin_and_absolute_form() {
        assert_eq!(request_target_path("/health"), "/health");
        assert_eq!(request_target_path("/health?x=1"), "/health");
        assert_eq!(request_target_path("/health/"), "/health/");
        assert_eq!(
            request_target_path("http://127.0.0.1:8080/health"),
            "/health"
        );
        assert_eq!(
            request_target_path("https://probe.example/health/?q=1"),
            "/health/"
        );
        assert_eq!(request_target_path("http://127.0.0.1:8080"), "/");
    }

    #[test]
    fn parse_probe_matches_get_and_head_liveness() {
        assert_eq!(
            parse_probe(b"GET /health HTTP/1.1"),
            Probe {
                liveness: true,
                json_error: false,
                head: false
            }
        );
        assert_eq!(
            parse_probe(b"HEAD /health HTTP/1.1"),
            Probe {
                liveness: true,
                json_error: false,
                head: true
            }
        );
        assert_eq!(
            parse_probe(b"GET http://127.0.0.1:8080/health HTTP/1.1"),
            Probe {
                liveness: true,
                json_error: false,
                head: false
            }
        );
        assert_eq!(
            parse_probe(b"GET / HTTP/1.1"),
            Probe {
                liveness: false,
                json_error: false,
                head: false
            }
        );
        assert_eq!(
            parse_probe(b"GET /api/me HTTP/1.1"),
            Probe {
                liveness: false,
                json_error: true,
                head: false
            }
        );
        assert_eq!(
            parse_probe(b"GET /login HTTP/1.1"),
            Probe {
                liveness: false,
                json_error: false,
                head: false
            }
        );
        assert_eq!(
            parse_probe(b"POST /health HTTP/1.1"),
            Probe {
                liveness: false,
                json_error: true,
                head: false
            }
        );
    }

    #[tokio::test]
    async fn health_listener_returns_ok() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bind = addr.to_string();
        let server = tokio::spawn(async move { serve(&bind).await });
        let text = exchange(addr, b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        assert!(text.contains("\r\n\r\nok"), "unexpected: {text}");
        server.abort();
    }

    #[tokio::test]
    async fn health_listener_resumes_accept_after_saturated_handlers_finish() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bind = addr.to_string();
        let server = tokio::spawn(async move { serve(&bind).await });
        let mut held = Vec::new();
        for _ in 0..super::MAX_HEALTH_CONNECTIONS {
            held.push(connect(addr).await);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Closing the silent peers frees permits. `select!` must wake on
        // that release — not only when `work` completes.
        drop(held);
        let text = tokio::time::timeout(
            Duration::from_millis(500),
            exchange(addr, b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        )
        .await
        .expect("permit release must resume accept");
        assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        server.abort();
    }

    #[tokio::test]
    async fn health_listener_answers_while_another_client_is_slow() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let bind = addr.to_string();
        let server = tokio::spawn(async move { serve(&bind).await });
        // Hold a connection open without sending. Sequential accept would
        // wait the 2s read timeout before the next probe can complete.
        let _slow = connect(addr).await;
        let text = tokio::time::timeout(
            Duration::from_millis(500),
            exchange(addr, b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"),
        )
        .await
        .expect("slow peer must not serialize liveness probes");
        assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        server.abort();
    }

    #[tokio::test]
    async fn serve_while_answers_health_until_work_completes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(addr, b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
            7u8
        };
        let result = super::serve_while(&listener, work).await;
        assert_eq!(result, 7);
    }

    #[tokio::test]
    async fn serve_while_rejects_application_routes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(addr, b"GET /api/me HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            assert!(
                text.starts_with("HTTP/1.1 503 Service Unavailable"),
                "unexpected: {text}"
            );
            assert!(
                text.contains("retry-after: 1"),
                "warmup must advertise Retry-After: {text}"
            );
            assert!(
                text.contains(NOT_READY_JSON),
                "warmup must use the API JSON error shape: {text}"
            );
            assert!(
                text.contains("content-type: application/json"),
                "API warmup must be JSON: {text}"
            );
        };
        super::serve_while(&listener, work).await;
    }

    #[tokio::test]
    async fn serve_while_retries_browser_oauth_navigations_as_html() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            for request in [
                &b"GET /login HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"[..],
                &b"GET /oauth/callback?code=x&state=y HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"[..],
            ] {
                let text = exchange(addr, request).await;
                assert!(
                    text.starts_with("HTTP/1.1 503 Service Unavailable"),
                    "unexpected: {text}"
                );
                assert!(
                    text.contains("retry-after: 1"),
                    "warmup must advertise Retry-After: {text}"
                );
                assert!(
                    text.contains("refresh: 1"),
                    "browser warmup must refresh in place: {text}"
                );
                assert!(
                    text.contains("content-type: text/html; charset=utf-8"),
                    "browser warmup must be HTML: {text}"
                );
                assert!(
                    text.contains(NOT_READY_HTML),
                    "browser warmup must reload without reflecting the target: {text}"
                );
                assert!(
                    !text.contains("code=x"),
                    "must not echo OAuth query into HTML: {text}"
                );
            }
        };
        super::serve_while(&listener, work).await;
    }

    #[tokio::test]
    async fn serve_while_rejects_root_as_not_ready() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(addr, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            assert!(
                text.starts_with("HTTP/1.1 503 Service Unavailable"),
                "unexpected: {text}"
            );
            assert!(
                text.contains("content-type: text/html; charset=utf-8"),
                "document requests must not get JSON: {text}"
            );
        };
        super::serve_while(&listener, work).await;
    }

    #[tokio::test]
    async fn health_trailing_slash_is_liveness() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(addr, b"GET /health/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        };
        super::serve_while(&listener, work).await;
    }

    #[tokio::test]
    async fn health_head_has_no_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(addr, b"HEAD /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
            assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
            assert!(
                text.contains("content-length: 2"),
                "HEAD must advertise GET body length: {text}"
            );
            assert!(
                text.ends_with("\r\n\r\n"),
                "HEAD must omit the body: {text:?}"
            );
        };
        super::serve_while(&listener, work).await;
    }

    #[tokio::test]
    async fn health_absolute_form_is_liveness() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = async {
            let text = exchange(
                addr,
                b"GET http://127.0.0.1/health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await;
            assert!(text.starts_with("HTTP/1.1 200 OK"), "unexpected: {text}");
        };
        super::serve_while(&listener, work).await;
    }
}
