//! The OS trust store as a PEM bundle, so a child verifies server certificates against the
//! same roots the in-app client does when a connection has no root certificate configured.
//! (`sslrootcert=system` would do this but only exists in libpq 16+; the discovered tool may
//! be older, and without the file libpq falls back to `~/.postgresql/root.crt`.)

use pgwhy_errors::{AppError, AppResult, ErrorKind};

/// Load the system root certificates as one PEM string.
pub(crate) fn system_roots_pem() -> AppResult<String> {
    let loaded = rustls_native_certs::load_native_certs();
    if loaded.certs.is_empty() {
        let mut err = AppError::new(
            ErrorKind::Tls,
            "no system root certificates could be loaded for pg_dump/pg_restore",
        )
        .with_hint("set a root certificate file on the connection");
        if let Some(first) = loaded.errors.first() {
            err = err.with_detail(first.to_string());
        }
        return Err(err);
    }
    let mut pem = String::new();
    for cert in &loaded.certs {
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        let b64 = base64(cert.as_ref());
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).unwrap_or_default());
            pem.push('\n');
        }
        pem.push_str("-----END CERTIFICATE-----\n");
    }
    Ok(pem)
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }
}
