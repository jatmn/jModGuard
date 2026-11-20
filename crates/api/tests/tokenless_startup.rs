use jmodguard::db::Db;
use jmodguard_updater::http_client_builder;
use std::path::PathBuf;

fn tmp_db_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "jmodguard_smoke_{label}_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn api_process_waits_for_first_boot_with_overlapping_read_bind_without_a_bot_token() {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    let root = tmp_db_path("tokenless_api").with_extension("");
    std::fs::create_dir(&root).unwrap();
    let database = root.join("app.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, format!(
        "[db]\npath={:?}\npool_size=2\n[web]\nbind={:?}\nbase_url={:?}\noauth_client_id='fixture'\noauth_client_secret='fixture'\n[log]\ndirectory=''\n",
        database.to_string_lossy(), address.to_string(), format!("http://{address}")
    )).unwrap();
    drop(listener);
    struct RunningApi {
        child: std::process::Child,
        root: PathBuf,
    }
    impl Drop for RunningApi {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    let mut process = RunningApi {
        child: Command::new(env!("CARGO_BIN_EXE_jmodguard-api"))
            .env_clear()
            .env("JMODGUARD_CONFIG", &config)
            // A valid key makes the private listener actually attempt the same bind.
            .env("JMODGUARD_CONTROL_SECRET", "x".repeat(32))
            .env("JMODGUARD_READ_BIND", address.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        root,
    };
    // Startup progress is read from the API's own log lines on stdout.
    let stdout = process.child.stdout.take().unwrap();
    let (line_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });
    /// Returns false if the API closed its output before logging `needle`.
    async fn next_log(
        lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
        seen: &mut String,
        needle: &str,
    ) -> bool {
        while let Some(line) = lines.recv().await {
            seen.push_str(&line);
            seen.push('\n');
            if line.contains(needle) {
                return true;
            }
        }
        false
    }
    let mut seen = String::new();
    let wait = Duration::from_secs(10);
    let client = http_client_builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let logged = tokio::time::timeout(wait, next_log(&mut lines, &mut seen, "API liveness on"))
        .await
        .expect("API did not serve liveness");
    assert!(logged, "API exited before serving liveness\n{seen}");
    // Keep the database absent across an open attempt and its retry backoff.
    for _ in 0..2 {
        let retried = tokio::time::timeout(
            wait,
            next_log(
                &mut lines,
                &mut seen,
                "API database is not ready; retrying without migrating",
            ),
        )
        .await
        .expect("API did not retry the database open");
        assert!(
            retried && process.child.try_wait().unwrap().is_none(),
            "API exited while waiting for the bot to create the database\n{seen}"
        );
        assert!(!database.exists(), "API must not create the database");
        for (path, expected) in [("health", 200), ("ready", 503)] {
            assert_eq!(
                client
                    .get(format!("http://{address}/{path}"))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                expected
            );
        }
    }
    // The real bot-owned bootstrap creates the schema; no Discord is contacted.
    drop(Db::open(&database, 2).unwrap());
    // The API logs this just before axum takes over the bound listener.
    let listening = tokio::time::timeout(wait, next_log(&mut lines, &mut seen, "API listening on"))
        .await
        .expect("API did not finish startup");
    assert!(
        listening && process.child.try_wait().unwrap().is_none(),
        "API exited before serving without a bot token\n{seen}"
    );
    let response = client
        .get(format!("http://{address}/api/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = response.text().await.unwrap();
    assert!(body.contains("not authenticated"), "{body}");
    assert_eq!(
        client
            .get(format!("http://{address}/ready"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        200
    );
}
