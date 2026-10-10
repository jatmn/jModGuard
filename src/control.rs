//! Private API-to-bot writer link. The API never opens a write connection.
//!
//! The bot listens on a loopback or private address. Each request carries a
//! short-lived HMAC over the nonce, expiry, actor, and operation. The key is
//! the dedicated control secret shared by the bot and the API. It is not a
//! database token.

use anyhow::{Context as _, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::db::{ControlOp, ControlReply};

const BODY_LIMIT: usize = 256 * 1024;
const MAC_HEX: usize = 64;
const MIN_SECRET_LEN: usize = 32;
pub const MAX_AGE_MS: i64 = 60_000;
/// One dashboard attempt. Every frame of that attempt uses this same expiry.
pub const CONTROL_REQUEST_LIFETIME_MS: i64 = 15_000;
const OUTCOME_GRACE_MS: i64 = 4_000;
const LOOKUP_POLL: Duration = Duration::from_millis(250);
const _: () = assert!(CONTROL_REQUEST_LIFETIME_MS <= MAX_AGE_MS);
pub(crate) const SKEW_MS: i64 = 5_000;
const _: () = assert!(OUTCOME_GRACE_MS < SKEW_MS);
const SEEN_LIMIT: usize = 4096;
const REQUEST_ID_HEX: usize = 64;

/// HMAC direction for API-to-bot writer frames.
pub const CONTROL_LINK_DIRECTION: &str = "bot-control";
/// HMAC route for API-to-bot writer frames.
pub const CONTROL_LINK_ROUTE: &str = "control-write";

/// Exact status when the API cannot reach the bot writer.
pub const CONTROL_LINK_UNAVAILABLE: &str = "control link unavailable";
/// Exact status when this process does not hold the writer lease.
pub const NOT_THE_WRITER: &str = "not the writer";
/// Exact status while the bot is still applying maintenance recovery.
pub const RECOVERY_IN_PROGRESS: &str = "recovery in progress";
/// The attempt's expiry passed without a stored outcome. Reload before retrying.
pub const OUTCOME_UNKNOWN: &str = "outcome unknown; reload before retrying";

#[derive(Debug)]
pub struct WriterLinkNotReady;

impl std::fmt::Display for WriterLinkNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("writer link is unusable")
    }
}

impl std::error::Error for WriterLinkNotReady {}

/// Missing or too-short control secret. The display text is safe to log.
#[derive(Debug)]
pub struct ControlSecretError;

impl std::fmt::Display for ControlSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "JMODGUARD_CONTROL_SECRET must be the same value on the bot and the API and at least 32 bytes. It is separate from both database tokens. Without it the control link stays off.",
        )
    }
}

impl std::error::Error for ControlSecretError {}

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

    pub async fn submit(&self, op: ControlOp, actor_user_id: i64) -> Result<ControlReply> {
        let request_id = random_request_id()?;
        self.submit_request(op, actor_user_id, request_id).await
    }

    /// One dashboard attempt keeps `request_id` and one expiry. A later attempt
    /// calls [`Self::submit`] and gets a new id.
    pub async fn submit_request(
        &self,
        op: ControlOp,
        actor_user_id: i64,
        request_id: String,
    ) -> Result<ControlReply> {
        let expires_ms = now_ms().saturating_add(CONTROL_REQUEST_LIFETIME_MS);
        let addr = self.addr;
        let key = self.key.clone();
        let id = request_id.clone();
        let first = tokio::task::spawn_blocking(move || {
            submit_blocking(addr, &key, &op, actor_user_id, expires_ms, &id)
        })
        .await
        .context("write failed")??;
        if !matches!(first, ControlReply::StillWaiting) {
            return Ok(first);
        }
        let grace_deadline = expires_ms.saturating_add(OUTCOME_GRACE_MS);
        loop {
            if now_ms() > grace_deadline {
                return Ok(outcome_unknown());
            }
            tokio::time::sleep(LOOKUP_POLL).await;
            if now_ms() > grace_deadline {
                return Ok(outcome_unknown());
            }
            let addr = self.addr;
            let key = self.key.clone();
            let id = request_id.clone();
            let looked_up = tokio::task::spawn_blocking(move || {
                submit_blocking(
                    addr,
                    &key,
                    &ControlOp::LookupResult,
                    actor_user_id,
                    expires_ms,
                    &id,
                )
            })
            .await;
            match looked_up {
                Ok(Ok(ControlReply::StillWaiting)) => {}
                Ok(Ok(done)) => return Ok(done),
                Ok(Err(_)) | Err(_) => {}
            }
        }
    }
}

