use std::time::{Duration, Instant, SystemTime};

#[test]
fn std_instant() {
    let _ = Instant::now();
}

#[test]
fn std_system_time() {
    let _ = SystemTime::now();
}

#[test]
fn std_sleep() {
    std::thread::sleep(Duration::ZERO);
}

#[tokio::test]
async fn tokio_unpaused() {
    let _ = tokio::time::Instant::now();
    tokio::time::sleep(Duration::ZERO).await;
}

#[tokio::test(start_paused = true)]
async fn tokio_starts_paused() {
    let _ = tokio::time::Instant::now();
    tokio::time::sleep(Duration::ZERO).await;
}

#[tokio::test]
async fn tokio_pauses_explicitly() {
    tokio::time::pause();
    let _ = tokio::time::Instant::now();
    tokio::time::sleep(Duration::ZERO).await;
}

#[tokio::test(start_paused = true)]
async fn tokio_after_resume() {
    tokio::time::resume();
    let _ = tokio::time::Instant::now();
    tokio::time::sleep(Duration::ZERO).await;
}

#[tokio::test(start_paused = true)]
async fn std_clock_is_not_paused_by_tokio() {
    let _ = Instant::now();
}

fn non_test_helper() {
    let _ = Instant::now();
    std::thread::sleep(Duration::ZERO);
}

#[test]
fn calls_helper() {
    non_test_helper();
}
