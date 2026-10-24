//! Locating pg client tools: `pg_config --bindir`, standard per-OS install paths, and a PATH
//! scan, each binary's `--version` parsed into a registry of available majors.
//!
//! Discovery never fails: an empty registry is a valid answer, and the gates in
//! [`crate::gates`] turn "nothing suitable" into an actionable refusal that lists every
//! directory searched ([`ToolRegistry::searched`]).

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The client tools pgWhy drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    PgDump,
    PgRestore,
}

impl ToolKind {
    pub const ALL: [ToolKind; 2] = [ToolKind::PgDump, ToolKind::PgRestore];

    pub fn name(self) -> &'static str {
        match self {
            ToolKind::PgDump => "pg_dump",
            ToolKind::PgRestore => "pg_restore",
        }
    }

    /// Executable file name on this OS.
    pub fn exe_name(self) -> String {
        exe(self.name())
    }
}

impl fmt::Display for ToolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

fn exe(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// A PostgreSQL version as printed by tools and servers. For pre-10 releases (`9.6.24`)
/// `major` is 9 and `minor` 6 — irrelevant for gating, since the supported range is 13+.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PgVersion {
    pub major: u32,
    pub minor: u32,
    /// The text it was parsed from (e.g. `16.15 (Homebrew)`), for display.
    pub raw: String,
}

impl PgVersion {
    /// Parse the first `N[.M]` token of a version string: `16.15 (Homebrew)`, `17beta2`,
    /// `9.6.24`, `18devel`.
    pub fn parse(text: &str) -> Option<PgVersion> {
        let text = text.trim();
        let start = text.find(|c: char| c.is_ascii_digit())?;
        let rest = &text[start..];
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .next()?
            .split('.')
            .filter(|s| !s.is_empty());
        let major: u32 = nums.next()?.parse().ok()?;
        let minor: u32 = nums.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        Some(PgVersion {
            major,
            minor,
            raw: text.to_string(),
        })
    }

    /// Parse a tool's `--version` output: `pg_dump (PostgreSQL) 16.15 (Homebrew)`.
    pub fn parse_tool_output(output: &str) -> Option<PgVersion> {
        let line = output.lines().find(|l| !l.trim().is_empty())?;
        let after = match line.find(')') {
            Some(i) if line[..i].contains("PostgreSQL") => &line[i + 1..],
            _ => line,
        };
        let mut v = PgVersion::parse(after)?;
        v.raw = after.trim().to_string();
        Some(v)
    }

    /// Server major from `server_version_num` (e.g. `170002` → 17, `90624` → 9).
    pub fn major_from_version_num(num: u32) -> u32 {
        // 10+ is MMmmmm; 9.x is Mmmpp — both put the (first) major digits above 10_000.
        num / 10_000
    }

    fn order(&self, other: &PgVersion) -> Ordering {
        (self.major, self.minor).cmp(&(other.major, other.minor))
    }
}

impl fmt::Display for PgVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Where a tool was found — shown in the tools list and refusal details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// Caller-supplied directory (user-configured tools folder).
    Configured,
    /// `pg_config --bindir`.
    PgConfig,
    /// A well-known per-OS install location.
    Standard,
    /// A `PATH` entry.
    Path,
}

/// One usable client binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredTool {
    pub kind: ToolKind,
    /// Path to execute (as found, not canonicalized, so Homebrew `opt/` links stay readable).
    pub path: PathBuf,
    pub version: PgVersion,
    pub source: DiscoverySource,
}

/// What discovery found plus where it looked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolRegistry {
    pub tools: Vec<DiscoveredTool>,
    /// Every directory examined, in search order (for "where it looked" refusals).
    pub searched: Vec<PathBuf>,
}

impl ToolRegistry {
    pub fn of_kind(&self, kind: ToolKind) -> impl Iterator<Item = &DiscoveredTool> {
        self.tools.iter().filter(move |t| t.kind == kind)
    }

    /// Selection rule: major ≥ `min_major`; prefer an exact major match (newest minor),
    /// else the highest compatible version.
    pub fn select(&self, kind: ToolKind, min_major: u32) -> Option<&DiscoveredTool> {
        let compatible = || self.of_kind(kind).filter(|t| t.version.major >= min_major);
        compatible()
            .filter(|t| t.version.major == min_major)
            .max_by(|a, b| a.version.order(&b.version))
            .or_else(|| compatible().max_by(|a, b| a.version.order(&b.version)))
    }

    /// The newest available tool of a kind (used to read archives of unknown version).
    pub fn highest(&self, kind: ToolKind) -> Option<&DiscoveredTool> {
        self.of_kind(kind)
            .max_by(|a, b| a.version.order(&b.version))
    }

