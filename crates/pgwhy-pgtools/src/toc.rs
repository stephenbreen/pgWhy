//! Archive TOC parsing (`pg_restore --list`) and the archive reusability verdict.
//!
//! The header's `Dumped by pg_dump version` feeds the pg_restore gate; the entry count is the
//! exact restore-progress total; and whether the archive can be read at all decides if a
//! failed run's backup may be reused on retry (retry matrix: archive-corrupt → re-dump).

use crate::discover::{DiscoveredTool, PgVersion, ToolKind};
use crate::runner::{run_child, ChildSpec, FailureCause, RunContext};
use pgwhy_errors::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Header fields from the `;`-comment block of `pg_restore -l`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TocHeader {
    pub archive_created: Option<String>,
    pub dbname: Option<String>,
    pub toc_entries: Option<u32>,
    pub compression: Option<String>,
    /// Archive format version (`1.15-0`).
    pub dump_version: Option<String>,
    pub format: Option<String>,
    pub dumped_from: Option<PgVersion>,
    pub dumped_by: Option<PgVersion>,
}

/// One TOC line: `3420; 0 16384 TABLE DATA public t1 postgres`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TocEntry {
    pub dump_id: u32,
    pub table_oid: u32,
    pub oid: u32,
    /// Object type (`TABLE DATA`, `INDEX`, `FK CONSTRAINT`, ...).
    pub desc: String,
    /// Schema (`None` for `-`); names containing spaces make this best-effort.
    pub schema: Option<String>,
    /// Remaining text after desc + schema (name and owner), as printed.
    pub rest: String,
    pub raw: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toc {
    pub header: TocHeader,
    pub entries: Vec<TocEntry>,
}

impl Toc {
    /// Entries pg_restore will process given ownership/privilege options — the restore
    /// progress total. ACL entries are skipped under `--no-privileges`.
    pub fn restorable_count(&self, no_privileges: bool) -> u64 {
        self.entries
            .iter()
            .filter(|e| !(no_privileges && (e.desc == "ACL" || e.desc == "DEFAULT ACL")))
            .count() as u64
    }

    /// Count of entries of one type (e.g. `TABLE DATA`).
    pub fn count_of(&self, desc: &str) -> usize {
        self.entries.iter().filter(|e| e.desc == desc).count()
    }
}

/// Multi-word TOC descriptors, longest first so `MATERIALIZED VIEW DATA` wins over
/// `MATERIALIZED VIEW`.
const MULTI_WORD_DESCS: &[&str] = &[
    "MATERIALIZED VIEW DATA",
    "TEXT SEARCH CONFIGURATION",
    "TEXT SEARCH DICTIONARY",
    "TEXT SEARCH TEMPLATE",
    "TEXT SEARCH PARSER",
    "PUBLICATION TABLES IN SCHEMA",
    "LARGE OBJECT METADATA",
    "PUBLICATION TABLE",
    "MATERIALIZED VIEW",
    "CHECK CONSTRAINT",
    "FOREIGN DATA WRAPPER",
    "FOREIGN TABLE",
    "OPERATOR CLASS",
    "OPERATOR FAMILY",
    "ACCESS METHOD",
    "DEFAULT ACL",
    "EVENT TRIGGER",
    "FK CONSTRAINT",
    "INDEX ATTACH",
    "LARGE OBJECT",
    "PROCEDURAL LANGUAGE",
    "ROW SECURITY",
    "SEQUENCE OWNED BY",
    "SEQUENCE SET",
    "SHELL TYPE",
    "TABLE ATTACH",
    "TABLE DATA",
    "BLOB METADATA",
    "BLOBS",
    "STATISTICS DATA",
    "USER MAPPING",
    "DATABASE PROPERTIES",
];

/// Parse `pg_restore -l` output. Lines that aren't entries or known header keys are ignored
/// (tolerant of drift); an output with neither a header nor entries is an error.
pub fn parse_toc(text: &str) -> AppResult<Toc> {
    let mut toc = Toc::default();
    let mut saw_header = false;
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(comment) = line.strip_prefix(';') {
            let c = comment.trim();
            if let Some((key, value)) = c.split_once(':') {
                let value = value.trim();
                let h = &mut toc.header;
                match key.trim() {
                    "Archive created at" => h.archive_created = Some(value.into()),
                    "dbname" => h.dbname = Some(value.into()),
                    "TOC Entries" => h.toc_entries = value.parse().ok(),
                    "Compression" => h.compression = Some(value.into()),
                    "Dump Version" => h.dump_version = Some(value.into()),
                    "Format" => h.format = Some(value.into()),
                    "Dumped from database version" => h.dumped_from = PgVersion::parse(value),
                    "Dumped by pg_dump version" => h.dumped_by = PgVersion::parse(value),
                    _ => continue,
                }
                saw_header = true;
            }
            continue;
        }
        if let Some(entry) = parse_entry(line) {
            toc.entries.push(entry);
        }
    }
    if !saw_header && toc.entries.is_empty() {
        return Err(AppError::new(
            ErrorKind::ToolFailed,
            "pg_restore produced no table of contents for this file",
        ));
    }
    Ok(toc)
}

