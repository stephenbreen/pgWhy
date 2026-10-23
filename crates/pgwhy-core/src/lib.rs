//! pgWhy core: everything that talks to Postgres from inside the app.
//!
//! Deliberately free of any Tauri dependency so it can be tested standalone; `src-tauri` is a
//! thin command/event/channel layer over this crate.

pub mod config;
pub mod conn;
pub mod introspect;
pub mod query;
pub mod scripts;
pub mod secret;
pub mod tunnel;

pub use pgwhy_errors::{AppError, AppResult, ErrorClass, ErrorKind};
