//! Child-process connection inputs and the quoting rules that keep them inert.
//!
//! Children receive connection parameters as ONE libpq conninfo string in a single
//! `--dbname=<conninfo>` argv element (no shell, no `-h/-U` splitting): every value is
//! single-quoted with `\` and `'` backslash-escaped, so spaces, quotes, `=`, and a leading
//! `--` in any field stay data. The password is never part of it — see [`crate::pgpass`].

use pgwhy_errors::{AppError, AppResult, ErrorKind};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

/// A fully-resolved server endpoint for a pg client child.
///
/// For tunneled connections `host` stays the REAL server hostname (what `verify-full` checks
/// and what pgpass matches) while `hostaddr` is the tunnel's loopback address and `port` the
/// tunnel's local port. libpq dials `hostaddr` and verifies the certificate against `host`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEndpoint {
    /// Real server hostname (never the tunnel address).
    pub host: String,
    /// Address libpq dials instead of resolving `host` (tunnel loopback), when set.
    pub hostaddr: Option<IpAddr>,
    pub port: u16,
    pub user: String,
    pub dbname: String,
    /// libpq sslmode: disable | allow | prefer | require | verify-ca | verify-full.
    pub sslmode: String,
    pub sslrootcert: Option<PathBuf>,
}

const SSLMODES: &[&str] = &[
    "disable",
    "allow",
    "prefer",
    "require",
    "verify-ca",
    "verify-full",
];

impl ToolEndpoint {
    /// Reject values libpq cannot carry inertly even when quoted: NUL/newlines anywhere
    /// (argv and pgpass lines can't hold them), and commas in `host` (libpq treats a comma
    /// as a multi-host list separator *after* unquoting).
    pub fn validate(&self) -> AppResult<()> {
        let fields = [
            ("host", self.host.as_str()),
            ("user", self.user.as_str()),
            ("database", self.dbname.as_str()),
        ];
        for (name, value) in fields {
            if value.is_empty() {
                return Err(config_err(format!("The connection's {name} is empty")));
            }
            if value.contains(['\0', '\n', '\r']) {
                return Err(config_err(format!(
                    "The connection's {name} contains a line break or NUL character, which pg_dump/pg_restore cannot accept"
                )));
            }
        }
        if self.host.contains(',') {
            return Err(config_err(
                "The connection's host contains a comma, which libpq would treat as a list of hosts",
            ));
        }
        if !SSLMODES.contains(&self.sslmode.as_str()) {
            return Err(config_err(format!(
                "Unknown sslmode \"{}\"",
                self.sslmode.escape_debug()
            ))
            .with_hint("Use one of: disable, allow, prefer, require, verify-ca, verify-full"));
        }
        if let Some(cert) = &self.sslrootcert {
            if cert.to_string_lossy().contains(['\0', '\n', '\r']) {
                return Err(config_err("The SSL root certificate path is not usable"));
            }
        }
        Ok(())
    }

    /// The libpq conninfo for this endpoint (no password, no sslmode — those travel via
    /// `PGPASSFILE`/`PGSSLMODE`). Every value is quoted; see [`quote_conninfo_value`].
    pub fn conninfo(&self) -> String {
        let mut parts = vec![format!("host={}", quote_conninfo_value(&self.host))];
        if let Some(addr) = self.hostaddr {
            parts.push(format!(
                "hostaddr={}",
                quote_conninfo_value(&addr.to_string())
            ));
        }
        parts.push(format!(
            "port={}",
            quote_conninfo_value(&self.port.to_string())
        ));
        parts.push(format!("user={}", quote_conninfo_value(&self.user)));
        parts.push(format!("dbname={}", quote_conninfo_value(&self.dbname)));
        parts.join(" ")
    }

    /// A copy pointing at another database on the same server (restore into staging, etc.).
    pub fn with_dbname(&self, dbname: impl Into<String>) -> Self {
        Self {
            dbname: dbname.into(),
            ..self.clone()
        }
    }
}

fn config_err(message: impl Into<String>) -> AppError {
    AppError::new(ErrorKind::Config, message)
}

/// libpq conninfo value quoting: wrap in single quotes, backslash-escape `\` and `'`.
/// (libpq: "Single quotes and backslashes within the value must be escaped with a backslash".)
pub fn quote_conninfo_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\\' || ch == '\'' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// A psql pattern that matches exactly one schema-qualified relation, for `pg_dump -t` /
/// `--exclude-table-data`. `-t` takes a *pattern*, not an identifier: unqualified names match
/// every schema and `*`/`?`/regex characters act as wildcards. Inside double quotes all of
/// them are literal, and an embedded `"` is written `""`.
pub fn table_pattern(schema: &str, table: &str) -> String {
    format!(
        "{}.{}",
        quote_pattern_part(schema),
        quote_pattern_part(table)
    )
}

/// A psql pattern matching exactly one schema (`pg_dump -n`), quoted as a literal.
pub fn schema_pattern(schema: &str) -> String {
    quote_pattern_part(schema)
}

fn quote_pattern_part(part: &str) -> String {
    format!("\"{}\"", part.replace('"', "\"\""))
}

/// A password for a child process. Debug is redacted; the buffer is scrubbed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolPassword(String);

impl ToolPassword {
    pub fn new(password: impl Into<String>) -> Self {
        Self(password.into())
    }

    /// Crate-internal: the only consumer is the pgpass writer.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ToolPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolPassword(***)")
    }
}

impl Drop for ToolPassword {
    fn drop(&mut self) {
        // Best-effort scrub; the String's buffer is ours to overwrite.
        // SAFETY: writing zero bytes keeps the buffer valid UTF-8.
        unsafe {
            for b in self.0.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ep() -> ToolEndpoint {
        ToolEndpoint {
            host: "db.example.com".into(),
            hostaddr: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            port: 6543,
            user: "app".into(),
            dbname: "shop".into(),
            sslmode: "verify-full".into(),
            sslrootcert: None,
        }
    }

    #[test]
    fn conninfo_uses_host_hostaddr_split() {
        assert_eq!(
            ep().conninfo(),
            "host='db.example.com' hostaddr='127.0.0.1' port='6543' user='app' dbname='shop'"
        );
    }

    #[test]
    fn hostile_values_are_quoted_inert() {
        let mut e = ep();
        e.user = r"o'neil \ x".into();
        e.dbname = "--x a=b 'c' \\d".into();
        let ci = e.conninfo();
        assert!(ci.contains(r"user='o\'neil \\ x'"), "{ci}");
        assert!(ci.contains(r"dbname='--x a=b \'c\' \\d'"), "{ci}");
        assert!(!ci.contains("password"));
    }

    #[test]
    fn validation_rejects_uncarryable_values() {
        let mut e = ep();
        e.dbname = "a\nb".into();
        assert_eq!(e.validate().unwrap_err().kind, ErrorKind::Config);
        let mut e = ep();
        e.host = "a,b".into();
        assert!(e.validate().is_err());
        let mut e = ep();
        e.sslmode = "verify-full --x".into();
        assert!(e.validate().is_err());
        assert!(ep().validate().is_ok());
    }

    #[test]
    fn table_patterns_are_literal() {
        assert_eq!(schema_pattern("audit"), r#""audit""#);
        assert_eq!(schema_pattern(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(table_pattern("public", "t"), r#""public"."t""#);
        assert_eq!(table_pattern("s.p", r#"we"ird*"#), r#""s.p"."we""ird*""#);
    }

    #[test]
    fn password_debug_is_redacted() {
        let p = ToolPassword::new("hunter2");
        assert_eq!(format!("{p:?}"), "ToolPassword(***)");
    }
}
