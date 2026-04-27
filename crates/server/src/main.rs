use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, ClientMessage, Direction, Genome, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST, ServerMessage,
};
use quinn::{Endpoint, ServerConfig};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};
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

    /// Simulation tick rate in Hz.
    #[arg(long, default_value_t = 10)]
    tick_hz: u32,

    /// Path to a world snapshot file. Loaded at startup if it exists
    /// (overriding --world-width/--world-height); otherwise a fresh world is
    /// built and saved here on graceful shutdown. Without this flag, the
    /// world is ephemeral.
    #[arg(long)]
    world_file: Option<PathBuf>,

    /// Seconds between auto-saves to --world-file. Set to 0 to disable
    /// auto-saves; the final shutdown save still runs.
    #[arg(long, default_value_t = 30)]
    autosave_secs: u64,
}

#[derive(Debug)]
enum CertSource {
    Ephemeral,
    LoadedFromDisk,
    GeneratedAndSaved,
}

struct SimState {
    chunks_x: u32,
    chunks_y: u32,
    world: std::sync::Mutex<Vec<Chunk>>,
    tick_tx: broadcast::Sender<Arc<Vec<u8>>>,
    next_plant_id: AtomicU32,
}

#[derive(Serialize, Deserialize)]
struct WorldSnapshot {
    chunks_x: u32,
    chunks_y: u32,
    next_plant_id: u32,
    chunks: Vec<Chunk>,
}

fn load_world(path: &Path) -> Result<WorldSnapshot> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let snapshot: WorldSnapshot = rmp_serde::from_slice(&bytes)?;
    info!(
        path = %path.display(),
        bytes = bytes.len(),
        chunks = snapshot.chunks.len(),
        chunks_x = snapshot.chunks_x,
        chunks_y = snapshot.chunks_y,
        next_plant_id = snapshot.next_plant_id,
        "world loaded",
    );
    Ok(snapshot)
}

fn save_world(path: &Path, state: &SimState) -> Result<()> {
    let snapshot = WorldSnapshot {
        chunks_x: state.chunks_x,
        chunks_y: state.chunks_y,
        next_plant_id: state.next_plant_id.load(Ordering::Relaxed),
        chunks: state.world.lock().expect("sim lock poisoned").clone(),
    };
    let bytes = rmp_serde::to_vec(&snapshot)?;
    std::fs::write(path, &bytes).with_context(|| format!("write {path:?}"))?;
    info!(path = %path.display(), bytes = bytes.len(), "world saved");
    Ok(())
}

fn load_or_build(args: &Args) -> Result<WorldSnapshot> {
    if let Some(path) = &args.world_file {
        if path.exists() {
            return load_world(path);
        }
    }
    let mut chunks = build_world(args.world_width, args.world_height);
    place_showcase(&mut chunks, args.world_width);
    Ok(WorldSnapshot {
        chunks_x: args.world_width,
        chunks_y: args.world_height,
        next_plant_id: 1,
        chunks,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let initial = load_or_build(&args)?;
    let (tick_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(8);
    let state = Arc::new(SimState {
        chunks_x: initial.chunks_x,
        chunks_y: initial.chunks_y,
        world: std::sync::Mutex::new(initial.chunks),
        tick_tx,
        next_plant_id: AtomicU32::new(initial.next_plant_id),
    });

    info!(
        chunks_x = state.chunks_x,
        chunks_y = state.chunks_y,
        cells = (state.chunks_x as usize) * (state.chunks_y as usize) * CHUNK_AREA,
        "world ready"
    );

    if args.autosave_secs > 0 {
        if let Some(path) = args.world_file.clone() {
            let save_state = Arc::clone(&state);
            let interval = Duration::from_secs(args.autosave_secs);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(interval);
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if let Err(e) = save_world(&path, &save_state) {
                        warn!("autosave failed: {e:#}");
                    }
                }
            });
            info!(autosave_secs = args.autosave_secs, "autosave enabled");
        }
    }

    let sim_state = Arc::clone(&state);
    let tick_hz = args.tick_hz.max(1);
    tokio::spawn(async move {
        run_sim_loop(sim_state, tick_hz).await;
    });
    info!(tick_hz, "sim loop started");

    let (server_config, cert_source) = make_server_config(&args)?;
    let endpoint = Endpoint::server(server_config, args.listen)?;

    info!(addr = %args.listen, "server listening");
    info!(?cert_source, "tls cert ready");

    let serve_state = Arc::clone(&state);
    tokio::select! {
        _ = serve(serve_state, endpoint) => {},
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
    }

    if let Some(path) = &args.world_file {
        if let Err(e) = save_world(path, &state) {
            error!("final save failed: {e:#}");
        }
    }

    Ok(())
}

