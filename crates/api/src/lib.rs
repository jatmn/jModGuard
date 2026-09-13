//! HTTP dashboard and OAuth API. Production builds do not depend on the Discord bot.
pub use jmodguard::{
    bot_actions, config, db, defaults, health, ids, init_tracing, normalize, shutdown_signal,
    templates, tenant, types,
};
pub mod web;