fn outcome_unknown() -> ControlReply {
    ControlReply::Failed {
        message: OUTCOME_UNKNOWN.to_owned(),
    }
}

pub fn try_writer_link(bind: &str, secret: &str) -> Result<Option<WriterLink>> {
    let key = match load_writer_key(secret) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    let addr = parse_private_bind(bind)?;
    Ok(Some(WriterLink::connect(addr, key)?))
}

/// HMAC key bytes from the dedicated control secret. Database tokens are ignored.
pub fn load_writer_key(secret: &str) -> Result<Vec<u8>, ControlSecretError> {
    if secret.len() < MIN_SECRET_LEN {
        Err(ControlSecretError)
    } else {
        Ok(secret.as_bytes().to_vec())
    }
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

fn submit_blocking(
    addr: SocketAddr,
    key: &[u8],
    op: &ControlOp,
    actor_user_id: i64,
    expires_ms: i64,
    request_id: &str,
) -> Result<ControlReply> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    let frame = seal_with_request(key, op, actor_user_id, expires_ms, request_id)?;
    stream
        .write_all(&frame)
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    match read_frame(&mut stream) {
        Ok(body) => serde_json::from_slice(&body).context("write failed"),
        Err(_) => Ok(ControlReply::StillWaiting),
    }
}

#[cfg(test)]
pub(crate) fn seal(key: &[u8], op: &ControlOp, actor_user_id: i64) -> Result<Vec<u8>> {
    seal_with_expiry(key, op, actor_user_id, now_ms().saturating_add(MAX_AGE_MS))
}

pub fn seal_with_expiry(
    key: &[u8],
    op: &ControlOp,
    actor_user_id: i64,
    expires_ms: i64,
) -> Result<Vec<u8>> {
    seal_with_request(key, op, actor_user_id, expires_ms, &random_request_id()?)
}

