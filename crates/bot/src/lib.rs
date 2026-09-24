//! Discord gateway, moderation and bot-owned action execution.
pub use jmodguard::{
    bot_actions, config, db, defaults, health, ids, init_tracing, normalize, shutdown_signal,
    templates, tenant, types,
};
pub mod action_worker;
pub mod automod;
pub mod cases;
pub mod commands;
pub mod discord;
pub mod enrollment_repair;
pub mod handlers;
pub mod image_spam;
pub mod logging;
pub mod member_directory;
pub mod member_sync;
pub mod nickname_worker;
pub mod spam_actions;
pub mod state;
pub mod text_spam;