async fn serve(state: Arc<SimState>, endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, state).await {
                error!("connection error: {e:#}");
            }
        });
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn place_showcase(chunks: &mut [Chunk], chunks_x: u32) {
    let plant = 1u32;
    let energy = 200u16;
    let bare_genome = || Box::new(Genome { bytes: Vec::new() });

    let entries: Vec<(i32, i32, Occupant)> = vec![
        (
            10,
            20,
            Occupant::Leaf {
                plant,
                energy,
                facing: Direction::East,
            },
        ),
        (
            12,
            20,
            Occupant::Leaf {
                plant,
                energy,
                facing: Direction::North,
            },
        ),
        (14, 20, Occupant::Root { plant, energy }),
        (
            16,
            20,
            Occupant::Stem {
                plant,
                energy,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
            },
        ),
        (
            18,
            20,
            Occupant::Stem {
                plant,
                energy,
                connections: STEM_CONNECT_NORTH
                    | STEM_CONNECT_EAST
                    | STEM_CONNECT_SOUTH
                    | STEM_CONNECT_WEST,
            },
        ),
        (
            20,
            20,
            Occupant::Stem {
                plant,
                energy,
                connections: STEM_CONNECT_EAST | STEM_CONNECT_SOUTH,
            },
        ),
        (22, 20, Occupant::Antenna { plant, energy }),
        (
            24,
            20,
            Occupant::Sprout {
                plant,
                energy,
                facing: Direction::North,
                genome: bare_genome(),
            },
        ),
        (
            26,
            20,
            Occupant::Seed {
                plant,
                energy,
                facing: Direction::East,
                genome: bare_genome(),
            },
        ),
    ];
    for (x, y, occupant) in entries {
        place_at(chunks, chunks_x, x, y, occupant);
    }
}

fn place_at(chunks: &mut [Chunk], chunks_x: u32, x: i32, y: i32, occupant: Occupant) {
    if x < 0 || y < 0 {
        return;
    }
    let edge = CHUNK_EDGE as i32;
    let cx = x / edge;
    let cy = y / edge;
    let lx = (x % edge) as usize;
    let ly = (y % edge) as usize;
    let chunk_idx = (cy as usize) * (chunks_x as usize) + (cx as usize);
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
    if let Some(chunk) = chunks.get_mut(chunk_idx) {
        if let Some(cell) = chunk.cells.get_mut(cell_idx) {
            cell.occupant = occupant;
        }
    }
}

fn build_world(chunks_x: u32, chunks_y: u32) -> Vec<Chunk> {
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
    chunks
}

