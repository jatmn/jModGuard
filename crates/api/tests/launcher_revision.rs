//! A child started by a revision-2 launcher answers health and stays up.
//! The current launcher revision and a standalone start pass the gate.
use jmodguard::db::{LAUNCHER_REVISION, REQUIRED_LAUNCHER_REVISION};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Running {
    child: Child,
    log: Arc<Mutex<String>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(envs: &[(&str, &str)], remove: &[&str]) -> Running {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jmodguard-api"));
    cmd.env("JMODGUARD_CONFIG", "/no/such/jmodguard-config.toml")
        .env("JMODGUARD_LOG_LEVEL", "info")
        .env("JMODGUARD_LOG_DIRECTORY", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    for key in remove {
        cmd.env_remove(key);
    }
    let mut child = cmd.spawn().unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    if let Some(pipe) = child.stdout.take() {
        pump(pipe, Arc::clone(&log));
    }
    if let Some(pipe) = child.stderr.take() {
        pump(pipe, Arc::clone(&log));
    }
    Running { child, log }
}

fn pump<R: Read + Send + 'static>(mut pipe: R, log: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => log
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
    });
}

fn wait_log(running: &Running, needle: &str) -> String {
    let started = Instant::now();
    loop {
        let text = running.log.lock().unwrap().clone();
        if text.contains(needle) {
            return text;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "timed out waiting for {needle}\n{text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn health_addr(log: &str) -> String {
    let start = log.find("health listener").expect(log);
    let http = log[start..].find("http://").unwrap() + start + "http://".len();
    let end = log[http..].find("/health").unwrap() + http;
    log[http..end].to_string()
}

fn get(addr: &str, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    use std::io::Write;
    // The startup listener can answer after its first read. Do not let
    // formatting split the request into separately scheduled socket writes.
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut body = String::new();
    let _ = stream.read_to_string(&mut body);
    body
}

fn root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("api-launcher-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn revision_two_api_child_answers_health_and_does_not_exit() {
    let dir = root("rev2");
    let mut running = spawn(
        &[
            ("JMODGUARD_WEB_BIND", "127.0.0.1:0"),
            ("JMODGUARD_READY_FILE", dir.join("ready").to_str().unwrap()),
            ("JMODGUARD_UPDATE_MAINTENANCE", "0"),
        ],
        &[
            "JMODGUARD_LAUNCHER_REVISION",
            "JMODGUARD_OAUTH_CLIENT_ID",
            "JMODGUARD_OAUTH_CLIENT_SECRET",
        ],
    );
    wait_log(
        &running,
        &format!(
            "this release requires launcher revision {REQUIRED_LAUNCHER_REVISION}; the running launcher is revision 2"
        ),
    );
    let log = wait_log(&running, "health listener");
    let addr = health_addr(&log);
    let health = get(&addr, "/health");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    let ready = get(&addr, "/ready");
    assert!(ready.starts_with("HTTP/1.1 503"), "{ready}");
    std::thread::sleep(Duration::from_secs(2));
    assert!(running.child.try_wait().unwrap().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn current_launcher_revision_api_child_passes_the_launcher_gate() {
    let dir = root("current");
    let revision = LAUNCHER_REVISION.to_string();
    let mut running = spawn(
        &[
            ("JMODGUARD_WEB_BIND", "127.0.0.1:0"),
            ("JMODGUARD_READY_FILE", dir.join("ready").to_str().unwrap()),
            ("JMODGUARD_UPDATE_MAINTENANCE", "0"),
            ("JMODGUARD_LAUNCHER_REVISION", revision.as_str()),
        ],
        &["JMODGUARD_OAUTH_CLIENT_ID", "JMODGUARD_OAUTH_CLIENT_SECRET"],
    );
    let log = wait_log(&running, "Discord OAuth credentials required");
    assert!(!log.contains("requires launcher revision"));
    let started = Instant::now();
    loop {
        if running.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "current launcher API did not continue past the launcher gate\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn standalone_api_without_launcher_markers_is_not_gated() {
    let mut running = spawn(
        &[("JMODGUARD_WEB_BIND", "127.0.0.1:0")],
        &[
            "JMODGUARD_LAUNCHER_REVISION",
            "JMODGUARD_READY_FILE",
            "JMODGUARD_UPDATE_MAINTENANCE",
            "JMODGUARD_OAUTH_CLIENT_ID",
            "JMODGUARD_OAUTH_CLIENT_SECRET",
        ],
    );
    let log = wait_log(&running, "Discord OAuth credentials required");
    assert!(!log.contains("requires launcher revision"));
    let started = Instant::now();
    loop {
        if running.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "standalone API did not continue past startup\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
