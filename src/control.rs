//! Private API-to-bot writer link. The API never opens a write connection.
//!
//! The bot listens on a loopback or private address. Each request carries a
//! short-lived HMAC over the nonce, expiry, and operation. The key is the
//! database token when one is configured, otherwise a mode-0600 file beside
//! the local database that the bot creates and the API only reads.

use anyhow::{Context as _, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::DbConfig;
use crate::db::{ControlOp, ControlReply};

const BODY_LIMIT: usize = 256 * 1024;
const MAC_HEX: usize = 64;
const MAX_AGE_MS: i64 = 60_000;
const SKEW_MS: i64 = 5_000;
const SEEN_LIMIT: usize = 4096;

#[derive(Debug)]
pub struct WriterLinkNotReady;

impl std::fmt::Display for WriterLinkNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("writer link is unusable")
    }
}

impl std::error::Error for WriterLinkNotReady {}

#[derive(Clone)]
pub struct WriterLink {
    addr: SocketAddr,
    key: Vec<u8>,
}

impl std::fmt::Debug for WriterLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterLink")
    }
}

impl WriterLink {
    pub fn connect(addr: SocketAddr, key: Vec<u8>) -> Result<Self> {
        anyhow::ensure!(!key.is_empty(), "writer link is unusable");
        parse_private_bind(&addr.to_string())?;
        Ok(Self { addr, key })
    }

    pub async fn submit(&self, op: ControlOp) -> Result<ControlReply> {
        let addr = self.addr;
        let key = self.key.clone();
        tokio::task::spawn_blocking(move || submit_blocking(addr, &key, &op))
            .await
            .context("write failed")?
    }
}

pub fn try_writer_link(bind: &str, db: &DbConfig) -> Result<Option<WriterLink>> {
    let addr = parse_private_bind(bind)?;
    match load_writer_key(db, false) {
        Ok(key) => Ok(Some(WriterLink::connect(addr, key)?)),
        Err(error) if writer_link_not_ready(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn writer_link_not_ready(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<WriterLinkNotReady>().is_some())
}

pub fn load_writer_key(db: &DbConfig, create: bool) -> Result<Vec<u8>> {
    if !db.token.is_empty() {
        return Ok(db.token.as_bytes().to_vec());
    }
    let path = writer_link_path(&db.path);
    if create && !path.exists() {
        write_key_file(&path)?;
    }
    let bytes = std::fs::read(&path).map_err(|_| WriterLinkNotReady)?;
    if bytes.len() != 32 {
        return Err(WriterLinkNotReady.into());
    }
    Ok(bytes)
}

pub fn parse_private_bind(raw: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("control bind is not private"))?;
    if !is_private_ip(addr.ip()) {
        anyhow::bail!("control bind is not private");
    }
    Ok(addr)
}

pub fn public_error(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        let text = cause.to_string();
        if !text.is_empty() && text.len() <= 300 && !text.contains(['/', '\\', '@']) {
            return text;
        }
    }
    "write failed".to_owned()
}

pub struct NonceWindow {
    seen: HashSet<String>,
    order: VecDeque<(String, i64)>,
}

