//! Authenticated container-use readings on a dedicated loopback TCP listener
//! (`JMODGUARD_READ_BIND`). The route is not mounted on the public WebUI router.

use crate::cgroup::ContainerSample;
use crate::control::{
    CONTROL_REQUEST_LIFETIME_MS, NonceWindow, SKEW_MS, load_writer_key, now_ms, parse_private_bind,
};
use anyhow::{Context as _, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub const CONTAINER_READ_PATH: &str = "/private/container-use";
pub const CONTAINER_READ_DIRECTION: &str = "api-container-read";
const MAC_HEX: usize = 64;
const BODY_LIMIT: usize = 4096;
/// Shared budget for the bot's container-use fetch during `!stats`.
///
/// Connect, write, and read share this one deadline. The caller's join timeout
/// uses the same value. A miss, or a response larger than `RESPONSE_LIMIT`,
/// returns no reading and `!stats` shows API container use as unavailable.
pub const STATS_FETCH_TIMEOUT: Duration = Duration::from_millis(500);
/// Bytes of the HTTP response, headers included. A real reading is a small JSON
/// document; anything larger is not a container-use response.
const RESPONSE_LIMIT: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_READ_BIND: &str = "127.0.0.1:8083";
/// In-flight private reads. Each one holds a blocking thread until its socket
/// timeout, so a local flood must not be able to grow that set without bound.
const MAX_CONTAINER_READ_CONNECTIONS: usize = 4;

#[derive(Clone)]
pub struct ReadLink {
    addr: SocketAddr,
    key: Vec<u8>,
}

impl std::fmt::Debug for ReadLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadLink")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadWire {
    direction: String,
    route: String,
    nonce: String,
    expires_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerReadResponse {
    pub sample: ContainerSample,
    /// Identifies which API copy answered; not an aggregate across replicas.
    pub copy: String,
}

/// Loopback-only bind for the API container-read listener.
pub fn parse_loopback_bind(raw: &str) -> Result<SocketAddr> {
    let addr = parse_private_bind(raw)?;
    if !addr.ip().is_loopback() {
        anyhow::bail!("read bind is not loopback");
    }
    Ok(addr)
}

pub fn default_read_bind() -> &'static str {
    DEFAULT_READ_BIND
}

pub fn try_read_link(target: &str, secret: &str) -> Result<Option<ReadLink>> {
    if target.trim().is_empty() {
        return Ok(None);
    }
    let key = match load_writer_key(secret) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    let addr = parse_loopback_bind(target.trim())?;
    Ok(Some(ReadLink { addr, key }))
}

impl ReadLink {
    /// Blocking fetch for use from `spawn_blocking` during `!stats` only.
    pub fn fetch_container_use_blocking(&self) -> Option<ContainerReadResponse> {
        fetch_blocking(self.addr, &self.key)
    }
}

/// Start the API container-read listener. Refused or invalid binds disable the
/// link without affecting the public web listener.
pub fn spawn_container_read_listener(bind: &str, secret: &str) -> Option<SocketAddr> {
    let key = match load_writer_key(secret) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!("{error}");
            return None;
        }
    };
    let addr = match parse_loopback_bind(bind) {
        Ok(addr) => addr,
        Err(error) => {
            tracing::error!("container read listener disabled: {error:#}");
            return None;
        }
    };
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("container read listener disabled: {error}");
            return None;
        }
    };
    let bound = match listener.local_addr() {
        Ok(bound) => bound,
        Err(error) => {
            tracing::error!("container read listener disabled: {error}");
            return None;
        }
    };
    listener
        .set_nonblocking(true)
        .map_err(|error| tracing::error!("container read listener disabled: {error}"))
        .ok()?;
    tracing::info!(
        "container read listener on http://{bound}{path}",
        path = CONTAINER_READ_PATH
    );
    let permits = Arc::new(Semaphore::new(MAX_CONTAINER_READ_CONNECTIONS));
    tokio::spawn(async move {
        let listener =
            tokio::net::TcpListener::from_std(listener).expect("container read listener transport");
        let key = key;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let Ok(permit) = permits.clone().try_acquire_owned() else {
                        tracing::debug!("container read connection dropped: concurrency limit");
                        drop(stream);
                        continue;
                    };
                    let key = key.clone();
                    tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        let stream = match stream.into_std() {
                            Ok(stream) => stream,
                            Err(error) => {
                                tracing::debug!("container read transport: {error}");
                                return;
                            }
                        };
                        if let Err(error) = serve_read_connection(stream, &key) {
                            tracing::debug!("container read connection ended: {error:#}");
                        }
                    });
                }
                Err(error) => tracing::warn!("container read accept failed: {error}"),
            }
        }
    });
    Some(bound)
}

