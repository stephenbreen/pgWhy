//! The single pgWhy error taxonomy (plan flow finding ER-3).
//!
//! Every failure surfaced to the UI is an [`AppError`]: a [`ErrorKind`] saying *what* went
//! wrong, an [`ErrorClass`] saying *what the user can do about it*, a user-facing message,
//! and an optional actionable hint. UI surfaces map the class to an affordance:
//!
//! - [`ErrorClass::Retriable`] → a retry button (network blips, tunnel drops, lock timeouts)
//! - [`ErrorClass::FixThenRetry`] → the message names what to fix (auth, versions, disk, permissions)
//! - [`ErrorClass::Fatal`] → nothing to retry; the operation cleans up and reports
//!
//! Messages and hints must never contain credential material.

use serde::{Deserialize, Serialize};
use std::fmt;

/// What the user can do about an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Transient: the same action may succeed if retried as-is.
    Retriable,
    /// The user must change something (credentials, tools, disk, privileges) before retrying.
    FixThenRetry,
    /// Not retryable; the operation cleans up and reports.
    Fatal,
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// TCP-level failure: refused, unreachable, reset, timed out.
    Network,
    /// Authentication rejected by the server.
    Auth,
    /// TLS handshake or certificate verification failure.
    Tls,
    /// The SSH tunnel failed or dropped.
    Tunnel,
    /// SSH host key unknown or changed.
    HostKey,
    /// The server rejected a statement (syntax, constraint, missing object, ...).
    Sql,
    /// The connected role lacks a required privilege.
    Permission,
    /// A pg client tool (pg_dump, pg_restore, ...) could not be found.
    ToolMissing,
    /// A pg client tool's version is incompatible with the server or archive.
    ToolVersion,
    /// A child process (pg_dump, pg_restore) failed.
    ToolFailed,
    /// Out of disk space or the filesystem refused a write.
    Disk,
    /// Credential store unavailable or a credential missing.
    Credentials,
    /// Invalid or inconsistent configuration (connection entry, workflow definition, ...).
    Config,
    /// The connection was lost and will not be silently re-established.
    Disconnected,
    /// The user cancelled the operation.
    Cancelled,
    /// A precondition gate refused to start the operation (nothing was changed).
    Refused,
    /// A bug or unexpected state inside pgWhy.
    Internal,
}

impl ErrorKind {
    /// The default class for this kind; constructors may override it.
    pub fn default_class(self) -> ErrorClass {
        use ErrorKind::*;
        match self {
            Network | Tunnel | Disconnected => ErrorClass::Retriable,
            Auth | Tls | HostKey | Permission | ToolMissing | ToolVersion | Disk | Credentials
            | Config | Refused | Sql | ToolFailed => ErrorClass::FixThenRetry,
            Cancelled | Internal => ErrorClass::Fatal,
        }
    }
}

/// A classified, user-presentable error. Serializes to the UI as-is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
pub struct AppError {
    pub kind: ErrorKind,
    pub class: ErrorClass,
    /// One-sentence, user-facing statement of what went wrong.
    pub message: String,
    /// What to do about it, when pgWhy knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Underlying error text (server/tool output), for the details disclosure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Structured payload for UI affordances (e.g. host-key fingerprints for a trust prompt,
    /// blocking session pids for a swap failure). Never credential material.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl AppError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            class: kind.default_class(),
            message: message.into(),
            hint: None,
            detail: None,
            data: None,
        }
    }

    pub fn with_class(mut self, class: ErrorClass) -> Self {
        self.class = class;
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }

    pub fn is_retriable(&self) -> bool {
        self.class == ErrorClass::Retriable
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_classes_map_to_affordances() {
        assert_eq!(ErrorKind::Network.default_class(), ErrorClass::Retriable);
        assert_eq!(ErrorKind::Auth.default_class(), ErrorClass::FixThenRetry);
        assert_eq!(ErrorKind::Internal.default_class(), ErrorClass::Fatal);
    }

    #[test]
    fn serializes_for_the_ui() {
        let e = AppError::new(
            ErrorKind::ToolVersion,
            "pg_dump 15 cannot dump a PostgreSQL 17 server",
        )
        .with_hint("Install PostgreSQL 17 client tools");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "tool_version");
        assert_eq!(v["class"], "fix_then_retry");
        assert_eq!(v["hint"], "Install PostgreSQL 17 client tools");
        assert!(v.get("detail").is_none());
        assert_eq!(
            e.to_string(),
            "pg_dump 15 cannot dump a PostgreSQL 17 server (Install PostgreSQL 17 client tools)"
        );
    }
}
