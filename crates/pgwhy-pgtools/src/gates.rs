//! Version gates: refuse to start — with a message saying what is wrong, how to
//! fix it, and where pgWhy looked — instead of failing mid-backup.
//!
//! - pg_dump major must be ≥ the source server major (pg_dump refuses newer servers).
//! - pg_restore major must be ≥ the pg_dump that produced the archive (`Dumped by pg_dump
//!   version` in the TOC, see [`crate::toc`]).
//! - Restoring into a target server older than the source is a warning, not a refusal.

use crate::discover::{DiscoveredTool, PgVersion, ToolKind, ToolRegistry};
use pgwhy_errors::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Select the pg_dump to back up a server of `source_major`, or refuse.
pub fn require_pg_dump(registry: &ToolRegistry, source_major: u32) -> AppResult<DiscoveredTool> {
    if let Some(tool) = registry.select(ToolKind::PgDump, source_major) {
        return Ok(tool.clone());
    }
    Err(refusal(
        registry,
        ToolKind::PgDump,
        source_major,
        |found| format!("pg_dump {found} cannot back up a PostgreSQL {source_major} server"),
        "pg_dump must be the same major version as the server, or newer".to_string(),
    ))
}

/// Select the pg_restore to read an archive written by pg_dump `dumped_by`, or refuse.
pub fn require_pg_restore(
    registry: &ToolRegistry,
    dumped_by: &PgVersion,
) -> AppResult<DiscoveredTool> {
    let need = dumped_by.major;
    if let Some(tool) = registry.select(ToolKind::PgRestore, need) {
        return Ok(tool.clone());
    }
    Err(refusal(
        registry,
        ToolKind::PgRestore,
        need,
        |found| format!("pg_restore {found} cannot restore a backup made by pg_dump {need}"),
        format!(
            "pg_restore must be at least the version of the pg_dump that made the backup ({})",
            dumped_by.raw
        ),
    ))
}

fn refusal(
    registry: &ToolRegistry,
    kind: ToolKind,
    need: u32,
    mismatch_message: impl FnOnce(u32) -> String,
    rule: String,
) -> AppError {
    let found: Vec<serde_json::Value> = registry
        .of_kind(kind)
        .map(|t| json!({ "path": t.path, "version": t.version.raw, "major": t.version.major }))
        .collect();
    let looked = looked_in(registry);
    let data = json!({
        "tool": kind.name(),
        "required_major": need,
        "found": found,
        "searched": registry.searched,
    });
    let install = install_hint(need);
    match registry.highest(kind) {
        None => AppError::new(
            ErrorKind::ToolMissing,
            format!("{kind} was not found on this computer"),
        )
        .with_hint(format!(
            "{install}, or add its bin folder in Settings → PostgreSQL tools. pgWhy looked in: {looked}"
        ))
        .with_detail(format!("Searched: {looked}"))
        .with_data(data),
        Some(best) => {
            let listing = registry
                .of_kind(kind)
                .map(|t| format!("{} {} ({})", kind, t.version.raw, t.path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            AppError::new(ErrorKind::ToolVersion, mismatch_message(best.version.major))
                .with_hint(format!(
                    "{rule}. {install}; pgWhy will pick it up automatically. Found: {listing}"
                ))
                .with_detail(format!("Found: {listing}. Searched: {looked}"))
                .with_data(data)
        }
    }
}

fn looked_in(registry: &ToolRegistry) -> String {
    if registry.searched.is_empty() {
        return "(no directories)".into();
    }
    registry
        .searched
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn install_hint(major: u32) -> String {
    if cfg!(target_os = "macos") {
        format!("Install PostgreSQL {major} client tools (e.g. `brew install postgresql@{major}` or Postgres.app)")
    } else if cfg!(windows) {
        format!("Install PostgreSQL {major} client tools (the EDB installer's \"Command Line Tools\" component)")
    } else {
        format!("Install PostgreSQL {major} client tools (e.g. `postgresql-client-{major}` from the PGDG repository)")
    }
}

/// A non-blocking preflight finding shown alongside the confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateWarning {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
}

/// Warn when restoring into a server older than the source (unsupported by PostgreSQL and
/// frequently fails on newer syntax), without refusing.
pub fn target_version_warning(source_major: u32, target_major: u32) -> Option<GateWarning> {
    (target_major < source_major).then(|| GateWarning {
        code: "target_older_than_source".into(),
        message: format!(
            "The target server is PostgreSQL {target_major}, older than the source (PostgreSQL {source_major}); restoring a newer dump into an older server is unsupported and may fail"
        ),
        hint: Some(format!(
            "Upgrade the target server to PostgreSQL {source_major} or newer if the restore fails"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::DiscoverySource;
    use std::path::PathBuf;

    fn reg(tools: &[(ToolKind, u32)]) -> ToolRegistry {
        ToolRegistry {
            tools: tools
                .iter()
                .map(|&(kind, major)| DiscoveredTool {
                    kind,
                    path: PathBuf::from(format!("/opt/pg{major}/bin/{}", kind.name())),
                    version: PgVersion {
                        major,
                        minor: 3,
                        raw: format!("{major}.3"),
                    },
                    source: DiscoverySource::Standard,
                })
                .collect(),
            searched: vec![PathBuf::from("/opt/pg15/bin"), PathBuf::from("/usr/bin")],
        }
    }

    #[test]
    fn pg_dump_too_old_refuses_with_exact_message() {
        let err = require_pg_dump(&reg(&[(ToolKind::PgDump, 15)]), 17).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ToolVersion);
        assert_eq!(
            err.message,
            "pg_dump 15 cannot back up a PostgreSQL 17 server"
        );
        let hint = err.hint.unwrap();
        assert!(hint.contains("PostgreSQL 17 client tools"), "{hint}");
        assert!(hint.contains("/opt/pg15/bin/pg_dump"), "{hint}");
        assert!(err.detail.unwrap().contains("/usr/bin"));
    }

    #[test]
    fn missing_tool_names_where_it_looked() {
        let err = require_pg_dump(&reg(&[]), 16).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ToolMissing);
        assert_eq!(err.message, "pg_dump was not found on this computer");
        assert!(err.hint.unwrap().contains("/opt/pg15/bin, /usr/bin"));
        assert_eq!(err.data.unwrap()["searched"][1], "/usr/bin");
    }

    #[test]
    fn pg_restore_gate_uses_dump_version() {
        let r = reg(&[(ToolKind::PgRestore, 16), (ToolKind::PgDump, 18)]);
        let v17 = PgVersion::parse("17.2").unwrap();
        let err = require_pg_restore(&r, &v17).unwrap_err();
        assert_eq!(
            err.message,
            "pg_restore 16 cannot restore a backup made by pg_dump 17"
        );
        let v16 = PgVersion::parse("16.15 (Homebrew)").unwrap();
        assert_eq!(require_pg_restore(&r, &v16).unwrap().version.major, 16);
    }

    #[test]
    fn older_target_warns_not_refuses() {
        assert!(target_version_warning(16, 16).is_none());
        assert!(target_version_warning(15, 17).is_none());
        let w = target_version_warning(17, 15).unwrap();
        assert!(w
            .message
            .contains("PostgreSQL 15, older than the source (PostgreSQL 17)"));
    }
}
