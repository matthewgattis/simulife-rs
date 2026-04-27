use std::net::SocketAddr;

use anyhow::{Context, Result};
use protocol::{ClientMessage, ServerMessage};
use quinn::{Endpoint, ServerConfig};

const LISTEN_ADDR: &str = "127.0.0.1:4433";

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let (server_config, cert_der) = make_server_config()?;
    let addr: SocketAddr = LISTEN_ADDR.parse()?;
    let endpoint = Endpoint::server(server_config, addr)?;

    println!("server listening on {addr}");
    println!("self-signed cert ({} bytes DER) — clients must trust this", cert_der.len());

    while let Some(incoming) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming).await {
                eprintln!("connection error: {e:#}");
            }
        });
    }

    Ok(())
}

fn make_server_config() -> Result<(ServerConfig, Vec<u8>)> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .context("generate self-signed cert")?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.key_pair.serialize_der();

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der.clone())];
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
    );

    let config = ServerConfig::with_single_cert(cert_chain, private_key)?;
    Ok((config, cert_der))
}

async fn handle_connection(incoming: quinn::Incoming) -> Result<()> {
    let conn = incoming.await?;
    println!("connection from {}", conn.remote_address());

    let (mut send, mut recv) = conn.accept_bi().await?;
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    println!("received: {msg:?}");

    let welcome = ServerMessage::Welcome {
        world_chunks_x: 16,
        world_chunks_y: 16,
    };
    let bytes = rmp_serde::to_vec(&welcome)?;
    send.write_all(&bytes).await?;
    send.finish()?;

    conn.closed().await;
    Ok(())
}
