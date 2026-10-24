//! Tolerant parsing of pg_dump/pg_restore `--verbose` stderr into a progress stream.
//!
//! The verbose grammar is not a documented contract, so every rule here is a best-effort
//! match pinned against real 13–18 output; an unrecognised line is kept as `last_line` and
//! otherwise ignored. When nothing matches at all the readout degrades to step + bytes
//! written (file stat) + elapsed — it never fails the run. No percentage is ever promised:
//! dump totals are a caller-supplied catalog pre-count ("~X of ~Y"); restore totals come
//! from the archive's TOC item count.

use crate::discover::ToolKind;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Coarse step within a tool run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Starting,
    Connecting,
    /// pg_dump reading catalogs ("reading schemas", "reading indexes", ...).
    ReadingCatalog,
    /// pg_dump writing table contents / pg_restore loading data.
    Data,
    /// pg_restore creating schema objects.
    CreatingObjects,
    Finishing,
}

/// One progress readout, emitted on every matched line and on a periodic tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub tool: ToolKind,
    pub phase: ProgressPhase,
    /// Object currently being processed (`public.orders`, `INDEX public.t1_v_idx`).
    pub current_object: Option<String>,
    pub objects_done: u64,
    pub objects_total: Option<u64>,
    /// True when `objects_total` is an estimate (dump pre-count) — render as "~X of ~Y".
    pub total_is_estimate: bool,
    /// Size of the output file so far (dump), when known.
    pub bytes_written: Option<u64>,
    pub elapsed_ms: u64,
    /// Latest raw stderr line, for the "details" disclosure.
    pub last_line: Option<String>,
    /// Non-fatal warnings seen so far.
    pub warnings: u32,
    /// Error lines seen so far (only non-fatal under continue-on-error).
    pub errors: u32,
}

/// What a single stderr line means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineEvent {
    /// A coarse step change ("reading indexes", "connecting to database for restore").
    Step(ProgressPhase, String),
    /// Started work on one object. `counts` is false for parallel-mode bookkeeping lines
    /// (`processing item N`, `launching item N`), which pg_restore prints in addition to the
    /// per-object `creating ...` / `processing data ...` / `executing ...` line — and also for
    /// entries it then skips — so only the per-object lines are counted.
    Object {
        phase: ProgressPhase,
        label: String,
        counts: bool,
    },
    /// A completed parallel item (`finished item N ...`); informational.
    Finished {
        id: String,
    },
    Warning(String),
    Error(String),
    Unknown,
}

/// Classify one stderr line. Never panics; unknown shapes → [`LineEvent::Unknown`].
pub fn parse_line(tool: ToolKind, line: &str) -> LineEvent {
    let line = line.trim_end();
    let body = strip_prog_prefix(line);
    let Some(body) = body else {
        return LineEvent::Unknown;
    };
    if let Some(rest) = body.strip_prefix("error: ") {
        return LineEvent::Error(rest.to_string());
    }
    if let Some(rest) = body.strip_prefix("warning: ") {
        return LineEvent::Warning(rest.to_string());
    }
    match tool {
        ToolKind::PgDump => parse_dump(body),
        ToolKind::PgRestore => parse_restore(body),
    }
}

/// `pg_dump: xxx` / `pg_restore: xxx` → `xxx` (also tolerates Windows `pg_dump.exe:`).
fn strip_prog_prefix(line: &str) -> Option<&str> {
    for prog in ["pg_dump", "pg_restore", "pg_dump.exe", "pg_restore.exe"] {
        if let Some(rest) = line.strip_prefix(prog) {
            if let Some(rest) = rest.strip_prefix(": ") {
                return Some(rest);
            }
        }
    }
    None
}

