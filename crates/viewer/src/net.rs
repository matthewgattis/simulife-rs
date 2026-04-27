use std::{net::SocketAddr, sync::Arc};

use anyhow::{Result, bail};
use protocol::{ClientMessage, ServerMessage};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, info, warn};
use winit::event_loop::EventLoopProxy;

use crate::app::{NetworkStatus, UserEvent};

pub async fn run_client(
    server_addr: SocketAddr,
    proxy: EventLoopProxy<UserEvent>,
    mut outgoing: UnboundedReceiver<ClientMessage>,
) -> Result<()> {
    let _ = proxy.send_event(UserEvent::Network(NetworkStatus::Connecting));

    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(make_insecure_client_config()?);

    let conn = endpoint.connect(server_addr, "localhost")?.await?;
    info!(remote = %conn.remote_address(), "connected");

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
            info!(count = chunks.len(), "received initial chunk batch");
            let _ = proxy.send_event(UserEvent::Chunks(chunks));
        }
        other => bail!("expected ChunkBatch, got {other:?}"),
    }

    let outgoing_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if let Err(e) = send_command(&outgoing_conn, &msg).await {
                warn!("send command failed: {e:#}");
            }
        }
    });

    info!("listening for tick updates");
    loop {
        let mut recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(e) => {
                info!("server stream closed: {e}");
                return Ok(());
            }
        };
        let buf = recv.read_to_end(8 * 1024 * 1024).await?;
        let msg: ServerMessage = rmp_serde::from_slice(&buf)?;
        match msg {
            ServerMessage::ChunkBatch(chunks) => {
                debug!(count = chunks.len(), "tick chunk batch");
                let _ = proxy.send_event(UserEvent::Chunks(chunks));
            }
            other => warn!(?other, "unexpected push message"),
        }
    }
}

async fn send_command(conn: &quinn::Connection, msg: &ClientMessage) -> Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(&rmp_serde::to_vec(msg)?).await?;
    send.finish()?;
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
