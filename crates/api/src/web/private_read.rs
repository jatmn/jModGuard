//! Authenticated container-use readings for the bot. Not session/OAuth gated.

use crate::web::state::ApiState;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use jmodguard::control::{NonceWindow, load_writer_key};
use jmodguard::read_link::{container_read_response, open_read};
use std::sync::{Arc, Mutex, OnceLock};

fn nonce_window() -> &'static Mutex<NonceWindow> {
    static WINDOW: OnceLock<Mutex<NonceWindow>> = OnceLock::new();
    WINDOW.get_or_init(|| Mutex::new(NonceWindow::new()))
}

pub async fn container_use(State(app): State<Arc<ApiState>>, body: Bytes) -> Response {
    let secret = app.cfg.bot.control_secret.as_str();
    let key = match load_writer_key(secret) {
        Ok(key) => key,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if open_read(&key, &body, nonce_window()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !jmodguard::process_ready() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Json(container_read_response()).into_response()
}
