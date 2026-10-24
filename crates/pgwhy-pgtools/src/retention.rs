//! Backup retention: keep the newest K backups in a folder (default 3) plus every
//! backup in the caller's protected set (backups referenced by retryable failed runs —
//! computed by the workflow layer). Protected backups never count against K and are never
//! deleted. The planner is pure; [`apply_retention`] does the filesystem work.

use pgwhy_errors::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Newest backups to keep (clamped to ≥ 1 so the backup just taken always survives).
    pub keep_last: usize,
    /// File-name suffixes that identify backups in the folder (others are never touched).
    pub suffixes: Vec<String>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_last: 3,
            suffixes: vec![".dump".into()],
        }
    }
}

/// A backup candidate found in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFile {
    pub path: PathBuf,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPlan {
    pub keep: Vec<PathBuf>,
    pub delete: Vec<PathBuf>,
}

/// Decide what to keep: protected files always; then the newest `keep_last` of the rest
/// (ties broken by path, descending, so timestamped names order deterministically).
pub fn plan_retention(
    mut files: Vec<BackupFile>,
    policy: &RetentionPolicy,
    protected: &HashSet<PathBuf>,
) -> RetentionPlan {
    let keep_n = policy.keep_last.max(1);
    files.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.path.cmp(&a.path))
    });
    let mut plan = RetentionPlan::default();
    let mut kept_unprotected = 0;
    for f in files {
        if is_protected(&f.path, protected) {
            plan.keep.push(f.path);
        } else if kept_unprotected < keep_n {
            kept_unprotected += 1;
            plan.keep.push(f.path);
        } else {
            plan.delete.push(f.path);
        }
    }
    plan
}

fn is_protected(path: &Path, protected: &HashSet<PathBuf>) -> bool {
    if protected.contains(path) {
        return true;
    }
    match std::fs::canonicalize(path) {
        Ok(c) => protected
            .iter()
            .any(|p| std::fs::canonicalize(p).is_ok_and(|pc| pc == c)),
        Err(_) => false,
    }
}

/// List backup files (and directory-format dumps) in `dir` matching the policy's suffixes.
pub fn scan_backups(dir: &Path, policy: &RetentionPolicy) -> AppResult<Vec<BackupFile>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(
                AppError::new(ErrorKind::Disk, "Could not read the backup folder")
                    .with_detail(format!("{}: {e}", dir.display())),
            )
        }
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !policy.suffixes.iter().any(|s| name.ends_with(s.as_str())) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        out.push(BackupFile {
            path: entry.path(),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    Ok(out)
}

/// Scan, plan and delete. Returns the executed plan; a failed delete is reported in the
/// error after attempting the rest.
pub fn apply_retention(
    dir: &Path,
    policy: &RetentionPolicy,
    protected: &HashSet<PathBuf>,
) -> AppResult<RetentionPlan> {
    let plan = plan_retention(scan_backups(dir, policy)?, policy, protected);
    let mut failures = Vec::new();
    for path in &plan.delete {
        let res = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(e) = res {
            if e.kind() != std::io::ErrorKind::NotFound {
                failures.push(format!("{}: {e}", path.display()));
            }
        }
    }
    if !failures.is_empty() {
        return Err(
            AppError::new(ErrorKind::Disk, "Some old backups could not be deleted")
                .with_detail(failures.join("\n")),
        );
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn f(name: &str, age_s: u64) -> BackupFile {
        BackupFile {
            path: PathBuf::from(format!("/b/{name}")),
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_s),
        }
    }

    #[test]
    fn keeps_newest_k_plus_protected() {
        let files = vec![f("a", 50), f("b", 40), f("c", 30), f("d", 20), f("e", 10)];
        let protected: HashSet<PathBuf> = [PathBuf::from("/b/a")].into();
        let plan = plan_retention(files, &RetentionPolicy::default(), &protected);
        assert_eq!(
            plan.keep,
            ["/b/e", "/b/d", "/b/c", "/b/a"].map(PathBuf::from).to_vec()
        );
        assert_eq!(plan.delete, vec![PathBuf::from("/b/b")]);
    }

    #[test]
    fn zero_k_still_keeps_newest() {
        let policy = RetentionPolicy {
            keep_last: 0,
            ..Default::default()
        };
        let plan = plan_retention(vec![f("a", 2), f("b", 1)], &policy, &HashSet::new());
        assert_eq!(plan.keep, vec![PathBuf::from("/b/b")]);
    }
}
