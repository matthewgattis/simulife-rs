use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, ClientMessage, Occupant, ServerMessage,
};
use quinn::{Endpoint, ServerConfig};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "cellular-automata simulation server")]
struct Args {
    /// Address to bind the QUIC listener on.
    #[arg(long, default_value = "127.0.0.1:4433")]
    listen: SocketAddr,

    /// Path to a TLS certificate (DER). If both --cert-path and --key-path are
    /// provided, the cert is loaded from disk; if the files don't exist, a
    /// fresh self-signed cert is generated and written there. If neither path
    /// is provided, an ephemeral cert is generated per startup.
    #[arg(long, requires = "key_path")]
    cert_path: Option<PathBuf>,

    /// Path to the matching PKCS#8 private key (DER). See --cert-path.
    #[arg(long, requires = "cert_path")]
    key_path: Option<PathBuf>,

    /// World size in chunks (X axis).
    #[arg(long, default_value_t = 4)]
    world_width: u32,

    /// World size in chunks (Y axis).
    #[arg(long, default_value_t = 4)]
    world_height: u32,
}

#[derive(Debug)]
enum CertSource {
    Ephemeral,
    LoadedFromDisk,
    GeneratedAndSaved,
}

struct World {
    chunks_x: u32,
    chunks_y: u32,
    chunks: Vec<Chunk>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let world = Arc::new(build_world(args.world_width, args.world_height));
    info!(
        chunks_x = world.chunks_x,
        chunks_y = world.chunks_y,
        cells = world.chunks.len() * CHUNK_AREA,
        "world built"
    );

    let (server_config, cert_source) = make_server_config(&args)?;
    let endpoint = Endpoint::server(server_config, args.listen)?;

    info!(addr = %args.listen, "server listening");
    info!(?cert_source, "tls cert ready");

    while let Some(incoming) = endpoint.accept().await {
        let world = Arc::clone(&world);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, world).await {
                error!("connection error: {e:#}");
            }
        });
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn build_world(chunks_x: u32, chunks_y: u32) -> World {
    let mut chunks = Vec::with_capacity((chunks_x * chunks_y) as usize);
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            let cells = (0..CHUNK_AREA)
                .map(|i| {
                    let local_x = (i % CHUNK_EDGE as usize) as u32;
                    let local_y = (i / CHUNK_EDGE as usize) as u32;
                    let world_x = cx * CHUNK_EDGE as u32 + local_x;
                    let world_y = cy * CHUNK_EDGE as u32 + local_y;
                    Cell {
                        organic: ((world_x ^ world_y) & 0xff) as u16,
                        soil_energy: 100,
                        sunlit: (world_x.wrapping_add(world_y)) % 3 != 0,
                        occupant: Occupant::Empty,
                    }
                })
                .collect();
            chunks.push(Chunk {
                coord: ChunkCoord {
                    x: cx as i32,
                    y: cy as i32,
                },
                cells,
            });
        }
    }
    World {
        chunks_x,
        chunks_y,
        chunks,
    }
}

fn make_server_config(args: &Args) -> Result<(ServerConfig, CertSource)> {
    match (&args.cert_path, &args.key_path) {
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

async fn handle_connection(incoming: quinn::Incoming, world: Arc<World>) -> Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    info!(%remote, "connection accepted");

    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => {
                info!(%remote, "connection closed");
                return Ok(());
            }
        };
        let world = Arc::clone(&world);
        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, world).await {
                error!("stream error: {e:#}");
            }
        });
    }
}

async fn handle_stream(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    world: Arc<World>,
) -> Result<()> {
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    debug!(?msg, "received");

    let response = match msg {
        ClientMessage::Hello => ServerMessage::Welcome {
            world_chunks_x: world.chunks_x,
            world_chunks_y: world.chunks_y,
        },
        ClientMessage::Subscribe => ServerMessage::ChunkBatch(world.chunks.clone()),
    };
    let bytes = rmp_serde::to_vec(&response)?;
    send.write_all(&bytes).await?;
    send.finish()?;

    Ok(())
}
