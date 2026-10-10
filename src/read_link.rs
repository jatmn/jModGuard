//! Private bot-to-API container-use reading. Uses the same control secret as the
//! writer link. The route is not proxied by the public admin Pull Zone.

use crate::cgroup::ContainerSample;
use crate::control::{NonceWindow, load_writer_key, now_ms, parse_private_bind};
use anyhow::{Context as _, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

pub const CONTAINER_READ_PATH: &str = "/private/container-use";
const MAC_HEX: usize = 64;
const BODY_LIMIT: usize = 4096;
/// Bot-side upper bound for waiting on an API container-use response during `!stats`.
pub const STATS_FETCH_TIMEOUT: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const IO_TIMEOUT: Duration = Duration::from_millis(800);

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
struct ReadWire {
    nonce: String,
    expires_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerReadResponse {
    pub sample: ContainerSample,
    /// Identifies which API copy answered; not an aggregate across replicas.
    pub copy: String,
}

pub fn try_read_link(target: &str, secret: &str) -> Result<Option<ReadLink>> {
    if target.trim().is_empty() {
        return Ok(None);
    }
    let key = match load_writer_key(secret) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    let addr = parse_private_bind(target.trim())?;
    Ok(Some(ReadLink { addr, key }))
}

impl ReadLink {
    pub async fn fetch_container_use(&self) -> Option<ContainerReadResponse> {
        let addr = self.addr;
        let key = self.key.clone();
        tokio::task::spawn_blocking(move || fetch_blocking(addr, &key))
            .await
            .ok()
            .flatten()
    }
}

fn fetch_blocking(addr: SocketAddr, key: &[u8]) -> Option<ContainerReadResponse> {
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
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.write_all(&body).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let text = String::from_utf8_lossy(&response);
    let (_, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
    if !text.contains("HTTP/1.1 200") {
        return None;
    }
    serde_json::from_str(body.trim()).ok()
}

pub fn seal_read(key: &[u8]) -> Result<Vec<u8>> {
    let wire = ReadWire {
        nonce: random_nonce()?,
        expires_ms: now_ms().saturating_add(crate::control::CONTROL_REQUEST_LIFETIME_MS),
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
    if wire.nonce.len() != 32 || !wire.nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("read failed");
    }
    let mut guard = nonces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.accept(&wire.nonce, wire.expires_ms, now_ms()) {
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
    use std::sync::Mutex;

    #[test]
    fn sealed_read_rejects_a_replayed_body() {
        let key = load_writer_key(&"x".repeat(32)).unwrap();
        let body = seal_read(&key).unwrap();
        let nonces = Mutex::new(NonceWindow::new());
        open_read(&key, &body, &nonces).unwrap();
        assert!(open_read(&key, &body, &nonces).is_err());
    }
}