const HEADER_READ_LIMIT: usize = 8192;

fn serve_read_connection(mut stream: std::net::TcpStream, key: &[u8]) -> Result<()> {
    // Tokio's into_std retains nonblocking mode; synchronous I/O needs blocking timeouts.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::new();
    let mut scratch = [0u8; 512];
    let head_end = loop {
        if request.len() >= HEADER_READ_LIMIT {
            write_http_response(&mut stream, 400, "text/plain", b"bad request")?;
            return Ok(());
        }
        let n = stream.read(&mut scratch)?;
        if n == 0 {
            return Ok(());
        }
        request.extend_from_slice(&scratch[..n]);
        if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    if !request.starts_with(b"POST ") {
        write_http_response(&mut stream, 405, "text/plain", b"method not allowed")?;
        return Ok(());
    }
    let head = std::str::from_utf8(&request[..head_end]).unwrap_or("");
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));
    if path != Some(CONTAINER_READ_PATH) {
        write_http_response(&mut stream, 404, "text/plain", b"not found")?;
        return Ok(());
    }
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    if content_length > BODY_LIMIT {
        write_http_response(&mut stream, 413, "text/plain", b"payload too large")?;
        return Ok(());
    }
    let body_start = head_end;
    while request.len() < body_start + content_length {
        if request.len() - body_start > BODY_LIMIT {
            write_http_response(&mut stream, 413, "text/plain", b"payload too large")?;
            return Ok(());
        }
        let n = stream.read(&mut scratch)?;
        if n == 0 {
            write_http_response(&mut stream, 400, "text/plain", b"bad request")?;
            return Ok(());
        }
        request.extend_from_slice(&scratch[..n]);
    }
    let body = &request[body_start..body_start + content_length];
    let nonces = nonce_window();
    if open_read(key, body, nonces).is_err() {
        write_http_response(&mut stream, 401, "text/plain", b"unauthorized")?;
        return Ok(());
    }
    if !crate::process_ready() {
        write_http_response(&mut stream, 503, "text/plain", b"not ready")?;
        return Ok(());
    }
    let json = serde_json::to_vec(&container_read_response()).context("read failed")?;
    write_http_response(&mut stream, 200, "application/json", &json)?;
    Ok(())
}

fn write_http_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {text}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\ncache-control: no-store\r\n\r\n",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn nonce_window() -> &'static Mutex<NonceWindow> {
    static WINDOW: OnceLock<Mutex<NonceWindow>> = OnceLock::new();
    WINDOW.get_or_init(|| Mutex::new(NonceWindow::new()))
}

pub fn fetch_blocking(addr: SocketAddr, key: &[u8]) -> Option<ContainerReadResponse> {
    let deadline = Instant::now() + STATS_FETCH_TIMEOUT;
    let body = match seal_read(key) {
        Ok(body) => body,
        Err(_) => return None,
    };
    let host = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
        path = CONTAINER_READ_PATH,
        host = host,
        port = addr.port(),
        len = body.len(),
    );
    let connect_for = time_left(deadline)?.min(CONNECT_TIMEOUT);
    let mut stream = TcpStream::connect_timeout(&addr, connect_for).ok()?;
    write_within_deadline(&mut stream, request.as_bytes(), deadline)?;
    write_within_deadline(&mut stream, &body, deadline)?;
    let response = read_capped_response(&mut stream, deadline, RESPONSE_LIMIT)?;
    let text = String::from_utf8_lossy(&response);
    let (_, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
    if !text.contains("HTTP/1.1 200") {
        return None;
    }
    serde_json::from_str(body.trim()).ok()
}

