//! Shared guardrails for user-initiated destructive operations (v1.1, Epic 0).
//!
//! Every verb that can destroy data — drop/rename database, replace-restore, restore-on-top,
//! truncate, drop object — goes through this module, so the rules live in one place and are
//! tested once:
//!
//! 1. [`check_target`]: the connection's environment tag must allow destruction (Local or Dev,
//!    R18). Untagged connections are Production, so they are refused by default.
//! 2. [`require_typed_name`]: the user must have typed the exact database name. Verified here,
//!    server-side, so no UI bug can skip it — this is the confirmation that cannot be clicked
//!    through by accident.
//! 3. [`acquire_lock`]: take the same per-server lock Refresh runs use. Two swaps, or a manual
//!    drop racing a Refresh, would otherwise fight over the same databases and sessions.
//!
//! Ordering matters: refuse before anything is created, dropped or locked, and always before a
//! child process starts. Nothing here touches a server; the caller does that only after all
//! three pass.

use crate::lock::{RunLockGuard, RunLocks};
use pgwhy_core::config::{EnvTag, ServerKey};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use serde_json::json;

/// What a destructive action is about to touch, and how to name it in messages and the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestructiveContext {
    pub server: ServerKey,
    /// User-facing connection name, so a message never shows a bare host.
    pub connection_name: String,
    pub database: String,
    pub env_tag: EnvTag,
    /// Short verb phrase for the lock holder and messages, e.g. `drop of app_local`.
    pub action: String,
}

impl DestructiveContext {
    pub fn new(
        server: ServerKey,
        connection_name: impl Into<String>,
        database: impl Into<String>,
        env_tag: EnvTag,
        action: impl Into<String>,
    ) -> Self {
        Self {
            server,
            connection_name: connection_name.into(),
            database: database.into(),
            env_tag,
            action: action.into(),
        }
    }

    /// `<database> on <connection> (<tag>)` — for messages that must name the exact target.
    pub fn describe(&self) -> String {
        format!(
            "{} on {} ({})",
            self.database,
            self.connection_name,
            self.env_tag.label()
        )
    }
}

/// Refuse to destroy anything on a connection that is not tagged Local or Dev (R18).
///
/// There is deliberately no override: the tag is a property of the connection the user saved, not
/// a checkbox at confirmation time.
pub fn check_target(ctx: &DestructiveContext) -> AppResult<()> {
    if ctx.env_tag.allows_destructive_target() {
        return Ok(());
    }
    Err(AppError::new(
        ErrorKind::Refused,
        format!(
            "{} is tagged {}; the {} only runs against Local or Dev connections",
            ctx.describe(),
            ctx.env_tag.label(),
            ctx.action
        ),
    )
    .with_hint(
        "if this really is a local or development server, change the connection's environment tag \
         in Connections first",
    )
    .with_data(json!({ "env_tag": ctx.env_tag })))
}

/// Require the user to have typed the exact database name for this context.
///
/// Exact match, case-sensitive: Postgres database names are case-sensitive, and a lenient match
/// would let `App` confirm `app`.
pub fn require_typed_name(ctx: &DestructiveContext, typed: &str) -> AppResult<()> {
    if typed == ctx.database {
        return Ok(());
    }
    let message = if typed.trim().is_empty() {
        format!(
            "type the database name ({}) to confirm the {}",
            ctx.database, ctx.action
        )
    } else {
        format!(
            "\"{typed}\" does not match {}. Type the database name exactly to confirm the {}",
            ctx.database, ctx.action
        )
    };
    Err(AppError::new(ErrorKind::Refused, message)
        .with_hint("typing the name is the one confirmation that cannot be dismissed by accident"))
}

/// Take the target server's run lock under this action's name.
///
/// `RunLocks` is process-local; the app enforces a single instance, which is what makes it sound.
/// A refusal is retriable: the action can be started again once the holder finishes.
pub fn acquire_lock(locks: &RunLocks, ctx: &DestructiveContext) -> AppResult<RunLockGuard> {
    locks.try_acquire(&ctx.server, format!("the {}", ctx.action))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(tag: EnvTag) -> DestructiveContext {
        DestructiveContext::new(
            ServerKey {
                host: "db.local".into(),
                port: 5432,
                via: None,
            },
            "Local dev",
            "app_local",
            tag,
            "drop of app_local",
        )
    }

    #[test]
    fn only_local_and_dev_may_be_destroyed() {
        for tag in [EnvTag::Local, EnvTag::Dev] {
            assert!(check_target(&ctx(tag)).is_ok(), "{tag:?}");
        }
        for tag in [EnvTag::Staging, EnvTag::Production] {
            let e = check_target(&ctx(tag)).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Refused);
            assert!(
                e.message.contains("app_local on Local dev"),
                "message must name the exact target: {}",
                e.message
            );
            assert!(e.message.contains(tag.label()));
        }
    }

    #[test]
    fn typed_name_must_match_exactly() {
        let c = ctx(EnvTag::Local);
        assert!(require_typed_name(&c, "app_local").is_ok());
        for wrong in ["", "  ", "app", "App_Local", "app_local ", "app_other"] {
            let e = require_typed_name(&c, wrong).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Refused, "{wrong:?}");
        }
        // the message for a wrong name echoes what was typed, without treating it as a command
        let e = require_typed_name(&c, "app").unwrap_err();
        assert!(
            e.message.contains("does not match app_local"),
            "{}",
            e.message
        );
    }

    #[test]
    fn the_lock_is_taken_once_and_released_on_drop() {
        let locks = RunLocks::new();
        let c = ctx(EnvTag::Local);
        let guard = acquire_lock(&locks, &c).unwrap();
        assert_eq!(
            locks.holder(&c.server).as_deref(),
            Some("the drop of app_local")
        );

        let e = acquire_lock(&locks, &c).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Refused);
        assert!(e.is_retriable(), "a held lock must be retriable: {e:?}");
        assert!(e.message.contains("the drop of app_local"), "{}", e.message);

        drop(guard);
        assert!(acquire_lock(&locks, &c).is_ok());
    }

    #[test]
    fn locks_are_per_server() {
        let locks = RunLocks::new();
        let a = ctx(EnvTag::Local);
        let mut b = ctx(EnvTag::Local);
        b.server.host = "other.local".into();
        let _first = acquire_lock(&locks, &a).unwrap();
        assert!(acquire_lock(&locks, &b).is_ok());
    }
}
