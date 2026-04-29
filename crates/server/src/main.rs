mod net;
mod persist;
mod sim;
mod tls;
mod world;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64},
    },
    time::Duration,
};

use anyhow::Result;
use clap::Parser;
use protocol::CHUNK_AREA;
use quinn::Endpoint;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha12Rng;
use tokio::sync::broadcast;
use tracing::{error, info, warn};
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

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
    #[arg(long, default_value_t = 12)]
    world_width: u32,

    /// World size in chunks (Y axis).
    #[arg(long, default_value_t = 12)]
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

    /// World seed (u64). If omitted, a random seed is drawn from the OS RNG.
    /// Loaded snapshots already include their seed and override this flag.
    #[arg(long)]
    seed: Option<u64>,

    /// If set, write a Chrome-trace JSON profile to this path. Open with
    /// `chrome://tracing` or https://ui.perfetto.dev.
    #[arg(long)]
    trace_chrome: Option<PathBuf>,

    /// Start with the simulation running (skip the default paused state).
    /// Useful for unattended profiling runs.
    #[arg(long)]
    start_running: bool,

    /// Optional graceful exit after N seconds. Lets profiling runs flush
    /// trace data without manual intervention.
    #[arg(long)]
    profile_duration_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _trace_guard = init_tracing(args.trace_chrome.as_deref());

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    let mut initial = persist::load_or_build(
        args.world_file.as_deref(),
        args.world_width,
        args.world_height,
    )?;
    // Loaded snapshots carry their own seed/RNG; only the CLI flag (or a
    // freshly-drawn random) takes effect for newly-built worlds.
    let fresh_world = initial.seed.is_none();
    let seed = initial
        .seed
        .unwrap_or_else(|| args.seed.unwrap_or_else(|| rand::thread_rng().r#gen()));
    let mut rng = initial
        .rng
        .clone()
        .unwrap_or_else(|| ChaCha12Rng::seed_from_u64(seed));
    info!(seed, "world seed");
    if fresh_world {
        let count = world::place_random_sprout_grid(
            &mut initial.chunks,
            initial.chunks_x,
            initial.chunks_y,
            &mut rng,
        );
        initial.next_plant_id = count + 1;
        info!(sprouts = count, "placed initial sprout grid");
    }
    let (tick_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(8);
    let state = Arc::new(SimState {
        chunks_x: initial.chunks_x,
        chunks_y: initial.chunks_y,
        world: std::sync::Mutex::new(initial.chunks),
        tick_tx,
        next_plant_id: AtomicU32::new(initial.next_plant_id),
        current_tick: AtomicU64::new(initial.current_tick),
        control: std::sync::Mutex::new(SimControl {
            paused: !args.start_running,
            tick_hz: args.tick_hz.max(1),
            step_pending: 0,
        }),
        seed: AtomicU64::new(seed),
        rng: std::sync::Mutex::new(rng),
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
    let profile_timeout = async {
        match args.profile_duration_secs {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = net::serve(serve_state, endpoint) => {},
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
        _ = profile_timeout => {
            info!(secs = args.profile_duration_secs, "profile duration elapsed");
        }
    }

    if let Some(path) = &args.world_file {
        if let Err(e) = persist::save_world(path, &state) {
            error!("final save failed: {e:#}");
        }
    }

    Ok(())
}

fn init_tracing(trace_chrome: Option<&std::path::Path>) -> Option<FlushGuard> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn"));
    let fmt_layer = tracing_subscriber::fmt::layer();
    match trace_chrome {
        Some(path) => {
            let (chrome_layer, guard) = ChromeLayerBuilder::new()
                .file(path)
                .include_args(true)
                .build();
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .with(chrome_layer)
                .init();
            info!(path = %path.display(), "chrome trace recording");
            Some(guard)
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
            None
        }
    }
}