fn parse_dump(body: &str) -> LineEvent {
    if let Some(rest) = body.strip_prefix("dumping contents of table ") {
        let name = unquote(rest);
        return LineEvent::Object {
            phase: ProgressPhase::Data,
            label: name,
            counts: true,
        };
    }
    if body.starts_with("reading ")
        || body.starts_with("identifying ")
        || body.starts_with("finding ")
        || body.starts_with("flagging ")
        || body.starts_with("last built-in OID")
    {
        return LineEvent::Step(ProgressPhase::ReadingCatalog, body.to_string());
    }
    if body.starts_with("saving ") {
        return LineEvent::Step(ProgressPhase::Finishing, body.to_string());
    }
    LineEvent::Unknown
}

fn parse_restore(body: &str) -> LineEvent {
    if body.starts_with("connecting to database") {
        return LineEvent::Step(ProgressPhase::Connecting, body.to_string());
    }
    if let Some(rest) = body.strip_prefix("processing data for table ") {
        return LineEvent::Object {
            phase: ProgressPhase::Data,
            label: unquote(rest),
            counts: true,
        };
    }
    if let Some(rest) = body.strip_prefix("creating ") {
        // "creating TABLE "public.t1"", "creating FK CONSTRAINT "public.a a_fk""
        let (kind, name) = split_kind_name(rest);
        return LineEvent::Object {
            phase: ProgressPhase::CreatingObjects,
            label: format!("{kind} {name}"),
            counts: true,
        };
    }
    if let Some(rest) = body.strip_prefix("executing ") {
        // "executing SEQUENCE SET sq"
        return LineEvent::Object {
            phase: ProgressPhase::CreatingObjects,
            label: rest.to_string(),
            counts: true,
        };
    }
    // Parallel mode: "launching item 3420 TABLE DATA public t1" / "processing item 3420 ..."
    for prefix in ["launching item ", "processing item "] {
        if let Some(rest) = body.strip_prefix(prefix) {
            let (_id, label) = rest.split_once(' ').unwrap_or((rest, ""));
            return LineEvent::Object {
                phase: if label.contains("DATA") {
                    ProgressPhase::Data
                } else {
                    ProgressPhase::CreatingObjects
                },
                label: label.to_string(),
                counts: false,
            };
        }
    }
    if let Some(rest) = body.strip_prefix("finished item ") {
        let id = rest.split(' ').next().unwrap_or(rest);
        return LineEvent::Finished { id: id.to_string() };
    }
    if body.starts_with("entering main parallel loop") {
        return LineEvent::Step(ProgressPhase::Data, body.to_string());
    }
    if body.starts_with("finished main parallel loop") || body.starts_with("implied data-only") {
        return LineEvent::Step(ProgressPhase::Finishing, body.to_string());
    }
    LineEvent::Unknown
}

fn split_kind_name(rest: &str) -> (String, String) {
    match rest.find('"') {
        Some(i) => (rest[..i].trim().to_string(), unquote(&rest[i..])),
        None => {
            let (k, n) = rest.rsplit_once(' ').unwrap_or((rest, ""));
            (k.to_string(), n.to_string())
        }
    }
}

/// `"public.t1"` → `public.t1` (outer quotes only; inner text untouched).
fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// Folds parsed lines into [`ProgressEvent`]s.
#[derive(Debug)]
pub struct ProgressTracker {
    tool: ToolKind,
    started: Instant,
    phase: ProgressPhase,
    current: Option<String>,
    done: u64,
    total: Option<u64>,
    estimate: bool,
    bytes: Option<u64>,
    last_line: Option<String>,
    warnings: u32,
    errors: u32,
}

impl ProgressTracker {
    pub fn new(tool: ToolKind, total: Option<u64>, total_is_estimate: bool) -> Self {
        Self {
            tool,
            started: Instant::now(),
            phase: ProgressPhase::Starting,
            current: None,
            done: 0,
            total,
            estimate: total_is_estimate,
            bytes: None,
            last_line: None,
            warnings: 0,
            errors: 0,
        }
    }

