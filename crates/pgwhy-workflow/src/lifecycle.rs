//! User-initiated database lifecycle verbs (v1.1, Epic 4): create, rename, drop.
//!
//! These are not workflows — there is no source, no dump and no retry matrix — but they are the
//! first real callers of [`crate::destructive`] and they reuse the Refresh pipeline's own
//! primitives so the risky parts exist once:
//!
//! - **drain** (`swap::drain`): terminate sessions on the database and poll until none remain,
//!   failing retriably with the blocking pids/applications named;
//! - **in-use retry** (`swap::rename_with_drain` / `swap::drop_with_drain`): a client that
//!   reconnects between the drain and the DDL races the statement; retry through SQLSTATE 55006
//!   until the deadline rather than surfacing a confusing failure;
//! - **prepared transactions** are refused before a drop, never discarded (`swap::prepared_refusal`);
//! - **the per-server run lock** (`crate::lock`) so a manual drop cannot race a Refresh swap.
//!
//! Every verb writes an [`OperationRecord`] into the same run store a Refresh uses, rewritten at
//! each step boundary, so history answers "what did I just drop, and where is the backup".

use crate::destructive::{acquire_lock, check_target, require_typed_name, DestructiveContext};
use crate::engine::Engine;
use crate::history::{OperationDetail, OperationKind, OperationRecord, OperationStep, RunStatus};
use crate::staging::LocaleSpec;
use crate::swap;
use pgwhy_core::config::{ConnectionId, EnvTag, ServerKey};
use pgwhy_core::{AppError, AppResult, ErrorKind};
use std::path::PathBuf;
use std::time::Duration;

/// The server and database a verb acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbTarget {
    pub connection_id: ConnectionId,
    pub connection_name: String,
    pub server: ServerKey,
    pub env_tag: EnvTag,
    /// The database being created/renamed/dropped.
    pub database: String,
}

impl DbTarget {
    /// The guardrail context for this target and action (used by commands that must refuse
    /// *before* any side effect, e.g. before taking a pre-drop backup).
    pub fn context(&self, action: &str) -> DestructiveContext {
        DestructiveContext::new(
            self.server.clone(),
            self.connection_name.clone(),
            self.database.clone(),
            self.env_tag,
            action,
        )
    }
}

