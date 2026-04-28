use std::path::Path;

use anyhow::{Context, Result};
use quinn::ServerConfig;

#[derive(Debug)]
pub enum CertSource {
    Ephemeral,
    LoadedFromDisk,
    GeneratedAndSaved,
}

pub fn make_server_config(
    cert_path: Option<&Path>,
    key_path: Option<&Path>,
) -> Result<(ServerConfig, CertSource)> {
    match (cert_path, key_path) {
        (Some(cp), Some(kp)) if cp.exists() && kp.exists() => {
            let cert_der = std::fs::read(cp).with_context(|| format!("reading {cp:?}"))?;
            let key_der = std::fs::read(kp).with_context(|| format!("reading {kp:?}"))?;
            Ok((build_config(cert_der, key_der)?, CertSource::LoadedFromDisk))
        }
        (Some(cp), Some(kp)) => {
            let (cert_der, key_der) = generate_self_signed()?;
            std::fs::write(cp, &cert_der).with_context(|| format!("writing {cp:?}"))?;
            std::fs::write(kp, &key_der).with_context(|| format!("writing {kp:?}"))?;
            Ok((
                build_config(cert_der, key_der)?,
                CertSource::GeneratedAndSaved,
            ))
        }
        (None, None) => {
            let (cert_der, key_der) = generate_self_signed()?;
            Ok((build_config(cert_der, key_der)?, CertSource::Ephemeral))
        }
        _ => unreachable!("clap enforces both cert-path and key-path or neither"),
    }
}

fn generate_self_signed() -> Result<(Vec<u8>, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("generate self-signed cert")?;
    Ok((cert.cert.der().to_vec(), cert.key_pair.serialize_der()))
}

fn build_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<ServerConfig> {
    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );
    Ok(ServerConfig::with_single_cert(cert_chain, private_key)?)
}
