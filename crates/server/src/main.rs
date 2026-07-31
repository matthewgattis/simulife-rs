#![allow(clippy::too_many_arguments)]

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
        atomic::{AtomicU32, AtomicU64, Ordering},
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

    /// World size in chunks (X axis). Default 36 = 3 boxes wide × 12
    /// chunks per box (each box matches the original single-box size).
    #[arg(long, default_value_t = 36)]
    world_width: u32,

    /// World size in chunks (Y axis). Default 24 = 2 boxes tall × 12.
    #[arg(long, default_value_t = 24)]
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

    /// Force the encode pipeline to run every tick even when no
    /// viewers are connected. Off by default — sim still ticks, but
    /// the wire-chunk build, diff, msgpack, and zstd are skipped to
    /// save CPU on long-running idle servers. Use this for profiling
    /// or when you want trace events for an unattended run.
    #[arg(long)]
    always_encode: bool,

    /// Determinism test mode: regenerate world with given seed, run for
    /// N ticks, print the final world state hash, and exit. Use this to
    /// verify that running with the same seed produces identical results.
    /// Example: --determinism-test 42:100 (seed 42, run 100 ticks).
    #[arg(long, value_name = "SEED:TICKS")]
    determinism_test: Option<String>,
}

fn count_occupants(chunks: &[protocol::Chunk]) -> (u32, u32, u32) {
    let mut sprouts = 0;
    let mut seeds = 0;
    let mut leaves = 0;
    for chunk in chunks {
        for cell in &chunk.cells {
            match cell.occupant {
                protocol::Occupant::Sprout { .. } => sprouts += 1,
                protocol::Occupant::Seed { .. } => seeds += 1,
                protocol::Occupant::Leaf { .. } => leaves += 1,
                _ => {}
            }
        }
    }
    (sprouts, seeds, leaves)
}