pub fn seal_with_request(
    key: &[u8],
    op: &ControlOp,
    actor_user_id: i64,
    expires_ms: i64,
    request_id: &str,
) -> Result<Vec<u8>> {
    anyhow::ensure!(valid_request_id(request_id), "write failed");
    let wire = Wire {
        direction: CONTROL_LINK_DIRECTION.to_owned(),
        route: CONTROL_LINK_ROUTE.to_owned(),
        nonce: random_nonce()?,
        request_id: request_id.to_owned(),
        expires_ms,
        actor_user_id,
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

pub fn open_frame(
    key: &[u8],
    body: &[u8],
    nonces: &Mutex<NonceWindow>,
) -> Result<(ControlOp, i64, i64, String)> {
    if body.len() < MAC_HEX || body.len() > BODY_LIMIT {
        anyhow::bail!("write failed");
    }
    let (mac, json) = body.split_at(MAC_HEX);
    let mac = std::str::from_utf8(mac).map_err(|_| anyhow::anyhow!("write failed"))?;
    if !verify_mac(key, mac, json) {
        anyhow::bail!("write failed");
    }
    let wire: Wire = serde_json::from_slice(json).map_err(|_| anyhow::anyhow!("write failed"))?;
    if wire.direction != CONTROL_LINK_DIRECTION {
        anyhow::bail!("write failed: direction binding");
    }
    if wire.route != CONTROL_LINK_ROUTE {
        anyhow::bail!("write failed: route binding");
    }
    let now = now_ms();
    if wire.expires_ms < now.saturating_sub(SKEW_MS) {
        anyhow::bail!("write failed");
    }
    if !valid_request_id(&wire.request_id) {
        anyhow::bail!("write failed");
    }
    let mut guard = nonces
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.accept(&wire.nonce, wire.expires_ms, now_ms()) {
        anyhow::bail!("write failed");
    }
    Ok((
        wire.op,
        wire.actor_user_id,
        wire.expires_ms,
        wire.request_id,
    ))
}

pub fn read_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|_| anyhow::anyhow!("waiting"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if !(1..=BODY_LIMIT).contains(&len) {
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

/// A new id for one API attempt. Retries of that attempt reuse it.
pub fn random_request_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("write failed"))?;
    Ok(to_hex(&bytes))
}

fn valid_request_id(request_id: &str) -> bool {
    request_id.len() == REQUEST_ID_HEX && request_id.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    direction: String,
    route: String,
    nonce: String,
    request_id: String,
    expires_ms: i64,
    actor_user_id: i64,
    op: ControlOp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_bind_rejects_public_addresses() {
        assert!(parse_private_bind("127.0.0.1:9").is_ok());
        assert!(parse_private_bind("10.1.2.3:1").is_ok());
        assert!(parse_private_bind("[::1]:9").is_ok());
        assert!(parse_private_bind("0.0.0.0:9").is_err());
        assert!(parse_private_bind("[::]:9").is_err());
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

    fn fixture_secret() -> String {
        "x".repeat(32)
    }

    fn sample_op() -> crate::db::ControlOp {
        crate::db::ControlOp::AbortUpgrade { id: "1".into() }
    }

    #[test]
    fn short_control_replies_round_trip() {
        for reply in [
            ControlReply::Waiting,
            ControlReply::Saved {
                body: serde_json::Value::Null,
            },
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &reply).unwrap();
            assert!(buf.len() < MAC_HEX, "a short reply must still be a frame");
            let body = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
            let got: ControlReply = serde_json::from_slice(&body).unwrap();
            assert_eq!(got, reply);
        }
    }

    #[test]
    fn truncated_control_frame_is_rejected() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ControlReply::Waiting).unwrap();
        buf.pop();
        assert!(read_frame(&mut std::io::Cursor::new(buf)).is_err());
    }

    #[test]
    fn forged_control_mac_is_rejected() {
        let key = load_writer_key(&fixture_secret()).unwrap();
        let frame = seal(&key, &sample_op(), 7).unwrap();
        let mut body = frame[4..].to_vec();
        body[0] ^= 0x01;
        let nonces = Mutex::new(NonceWindow::new());
        assert!(open_frame(&key, &body, &nonces).is_err());
        assert!(open_frame(&key, b"short", &Mutex::new(NonceWindow::new())).is_err());
    }

    #[test]
    fn matching_control_secret_seals_and_opens() {
        let key = load_writer_key(&fixture_secret()).unwrap();
        let frame = seal(&key, &sample_op(), 4).unwrap();
        let again = seal(&key, &sample_op(), 4).unwrap();
        let (op, actor, _, request_id) =
            open_frame(&key, &frame[4..], &Mutex::new(NonceWindow::new())).unwrap();
        let (_, _, _, other) =
            open_frame(&key, &again[4..], &Mutex::new(NonceWindow::new())).unwrap();
        assert_eq!(actor, 4);
        assert_ne!(
            request_id, other,
            "two attempts must not share a request id"
        );
        let reused = seal_with_request(
            &key,
            &sample_op(),
            4,
            now_ms().saturating_add(60_000),
            &request_id,
        )
        .unwrap();
        let (_, _, _, replay) =
            open_frame(&key, &reused[4..], &Mutex::new(NonceWindow::new())).unwrap();
        assert_eq!(replay, request_id);
        assert!(matches!(op, crate::db::ControlOp::AbortUpgrade { id } if id == "1"));
    }

    #[test]
    fn mismatched_control_secret_is_rejected() {
        let left = load_writer_key(&fixture_secret()).unwrap();
        let right = load_writer_key(&"y".repeat(32)).unwrap();
        let frame = seal(&left, &sample_op(), 0).unwrap();
        assert!(open_frame(&right, &frame[4..], &Mutex::new(NonceWindow::new())).is_err());
        let text = load_writer_key("short").unwrap_err().to_string();
        assert!(text.contains("JMODGUARD_CONTROL_SECRET"));
        assert!(!text.contains("short"));
        assert!(!text.contains('/'));
    }

    #[test]
    fn different_database_tokens_still_authenticate_when_the_secret_matches() {
        let secret = fixture_secret();
        let token_a = "token-a-not-used-as-the-control-key";
        let token_b = "token-b-not-used-as-the-control-key";
        let left = load_writer_key(&secret).unwrap();
        let right = load_writer_key(&secret).unwrap();
        assert_ne!(left.as_slice(), token_a.as_bytes());
        assert_ne!(right.as_slice(), token_b.as_bytes());
        let frame = seal(&left, &sample_op(), 0).unwrap();
        let (op, actor, _, _) =
            open_frame(&right, &frame[4..], &Mutex::new(NonceWindow::new())).unwrap();
        assert_eq!(actor, 0);
        assert!(matches!(op, crate::db::ControlOp::AbortUpgrade { .. }));
        assert!(
            open_frame(
                token_a.as_bytes(),
                &frame[4..],
                &Mutex::new(NonceWindow::new())
            )
            .is_err()
        );
        assert!(
            open_frame(
                token_b.as_bytes(),
                &frame[4..],
                &Mutex::new(NonceWindow::new())
            )
            .is_err()
        );
    }
}
