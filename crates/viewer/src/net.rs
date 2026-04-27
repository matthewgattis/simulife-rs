use std::sync::Arc;

use anyhow::{Result, bail};
use protocol::{ClientMessage, ServerMessage};
use winit::event_loop::EventLoopProxy;

use crate::app::{NetworkStatus, UserEvent};

pub const SERVER_ADDR: &str = "127.0.0.1:4433";

pub async fn run_client(proxy: EventLoopProxy<UserEvent>) -> Result<()> {
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connecting));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(make_insecure_client_config()?);

    let server_addr = SERVER_ADDR.parse()?;
    let conn = endpoint.connect(server_addr, "localhost")?.await?;
    println!("connected to {}", conn.remote_address());

    let welcome = request(&conn, &ClientMessage::Hello).await?;
    let (world_chunks_x, world_chunks_y) = match welcome {
        ServerMessage::Welcome {
            world_chunks_x,
            world_chunks_y,
        } => (world_chunks_x, world_chunks_y),
        other => bail!("unexpected first message: {other:?}"),
    };
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connected {
        world_chunks_x,
        world_chunks_y,
    }));

    let batch = request(&conn, &ClientMessage::Subscribe).await?;
    match batch {
        ServerMessage::ChunkBatch(chunks) => {
            println!("received {} chunks", chunks.len());
            let _ = proxy.send_event(UserEvent::Chunks(chunks));
        }
        other => bail!("expected ChunkBatch, got {other:?}"),
    }

    Ok(())
}

async fn request(conn: &quinn::Connection, msg: &ClientMessage) -> Result<ServerMessage> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&rmp_serde::to_vec(msg)?).await?;
    send.finish()?;
    let buf = recv.read_to_end(8 * 1024 * 1024).await?;
    Ok(rmp_serde::from_slice(&buf)?)
}

fn make_insecure_client_config() -> Result<quinn::ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(quic)))
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
