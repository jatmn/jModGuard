//! Independent conductor entry point. Owner administration uses this process's
//! control store and Discord login. Guild credentials and tenant routing stay out.
pub mod backup;
pub mod config;
pub mod guild_move;
pub mod http;
pub mod notify;
pub mod settings;
pub mod state;
pub mod store;

use axum::Router;
use std::sync::Arc;

pub use state::Conductor;

pub fn router() -> Router {
    http::router(None)
}

pub fn router_for(state: Arc<Conductor>) -> Router {
    http::router(Some(state))
}