    /// Feed one stderr line; returns the line's classification.
    pub fn feed(&mut self, line: &str) -> LineEvent {
        let ev = parse_line(self.tool, line);
        if !line.trim().is_empty() {
            self.last_line = Some(line.trim_end().to_string());
        }
        match &ev {
            LineEvent::Step(phase, _) => self.phase = *phase,
            LineEvent::Object {
                phase,
                label,
                counts,
            } => {
                self.phase = *phase;
                self.current = Some(label.clone());
                if *counts {
                    self.done += 1;
                }
            }
            LineEvent::Finished { .. } => {}
            LineEvent::Warning(_) => self.warnings += 1,
            LineEvent::Error(_) => self.errors += 1,
            LineEvent::Unknown => {}
        }
        ev
    }

    pub fn set_bytes(&mut self, bytes: Option<u64>) {
        self.bytes = bytes;
    }

    pub fn set_phase(&mut self, phase: ProgressPhase) {
        self.phase = phase;
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn objects_done(&self) -> u64 {
        self.done
    }

    pub fn snapshot(&self) -> ProgressEvent {
        // A pre-count can undershoot; never show "12 of ~10".
        let done = match (self.total, self.estimate) {
            (Some(t), false) => self.done.min(t),
            _ => self.done,
        };
        let total = match (self.total, self.estimate) {
            (Some(t), true) => Some(t.max(self.done)),
            (t, _) => t,
        };
        ProgressEvent {
            tool: self.tool,
            phase: self.phase,
            current_object: self.current.clone(),
            objects_done: done,
            objects_total: total,
            total_is_estimate: self.estimate,
            bytes_written: self.bytes,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            last_line: self.last_line.clone(),
            warnings: self.warnings,
            errors: self.errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `pg_restore -v -j 2` output (16.15) for a 10-entry TOC.
    const PARALLEL_RESTORE: &str = r#"pg_restore: connecting to database for restore
pg_restore: processing item 3425 ENCODING ENCODING
pg_restore: processing item 3426 STDSTRINGS STDSTRINGS
pg_restore: processing item 3427 SEARCHPATH SEARCHPATH
pg_restore: processing item 3428 DATABASE postgres
pg_restore: processing item 3429 COMMENT DATABASE postgres
pg_restore: processing item 6 SCHEMA s2
pg_restore: creating SCHEMA "s2"
pg_restore: processing item 219 SEQUENCE sq
pg_restore: creating SEQUENCE "public.sq"
pg_restore: processing item 216 TABLE t1
pg_restore: creating TABLE "public.t1"
pg_restore: processing item 218 VIEW v1
pg_restore: creating VIEW "public.v1"
pg_restore: processing item 217 TABLE we ird
pg_restore: creating TABLE "s2.we ird"
pg_restore: entering main parallel loop
pg_restore: launching item 3420 TABLE DATA t1
pg_restore: launching item 3421 TABLE DATA we ird
pg_restore: processing data for table "public.t1"
pg_restore: processing data for table "s2.we ird"
pg_restore: finished item 3421 TABLE DATA we ird
pg_restore: launching item 3430 SEQUENCE SET sq
pg_restore: executing SEQUENCE SET sq
pg_restore: finished item 3430 SEQUENCE SET sq
pg_restore: finished item 3420 TABLE DATA t1
pg_restore: launching item 3274 CONSTRAINT t1 t1_pkey
pg_restore: creating CONSTRAINT "public.t1 t1_pkey"
pg_restore: finished item 3274 CONSTRAINT t1 t1_pkey
pg_restore: launching item 3275 INDEX t1_v_idx
pg_restore: creating INDEX "public.t1_v_idx"
pg_restore: finished item 3275 INDEX t1_v_idx
pg_restore: finished main parallel loop"#;

    /// Real serial `pg_restore -v` output for the same archive.
    const SERIAL_RESTORE: &str = r#"pg_restore: connecting to database for restore
pg_restore: creating SCHEMA "s2"
pg_restore: creating SEQUENCE "public.sq"
pg_restore: creating TABLE "public.t1"
pg_restore: creating VIEW "public.v1"
pg_restore: creating TABLE "s2.we ird"
pg_restore: processing data for table "public.t1"
pg_restore: processing data for table "s2.we ird"
pg_restore: executing SEQUENCE SET sq
pg_restore: creating CONSTRAINT "public.t1 t1_pkey"
pg_restore: creating INDEX "public.t1_v_idx""#;

    #[test]
    fn restore_counts_match_toc_in_serial_and_parallel() {
        for text in [SERIAL_RESTORE, PARALLEL_RESTORE] {
            let mut t = ProgressTracker::new(ToolKind::PgRestore, Some(10), false);
            for l in text.lines() {
                t.feed(l);
            }
            let s = t.snapshot();
            assert_eq!(s.objects_done, 10, "{text}");
            assert_eq!(s.current_object.as_deref(), Some("INDEX public.t1_v_idx"));
        }
    }

    #[test]
    fn dump_lines() {
        assert_eq!(
            parse_line(
                ToolKind::PgDump,
                r#"pg_dump: dumping contents of table "s2.we ird""#
            ),
            LineEvent::Object {
                phase: ProgressPhase::Data,
                label: "s2.we ird".into(),
                counts: true,
            }
        );
        assert!(matches!(
            parse_line(ToolKind::PgDump, "pg_dump: reading indexes"),
            LineEvent::Step(ProgressPhase::ReadingCatalog, _)
        ));
        assert!(matches!(
            parse_line(ToolKind::PgDump, "pg_dump: error: query failed"),
            LineEvent::Error(_)
        ));
        assert!(matches!(
            parse_line(ToolKind::PgDump, "pg_dump.exe: warning: x"),
            LineEvent::Warning(_)
        ));
    }

    #[test]
    fn restore_lines() {
        assert_eq!(
            parse_line(
                ToolKind::PgRestore,
                r#"pg_restore: creating CONSTRAINT "public.t1 t1_pkey""#,
            ),
            LineEvent::Object {
                phase: ProgressPhase::CreatingObjects,
                label: "CONSTRAINT public.t1 t1_pkey".into(),
                counts: true,
            }
        );
        assert!(matches!(
            parse_line(
                ToolKind::PgRestore,
                "pg_restore: launching item 3420 TABLE DATA t1"
            ),
            LineEvent::Object {
                phase: ProgressPhase::Data,
                counts: false,
                ..
            }
        ));
        assert_eq!(
            parse_line(
                ToolKind::PgRestore,
                "pg_restore: finished item 3420 TABLE DATA t1"
            ),
            LineEvent::Finished { id: "3420".into() }
        );
    }

    #[test]
    fn drifted_lines_degrade_without_failing() {
        let mut t = ProgressTracker::new(ToolKind::PgDump, Some(2), true);
        for l in [
            "",
            "pg_dump: dumping kontents of tabel \"x\"",
            "totally unrelated output",
            "pg_dump: ",
            "pg_dump:",
            "\u{1F600} \"\"\"",
        ] {
            assert_eq!(t.feed(l), LineEvent::Unknown, "{l:?}");
        }
        t.set_bytes(Some(4096));
        let s = t.snapshot();
        assert_eq!(s.objects_done, 0);
        assert_eq!(s.bytes_written, Some(4096));
        assert_eq!(s.last_line.as_deref(), Some("\u{1F600} \"\"\""));
    }

    #[test]
    fn estimates_never_undershoot_and_exact_totals_cap() {
        let mut t = ProgressTracker::new(ToolKind::PgDump, Some(1), true);
        t.feed(r#"pg_dump: dumping contents of table "public.a""#);
        t.feed(r#"pg_dump: dumping contents of table "public.b""#);
        let s = t.snapshot();
        assert_eq!((s.objects_done, s.objects_total), (2, Some(2)));
        assert_eq!(s.current_object.as_deref(), Some("public.b"));

        let mut r = ProgressTracker::new(ToolKind::PgRestore, Some(1), false);
        r.feed(r#"pg_restore: creating TABLE "public.a""#);
        r.feed(r#"pg_restore: creating TABLE "public.b""#);
        let s = r.snapshot();
        assert_eq!((s.objects_done, s.objects_total), (1, Some(1)));
    }
}
