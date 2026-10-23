//! tokio-postgres errors → the pgWhy taxonomy.

use crate::{AppError, ErrorClass, ErrorKind};
use std::error::Error as StdError;

/// Classify an error from connecting or running a statement. `context` is a short phrase
/// naming what was being attempted ("connect to db.example.com:5432/app").
pub fn classify(err: &tokio_postgres::Error, context: &str) -> AppError {
    if let Some(db) = err.as_db_error() {
        return classify_db(db, context);
    }
    let mut src: Option<&(dyn StdError + 'static)> = err.source();
    while let Some(e) = src {
        if let Some(tls) = e.downcast_ref::<rustls::Error>() {
            return AppError::new(
                ErrorKind::Tls,
                format!("could not {context}: the TLS handshake failed"),
            )
            .with_detail(tls.to_string())
            .with_hint(tls_hint(tls));
        }
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            // rustls errors surface wrapped in io::Error during the handshake
            if let Some(inner) = io.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                return AppError::new(
                    ErrorKind::Tls,
                    format!("could not {context}: the TLS handshake failed"),
                )
                .with_detail(inner.to_string())
                .with_hint(tls_hint(inner));
            }
            return AppError::new(
                ErrorKind::Network,
                format!("could not {context}: {}", io_phrase(io)),
            )
            .with_detail(io.to_string());
        }
        src = e.source();
    }
    let text = err.to_string();
    if err.is_closed() {
        return AppError::new(
            ErrorKind::Disconnected,
            format!("the connection closed while trying to {context}"),
        )
        .with_detail(text);
    }
    if text.contains("TLS") || text.contains("tls") || text.contains("SSL") {
        return AppError::new(
            ErrorKind::Tls,
            format!("could not {context}: TLS is not available"),
        )
        .with_detail(text);
    }
    if text.contains("password missing") {
        return AppError::new(
            ErrorKind::Credentials,
            format!("could not {context}: the server asked for a password"),
        )
        .with_hint("enter the password for this connection");
    }
    AppError::new(ErrorKind::Network, format!("could not {context}")).with_detail(text)
}

fn classify_db(db: &tokio_postgres::error::DbError, context: &str) -> AppError {
    let code = db.code().code();
    let mut detail = db.message().to_string();
    if let Some(d) = db.detail() {
        detail.push_str("\nDETAIL: ");
        detail.push_str(d);
    }
    let (kind, class) = match code {
        c if c.starts_with("28") => (ErrorKind::Auth, ErrorClass::FixThenRetry),
        "42501" => (ErrorKind::Permission, ErrorClass::FixThenRetry),
        "3D000" => (ErrorKind::Config, ErrorClass::FixThenRetry),
        // terminated by administrator / crash shutdown / cannot connect now
        "57P01" | "57P02" | "57P03" => (ErrorKind::Disconnected, ErrorClass::Retriable),
        "57014" => (ErrorKind::Cancelled, ErrorClass::Fatal),
        // serialization failure, deadlock, lock not available, too many connections, out of resources
        "40001" | "40P01" | "55P03" => (ErrorKind::Sql, ErrorClass::Retriable),
        c if c.starts_with("53") => (ErrorKind::Network, ErrorClass::Retriable),
        _ => (ErrorKind::Sql, ErrorClass::FixThenRetry),
    };
    let message = match kind {
        ErrorKind::Auth => format!("could not {context}: authentication failed"),
        ErrorKind::Permission => format!("could not {context}: permission denied"),
        ErrorKind::Config => format!("could not {context}: {}", db.message()),
        _ => db.message().to_string(),
    };
    let mut e = AppError::new(kind, message)
        .with_class(class)
        .with_detail(format!("{detail}\nSQLSTATE {code}"));
    if let Some(h) = db.hint() {
        e = e.with_hint(h.to_string());
    } else if kind == ErrorKind::Auth {
        e = e.with_hint("check the user name and password for this connection");
    }
    e
}

fn io_phrase(io: &std::io::Error) -> &'static str {
    use std::io::ErrorKind as K;
    match io.kind() {
        K::ConnectionRefused => {
            "the server refused the connection (is it running and accepting TCP connections?)"
        }
        K::TimedOut => "the connection timed out",
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe | K::UnexpectedEof => {
            "the connection was reset"
        }
        K::HostUnreachable | K::NetworkUnreachable => "the server is unreachable",
        K::AddrNotAvailable | K::NotFound => "the host name could not be resolved",
        _ => "a network error occurred",
    }
}

fn tls_hint(e: &rustls::Error) -> &'static str {
    match e {
        rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName)
        | rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForNameContext { .. }) => {
            "the server certificate is for a different host name; use verify-ca, or connect using the name on the certificate"
        }
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer) => {
            "the certificate is not signed by a trusted root; set the root certificate file on the connection"
        }
        rustls::Error::InvalidCertificate(rustls::CertificateError::Expired) => "the server certificate has expired",
        _ => "check the connection's sslmode and root certificate",
    }
}
