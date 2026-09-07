//! Real local process checks: no Discord, database or provider account.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Process {
    child: Child,
    root: PathBuf,
    addr: SocketAddr,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jmodguard-conductor"));
    cmd.env_clear().env("JMODGUARD_CONDUCTOR_LOG", "warn");
    cmd
}
impl Process {
    fn start() -> Self {
        let root = std::env::temp_dir().join(format!(
            "jmodguard-conductor-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        // Invalid bot config/DB settings must not be consumed by the conductor.
        std::fs::write(root.join("config.toml"), "not valid TOML [").unwrap();
        // Let the child retain its kernel-assigned port; reserving then releasing
        // a port in the parent races other parallel process tests.
        let addr = "127.0.0.1:0".parse().unwrap();
        let output = std::fs::File::create(root.join("process.log")).unwrap();
        let child = command()
            .current_dir(&root)
            .env("JMODGUARD_CONDUCTOR_BIND", "127.0.0.1:0")
            .env("JMODGUARD_CONDUCTOR_LOG", "info")
            .env("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "1")
            .env("JMODGUARD_CONFIG", root.join("config.toml"))
            .env("JMODGUARD_DB_URL", "invalid-database-url")
            .env("JMODGUARD_REQUIRE_REMOTE_DB", "1")
            .env("JMODGUARD_DISCORD_TOKEN", "synthetic-do-not-expose")
            .env("JMODGUARD_WEB_BIND", "invalid-bot-api-bind")
            .stdout(output)
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut process = Self { child, root, addr };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = std::fs::read_to_string(process.root.join("process.log")).unwrap();
            if let Some((address, _)) = log
                .split_once("conductor listening on ")
                .and_then(|(_, rest)| rest.split_once(';'))
            {
                process.addr = address.parse().expect("bound socket address");
                if TcpStream::connect(process.addr).is_ok() {
                    return process;
                }
            }
            assert!(
                process.child.try_wait().unwrap().is_none(),
                "conductor exited during startup"
            );
            assert!(Instant::now() < deadline, "conductor did not listen");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn request(&self, method: &str, path: &str) -> String {
        let mut s = TcpStream::connect(self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        text
    }
}
#[test]
fn standalone_process_reports_liveness_but_not_operational_readiness() {
    let p = Process::start();
    for path in ["/health", "/health/"] {
        let text = p.request("GET", path);
        assert!(text.starts_with("HTTP/1.1 200"));
        assert!(text.ends_with("ok"));
    }
    let head = p.request("HEAD", "/health");
    assert!(head.starts_with("HTTP/1.1 200"));
    assert!(head.ends_with("\r\n\r\n"));
    let ready = p.request("GET", "/ready");
    assert!(ready.starts_with("HTTP/1.1 503"));
    assert!(ready.contains("control_not_configured"));
    let version = p.request("GET", "/version");
    assert!(version.starts_with("HTTP/1.1 200"));
    assert!(version.contains("\"operations_available\":false"));
    for text in [ready, version] {
        assert!(text.contains("cache-control: no-store"));
        assert!(!text.contains("synthetic-do-not-expose"));
        assert!(!text.contains("invalid-database-url"));
    }
    for path in ["/api/guilds", "/uploads", "/updates", "/login"] {
        assert!(p.request("POST", path).starts_with("HTTP/1.1 404"));
    }
    assert!(!p.root.join("jmodguard.db").exists());
    assert!(!p.root.join("logs").exists());
}
#[test]
fn invalid_process_settings_and_occupied_ports_fail_startup() {
    for (key, value) in [
        ("JMODGUARD_CONDUCTOR_BIND", "garbage"),
        ("JMODGUARD_CONDUCTOR_SHUTDOWN_SECS", "0"),
        ("JMODGUARD_CONDUCTOR_LOG", "[broken"),
    ] {
        let output = command().env(key, value).output().unwrap();
        assert!(!output.status.success(), "{key}");
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = command()
        .env(
            "JMODGUARD_CONDUCTOR_BIND",
            listener.local_addr().unwrap().to_string(),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
}
#[cfg(unix)]
#[test]
fn sigterm_stops_the_actual_conductor_even_with_an_incomplete_request() {
    let mut p = Process::start();
    let mut slow = TcpStream::connect(p.addr).unwrap();
    slow.write_all(b"GET /health HTTP/1.1\r\n").unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &p.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = p.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "conductor exceeded shutdown grace"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