async fn run_sim_loop(state: Arc<SimState>, tick_hz: u32) {
    let mut interval = tokio::time::interval(Duration::from_millis(1000 / tick_hz as u64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tick: u64 = 0;

    loop {
        interval.tick().await;
        tick = tick.wrapping_add(1);

        let snapshot_chunks = {
            let mut chunks = state.world.lock().expect("sim lock poisoned");
            mutate_world(&mut chunks, tick);
            chunks.clone()
        };

        let msg = ServerMessage::ChunkBatch(snapshot_chunks);
        match rmp_serde::to_vec(&msg) {
            Ok(bytes) => {
                let _ = state.tick_tx.send(Arc::new(bytes));
            }
            Err(e) => error!("serialize tick failed: {e:#}"),
        }
    }
}

fn mutate_world(chunks: &mut [Chunk], tick: u64) {
    let edge = CHUNK_EDGE as i64;
    for chunk in chunks {
        let cx = chunk.coord.x as i64;
        let cy = chunk.coord.y as i64;
        for (i, cell) in chunk.cells.iter_mut().enumerate() {
            let lx = (i as i64) % edge;
            let ly = (i as i64) / edge;
            let world_x = cx * edge + lx;
            let world_y = cy * edge + ly;
            let phase = (world_x + world_y).wrapping_sub(tick as i64).div_euclid(6);
            cell.sunlit = phase.rem_euclid(2) == 0;
        }
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

async fn handle_connection(incoming: quinn::Incoming, state: Arc<SimState>) -> Result<()> {
    let conn = incoming.await?;
    let remote = conn.remote_address();
    info!(%remote, "connection accepted");

    let push_conn = conn.clone();
    let push_rx = state.tick_tx.subscribe();
    let push_task = tokio::spawn(async move {
        if let Err(e) = push_loop(push_conn, push_rx).await {
            warn!("push loop ended: {e:#}");
        }
    });

    let uni_conn = conn.clone();
    let uni_state = Arc::clone(&state);
    let uni_task = tokio::spawn(async move {
        accept_client_uni_streams(uni_conn, uni_state).await;
    });

    let result = handle_request_streams(conn, state).await;
    push_task.abort();
    uni_task.abort();
    result
}

async fn accept_client_uni_streams(conn: quinn::Connection, state: Arc<SimState>) {
    loop {
        let recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(_) => return,
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_client_uni(recv, state).await {
                warn!("client uni stream error: {e:#}");
            }
        });
    }
}

async fn handle_client_uni(mut recv: quinn::RecvStream, state: Arc<SimState>) -> Result<()> {
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    debug!(?msg, "received client command");

    match msg {
        ClientMessage::SpawnSprout { x, y, facing } => {
            spawn_sprout(&state, x, y, facing);
        }
        other => warn!(?other, "unexpected message on client uni stream"),
    }
    Ok(())
}

fn spawn_sprout(state: &SimState, x: i32, y: i32, facing: Direction) {
    let edge = CHUNK_EDGE as i32;
    let max_x = state.chunks_x as i32 * edge;
    let max_y = state.chunks_y as i32 * edge;
    if x < 0 || y < 0 || x >= max_x || y >= max_y {
        warn!(x, y, "spawn out of bounds");
        return;
    }
    let cx = x / edge;
    let cy = y / edge;
    let lx = (x % edge) as usize;
    let ly = (y % edge) as usize;
    let chunk_idx = (cy as usize) * (state.chunks_x as usize) + (cx as usize);
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;

    let plant = state.next_plant_id.fetch_add(1, Ordering::Relaxed);
    let mut chunks = state.world.lock().expect("sim lock poisoned");
    chunks[chunk_idx].cells[cell_idx].occupant = Occupant::Sprout {
        plant,
        energy: 100,
        facing,
        genome: Box::new(Genome { bytes: Vec::new() }),
    };
    info!(x, y, plant, ?facing, "sprout spawned");
}

async fn handle_request_streams(conn: quinn::Connection, state: Arc<SimState>) -> Result<()> {
    let remote = conn.remote_address();
    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => {
                info!(%remote, "connection closed");
                return Ok(());
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, state).await {
                error!("stream error: {e:#}");
            }
        });
    }
}

async fn handle_stream(
    (mut send, mut recv): (quinn::SendStream, quinn::RecvStream),
    state: Arc<SimState>,
) -> Result<()> {
    let buf = recv.read_to_end(64 * 1024).await?;
    let msg: ClientMessage = rmp_serde::from_slice(&buf)?;
    debug!(?msg, "received");

    let response = match msg {
        ClientMessage::Hello => Some(ServerMessage::Welcome {
            world_chunks_x: state.chunks_x,
            world_chunks_y: state.chunks_y,
        }),
        ClientMessage::Subscribe => {
            let chunks = state.world.lock().expect("sim lock poisoned").clone();
            Some(ServerMessage::ChunkBatch(chunks))
        }
        ClientMessage::SpawnSprout { .. } => {
            warn!("SpawnSprout arrived on bidi stream; expected on uni");
            None
        }
    };
    if let Some(response) = response {
        let bytes = rmp_serde::to_vec(&response)?;
        send.write_all(&bytes).await?;
    }
    send.finish()?;

    Ok(())
}

async fn push_loop(
    conn: quinn::Connection,
    mut rx: broadcast::Receiver<Arc<Vec<u8>>>,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let bytes = match rx.recv().await {
            Ok(b) => b,
            Err(RecvError::Lagged(n)) => {
                warn!(skipped = n, "push receiver lagged");
                continue;
            }
            Err(RecvError::Closed) => return Ok(()),
        };
        let mut send = match conn.open_uni().await {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        send.write_all(&bytes).await?;
        send.finish()?;
    }
}
