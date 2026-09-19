use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio_postgres::{CancelToken, Client};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyFull,
}

#[derive(Debug, Clone)]
pub struct Dsn {
    pub config: tokio_postgres::Config,
    pub mode: SslMode,
    pub root_cert: Option<String>,
}

impl Dsn {
    pub fn parse(dsn: &str) -> Result<Dsn> {
        if dsn.contains("://") {
            let config: tokio_postgres::Config = dsn.parse().context("parsing the URI dsn")?;
            let mode = match config.get_ssl_mode() {
                tokio_postgres::config::SslMode::Disable => SslMode::Disable,
                tokio_postgres::config::SslMode::Require => SslMode::Require,
                _ => SslMode::Prefer,
            };
            return Ok(Dsn {
                config,
                mode,
                root_cert: None,
            });
        }
        let mut mode = SslMode::Prefer;
        let mut root_cert = None;
        let mut kept: Vec<String> = Vec::new();
        for part in dsn.split_whitespace() {
            let Some((key, value)) = part.split_once('=') else {
                kept.push(part.to_string());
                continue;
            };
            let value = value.trim_matches('\'');
            match key {
                "sslmode" => {
                    mode = match value {
                        "disable" => SslMode::Disable,
                        "allow" | "prefer" => SslMode::Prefer,
                        "require" => SslMode::Require,
                        "verify-full" => SslMode::VerifyFull,
                        "verify-ca" => bail!(
                            "sslmode=verify-ca checks the chain but not the host name, which \
                             this connector does not offer; use verify-full or require"
                        ),
                        other => bail!("unknown sslmode {other:?}"),
                    };
                }
                "sslrootcert" => root_cert = Some(value.to_string()),
                _ => kept.push(part.to_string()),
            }
        }
        kept.push(format!(
            "sslmode={}",
            match mode {
                SslMode::Disable => "disable",
                SslMode::Prefer => "prefer",
                SslMode::Require | SslMode::VerifyFull => "require",
            }
        ));
        let config: tokio_postgres::Config = kept
            .join(" ")
            .parse()
            .inspect_err(|e| {
                tracing::warn!(
                    target: "odoo_kernel::connect",
                    error = %e,
                    keywords = kept.len(),
                    "the connector rejected the dsn; no connection will be opened from it"
                );
            })
            .context("parsing the keyword dsn")?;
        Ok(Dsn {
            config,
            mode,
            root_cert,
        })
    }

    pub fn tls(&self) -> Result<Option<tokio_postgres_rustls::MakeRustlsConnect>> {
        tracing::debug!(
            target: "odoo_kernel::connect",
            mode = ?self.mode,
            root_cert = self.root_cert.as_deref().unwrap_or("-"),
            verifies_host = self.mode == SslMode::VerifyFull,
            "building the TLS connector"
        );
        if self.mode == SslMode::Disable {
            return Ok(None);
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("rustls protocol versions")?;
        let config = match self.mode {
            SslMode::VerifyFull => {
                let mut roots = rustls::RootCertStore::empty();
                let native = rustls_native_certs::load_native_certs();
                for cert in native.certs {
                    let _ = roots.add(cert);
                }
                if let Some(path) = &self.root_cert {
                    let pem = std::fs::read(path)
                        .with_context(|| format!("reading sslrootcert {path}"))?;
                    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                        roots
                            .add(cert.context("a certificate in sslrootcert")?)
                            .context("adding sslrootcert to the root store")?;
                    }
                }
                builder.with_root_certificates(roots).with_no_client_auth()
            }
            _ => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(EncryptOnly(provider)))
                .with_no_client_auth(),
        };
        Ok(Some(tokio_postgres_rustls::MakeRustlsConnect::new(config)))
    }
}

#[derive(Debug)]
struct EncryptOnly(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for EncryptOnly {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub type FaultSlot = Arc<std::sync::Mutex<Option<String>>>;

fn describe_fault(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(d) => format!("SQLSTATE:{}|{}", d.code().code(), d.message()),
        None => format!("SQLSTATE:|{e}"),
    }
}

pub async fn connect(dsn: &str) -> Result<Client> {
    connect_with_fault(dsn).await.map(|(client, _)| client)
}

pub async fn connect_with_fault(dsn: &str) -> Result<(Client, FaultSlot)> {
    let fault: FaultSlot = Arc::new(std::sync::Mutex::new(None));
    let t0 = std::time::Instant::now();
    let parsed = Dsn::parse(dsn)?;
    let tls = parsed.tls()?;
    let encrypted = tls.is_some();
    let opened = |t0: std::time::Instant| {
        tracing::debug!(
            target: "odoo_kernel::connect",
            dbname = parsed.config.get_dbname().unwrap_or("-"),
            user = parsed.config.get_user().unwrap_or("-"),
            encrypted,
            ms = t0.elapsed().as_secs_f64() * 1000.0,
            "opened a postgres connection"
        );
    };
    match tls {
        None => {
            let (client, conn) = parsed.config.connect(tokio_postgres::NoTls).await?;
            opened(t0);
            let slot = fault.clone();
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::error!(error = %e, "postgres connection dropped");
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(describe_fault(&e));
                    }
                }
            });
            Ok((client, fault))
        }
        Some(tls) => {
            let (client, conn) = parsed.config.connect(tls).await?;
            opened(t0);
            let slot = fault.clone();
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::error!(error = %e, "postgres connection dropped");
                    if let Ok(mut s) = slot.lock() {
                        *s = Some(describe_fault(&e));
                    }
                }
            });
            Ok((client, fault))
        }
    }
}

pub async fn cancel(token: CancelToken, dsn: &str) {
    tracing::debug!(
        target: "odoo_kernel::connect",
        "cancelling the query on a poisoned connection"
    );
    let Ok(parsed) = Dsn::parse(dsn) else {
        tracing::warn!(
            target: "odoo_kernel::connect",
            "cannot cancel: the dsn no longer parses"
        );
        return;
    };
    match parsed.tls() {
        Ok(Some(tls)) => {
            let _ = token.cancel_query(tls).await;
        }
        _ => {
            let _ = token.cancel_query(tokio_postgres::NoTls).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sslmode_and_rootcert_are_lifted_out_of_the_dsn_the_connector_parses() {
        let d = Dsn::parse("host=db user=u dbname=x sslmode=verify-full sslrootcert=/etc/ca.pem")
            .unwrap();
        assert_eq!(d.mode, SslMode::VerifyFull);
        assert_eq!(d.root_cert.as_deref(), Some("/etc/ca.pem"));
        assert_eq!(
            d.config.get_ssl_mode(),
            tokio_postgres::config::SslMode::Require
        );
        let d = Dsn::parse("host=/var/run/postgresql user=u dbname=x").unwrap();
        assert_eq!(d.mode, SslMode::Prefer);
        assert!(Dsn::parse("host=db sslmode=verify-ca").is_err());
        assert!(Dsn::parse("host=db sslmode=sideways").is_err());
    }

    #[test]
    fn disable_needs_no_connector_and_the_others_build_one() {
        assert!(
            Dsn::parse("host=db sslmode=disable")
                .unwrap()
                .tls()
                .unwrap()
                .is_none()
        );
        assert!(
            Dsn::parse("host=db sslmode=require")
                .unwrap()
                .tls()
                .unwrap()
                .is_some()
        );
        assert!(
            Dsn::parse("host=db sslmode=verify-full")
                .unwrap()
                .tls()
                .unwrap()
                .is_some()
        );
    }
}
