//! sslmode → rustls connector mapping, with libpq semantics.
//!
//! | sslmode     | TLS        | chain  | hostname | falls back to plaintext |
//! |-------------|------------|--------|----------|-------------------------|
//! | disable     | never      | –      | –        | –                       |
//! | prefer      | if offered | no     | no       | only if the server doesn't offer TLS |
//! | require     | required   | no     | no       | never                   |
//! | verify-ca   | required   | yes    | no       | never                   |
//! | verify-full | required   | yes    | yes      | never                   |
//!
//! Certificate errors are always hard failures — `prefer` never retries in plaintext after a
//! TLS failure. Through a tunnel the hostname checked is the entry's real host (the dial
//! address is supplied separately as `hostaddr`, which tokio-postgres and libpq both honour).

use crate::config::SslMode;
use crate::{AppError, AppResult, ErrorKind};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use std::path::Path;
use std::sync::Arc;

/// What a live connection's transport actually turned out to be (R2 badge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsState {
    Plaintext,
    EncryptedUnverified,
    EncryptedVerified,
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The connector plus the tokio-postgres sslmode to pair it with.
pub(crate) struct TlsSetup {
    pub connector: tokio_postgres_rustls::MakeRustlsConnect,
    pub mode: tokio_postgres::config::SslMode,
}

pub(crate) fn setup(mode: SslMode, root_cert: Option<&Path>) -> AppResult<TlsSetup> {
    use tokio_postgres::config::SslMode as Pg;
    let builder = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::internal("TLS provider setup failed").with_detail(e.to_string()))?;
    let (config, pg_mode) = match mode {
        // disable still gets a connector; tokio-postgres never uses it with SslMode::Disable
        SslMode::Disable | SslMode::Prefer | SslMode::Require => {
            let config = builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider())))
                .with_no_client_auth();
            let pg = match mode {
                SslMode::Disable => Pg::Disable,
                SslMode::Prefer => Pg::Prefer,
                _ => Pg::Require,
            };
            (config, pg)
        }
        SslMode::VerifyCa => {
            let inner = webpki_verifier(root_cert)?;
            let config = builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ChainOnly(inner)))
                .with_no_client_auth();
            (config, Pg::Require)
        }
        SslMode::VerifyFull => {
            let config = builder
                .with_webpki_verifier(webpki_verifier(root_cert)?)
                .with_no_client_auth();
            (config, Pg::Require)
        }
    };
    Ok(TlsSetup {
        connector: tokio_postgres_rustls::MakeRustlsConnect::new(config),
        mode: pg_mode,
    })
}

/// The TLS state a successful connection in `mode` implies, given whether the server reported
/// the session as encrypted.
pub(crate) fn resulting_state(mode: SslMode, encrypted: bool) -> TlsState {
    match (encrypted, mode.verifies_chain()) {
        (false, _) => TlsState::Plaintext,
        (true, true) => TlsState::EncryptedVerified,
        (true, false) => TlsState::EncryptedUnverified,
    }
}

fn load_roots(root_cert: Option<&Path>) -> AppResult<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    match root_cert {
        Some(path) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
                .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                .map_err(|e| {
                    AppError::new(
                        ErrorKind::Tls,
                        format!("could not read root certificates from {}", path.display()),
                    )
                    .with_detail(e.to_string())
                })?;
            let (added, _) = roots.add_parsable_certificates(certs);
            if added == 0 {
                return Err(AppError::new(
                    ErrorKind::Tls,
                    format!("{} contains no usable root certificates", path.display()),
                ));
            }
        }
        None => {
            let native = rustls_native_certs::load_native_certs();
            roots.add_parsable_certificates(native.certs);
            if roots.is_empty() {
                return Err(AppError::new(
                    ErrorKind::Tls,
                    "no system root certificates could be loaded",
                )
                .with_hint("set a root certificate file on the connection"));
            }
        }
    }
    Ok(roots)
}

fn webpki_verifier(root_cert: Option<&Path>) -> AppResult<Arc<WebPkiServerVerifier>> {
    WebPkiServerVerifier::builder_with_provider(Arc::new(load_roots(root_cert)?), provider())
        .build()
        .map_err(|e| {
            AppError::new(ErrorKind::Tls, "could not build the certificate verifier")
                .with_detail(e.to_string())
        })
}

/// sslmode=prefer/require: encrypted, identity not verified.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// sslmode=verify-ca: full chain verification, hostname mismatch tolerated.
#[derive(Debug)]
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resulting_state_matrix() {
        assert_eq!(resulting_state(SslMode::Prefer, false), TlsState::Plaintext);
        assert_eq!(
            resulting_state(SslMode::Prefer, true),
            TlsState::EncryptedUnverified
        );
        assert_eq!(
            resulting_state(SslMode::Require, true),
            TlsState::EncryptedUnverified
        );
        assert_eq!(
            resulting_state(SslMode::VerifyCa, true),
            TlsState::EncryptedVerified
        );
        assert_eq!(
            resulting_state(SslMode::VerifyFull, true),
            TlsState::EncryptedVerified
        );
    }

    #[test]
    fn missing_root_file_is_a_tls_error() {
        let err = setup(
            SslMode::VerifyFull,
            Some(Path::new("/nonexistent/root.crt")),
        )
        .err()
        .unwrap();
        assert_eq!(err.kind, ErrorKind::Tls);
    }
}
