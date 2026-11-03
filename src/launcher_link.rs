//! Bot-launcher update-control listener. The bot launcher keeps running, and
//! keeps its launcher fence, while the bot process is stopped for a paired
//! upgrade. The read-only API launcher sends its paired-upgrade steps here, and
//! the API sends owner upgrade recovery here. Frames use the control secret.
//! [`UpdateControl::apply_launcher_op`] accepts only those operations.

use crate::control::{self, NonceWindow};
use crate::db::{ControlReply, UpdateControl};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::Duration;

/// Serve the launcher link on `bind`. A missing secret or a refused bind only
/// disables the link with a warning: paired steps then stay pending.
pub fn spawn(control: UpdateControl, bind: &str, secret: &str) -> Option<SocketAddr> {
    let key = match control::load_writer_key(secret) {
        Ok(key) => key,
        Err(error) => {
            tracing::warn!("bot launcher link disabled: {error}");
            return None;
        }
    };
    let listener = match control::parse_private_bind(bind).and_then(|addr| {
        let listener = TcpListener::bind(addr)?;
        let bound = listener.local_addr()?;
        Ok((listener, bound))
    }) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("bot launcher link disabled: {error:#}");
            return None;
        }
    };
    let (listener, bound) = listener;
    let runtime = tokio::runtime::Handle::current();
    let spawned = std::thread::Builder::new()
        .name("launcher-link".into())
        .spawn(move || {
            let nonces = Mutex::new(NonceWindow::new());
            // One request at a time keeps this listener bounded. Each client
            // has its own short socket timeouts.
            for stream in listener.incoming().flatten() {
                handle(stream, &key, &nonces, &control, &runtime);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!("bot launcher link disabled: {error}");
        return None;
    }
    Some(bound)
}

fn handle(
    mut stream: TcpStream,
    key: &[u8],
    nonces: &Mutex<NonceWindow>,
    control: &UpdateControl,
    runtime: &tokio::runtime::Handle,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Ok(body) = control::read_frame(&mut stream) else {
        return;
    };
    let reply = match control::open_frame(key, &body, nonces) {
        Ok((_, _, expires_ms, _)) if control::now_ms() > expires_ms => ControlReply::Failed {
            code: Some("save_expired".into()),
            message: "request expired".into(),
        },
        Ok((op, _, _, _)) => runtime
            .block_on(control.apply_launcher_op(op))
            .unwrap_or_else(|error| control::failure_reply(&error)),
        Err(error) => ControlReply::Failed {
            code: None,
            message: control::public_error(&error),
        },
    };
    let _ = control::write_frame(&mut stream, &reply);
}
