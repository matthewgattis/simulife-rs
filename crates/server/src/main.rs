mod net;
mod persist;
mod sim;
mod tls;
mod world;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU32},
    time::Duration,
};

use anyhow::Result;
use clap::Parser;
use protocol::CHUNK_AREA;
use quinn::Endpoint;
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::sim::{SimControl, SimState};

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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let initial = persist::load_or_build(
        args.world_file.as_deref(),
        args.world_width,
        args.world_height,
    )?;
    let (tick_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(8);
    let state = Arc::new(SimState {
        chunks_x: initial.chunks_x,
        chunks_y: initial.chunks_y,
        world: std::sync::Mutex::new(initial.chunks),
        tick_tx,
        next_plant_id: AtomicU32::new(initial.next_plant_id),
        control: std::sync::Mutex::new(SimControl {
            paused: false,
            tick_hz: args.tick_hz.max(1),
            step_pending: 0,
        }),
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
                    if let Err(e) = persist::save_world(&path, &save_state) {
                        warn!("autosave failed: {e:#}");
                    }
                }
            });
            info!(autosave_secs = args.autosave_secs, "autosave enabled");
        }
    }

    let sim_state = Arc::clone(&state);
    tokio::spawn(async move {
        sim::run_sim_loop(sim_state).await;
    });
    info!(tick_hz = args.tick_hz, "sim loop started");

    let (server_config, cert_source) =
        tls::make_server_config(args.cert_path.as_deref(), args.key_path.as_deref())?;
    let endpoint = Endpoint::server(server_config, args.listen)?;

    info!(addr = %args.listen, "server listening");
    info!(?cert_source, "tls cert ready");

    let serve_state = Arc::clone(&state);
    tokio::select! {
        _ = net::serve(serve_state, endpoint) => {},
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
    }

    if let Some(path) = &args.world_file {
        if let Err(e) = persist::save_world(path, &state) {
            error!("final save failed: {e:#}");
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
