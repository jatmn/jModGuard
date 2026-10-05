//! Discord gateway, moderation and bot-owned action execution.
pub use jmodguard::{
    bot_actions, config, db, defaults, health, ids, init_tracing, log_shutdown_finished, normalize,
    process_ready, set_process_ready, set_shutdown_context, shutdown_signal, templates, tenant,
    types,
};
pub mod action_worker;
pub mod activation;
pub mod automod;
pub mod cases;
pub mod commands;
pub mod discord;
pub mod enrollment_repair;
pub mod gateway_budget;
pub mod handlers;
pub mod image_spam;
pub mod logging;
pub mod member_directory;
pub mod member_sync;
pub mod nickname_worker;
pub mod snapshot_capture;
pub mod spam_actions;
pub mod startup;
pub mod state;
pub mod text_spam;