fn time_left(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() { None } else { Some(left) }
}

fn write_within_deadline(stream: &mut TcpStream, bytes: &[u8], deadline: Instant) -> Option<()> {
    stream.set_write_timeout(Some(time_left(deadline)?)).ok()?;
    stream.write_all(bytes).ok()
}

/// Read until EOF, the overall deadline, or one byte past `limit`.
///
/// The cap is checked before those bytes are stored. A peer that never closes
/// cannot hold this call open past `deadline`.
fn read_capped_response(
    stream: &mut TcpStream,
    deadline: Instant,
    limit: usize,
) -> Option<Vec<u8>> {
    let mut response = Vec::new();
    let mut scratch = [0u8; 512];
    loop {
        stream.set_read_timeout(Some(time_left(deadline)?)).ok()?;
        match stream.read(&mut scratch) {
            Ok(0) => return Some(response),
            Ok(n) => {
                if n > limit.saturating_sub(response.len()) {
                    return None;
                }
                response.extend_from_slice(&scratch[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}

pub fn seal_read(key: &[u8]) -> Result<Vec<u8>> {
    let wire = ReadWire {
        direction: CONTAINER_READ_DIRECTION.to_owned(),
        route: CONTAINER_READ_PATH.to_owned(),
        nonce: random_nonce()?,
        expires_ms: now_ms().saturating_add(CONTROL_REQUEST_LIFETIME_MS),
    };
    let json = serde_json::to_vec(&wire).context("read failed")?;
    anyhow::ensure!(json.len() + MAC_HEX <= BODY_LIMIT, "read failed");
    let mut frame = Vec::with_capacity(MAC_HEX + json.len());
    frame.extend_from_slice(mac_hex(key, &json).as_bytes());
    frame.extend_from_slice(&json);
    Ok(frame)
}

pub fn open_read(key: &[u8], body: &[u8], nonces: &Mutex<NonceWindow>) -> Result<()> {
    if body.len() < MAC_HEX || body.len() > BODY_LIMIT {
        anyhow::bail!("read failed");
    }
    let (mac, json) = body.split_at(MAC_HEX);
    let mac = std::str::from_utf8(mac).map_err(|_| anyhow::anyhow!("read failed"))?;
    if !verify_mac(key, mac, json) {
        anyhow::bail!("read failed");
    }
    let wire: ReadWire =
        serde_json::from_slice(json).map_err(|_| anyhow::anyhow!("read failed"))?;
    if wire.direction != CONTAINER_READ_DIRECTION {
        anyhow::bail!("read failed: direction binding");
    }
    if wire.route != CONTAINER_READ_PATH {
        anyhow::bail!("read failed: route binding");
    }
    let now = now_ms();
    if wire.expires_ms < now.saturating_sub(SKEW_MS) {
        anyhow::bail!("read failed");
    }
    if wire.nonce.len() != 32 || !wire.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("read failed");
    }
    let mut guard = nonces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.accept(&wire.nonce, wire.expires_ms, now) {
        anyhow::bail!("read failed");
    }
    Ok(())
}

pub fn container_read_response() -> ContainerReadResponse {
    ContainerReadResponse {
        sample: crate::cgroup::latest_container_sample(),
        copy: format!("api pid {}", std::process::id()),
    }
}

fn random_nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("read failed"))?;
    Ok(to_hex(&bytes))
}

fn mac_hex(key: &[u8], payload: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    to_hex(hmac::sign(&key, payload).as_ref())
}

fn verify_mac(key: &[u8], mac_hex: &str, payload: &[u8]) -> bool {
    let Ok(mac) = from_hex(mac_hex) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&key, payload, &mac).is_ok()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn from_hex(text: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(text.len().is_multiple_of(2), "read failed");
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_nibble(bytes[index])?;
        let lo = hex_nibble(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => anyhow::bail!("read failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CONTROL_LINK_DIRECTION, CONTROL_LINK_ROUTE, open_frame, seal};
    use std::sync::Mutex;

    #[test]
    fn sealed_read_rejects_a_replayed_body() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let body = seal_read(&key).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        open_read(&key, &body, &nonces).unwrap();
        assert!(open_read(&key, &body, &nonces).is_err());
    }

    fn signed_read_body(key: &[u8], wire: &ReadWire) -> Vec<u8> {
        let json = serde_json::to_vec(wire).unwrap();
        let mut body = mac_hex(key, &json).into_bytes();
        body.extend_from_slice(&json);
        body
    }

    fn resign_payload(key: &[u8], json: &[u8]) -> Vec<u8> {
        let mut body = mac_hex(key, json).into_bytes();
        body.extend_from_slice(json);
        body
    }

    #[test]
    fn container_read_rejects_a_control_bound_signature() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        let read_body = seal_read(&key).unwrap();
        open_read(&key, &read_body, &nonces).expect("read route accepts its own frame");

        let cross = ReadWire {
            direction: CONTROL_LINK_DIRECTION.to_owned(),
            route: CONTROL_LINK_ROUTE.to_owned(),
            nonce: random_nonce().unwrap(),
            expires_ms: now_ms().saturating_add(CONTROL_REQUEST_LIFETIME_MS),
        };
        let cross_body = signed_read_body(&key, &cross);
        let err = open_read(&key, &cross_body, &nonces).unwrap_err();
        assert_eq!(
            err.to_string(),
            "read failed: direction binding",
            "control-bound wire must not open on the read route"
        );
    }

    #[test]
    fn container_read_rejects_a_mismatched_route_signature() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        let cross = ReadWire {
            direction: CONTAINER_READ_DIRECTION.to_owned(),
            route: CONTROL_LINK_ROUTE.to_owned(),
            nonce: random_nonce().unwrap(),
            expires_ms: now_ms().saturating_add(CONTROL_REQUEST_LIFETIME_MS),
        };
        let cross_body = signed_read_body(&key, &cross);
        let err = open_read(&key, &cross_body, &nonces).unwrap_err();
        assert_eq!(err.to_string(), "read failed: route binding");
    }

    #[test]
    fn control_link_rejects_a_mismatched_route_signature() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        let frame = seal(
            &key,
            &crate::db::ControlOp::AbortUpgrade { id: "1".into() },
            7,
        )
        .unwrap();
        let mut wire: serde_json::Value =
            serde_json::from_slice(&frame[4 + MAC_HEX..]).expect("control wire json");
        wire["route"] = serde_json::Value::String(CONTAINER_READ_PATH.to_owned());
        let json = serde_json::to_vec(&wire).unwrap();
        let cross_body = resign_payload(&key, &json);
        let err = open_frame(&key, &cross_body, &nonces).unwrap_err();
        assert_eq!(err.to_string(), "write failed: route binding");
    }

    #[test]
    fn control_link_rejects_a_container_read_bound_signature() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        let frame = seal(
            &key,
            &crate::db::ControlOp::AbortUpgrade { id: "1".into() },
            7,
        )
        .unwrap();
        open_frame(&key, &frame[4..], &nonces).expect("control route accepts its own frame");

        let mut wire: serde_json::Value =
            serde_json::from_slice(&frame[4 + MAC_HEX..]).expect("control wire json");
        wire["direction"] = serde_json::Value::String(CONTAINER_READ_DIRECTION.to_owned());
        wire["route"] = serde_json::Value::String(CONTAINER_READ_PATH.to_owned());
        let json = serde_json::to_vec(&wire).unwrap();
        let cross_body = resign_payload(&key, &json);
        let err = open_frame(&key, &cross_body, &nonces).unwrap_err();
        assert_eq!(
            err.to_string(),
            "write failed: direction binding",
            "read-bound wire must not open on the control route"
        );
    }

    #[test]
    fn loopback_bind_rejects_public_and_pod_addresses() {
        assert!(parse_loopback_bind("127.0.0.1:8083").is_ok());
        assert!(parse_loopback_bind("[::1]:8083").is_ok());
        assert!(parse_loopback_bind("0.0.0.0:8083").is_err());
        assert!(parse_loopback_bind("10.0.0.1:8083").is_err());
        assert!(parse_loopback_bind("8.8.8.8:53").is_err());
    }

    /// `None` is the production "stats unavailable" result: `!stats` renders API
    /// CPU and memory from a missing reading.
    fn assert_stats_unavailable(reading: Option<ContainerReadResponse>) {
        assert!(reading.is_none());
        let text = crate::templates::stats_with_container_use(
            "stats".into(),
            &crate::cgroup::ContainerSample::unavailable(),
            reading.as_ref().map(|response| &response.sample),
            reading.as_ref().map(|response| response.copy.as_str()),
        );
        assert!(text.contains("**API CPU:** `unavailable`"), "{text}");
        assert!(
            text.contains("**API memory (real program use):** `unavailable`"),
            "{text}"
        );
    }

    fn test_key() -> Vec<u8> {
        load_writer_key(&"x".repeat(32)).unwrap()
    }

    /// Read the sealed POST the way the API listener does, so the peer does not
    /// answer or close while the client is still writing the request.
    fn read_request(stream: &mut std::net::TcpStream) {
        use std::io::Read;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buf = Vec::new();
        let mut scratch = [0u8; 512];
        loop {
            let n = stream.read(&mut scratch).expect("request");
            assert_ne!(n, 0, "client closed before the request finished");
            buf.extend_from_slice(&scratch[..n]);
            let Some(split) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = std::str::from_utf8(&buf[..split]).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .expect("content-length");
            if buf.len() >= split + 4 + length {
                break;
            }
        }
    }

    #[test]
    fn closed_container_use_response_is_returned() {
        use std::io::Write;
        use std::net::TcpListener;

        let expected = ContainerReadResponse {
            sample: ContainerSample {
                memory: Some(crate::cgroup::MemoryUse {
                    program_bytes: 4096,
                    limit: crate::cgroup::MemoryLimit::Bytes(8192),
                }),
                cpu_tenths: Some(124),
            },
            copy: "api pid 7".into(),
        };
        let json = serde_json::to_vec(&expected).unwrap();
        assert!(json.len() < RESPONSE_LIMIT);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                json.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&json).unwrap();
        });
        let reading = fetch_blocking(addr, &test_key()).expect("closed reading");
        peer.join().unwrap();
        assert_eq!(reading, expected);
    }

    #[test]
    fn trickling_peer_releases_the_stats_fetch_at_the_overall_deadline() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let bytes_sent = Arc::new(AtomicUsize::new(0));
        let sent = Arc::clone(&bytes_sent);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n")
                .unwrap();
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(3) && !stop_flag.load(Ordering::SeqCst) {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                sent.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let started = Instant::now();
        let reading = fetch_blocking(addr, &test_key());
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        let _ = peer.join();
        assert_stats_unavailable(reading);
        let sent = bytes_sent.load(Ordering::SeqCst);
        assert!(
            sent > 0,
            "peer never trickled a byte; fetch failed before the deadline"
        );
        assert!(
            elapsed + Duration::from_millis(50) >= STATS_FETCH_TIMEOUT,
            "fetch returned in {elapsed:?}, before the overall deadline"
        );
        assert!(
            elapsed < STATS_FETCH_TIMEOUT + Duration::from_millis(400),
            "fetch kept running for {elapsed:?} while the peer was still trickling ({sent} bytes)"
        );
        assert!(
            sent < 40,
            "peer delivered {sent} bytes, so the fetch waited out the trickle instead of the deadline"
        );
    }

    #[test]
    fn oversized_container_use_response_is_unavailable_without_waiting_for_close() {
        use std::io::Write;
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, mpsc};
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicBool::new(false));
        let accepted_flag = Arc::clone(&accepted);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_flag = Arc::clone(&closed);
        let (release, hold) = mpsc::channel::<()>();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            accepted_flag.store(true, Ordering::SeqCst);
            read_request(&mut stream);
            let body = vec![b'x'; 64 * 1024];
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            if stream.write_all(header.as_bytes()).is_ok() {
                let _ = stream.write_all(&body);
            }
            let _ = hold.recv_timeout(Duration::from_secs(3));
            closed_flag.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let reading = fetch_blocking(addr, &test_key());
        let elapsed = started.elapsed();
        let peer_closed = closed.load(Ordering::SeqCst);
        drop(release);
        let _ = peer.join();
        assert_stats_unavailable(reading);
        assert!(
            accepted.load(Ordering::SeqCst),
            "peer never accepted the stats fetch"
        );
        assert!(
            !peer_closed,
            "fetch waited until the peer closed the oversized response"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "oversized response held the fetch for {elapsed:?}"
        );
    }

    #[test]
    fn stale_container_read_timestamp_is_rejected() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let wire = ReadWire {
            direction: CONTAINER_READ_DIRECTION.to_owned(),
            route: CONTAINER_READ_PATH.to_owned(),
            nonce: random_nonce().unwrap(),
            expires_ms: now_ms().saturating_sub(SKEW_MS + 1),
        };
        let json = serde_json::to_vec(&wire).unwrap();
        let mut body = mac_hex(&key, &json).into_bytes();
        body.extend_from_slice(&json);
        let nonces = Mutex::new(NonceWindow::new());
        assert!(open_read(&key, &body, &nonces).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn container_read_listener_drops_connections_over_the_concurrency_limit() {
        let secret = "c".repeat(32);
        let addr = spawn_container_read_listener("127.0.0.1:0", &secret).expect("listener");
        crate::set_process_ready(true);
        struct ClearReady;
        impl Drop for ClearReady {
            fn drop(&mut self) {
                crate::set_process_ready(false);
            }
        }
        let _clear_ready = ClearReady;
        std::thread::sleep(Duration::from_millis(50));

        // Fill the slots without successful probes: response EOF can arrive
        // before a probe's blocking handler releases its permit.
        let mut held = Vec::new();
        for _ in 0..MAX_CONTAINER_READ_CONNECTIONS {
            let stream = TcpStream::connect(addr).expect("slow connection");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            held.push(stream);
        }
        match sealed_read_exchange(addr, &secret) {
            Exchange::Dropped => {}
            Exchange::Served => panic!("a connection past the cap was served"),
            Exchange::Other(detail) => panic!("expected drop at the cap, got {detail}"),
        }

        held.pop();
        let mut served_after_release = false;
        for _ in 0..20 {
            if matches!(sealed_read_exchange(addr, &secret), Exchange::Served) {
                served_after_release = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            served_after_release,
            "a connection under the cap must still return container use"
        );
    }

    enum Exchange {
        Served,
        Dropped,
        Other(String),
    }

    fn sealed_read_exchange(addr: SocketAddr, secret: &str) -> Exchange {
        let started = std::time::Instant::now();
        let result = (|| -> std::io::Result<String> {
            let body = seal_read(secret.as_bytes()).expect("seal");
            let mut stream = TcpStream::connect(addr)?;
            stream.set_read_timeout(Some(Duration::from_millis(400)))?;
            stream.set_write_timeout(Some(Duration::from_millis(400)))?;
            let headers = format!(
                "POST {CONTAINER_READ_PATH} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            stream.write_all(headers.as_bytes())?;
            stream.write_all(&body)?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response)?;
            Ok(String::from_utf8_lossy(&response).into_owned())
        })();
        let elapsed = started.elapsed();
        match result {
            Ok(text) if text.contains("HTTP/1.1 200") => Exchange::Served,
            Ok(text) if elapsed < Duration::from_millis(400) && !text.contains("HTTP/1.1") => {
                Exchange::Dropped
            }
            Err(error)
                if elapsed < Duration::from_millis(400)
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::NotConnected
                    ) =>
            {
                Exchange::Dropped
            }
            Ok(text) => Exchange::Other(format!("status after {elapsed:?}: {text}")),
            Err(error) => Exchange::Other(format!("{error} after {elapsed:?}")),
        }
    }
}
