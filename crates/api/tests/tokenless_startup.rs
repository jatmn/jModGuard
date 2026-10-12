use jmodguard::db::Db;
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
    use std::time::{Duration, Instant};
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
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
        root,
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    // Keep the database absent across an open attempt and its retry backoff.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "API exited while waiting for the bot to create the database"
        );
        if let Ok(response) = client.get(format!("http://{address}/health")).send().await
            && response.status() == reqwest::StatusCode::OK
        {
            break;
        }
        assert!(Instant::now() < deadline, "API did not serve liveness");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for _ in 0..12 {
        assert!(process.child.try_wait().unwrap().is_none());
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The real bot-owned bootstrap creates the schema; no Discord is contacted.
    drop(Db::open(&database, 2).unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "API exited before serving without a bot token"
        );
        if let Ok(response) = client.get(format!("http://{address}/api/me")).send().await
            && response.status() == reqwest::StatusCode::BAD_REQUEST
        {
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
            break;
        }
        assert!(Instant::now() < deadline, "API did not finish startup");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