    /// Distinct majors available for a kind, ascending.
    pub fn majors(&self, kind: ToolKind) -> Vec<u32> {
        let mut m: Vec<u32> = self.of_kind(kind).map(|t| t.version.major).collect();
        m.sort_unstable();
        m.dedup();
        m
    }
}

/// Inputs to [`discover`]; the defaults search the real machine.
#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// User-configured tool directories, searched first.
    pub extra_dirs: Vec<PathBuf>,
    /// `PATH` value to scan; `None` reads the process environment.
    pub path_env: Option<OsString>,
    /// Run `pg_config --bindir` (the first `pg_config` found on the search path).
    pub use_pg_config: bool,
    /// Search the standard per-OS install locations.
    pub use_standard_dirs: bool,
    /// Per-binary `--version` probe timeout.
    pub probe_timeout: Duration,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            extra_dirs: Vec::new(),
            path_env: None,
            use_pg_config: true,
            use_standard_dirs: true,
            probe_timeout: Duration::from_secs(5),
        }
    }
}

impl DiscoveryOptions {
    /// Only the given directories and PATH value — for tests and "use exactly this folder".
    pub fn isolated(dirs: Vec<PathBuf>, path_env: Option<OsString>) -> Self {
        Self {
            extra_dirs: dirs,
            path_env: Some(path_env.unwrap_or_default()),
            use_pg_config: false,
            use_standard_dirs: false,
            ..Self::default()
        }
    }
}

/// Search all configured locations and probe every pg_dump/pg_restore found. Blocking
/// (spawns `--version` probes); call from `spawn_blocking` in async contexts.
pub fn discover(opts: &DiscoveryOptions) -> ToolRegistry {
    let path_dirs: Vec<PathBuf> = match &opts.path_env {
        Some(p) => std::env::split_paths(p).collect(),
        None => std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default(),
    };

    let mut candidates: Vec<(PathBuf, DiscoverySource)> = opts
        .extra_dirs
        .iter()
        .map(|d| (d.clone(), DiscoverySource::Configured))
        .collect();

    if opts.use_pg_config {
        let search: Vec<PathBuf> = path_dirs
            .iter()
            .cloned()
            .chain(if opts.use_standard_dirs {
                standard_dirs()
            } else {
                Vec::new()
            })
            .collect();
        if let Some(bindir) = pg_config_bindir(&search, opts.probe_timeout) {
            candidates.push((bindir, DiscoverySource::PgConfig));
        }
    }
    if opts.use_standard_dirs {
        candidates.extend(
            standard_dirs()
                .into_iter()
                .map(|d| (d, DiscoverySource::Standard)),
        );
    }
    candidates.extend(path_dirs.into_iter().map(|d| (d, DiscoverySource::Path)));

    let mut registry = ToolRegistry::default();
    let mut seen_dirs = HashSet::new();
    let mut seen_bins = HashSet::new();
    for (dir, source) in candidates {
        if dir.as_os_str().is_empty() || !seen_dirs.insert(dir.clone()) {
            continue;
        }
        registry.searched.push(dir.clone());
        for kind in ToolKind::ALL {
            let bin = dir.join(kind.exe_name());
            if !bin.is_file() {
                continue;
            }
            // Homebrew links one Cellar binary from several dirs; probe it once.
            let identity = std::fs::canonicalize(&bin).unwrap_or_else(|_| bin.clone());
            if !seen_bins.insert(identity) {
                continue;
            }
            match probe_version(&bin, opts.probe_timeout) {
                Some(version) => registry.tools.push(DiscoveredTool {
                    kind,
                    path: bin,
                    version,
                    source,
                }),
                None => tracing::debug!(path = %bin.display(), "pg tool --version probe failed"),
            }
        }
    }
    registry
}

/// Run `<bin> --version` with a timeout and parse the result.
pub fn probe_version(bin: &Path, timeout: Duration) -> Option<PgVersion> {
    run_with_timeout(bin, &["--version"], timeout)
        .and_then(|out| PgVersion::parse_tool_output(&out))
}

fn pg_config_bindir(search: &[PathBuf], timeout: Duration) -> Option<PathBuf> {
    let name = exe("pg_config");
    let bin = search.iter().map(|d| d.join(&name)).find(|p| p.is_file())?;
    let out = run_with_timeout(&bin, &["--bindir"], timeout)?;
    let dir = PathBuf::from(out.trim());
    dir.is_dir().then_some(dir)
}