impl Default for NonceWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl NonceWindow {
    pub fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    pub fn accept(&mut self, nonce: &str, expires_ms: i64, now_ms: i64) -> bool {
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        if expires_ms < now_ms.saturating_sub(SKEW_MS)
            || expires_ms > now_ms.saturating_add(MAX_AGE_MS + SKEW_MS)
        {
            return false;
        }
        self.order
            .retain(|(_, exp)| *exp >= now_ms.saturating_sub(SKEW_MS));
        self.seen
            .retain(|item| self.order.iter().any(|(n, _)| n == item));
        if !self.seen.insert(nonce.to_owned()) {
            return false;
        }
        self.order.push_back((nonce.to_owned(), expires_ms));
        while self.order.len() > SEEN_LIMIT {
            if let Some((old, _)) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn submit_blocking(addr: SocketAddr, key: &[u8], op: &ControlOp) -> Result<ControlReply> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    let frame = seal(key, op)?;
    stream
        .write_all(&frame)
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    match read_frame(&mut stream) {
        Ok(body) => serde_json::from_slice(&body).context("write failed"),
        Err(_) => Ok(ControlReply::StillWaiting),
    }
}

pub(crate) fn seal(key: &[u8], op: &ControlOp) -> Result<Vec<u8>> {
    let wire = Wire {
        nonce: random_nonce()?,
        expires_ms: now_ms().saturating_add(MAX_AGE_MS),
        op: op.clone(),
    };
    let json = serde_json::to_vec(&wire).context("write failed")?;
    anyhow::ensure!(json.len() + MAC_HEX <= BODY_LIMIT, "write failed");
    let mut frame = Vec::with_capacity(4 + MAC_HEX + json.len());
    frame.extend_from_slice(
        &u32::try_from(MAC_HEX + json.len())
            .unwrap_or(0)
            .to_be_bytes(),
    );
    frame.extend_from_slice(mac_hex(key, &json).as_bytes());
    frame.extend_from_slice(&json);
    Ok(frame)
}

pub fn open_frame(key: &[u8], body: &[u8], nonces: &Mutex<NonceWindow>) -> Result<ControlOp> {
    if body.len() < MAC_HEX || body.len() > BODY_LIMIT {
        anyhow::bail!("write failed");
    }
    let (mac, json) = body.split_at(MAC_HEX);
    let mac = std::str::from_utf8(mac).map_err(|_| anyhow::anyhow!("write failed"))?;
    if !verify_mac(key, mac, json) {
        anyhow::bail!("write failed");
    }
    let wire: Wire = serde_json::from_slice(json).map_err(|_| anyhow::anyhow!("write failed"))?;
    let mut guard = nonces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.accept(&wire.nonce, wire.expires_ms, now_ms()) {
        anyhow::bail!("write failed");
    }
    Ok(wire.op)
}

pub fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if !(MAC_HEX..=BODY_LIMIT).contains(&len) {
        anyhow::bail!("write failed");
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    Ok(body)
}

pub fn write_frame(stream: &mut impl Write, reply: &ControlReply) -> Result<()> {
    let json = serde_json::to_vec(reply).context("write failed")?;
    anyhow::ensure!(json.len() <= BODY_LIMIT, "write failed");
    let len = u32::try_from(json.len()).unwrap_or(0);
    stream
        .write_all(&len.to_be_bytes())
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    stream
        .write_all(&json)
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    Ok(())
}

fn writer_link_path(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("store"))
        .to_os_string();
    name.push(".writer-link");
    db_path.with_file_name(name)
}

fn write_key_file(path: &Path) -> Result<()> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("writer link is unusable"))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|_| anyhow::anyhow!("writer link is unusable"))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("writer link is unusable"))?;
    file.write_all(&bytes)
        .map_err(|_| anyhow::anyhow!("writer link is unusable"))?;
    Ok(())
}

fn random_nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("write failed"))?;
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
    anyhow::ensure!(text.len().is_multiple_of(2), "write failed");
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
        _ => anyhow::bail!("write failed"),
    }
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback() || is_unique_local(v6),
    }
}

fn is_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

#[derive(Serialize, Deserialize)]
struct Wire {
    nonce: String,
    expires_ms: i64,
    op: ControlOp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_bind_rejects_public_addresses() {
        assert!(parse_private_bind("127.0.0.1:8791").is_ok());
        assert!(parse_private_bind("10.1.2.3:1").is_ok());
        assert!(parse_private_bind("[::1]:9").is_ok());
        assert!(parse_private_bind("0.0.0.0:8791").is_err());
        assert!(parse_private_bind("8.8.8.8:53").is_err());
        assert!(parse_private_bind("192.0.2.1:1").is_err());
    }

    #[test]
    fn replayed_nonce_is_rejected() {
        let mut window = NonceWindow::new();
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(window.accept(nonce, 1_000_060_000, 1_000_000_000));
        assert!(!window.accept(nonce, 1_000_060_000, 1_000_000_000));
        assert!(!window.accept("short", 1_000_060_000, 1_000_000_000));
        assert!(!window.accept(nonce, 1, 1_000_000_000));
    }

    #[test]
    fn unspecified_v4_is_not_private() {
        assert!(!is_private_ip(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)));
    }
}
