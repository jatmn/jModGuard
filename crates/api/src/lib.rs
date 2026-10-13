//! HTTP dashboard and OAuth API. Production builds do not depend on the Discord bot.
pub use jmodguard::{
    bot_actions, config, db, defaults, health, ids, init_tracing, log_shutdown_finished, normalize,
    process_ready, set_process_ready, set_shutdown_context, shutdown_signal, templates, tenant,
    types,
};
pub mod process_ready_test_lock;
pub mod startup;
pub mod web;
