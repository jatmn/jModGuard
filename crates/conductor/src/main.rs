use anyhow::{Context as _, Result};
use jmodguard_conductor::{config::Config, router};
use jmodguard_runtime::shutdown_signal;
use std::future::IntoFuture;

// The control process has no gateway or CPU worker pool at idle. Future backup
// work must use an explicitly bounded blocking executor rather than this thread.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cfg = Config::load()?;
    let filter = tracing_subscriber::EnvFilter::try_new(&cfg.log_filter)
        .context("invalid JMODGUARD_CONDUCTOR_LOG filter")?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .try_init()
        .map_err(|_| anyhow::anyhow!("failed to initialize conductor logging"))?;
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .context("failed to bind conductor")?;
    tracing::info!(
        "conductor listening on {}; control operations unavailable",
        listener.local_addr()?
    );
    let server = axum::serve(listener, router())
        .with_graceful_shutdown(shutdown_signal())
        .into_future();
    tokio::pin!(server);
    // Both awaiters share the runtime crate's latched signal. A peer holding a
    // connection open cannot delay process exit beyond the configured grace.
    tokio::select! {
        result = &mut server => result.context("conductor server failed")?,
        _ = shutdown_signal() => {
            match tokio::time::timeout(cfg.shutdown_grace, &mut server).await {
                Ok(result) => result.context("conductor shutdown failed")?,
                Err(_) => tracing::warn!("conductor shutdown grace elapsed"),
            }
        }
    }
    Ok(())
}
