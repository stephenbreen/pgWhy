//! pgWhy pg client tools engine: discovery, version gates, child runner, pgpass lifecycle,
//! archive TOC parsing, progress parsing, and backup retention.
//!
//! Never depends on `pgwhy-core`: callers hand it fully-resolved endpoints ([`ToolEndpoint`]),
//! tunnel ports, and credential material ([`ToolPassword`]). The typical flow for a backup:
//!
//! 1. [`discover::discover`] once (cheap enough to repeat on a "rescan" click) → [`ToolRegistry`]
//! 2. [`gates::require_pg_dump`] with the source server's major → the version-matched tool, or
//!    an actionable refusal (AE5) before anything starts
//! 3. [`ops::backup`] with a [`RunContext`] (pgpass dir + cancel handle) and a progress callback
//! 4. [`toc::inspect_archive`] to record the dump version + reusability verdict
//! 5. [`retention::apply_retention`] with the workflow layer's protected set
//!
//! Credentials reach children only through a per-run temp `PGPASSFILE` ([`pgpass`]); argv and
//! the child environment never carry a password (R20, AE8).

pub mod discover;
pub mod endpoint;
pub mod gates;
pub mod ops;
pub mod pgpass;
pub mod progress;
pub mod retention;
pub mod runner;
mod sysroots;
pub mod toc;

pub use discover::{discover, DiscoveredTool, DiscoveryOptions, PgVersion, ToolKind, ToolRegistry};
pub use endpoint::{table_pattern, ToolEndpoint, ToolPassword};
pub use gates::{require_pg_dump, require_pg_restore, target_version_warning, GateWarning};
pub use ops::{
    backup, dump_schema_ddl, restore, BackupOptions, BackupOutcome, DdlOptions, DumpFormat,
    ErrorPolicy, RestoreOptions, RestoreOutcome,
};
pub use pgpass::{sweep_stale_pgpass, PgPassDir, PgPassFile};
pub use progress::{ProgressEvent, ProgressPhase};
pub use retention::{apply_retention, plan_retention, RetentionPlan, RetentionPolicy};
pub use runner::{CancelHandle, FailureCause, RunContext, ToolRun};
pub use toc::{inspect_archive, parse_toc, ArchiveCheck, ArchiveInspection, Toc, TocEntry};

pub use pgwhy_errors::{AppError, AppResult, ErrorClass, ErrorKind};