fn parse_entry(line: &str) -> Option<TocEntry> {
    let (id, rest) = line.split_once(';')?;
    let dump_id: u32 = id.trim().parse().ok()?;
    let mut it = rest.trim_start().splitn(3, ' ');
    let table_oid: u32 = it.next()?.parse().ok()?;
    let oid: u32 = it.next()?.parse().ok()?;
    let tail = it.next().unwrap_or("");
    let (desc, after) = MULTI_WORD_DESCS
        .iter()
        .find_map(|d| {
            tail.strip_prefix(d)
                .filter(|r| r.is_empty() || r.starts_with(' '))
                .map(|r| (d.to_string(), r.trim_start()))
        })
        .unwrap_or_else(|| match tail.split_once(' ') {
            Some((d, r)) => (d.to_string(), r),
            None => (tail.to_string(), ""),
        });
    let (schema, rest) = match after.split_once(' ') {
        Some(("-", r)) => (None, r.to_string()),
        Some((s, r)) => (Some(s.to_string()), r.to_string()),
        None => (None, after.to_string()),
    };
    Some(TocEntry {
        dump_id,
        table_oid,
        oid,
        desc,
        schema,
        rest,
        raw: line.to_string(),
    })
}

/// The fixed header of a custom-format archive (`pg_dump --format=custom`, which is what pgAdmin's
/// "Custom" backup format runs).
///
/// pgWhy reads this itself because `pg_restore` can't: one older than the archive stops at
/// "unsupported version" without saying which pg_dump wrote it, and that version is what the user
/// has to install. Everything else about an archive still comes from `pg_restore --list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveHeader {
    /// Archive format version (`1.16-0`).
    pub dump_version: String,
    pub dbname: Option<String>,
    pub dumped_from: Option<PgVersion>,
    pub dumped_by: Option<PgVersion>,
}

/// Enough for the header: its only variable parts are a database name and two version strings.
const ARCHIVE_HEADER_LIMIT: u64 = 4096;

/// [`parse_archive_header`] on the start of `path`. `None` for anything unreadable.
pub fn read_archive_header(path: &Path) -> Option<ArchiveHeader> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(ARCHIVE_HEADER_LIMIT)
        .read_to_end(&mut head)
        .ok()?;
    parse_archive_header(&head)
}

/// Parse the header written by pg_dump's `WriteHead`. `None` when these bytes aren't one: the file
/// can be anything the user picked, and `pg_restore` has the final say on what it can read.
pub fn parse_archive_header(bytes: &[u8]) -> Option<ArchiveHeader> {
    let mut r = HeaderReader { bytes, int_size: 0 };
    if r.take(5)? != b"PGDMP" {
        return None;
    }
    let (major, minor, rev) = (r.byte()?, r.byte()?, r.byte()?);
    // The layout below has been stable since 1.4 (PostgreSQL 7.x).
    if (major, minor) < (1, 4) {
        return None;
    }
    r.int_size = usize::from(r.byte()?);
    if !(1..=8).contains(&r.int_size) {
        return None;
    }
    let _offset_size = r.byte()?;
    let _format = r.byte()?;
    // Compression: an algorithm byte since 1.15 (PostgreSQL 16), a level before that.
    if (major, minor) >= (1, 15) {
        r.byte()?;
    } else {
        r.int()?;
    }
    // Creation time: sec, min, hour, mday, mon, year, isdst.
    for _ in 0..7 {
        r.int()?;
    }
    let dbname = r.string()?;
    let dumped_from = r.string()?;
    let dumped_by = r.string()?;
    Some(ArchiveHeader {
        dump_version: format!("{major}.{minor}-{rev}"),
        dbname: Some(dbname).filter(|d| !d.is_empty()),
        dumped_from: PgVersion::parse(&dumped_from),
        dumped_by: PgVersion::parse(&dumped_by),
    })
}

struct HeaderReader<'a> {
    bytes: &'a [u8],
    int_size: usize,
}

