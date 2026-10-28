//! Children of unsupported launchers idle with health up and never
//! take the writer lease. The current launcher revision and a standalone start are not gated.
use jmodguard::db::{LAUNCHER_REVISION, REQUIRED_LAUNCHER_REVISION, SqlExecutor};
use std::io::Read;
use std::path::{Path, PathBuf};
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

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bot-launcher-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn spawn(envs: &[(&str, &str)], remove: &[&str]) -> Running {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_jmodguard-bot"));
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
    use std::io::{ErrorKind, Write};
    // Under parallel test load the health listener can reset or close a
    // just-accepted connection (BrokenPipe on write, ConnectionReset), as in
    // CI runs 697/715. Retry only those two errors, at most 3 attempts.
    // Anything else (connection refused, other I/O errors) panics at once and
    // any response, even an empty one, is returned unchanged, so a child that
    // really died still fails the test.
    let mut attempt = 1;
    loop {
        let result = (|| -> std::io::Result<String> {
            let mut stream = std::net::TcpStream::connect(addr)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            write!(
                stream,
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            )?;
            let mut body = String::new();
            stream.read_to_string(&mut body)?;
            Ok(body)
        })();
        match result {
            Ok(body) => return body,
            Err(error)
                if attempt < 3
                    && matches!(
                        error.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
                    ) =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                panic!("health request to {addr}{path} failed on attempt {attempt}: {error}")
            }
        }
    }
}

fn lease_present(path: &Path) -> bool {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let opts = jmodguard::db::DbOpenOptions {
            read_only: true,
            path: path.to_owned(),
            url: String::new(),
            token: String::new(),
            pool_size: 1,
            migrate: false,
            default_prefix: "!".into(),
            retention_days: 1,
        };
        let Ok(db) = jmodguard::db::Db::open_with(opts) else {
            return false;
        };
        db.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM meta WHERE key='active_bot_lease'",
                    (),
                    |row| Ok(row.get::<String>(0)?),
                )
                .is_ok())
        })
        .await
        .unwrap_or(false)
    })
}

#[test]
fn old_or_unknown_launcher_child_is_refused_without_readiness_and_exits_cleanly() {
    for revision in [
        None,
        Some("nope"),
        Some("0"),
        Some("3"),
        Some("4"),
        Some("5"),
        Some("6"),
        Some("7"),
        Some("17"),
    ] {
        let root = temp_root("rev2");
        let db_path = root.join("core.db");
        let ready_file = root.join("ready");
        let bind = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)).to_string();
        let mut envs = vec![
            ("JMODGUARD_DISCORD_TOKEN", "dummy-token"),
            ("JMODGUARD_DB_PATH", db_path.to_str().unwrap()),
            ("JMODGUARD_HEALTH_BIND", bind.as_str()),
            ("JMODGUARD_READY_FILE", ready_file.to_str().unwrap()),
            ("JMODGUARD_UPDATE_MAINTENANCE", "0"),
        ];
        if let Some(revision) = revision {
            envs.push(("JMODGUARD_LAUNCHER_REVISION", revision));
        }
        let remove = if revision.is_none() {
            vec!["JMODGUARD_LAUNCHER_REVISION"]
        } else {
            vec![]
        };
        let mut running = spawn(&envs, &remove);
        wait_log(
            &running,
            &format!("this release requires launcher revision {REQUIRED_LAUNCHER_REVISION}"),
        );
        let log = wait_log(&running, "health listener");
        let addr = health_addr(&log);
        let health = get(&addr, "/health");
        assert!(health.starts_with("HTTP/1.1 200"), "{health}");
        let ready = get(&addr, "/ready");
        assert!(ready.starts_with("HTTP/1.1 503"), "{ready}");
        assert!(running.child.try_wait().unwrap().is_none());
        assert!(
            !db_path.exists(),
            "a refused bot must not open the database"
        );
        assert!(
            !ready_file.exists(),
            "refusal must not claim Discord readiness"
        );
        assert!(
            Command::new("kill")
                .arg("-TERM")
                .arg(running.child.id().to_string())
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = running.child.try_wait().unwrap() {
                assert_eq!(
                    status.code(),
                    Some(0),
                    "SIGTERM is an intentional clean exit"
                );
                break;
            }
            assert!(Instant::now() < deadline, "refused child did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn current_launcher_revision_child_starts_and_claims_the_lease() {
    let root = temp_root("current");
    let db_path = root.join("core.db");
    let revision = LAUNCHER_REVISION.to_string();
    let running = spawn(
        &[
            ("JMODGUARD_DISCORD_TOKEN", "dummy-token"),
            ("JMODGUARD_DB_PATH", db_path.to_str().unwrap()),
            ("JMODGUARD_HEALTH_BIND", "127.0.0.1:0"),
            ("JMODGUARD_READY_FILE", root.join("ready").to_str().unwrap()),
            ("JMODGUARD_UPDATE_MAINTENANCE", "0"),
            ("JMODGUARD_LAUNCHER_REVISION", revision.as_str()),
        ],
        &[],
    );
    let log = wait_log(&running, "database ready");
    assert!(!log.contains("requires launcher revision"));
    let started = Instant::now();
    while !lease_present(&db_path) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "current launcher child did not claim the writer lease\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn standalone_bot_without_launcher_markers_is_not_gated() {
    let root = temp_root("standalone");
    let db_path = root.join("core.db");
    let running = spawn(
        &[
            ("JMODGUARD_DISCORD_TOKEN", "dummy-token"),
            ("JMODGUARD_DB_PATH", db_path.to_str().unwrap()),
            ("JMODGUARD_HEALTH_BIND", "127.0.0.1:0"),
        ],
        &[
            "JMODGUARD_LAUNCHER_REVISION",
            "JMODGUARD_READY_FILE",
            "JMODGUARD_UPDATE_MAINTENANCE",
        ],
    );
    let log = wait_log(&running, "database ready");
    assert!(!log.contains("requires launcher revision"));
    let started = Instant::now();
    while !lease_present(&db_path) {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "standalone bot did not claim the writer lease\n{log}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&root);
}
