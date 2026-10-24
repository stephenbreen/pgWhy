//! The databases a run creates on the target server, and how pgWhy recognises its own.
//!
//! - Names: staging is `<target>_staging_<yyyymmdd_hhmmss>`, the swap's holding name is the
//!   fixed `<target>_old`. Postgres silently truncates identifiers to 63 bytes, so the target
//!   part is shortened to keep the suffix intact (a truncated name would collide or mislead).
//! - Markers: every database pgWhy creates or parks is stamped with a `COMMENT ON DATABASE`
//!   JSON [`Marker`] (run, workflow, role, original target — `_old` also carries the target's
//!   original comment so a rollback is self-describing). Nothing that merely *matches* the
//!   naming pattern is ever dropped: an unmarked database "may be a real database".
//! - Locale: staging is created `TEMPLATE template0` with the encoding / collation / ctype /
//!   provider read from the live source catalog, in version-appropriate syntax.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// `NAMEDATALEN - 1`: the longest identifier Postgres keeps.
pub const MAX_IDENT_BYTES: usize = 63;

/// `<target>_staging_<ts>` with `ts` from [`compact_timestamp`].
pub fn staging_name(target: &str, ts: &str) -> String {
    fit(target, &format!("_staging_{ts}"))
}

/// `<target>_old`.
pub fn old_name(target: &str) -> String {
    fit(target, "_old")
}

fn fit(base: &str, suffix: &str) -> String {
    let room = MAX_IDENT_BYTES.saturating_sub(suffix.len());
    let mut end = base.len().min(room);
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &base[..end])
}

/// What a marked database is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkerRole {
    /// Restored copy waiting to be swapped in.
    Staging,
    /// The previous target, parked under `<target>_old` during a swap.
    Old,
    /// Drop-first: the target-named database this run created and is restoring into.
    WorkTarget,
}

/// The JSON stamped on a database with `COMMENT ON DATABASE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Format version; also what identifies the comment as a pgWhy marker.
    pub pgwhy_marker: u32,
    pub role: MarkerRole,
    pub run_id: String,
    pub workflow_id: String,
    pub original_target: String,
    /// `Old` only: the target's comment before the swap overwrote it (restored on rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_comment: Option<String>,
}

impl Marker {
    pub fn new(role: MarkerRole, run_id: &str, workflow_id: &str, original_target: &str) -> Self {
        Self {
            pgwhy_marker: 1,
            role,
            run_id: run_id.to_string(),
            workflow_id: workflow_id.to_string(),
            original_target: original_target.to_string(),
            original_comment: None,
        }
    }

    pub fn to_comment(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse a database comment; `None` for anything that is not a pgWhy marker.
    pub fn parse(comment: &str) -> Option<Marker> {
        let m: Marker = serde_json::from_str(comment.trim()).ok()?;
        (m.pgwhy_marker >= 1).then_some(m)
    }

    /// Marked by pgWhy for this workflow's target in this role (any run).
    pub fn belongs_to(&self, role: MarkerRole, workflow_id: &str, target: &str) -> bool {
        self.role == role && self.workflow_id == workflow_id && self.original_target == target
    }
}

/// Locale provider of a database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocaleProvider {
    #[default]
    Libc,
    Icu,
    Builtin,
}

impl LocaleProvider {
    /// From `pg_database.datlocprovider` (`c`, `i`, `b`).
    pub fn from_catalog(code: Option<&str>) -> Self {
        match code {
            Some("i") => Self::Icu,
            Some("b") => Self::Builtin,
            _ => Self::Libc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Libc => "libc",
            Self::Icu => "icu",
            Self::Builtin => "builtin",
        }
    }
}

/// Encoding and locale settings for the database a run creates (read from the source).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocaleSpec {
    pub encoding: String,
    pub collate: String,
    pub ctype: String,
    pub provider: LocaleProvider,
    /// ICU or builtin locale name (`daticulocale` / `datlocale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// ICU tailoring rules (PostgreSQL 16+).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icu_rules: Option<String>,
}

/// A SQL string literal (standard_conforming_strings is on in every supported server).
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Always-quoted identifier.
pub fn ident(name: &str) -> String {
    pgwhy_core::introspect::quote_ident_always(name)
}

/// The locale columns a server reports, by `server_version_num` (the catalog moves them:
/// `daticulocale` until 16, `datlocale` from 17, no provider before 15).
pub fn locale_columns(server_version_num: i32) -> &'static str {
    if server_version_num >= 170_000 {
        "pg_encoding_to_char(encoding)::text, datcollate::text, datctype::text, \
         datlocprovider::text, datlocale::text, daticurules::text"
    } else if server_version_num >= 160_000 {
        "pg_encoding_to_char(encoding)::text, datcollate::text, datctype::text, \
         datlocprovider::text, daticulocale::text, daticurules::text"
    } else if server_version_num >= 150_000 {
        "pg_encoding_to_char(encoding)::text, datcollate::text, datctype::text, \
         datlocprovider::text, daticulocale::text, null::text"
    } else {
        "pg_encoding_to_char(encoding)::text, datcollate::text, datctype::text, \
         'c'::text, null::text, null::text"
    }
}

/// A `pg_database` locale read for `filter` (`datname = current_database()` or `datname = $1`).
/// `filter` is always a caller-supplied constant, never user input.
pub fn locale_sql(server_version_num: i32, filter: &str) -> String {
    format!(
        "select {} from pg_database where {filter}",
        locale_columns(server_version_num)
    )
}

