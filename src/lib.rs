//! Orbit's WhatsApp-only core.
//!
//! The crate deliberately keeps WhatsApp-specific process integration at the
//! edge. Raw source events are retained, while the rest of the application
//! reads a normalized projection that can survive connector upgrades.

pub mod config;
pub mod daemon;
pub mod install;
pub mod ipc;
pub mod model;
pub mod store;
pub mod tui;
pub mod wacli;
pub mod webhook;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const WACLI_VERSION: &str = "0.15.0";