impl<'a> HeaderReader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.bytes.len() {
            return None;
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    /// A sign byte, then `int_size` little-endian magnitude bytes.
    fn int(&mut self) -> Option<i64> {
        let negative = self.byte()? != 0;
        let magnitude = self
            .take(self.int_size)?
            .iter()
            .rev()
            .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
        let value = i64::try_from(magnitude).ok()?;
        Some(if negative { -value } else { value })
    }

    /// A length (negative for a null string), then that many bytes.
    fn string(&mut self) -> Option<String> {
        let len = self.int()?;
        if len <= 0 {
            return Some(String::new());
        }
        let raw = self.take(usize::try_from(len).ok()?)?;
        Some(String::from_utf8_lossy(raw).into_owned())
    }
}

/// How hard to look at an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveCheck {
    /// `pg_restore --list` only: header + TOC readable. Fast; misses data truncated after
    /// the TOC.
    TocOnly,
    /// Also replay the whole archive to a null script (`pg_restore --file=<null>`), reading
    /// and decompressing every data block — O(archive size), no server needed.
    Full,
}

/// The reusability verdict recorded in run history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveInspection {
    pub path: PathBuf,
    pub size_bytes: Option<u64>,
    pub reusable: bool,
    /// Why not reusable (user-facing), when `reusable == false`.
    pub reason: Option<String>,
    pub cause: Option<FailureCause>,
    pub toc: Option<Toc>,
}