/// The [`LocaleSpec`] from a [`locale_sql`] row.
pub fn locale_from_row(row: &tokio_postgres::Row) -> LocaleSpec {
    let provider = LocaleProvider::from_catalog(row.get::<_, Option<String>>(3).as_deref());
    LocaleSpec {
        encoding: row.get(0),
        collate: row.get(1),
        ctype: row.get(2),
        provider,
        locale: if provider == LocaleProvider::Libc {
            None
        } else {
            row.get(4)
        },
        icu_rules: if provider == LocaleProvider::Icu {
            row.get(5)
        } else {
            None
        },
    }
}

/// `CREATE DATABASE … TEMPLATE template0` matching `spec`, in the syntax `server_major`
/// accepts (LOCALE_PROVIDER from 15, ICU_RULES from 16, BUILTIN_LOCALE from 17).
pub fn create_database_sql(name: &str, spec: &LocaleSpec, server_major: u32) -> String {
    let mut sql = format!(
        "CREATE DATABASE {} TEMPLATE template0 ENCODING {} LC_COLLATE {} LC_CTYPE {}",
        ident(name),
        quote_literal(&spec.encoding),
        quote_literal(&spec.collate),
        quote_literal(&spec.ctype)
    );
    if server_major >= 15 {
        sql.push_str(&format!(" LOCALE_PROVIDER {}", spec.provider.label()));
        match (spec.provider, &spec.locale) {
            (LocaleProvider::Icu, Some(l)) => {
                sql.push_str(&format!(" ICU_LOCALE {}", quote_literal(l)))
            }
            (LocaleProvider::Builtin, Some(l)) => {
                sql.push_str(&format!(" BUILTIN_LOCALE {}", quote_literal(l)))
            }
            _ => {}
        }
        if spec.provider == LocaleProvider::Icu && server_major >= 16 {
            if let Some(rules) = &spec.icu_rules {
                sql.push_str(&format!(" ICU_RULES {}", quote_literal(rules)));
            }
        }
    }
    sql
}

/// Seconds since the epoch → `(year, month, day, hour, minute, second)` in UTC.
fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's days-from-civil inverse.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

fn epoch(t: SystemTime) -> (u64, u32) {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs(), d.subsec_millis())
}

/// `yyyymmdd_hhmmss` (UTC) for database and backup names.
pub fn compact_timestamp(t: SystemTime) -> String {
    let (y, mo, d, h, mi, s) = civil(epoch(t).0);
    format!("{y:04}{mo:02}{d:02}_{h:02}{mi:02}{s:02}")
}

/// RFC 3339 UTC with milliseconds, for run records.
pub fn rfc3339(t: SystemTime) -> String {
    let (secs, ms) = epoch(t);
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
}

pub fn now_rfc3339() -> String {
    rfc3339(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn names_fit_in_63_bytes_and_keep_the_suffix() {
        assert_eq!(
            staging_name("app", "20261009_120000"),
            "app_staging_20261009_120000"
        );
        assert_eq!(old_name("app"), "app_old");
        let long = "é".repeat(40);
        let s = staging_name(&long, "20261009_120000");
        assert!(s.len() <= MAX_IDENT_BYTES, "{}", s.len());
        assert!(s.ends_with("_staging_20261009_120000"));
        assert!(old_name(&long).len() <= MAX_IDENT_BYTES);
    }

    #[test]
    fn markers_round_trip_and_reject_foreign_comments() {
        let mut m = Marker::new(MarkerRole::Old, "r1", "w1", "app");
        m.original_comment = Some("it's mine".into());
        let parsed = Marker::parse(&m.to_comment()).unwrap();
        assert_eq!(parsed, m);
        assert!(parsed.belongs_to(MarkerRole::Old, "w1", "app"));
        assert!(!parsed.belongs_to(MarkerRole::Staging, "w1", "app"));
        assert!(Marker::parse("production copy, do not drop").is_none());
        assert!(Marker::parse(r#"{"role":"staging"}"#).is_none());
    }

    #[test]
    fn create_database_syntax_per_version() {
        let libc = LocaleSpec {
            encoding: "UTF8".into(),
            collate: "en_US.utf8".into(),
            ctype: "en_US.utf8".into(),
            provider: LocaleProvider::Libc,
            locale: None,
            icu_rules: None,
        };
        assert_eq!(
            create_database_sql("a\"b", &libc, 14),
            r#"CREATE DATABASE "a""b" TEMPLATE template0 ENCODING 'UTF8' LC_COLLATE 'en_US.utf8' LC_CTYPE 'en_US.utf8'"#
        );
        assert!(create_database_sql("x", &libc, 16).ends_with("LOCALE_PROVIDER libc"));
        let icu = LocaleSpec {
            provider: LocaleProvider::Icu,
            locale: Some("und-x-icu".into()),
            icu_rules: Some("&a < b".into()),
            ..libc.clone()
        };
        let s15 = create_database_sql("x", &icu, 15);
        assert!(
            s15.ends_with("LOCALE_PROVIDER icu ICU_LOCALE 'und-x-icu'"),
            "{s15}"
        );
        assert!(create_database_sql("x", &icu, 16).ends_with("ICU_RULES '&a < b'"));
        let b = LocaleSpec {
            provider: LocaleProvider::Builtin,
            locale: Some("C.UTF-8".into()),
            ..libc
        };
        assert!(create_database_sql("x", &b, 17)
            .ends_with("LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8'"));
    }

    #[test]
    fn timestamps() {
        let t = UNIX_EPOCH + Duration::from_millis(1_791_547_200_123); // 2026-10-09T12:00:00.123Z
        assert_eq!(compact_timestamp(t), "20261009_120000");
        assert_eq!(rfc3339(t), "2026-10-09T12:00:00.123Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(compact_timestamp(leap), "20000229_000000");
    }
}