fn run_determinism_test(
    spec: &str,
    _chunks: Vec<protocol::Chunk>,
    chunks_x: u32,
    chunks_y: u32,
    world_gen_params: protocol::WorldGenParams,
    _sim_params: protocol::SimParams,
    _seed: u64,
    _rng: ChaCha12Rng,
    _next_plant_id: u32,
) -> Result<()> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let (seed_str, ticks_str) = spec
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected SEED:TICKS format"))?;
    let test_seed: u64 = seed_str.parse()?;
    let test_ticks: u64 = ticks_str.parse()?;

    // Use the same params as unit tests for consistency
    let test_sim_params = protocol::SimParams {
        world_wrap: false,
        ..protocol::SimParams::default()
    };

    println!("🔬 Determinism Test: seed={}, ticks={}, world_wrap={}", test_seed, test_ticks, test_sim_params.world_wrap);

    // Rebuild world with test seed
    let mut chunks = world::build_world(&world_gen_params);
    let mut rng = ChaCha12Rng::seed_from_u64(test_seed);
    let count = world::place_random_sprout_grid(&mut chunks, &world_gen_params, &mut rng);

    fn hash_chunks(chunks: &[protocol::Chunk]) -> u64 {
        let mut hasher = DefaultHasher::new();
        for chunk in chunks {
            for cell in &chunk.cells {
                // Hash ALL cell state comprehensively
                cell.organic.hash(&mut hasher);
                cell.soil_energy.hash(&mut hasher);
                cell.sunlit.hash(&mut hasher);
                cell.lineage_mutation_rate.to_bits().hash(&mut hasher);

                match &cell.occupant {
                    protocol::Occupant::Sprout { plant, clan, energy, facing, genome, current_gene, parent } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        std::mem::discriminant(facing).hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                        current_gene.hash(&mut hasher);
                        for gene in &genome.genes {
                            std::mem::discriminant(&gene.front).hash(&mut hasher);
                            std::mem::discriminant(&gene.left).hash(&mut hasher);
                            std::mem::discriminant(&gene.right).hash(&mut hasher);
                            gene.next.hash(&mut hasher);
                        }
                        genome.mutation_rate.hash(&mut hasher);
                    }
                    protocol::Occupant::Seed { plant, clan, energy, facing, genome, parent } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        std::mem::discriminant(facing).hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                        for gene in &genome.genes {
                            std::mem::discriminant(&gene.front).hash(&mut hasher);
                            std::mem::discriminant(&gene.left).hash(&mut hasher);
                            std::mem::discriminant(&gene.right).hash(&mut hasher);
                            gene.next.hash(&mut hasher);
                        }
                        genome.mutation_rate.hash(&mut hasher);
                    }
                    protocol::Occupant::Leaf { plant, clan, energy, facing, parent } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        std::mem::discriminant(facing).hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                    }
                    protocol::Occupant::Root { plant, clan, energy, parent } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                    }
                    protocol::Occupant::Antenna { plant, clan, energy, parent } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                    }
                    protocol::Occupant::Stem { plant, clan, energy, connections, parent, children } => {
                        plant.hash(&mut hasher);
                        clan.hash(&mut hasher);
                        energy.hash(&mut hasher);
                        connections.hash(&mut hasher);
                        std::mem::discriminant(parent).hash(&mut hasher);
                        children.hash(&mut hasher);
                    }
                    protocol::Occupant::Empty => {}
                }
            }
        }
        hasher.finish()
    }

    let hash_tick0 = hash_chunks(&chunks);
    println!("  Tick 0: hash={:016x}, chunks={}", hash_tick0, chunks.len());

    // Log RNG state before first tick
    println!("  RNG state before tick 1: (ChaCha12Rng is opaque, can't inspect directly)");

    let next_id = AtomicU32::new(count + 1);
    for tick in 1..=test_ticks {
        // Count occupants before tick
        let (sprouts_before, seeds_before, leaves_before) = count_occupants(&chunks);

        sim::mutate_world(
            &mut chunks,
            chunks_x,
            chunks_y,
            &test_sim_params,
            &next_id,
            &mut rng,
        );

        let hash_after = hash_chunks(&chunks);

        // Count occupants after tick
        let (sprouts_after, seeds_after, leaves_after) = count_occupants(&chunks);
        let next_id_value = next_id.load(std::sync::atomic::Ordering::Relaxed);

        if tick <= 5 || tick % 10 == 0 || tick == test_ticks {
            println!(
                "  Tick {}: hash={:016x} | sprouts:{}->{} seeds:{}->{} leaves:{}->{} next_id={}",
                tick, hash_after,
                sprouts_before, sprouts_after,
                seeds_before, seeds_after,
                leaves_before, leaves_after,
                next_id_value
            );
        }
    }

    let final_hash = hash_chunks(&chunks);
    println!("✓ Final state hash: {:016x}", final_hash);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _trace_guard = init_tracing(args.trace_chrome.as_deref());

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install default rustls crypto provider");

    // Seed initial WorldGenParams from CLI args (only chunks_x/y are
    // CLI-tunable; everything else takes the protocol default).
    let initial_world_gen = protocol::WorldGenParams {
        chunks_x: args.world_width,
        chunks_y: args.world_height,
        ..protocol::WorldGenParams::default()
    };
    let mut initial = persist::load_or_build(args.world_file.as_deref(), &initial_world_gen)?;
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
    // Pre-v2 snapshots don't carry world-gen params; reconstruct from
    // the snapshot's chunks_x/y plus defaults so the runtime mirror
    // matches the world that was actually built.
    let world_gen_params = initial.world_gen_params.unwrap_or(protocol::WorldGenParams {
        chunks_x: initial.chunks_x,
        chunks_y: initial.chunks_y,
        ..initial_world_gen
    });
    // Pre-v2 snapshots don't carry sim params either; fall back to
    // defaults. Newer snapshots restore exactly what was tuned in the
    // session that wrote them.
    let sim_params = initial.sim_params.unwrap_or_default();
    if fresh_world {
        let count = world::place_random_sprout_grid(
            &mut initial.chunks,
            &world_gen_params,
            &mut rng,
        );
        initial.next_plant_id = count + 1;
        info!(sprouts = count, "placed initial sprout grid");
    }
    // Handle determinism test mode
    if let Some(test_spec) = args.determinism_test.as_deref() {
        return run_determinism_test(
            test_spec,
            initial.chunks,
            initial.chunks_x,
            initial.chunks_y,
            world_gen_params,
            sim_params,
            seed,
            rng,
            initial.next_plant_id,
        );
    }

    let (tick_tx, _) = broadcast::channel::<Arc<Vec<u8>>>(8);
    let state = Arc::new(SimState {
        chunks_x: AtomicU32::new(initial.chunks_x),
        chunks_y: AtomicU32::new(initial.chunks_y),
        world: std::sync::Mutex::new(initial.chunks),
        tick_tx,
        next_plant_id: AtomicU32::new(initial.next_plant_id),
        current_tick: AtomicU64::new(initial.current_tick),
        control: std::sync::Mutex::new(SimControl {
            paused: !args.start_running,
            tick_hz: args.tick_hz.max(1),
            tick_rate_limited: false,
            step_pending: 0,
        }),
        seed: AtomicU64::new(seed),
        rng: std::sync::Mutex::new(rng),
        params: std::sync::Mutex::new(sim_params),
        world_gen_params: std::sync::Mutex::new(world_gen_params),
        latest_snapshot: sim::LatestSnapshot::new(),
        always_encode: args.always_encode,
        world_gen: AtomicU32::new(0),
    });

    info!(
        chunks_x = state.chunks_x.load(Ordering::Relaxed),
        chunks_y = state.chunks_y.load(Ordering::Relaxed),
        cells = (state.chunks_x.load(Ordering::Relaxed) as usize) * (state.chunks_y.load(Ordering::Relaxed) as usize) * CHUNK_AREA,
        "world ready"
    );

    if args.autosave_secs > 0
        && let Some(path) = args.world_file.clone()
    {
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

    let sim_state = Arc::clone(&state);
    tokio::spawn(async move {
        sim::run_sim_loop(sim_state).await;
    });
    info!(tick_hz = args.tick_hz, "sim loop started");

    // Encode + broadcast runs on its own task so msgpack+zstd time
    // doesn't gate sim throughput. Sim publishes the latest snapshot
    // into a slot; this loop takes from it. Drop-old semantics — sim
    // never blocks on encode.
    let encode_state = Arc::clone(&state);
    tokio::spawn(async move {
        sim::run_encode_loop(encode_state).await;
    });
    info!("encode loop started");

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

    if let Some(path) = &args.world_file
        && let Err(e) = persist::save_world(path, &state)
    {
        error!("final save failed: {e:#}");
    }

    Ok(())
}

fn init_tracing(trace_chrome: Option<&std::path::Path>) -> Option<FlushGuard> {
    use tracing_subscriber::Layer;
    // Per-tick phase events live on target="phase" so we can keep them
    // out of the noisy console log while still capturing them in the
    // chrome trace when the user explicitly enables profiling.
    let fmt_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,phase=off,quinn=warn"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_filter(fmt_filter);
    match trace_chrome {
        Some(path) => {
            let (chrome_layer, guard) = ChromeLayerBuilder::new()
                .file(path)
                .include_args(true)
                .build();
            let chrome_filter = EnvFilter::new("info,phase=info,quinn=warn");
            let chrome_layer = chrome_layer.with_filter(chrome_filter);
            tracing_subscriber::registry()
                .with(fmt_layer)
                .with(chrome_layer)
                .init();
            info!(path = %path.display(), "chrome trace recording");
            Some(guard)
        }
        None => {
            tracing_subscriber::registry().with(fmt_layer).init();
            None
        }
    }
}