/// Inspect an archive with `pg_restore` (use [`crate::ToolRegistry::highest`]: the dump
/// version is unknown until the header is read). Never errors for an unreadable archive —
/// that's a `reusable: false` verdict; errors are reserved for "couldn't even run the tool".
pub async fn inspect_archive(
    pg_restore: &DiscoveredTool,
    path: &Path,
    check: ArchiveCheck,
    ctx: &RunContext,
) -> AppResult<ArchiveInspection> {
    if pg_restore.kind != ToolKind::PgRestore {
        return Err(AppError::internal("inspect_archive needs pg_restore"));
    }
    let size_bytes = std::fs::metadata(path).ok().map(|m| m.len());
    let not_reusable = |reason: String, cause: FailureCause, toc: Option<Toc>| ArchiveInspection {
        path: path.to_path_buf(),
        size_bytes,
        reusable: false,
        reason: Some(reason),
        cause: Some(cause),
        toc,
    };
    if size_bytes.is_none() {
        return Ok(not_reusable(
            "The backup file no longer exists".into(),
            FailureCause::ArchiveCorrupt,
            None,
        ));
    }

    let mut spec = ChildSpec::new(
        pg_restore,
        vec!["--list".into(), "--".into(), path.as_os_str().to_owned()],
    );
    spec.capture_stdout = true;
    let run = run_child(&spec, ctx, &mut |_| {}).await?;
    let stdout = run.stdout.clone().unwrap_or_default();
    let toc = match run.into_result() {
        Ok(_) => match parse_toc(&String::from_utf8_lossy(&stdout)) {
            Ok(toc) => toc,
            Err(e) => {
                return Ok(not_reusable(e.message, FailureCause::ArchiveCorrupt, None));
            }
        },
        Err(e) => {
            let cause = FailureCause::from_error(&e).unwrap_or(FailureCause::Other);
            if cause == FailureCause::Cancelled {
                return Err(e);
            }
            return Ok(not_reusable(e.message, cause, None));
        }
    };

    if check == ArchiveCheck::Full {
        let null: OsString = if cfg!(windows) { "NUL" } else { "/dev/null" }.into();
        let mut file_arg = OsString::from("--file=");
        file_arg.push(&null);
        let spec = ChildSpec::new(
            pg_restore,
            vec![file_arg, "--".into(), path.as_os_str().to_owned()],
        );
        let run = run_child(&spec, ctx, &mut |_| {}).await?;
        if let Err(e) = run.into_result() {
            let cause = FailureCause::from_error(&e).unwrap_or(FailureCause::Other);
            if cause == FailureCause::Cancelled {
                return Err(e);
            }
            return Ok(not_reusable(e.message, cause, Some(toc)));
        }
    }

    Ok(ArchiveInspection {
        path: path.to_path_buf(),
        size_bytes,
        reusable: true,
        reason: None,
        cause: None,
        toc: Some(toc),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `pg_restore -l` output from pg_dump 16.15 against a PG 16.11 server.
    pub const REAL_TOC: &str = r#";
; Archive created at 2026-10-08 17:20:08 IST
;     dbname: postgres
;     TOC Entries: 15
;     Compression: gzip
;     Dump Version: 1.15-0
;     Format: CUSTOM
;     Integer: 4 bytes
;     Offset: 8 bytes
;     Dumped from database version: 16.11
;     Dumped by pg_dump version: 16.15 (Homebrew)
;
;
; Selected TOC Entries:
;
6; 2615 16392 SCHEMA - s2 postgres
219; 1259 16400 SEQUENCE public sq postgres
216; 1259 16384 TABLE public t1 postgres
218; 1259 16396 VIEW public v1 postgres
217; 1259 16393 TABLE s2 we ird postgres
3420; 0 16384 TABLE DATA public t1 postgres
3421; 0 16393 TABLE DATA s2 we ird postgres
3430; 0 0 SEQUENCE SET public sq postgres
3274; 2606 16390 CONSTRAINT public t1 t1_pkey postgres
3275; 1259 16391 INDEX public t1_v_idx postgres
"#;

    #[test]
    fn parses_real_toc() {
        let toc = parse_toc(REAL_TOC).unwrap();
        let h = &toc.header;
        assert_eq!(h.dumped_by.as_ref().unwrap().major, 16);
        assert_eq!(h.dumped_by.as_ref().unwrap().minor, 15);
        assert_eq!(h.dumped_from.as_ref().unwrap().minor, 11);
        assert_eq!(h.format.as_deref(), Some("CUSTOM"));
        assert_eq!(h.toc_entries, Some(15));
        assert_eq!(toc.entries.len(), 10);
        assert_eq!(toc.count_of("TABLE DATA"), 2);
        let e = &toc.entries[6];
        assert_eq!(e.desc, "TABLE DATA");
        assert_eq!(e.schema.as_deref(), Some("s2"));
        assert_eq!(e.rest, "we ird postgres");
        assert_eq!(toc.entries[0].schema, None);
        assert_eq!(toc.entries[7].desc, "SEQUENCE SET");
        assert_eq!(toc.restorable_count(true), 10);
    }

    const PG14: &[u8] = include_bytes!("../tests/fixtures/custom-header-pg14.bin");
    const PG16: &[u8] = include_bytes!("../tests/fixtures/custom-header-pg16.bin");
    const PG18: &[u8] = include_bytes!("../tests/fixtures/custom-header-pg18.bin");

    /// Real archives from each side of the 1.15 layout change, cut off after the header.
    #[test]
    fn reads_real_custom_format_headers() {
        for (bytes, version, major, minor) in [
            (PG14, "1.14-0", 14, 24),
            (PG16, "1.15-0", 16, 15),
            (PG18, "1.16-0", 18, 6),
        ] {
            let h = parse_archive_header(bytes).unwrap();
            assert_eq!(h.dump_version, version);
            assert_eq!(h.dbname.as_deref(), Some("appdb"));
            let by = h.dumped_by.unwrap();
            assert_eq!((by.major, by.minor), (major, minor));
            assert_eq!(h.dumped_from.unwrap().major, major);
        }
    }

    #[test]
    fn anything_else_is_not_a_header() {
        assert_eq!(parse_archive_header(b""), None);
        assert_eq!(
            parse_archive_header(b"--\n-- PostgreSQL database dump\n"),
            None
        );
        // a too-old format, and an integer size that can't be real
        assert_eq!(parse_archive_header(b"PGDMP\x01\x03\x00\x04\x08\x01"), None);
        assert_eq!(parse_archive_header(b"PGDMP\x01\x10\x00\x09\x08\x01"), None);
        // cut off anywhere inside the header, it never panics and never half-parses
        const PG18_HEADER_LEN: usize = 125;
        for n in 0..PG18_HEADER_LEN {
            assert_eq!(parse_archive_header(&PG18[..n]), None, "{n} bytes");
        }
        assert!(parse_archive_header(&PG18[..PG18_HEADER_LEN]).is_some());
    }

    #[test]
    fn hostile_string_lengths_are_refused() {
        // version 1.15, 8-byte ints, then a database name claiming to be i64::MAX bytes long
        let mut b = b"PGDMP\x01\x0f\x00\x08\x08\x01\x01".to_vec();
        for _ in 0..7 {
            b.extend_from_slice(&[0; 9]);
        }
        b.push(0);
        b.extend_from_slice(&i64::MAX.to_le_bytes());
        assert_eq!(parse_archive_header(&b), None);
    }

    #[test]
    fn tolerates_drift_and_rejects_empty() {
        let toc =
            parse_toc("; Dumped by pg_dump version: 18.1\nnonsense line\n12; x y z\n").unwrap();
        assert_eq!(toc.header.dumped_by.unwrap().major, 18);
        assert!(toc.entries.is_empty());
        assert!(parse_toc("").is_err());
        assert!(parse_toc("garbage\n").is_err());
    }
}