fn run_with_timeout(bin: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Well-known install locations for this OS, newest-first where versioned.
pub fn standard_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        for prefix in ["/opt/homebrew/opt", "/usr/local/opt"] {
            dirs.extend(versioned_children(Path::new(prefix), "postgresql@", "bin"));
            dirs.push(PathBuf::from(prefix).join("postgresql/bin"));
            dirs.push(PathBuf::from(prefix).join("libpq/bin"));
        }
        dirs.push("/opt/homebrew/bin".into());
        dirs.push("/usr/local/bin".into());
        dirs.extend(versioned_children(
            Path::new("/Applications/Postgres.app/Contents/Versions"),
            "",
            "bin",
        ));
        dirs.extend(versioned_children(
            Path::new("/Library/PostgreSQL"),
            "",
            "bin",
        ));
        dirs.push("/usr/local/pgsql/bin".into());
    } else if cfg!(windows) {
        for base in [
            std::env::var_os("ProgramFiles").map(PathBuf::from),
            std::env::var_os("ProgramFiles(x86)").map(PathBuf::from),
            Some(PathBuf::from(r"C:\Program Files")),
        ]
        .into_iter()
        .flatten()
        {
            dirs.extend(versioned_children(&base.join("PostgreSQL"), "", "bin"));
        }
    } else {
        dirs.extend(versioned_children(
            Path::new("/usr/lib/postgresql"),
            "",
            "bin",
        ));
        dirs.extend(versioned_children(Path::new("/usr"), "pgsql-", "bin"));
        dirs.push("/usr/local/pgsql/bin".into());
        dirs.push("/usr/bin".into());
        dirs.push("/usr/local/bin".into());
    }
    let mut seen = HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// `<parent>/<prefix><version>/<leaf>` for every child whose name starts with `prefix`
/// followed by a digit, highest version first.
fn versioned_children(parent: &Path, prefix: &str, leaf: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<(PgVersion, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(prefix)?;
            if !rest.starts_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            Some((PgVersion::parse(rest)?, e.path().join(leaf)))
        })
        .collect();
    found.sort_by(|a, b| b.0.order(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_version_output() {
        let v = PgVersion::parse_tool_output("pg_dump (PostgreSQL) 16.15 (Homebrew)\n").unwrap();
        assert_eq!((v.major, v.minor), (16, 15));
        assert_eq!(v.raw, "16.15 (Homebrew)");
        let v = PgVersion::parse_tool_output("pg_restore (PostgreSQL) 18beta1").unwrap();
        assert_eq!((v.major, v.minor), (18, 0));
        let v = PgVersion::parse_tool_output("pg_dump (PostgreSQL) 9.6.24").unwrap();
        assert_eq!((v.major, v.minor), (9, 6));
        assert!(PgVersion::parse_tool_output("garbage").is_none());
        assert!(PgVersion::parse_tool_output("").is_none());
    }

    #[test]
    fn version_num_to_major() {
        assert_eq!(PgVersion::major_from_version_num(170_002), 17);
        assert_eq!(PgVersion::major_from_version_num(130_015), 13);
    }

    fn tool(kind: ToolKind, major: u32, minor: u32) -> DiscoveredTool {
        DiscoveredTool {
            kind,
            path: PathBuf::from(format!("/x/{major}.{minor}/{}", kind.name())),
            version: PgVersion {
                major,
                minor,
                raw: format!("{major}.{minor}"),
            },
            source: DiscoverySource::Standard,
        }
    }

    #[test]
    fn selection_prefers_exact_then_highest_compatible() {
        let reg = ToolRegistry {
            tools: vec![
                tool(ToolKind::PgDump, 15, 4),
                tool(ToolKind::PgDump, 16, 1),
                tool(ToolKind::PgDump, 16, 15),
                tool(ToolKind::PgDump, 18, 0),
                tool(ToolKind::PgRestore, 17, 2),
            ],
            searched: vec![],
        };
        let pick = |m| {
            reg.select(ToolKind::PgDump, m)
                .map(|t| t.version.to_string())
        };
        assert_eq!(pick(16).as_deref(), Some("16.15"));
        assert_eq!(pick(17).as_deref(), Some("18.0"));
        assert_eq!(pick(14).as_deref(), Some("18.0"));
        assert_eq!(pick(15).as_deref(), Some("15.4"));
        assert_eq!(pick(19), None);
        assert_eq!(
            reg.select(ToolKind::PgRestore, 16).map(|t| t.version.major),
            Some(17)
        );
        assert_eq!(reg.majors(ToolKind::PgDump), vec![15, 16, 18]);
    }
}