/// The refusal checks a drop must pass before the caller takes its safety backup or touches the
/// server. Re-checked inside [`Engine::drop_database`] — this is not a substitute for that.
pub fn check_drop(target: &DbTarget, typed_name: &str) -> AppResult<()> {
    let ctx = target.context(&format!("drop of {}", target.database));
    check_target(&ctx)?;
    require_typed_name(&ctx, typed_name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateDatabaseRequest {
    pub target: DbTarget,
    /// Encoding/locale for the new database; `None` uses the server's `template0` defaults.
    pub locale: Option<LocaleSpec>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameDatabaseRequest {
    pub target: DbTarget,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropDatabaseRequest {
    pub target: DbTarget,
    /// What the user typed to confirm. Verified here, not in the UI.
    pub typed_name: String,
    /// A backup taken just before the drop, when the user asked for one. Recorded so retention
    /// keeps it (see `retry::protected_backups_with_operations`).
    pub backup: Option<PathBuf>,
}

impl Engine {
    /// Create a database on the target's server (mode `template0`, locale optional).
    pub async fn create_database(&self, req: CreateDatabaseRequest) -> AppResult<OperationRecord> {
        let ctx = req.target.context("create");
        check_target(&ctx)?;
        let _lock = acquire_lock(self.locks(), &ctx)?;

        let mut rec = OperationRecord::new(
            OperationKind::Create,
            format!("Create database {}", req.target.database),
            OperationDetail::Create {
                name: req.target.database.clone(),
                locale: req.locale.clone(),
            },
        );
        rec.steps.push(OperationStep::pending(
            "create",
            format!("Create database {}", req.target.database),
        ));
        self.save_operation(&rec)?;

        let maint = self.maintenance(&req.target.connection_id).await?;
        let step = rec.step_mut("create").expect("pushed above");
        step.start();
        if let Some(comment) = req.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            step.note = Some(format!("comment: {comment}"));
        }
        self.save_operation(&rec)?;

        if let Err(e) = maint
            .create_database(&req.target.database, req.locale.as_ref())
            .await
        {
            return Err(self.fail(&mut rec, "create", e)?);
        }
        if let Some(comment) = req.comment.as_deref().filter(|c| !c.trim().is_empty()) {
            // The database exists now: a failed comment must not read as a failed create.
            if let Err(e) = maint.set_comment(&req.target.database, Some(comment)).await {
                rec.warnings.push(format!(
                    "the database was created, but setting its comment failed: {}",
                    e.message
                ));
                if let Some(step) = rec.step_mut("create") {
                    step.note = Some("created; comment was not set".into());
                }
            }
        }

        rec.step_mut("create").expect("pushed above").succeed();
        rec.status = RunStatus::Succeeded;
        rec.finished_at = Some(crate::staging::now_rfc3339());
        self.save_operation(&rec)?;
        Ok(rec)
    }

    /// Rename a database. Sessions on it are drained first, and a client that reconnects mid-way
    /// is retried through "being accessed by other users" until the drain deadline.
    pub async fn rename_database(&self, req: RenameDatabaseRequest) -> AppResult<OperationRecord> {
        let ctx = req.target.context("rename");
        check_target(&ctx)?;
        if req.to.trim().is_empty() {
            return Err(AppError::new(
                ErrorKind::Config,
                "the new database name cannot be empty",
            ));
        }
        if req.to == req.target.database {
            return Ok(OperationRecord::new(
                OperationKind::Rename,
                format!("Rename {} (no change)", req.target.database),
                OperationDetail::Rename {
                    database: req.target.database.clone(),
                    to: req.to.clone(),
                },
            ));
        }
        let _lock = acquire_lock(self.locks(), &ctx)?;

        let mut rec = OperationRecord::new(
            OperationKind::Rename,
            format!("Rename {} to {}", req.target.database, req.to),
            OperationDetail::Rename {
                database: req.target.database.clone(),
                to: req.to.clone(),
            },
        );
        rec.steps.push(OperationStep::pending(
            "rename",
            format!("Rename to {}", req.to),
        ));
        self.save_operation(&rec)?;

        let maint = self.maintenance(&req.target.connection_id).await?;
        rec.step_mut("rename").expect("pushed above").start();
        self.save_operation(&rec)?;

        let timeout = self.drain_timeout();
        if let Err(e) =
            swap::rename_with_drain(&*maint, &req.target.database, &req.to, timeout).await
        {
            return Err(self.fail(&mut rec, "rename", e)?);
        }

        rec.step_mut("rename").expect("pushed above").succeed();
        rec.status = RunStatus::Succeeded;
        rec.finished_at = Some(crate::staging::now_rfc3339());
        self.save_operation(&rec)?;
        Ok(rec)
    }

    /// Drop a database: typed-name confirmation, session drain, prepared-transaction refusal,
    /// then `DROP DATABASE`. The database is only dropped when it exists.
    pub async fn drop_database(&self, req: DropDatabaseRequest) -> AppResult<OperationRecord> {
        let ctx = req
            .target
            .context(&format!("drop of {}", req.target.database));
        check_target(&ctx)?;
        require_typed_name(&ctx, &req.typed_name)?;
        let _lock = acquire_lock(self.locks(), &ctx)?;

        let mut rec = OperationRecord::new(
            OperationKind::Drop,
            format!("Drop database {}", req.target.database),
            OperationDetail::Drop {
                database: req.target.database.clone(),
                backup: req.backup.clone(),
                connection: Some(crate::history::ConnectionRef {
                    id: req.target.connection_id.clone(),
                    name: req.target.connection_name.clone(),
                }),
            },
        );
        rec.steps.push(OperationStep::pending(
            "check",
            "Check for prepared transactions",
        ));
        rec.steps.push(OperationStep::pending(
            "drop",
            format!("Terminate sessions on {} and drop it", req.target.database),
        ));
        self.save_operation(&rec)?;

        let maint = self.maintenance(&req.target.connection_id).await?;

        rec.step_mut("check").expect("pushed above").start();
        let prepared = match maint.prepared_xacts(&req.target.database).await {
            Ok(p) => p,
            Err(e) => return Err(self.fail(&mut rec, "check", e)?),
        };
        if !prepared.is_empty() {
            let e = swap::prepared_refusal(&req.target.database, &prepared);
            return Err(self.fail(&mut rec, "check", e)?);
        }
        rec.step_mut("check").expect("pushed above").succeed();

        rec.step_mut("drop").expect("pushed above").start();
        self.save_operation(&rec)?;
        let timeout = self.drain_timeout();
        if let Err(e) = swap::drop_with_drain(&*maint, &req.target.database, timeout).await {
            return Err(self.fail(&mut rec, "drop", e)?);
        }

        rec.step_mut("drop").expect("pushed above").succeed();
        rec.status = RunStatus::Succeeded;
        rec.finished_at = Some(crate::staging::now_rfc3339());
        self.save_operation(&rec)?;
        Ok(rec)
    }

    /// Record a failed step, persist it, and hand the error back to the caller.
    fn fail(&self, rec: &mut OperationRecord, step: &str, error: AppError) -> AppResult<AppError> {
        if let Some(s) = rec.step_mut(step) {
            s.fail(error.clone());
        }
        rec.status = RunStatus::FailedAtStep;
        rec.failed_step = Some(step.to_string());
        rec.finished_at = Some(crate::staging::now_rfc3339());
        rec.error = Some(error.clone());
        self.save_operation(rec)?;
        Ok(error)
    }

    fn drain_timeout(&self) -> Duration {
        self.config().session_drain_timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(tag: EnvTag, db: &str) -> DbTarget {
        DbTarget {
            connection_id: ConnectionId::new(),
            connection_name: "Local dev".into(),
            server: ServerKey {
                host: "127.0.0.1".into(),
                port: 5432,
                via: None,
            },
            env_tag: tag,
            database: db.into(),
        }
    }

    #[test]
    fn context_names_the_exact_target_and_action() {
        let ctx = target(EnvTag::Production, "prod_app").context("drop of prod_app");
        assert_eq!(ctx.describe(), "prod_app on Local dev (Production)");
        let e = check_target(&ctx).unwrap_err();
        assert!(e.message.contains("only runs against Local or Dev"));
    }
}
