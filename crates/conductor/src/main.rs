use anyhow::{Context as _, Result};
use jmodguard_conductor::{Conductor, config::Config, router_for};
use jmodguard_runtime::shutdown_signal;
use std::future::IntoFuture;

// Backup/update work is not enabled. Future provider jobs must use an
// explicitly bounded blocking executor rather than this thread.
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
    let bind = cfg.bind;
    let shutdown_grace = cfg.shutdown_grace;
    let conductor = Conductor::open(cfg).await?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .context("failed to bind conductor")?;
    tracing::info!(
        "conductor listening on {}; control operations {}",
        listener.local_addr()?,
        if conductor.operations_available() {
            "available"
        } else {
            "unavailable"
        }
    );
    let server = axum::serve(listener, router_for(conductor))
        .with_graceful_shutdown(shutdown_signal())
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("conductor server failed")?,
        _ = shutdown_signal() => {
            match tokio::time::timeout(shutdown_grace, &mut server).await {
                Ok(result) => result.context("conductor shutdown failed")?,
                Err(_) => tracing::warn!("conductor shutdown grace elapsed"),
            }
        }
    }
    Ok(())
}
