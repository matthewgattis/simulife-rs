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
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, ClientMessage, Direction, Energy, Genome,
    Occupant, STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
    ServerMessage,
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
    control: std::sync::Mutex<SimControl>,
}

#[derive(Debug)]
struct SimControl {
    paused: bool,
    tick_hz: u32,
    step_pending: u32,
}

#[derive(Serialize, Deserialize)]
struct WorldSnapshot {
    chunks_x: u32,
    chunks_y: u32,
    next_plant_id: u32,
    chunks: Vec<Chunk>,
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const ZSTD_LEVEL: i32 = 3;

fn load_world(path: &Path) -> Result<WorldSnapshot> {
    let raw = std::fs::read(path).with_context(|| format!("read {path:?}"))?;
    let payload = if raw.starts_with(&ZSTD_MAGIC) {
        zstd::decode_all(&raw[..]).context("zstd decode")?
    } else {
        raw.clone()
    };
    let snapshot: WorldSnapshot = rmp_serde::from_slice(&payload)?;
    info!(
        path = %path.display(),
        on_disk = raw.len(),
        decoded = payload.len(),
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
    let raw = rmp_serde::to_vec(&snapshot)?;
    let compressed = zstd::encode_all(&raw[..], ZSTD_LEVEL).context("zstd encode")?;
    atomic_save(path, &compressed)?;
    let ratio = raw.len() as f64 / compressed.len().max(1) as f64;
    info!(
        path = %path.display(),
        on_disk = compressed.len(),
        uncompressed = raw.len(),
        ratio = format!("{ratio:.1}x"),
        "world saved",
    );
    Ok(())
}

/// Crash-safe save: write `<path>.tmp`, fsync, rotate `<path>` → `<path>.bak`,
/// then atomically rename tmp → live. The live path is always either the old
/// version or the new one, never partial. fsync ensures the tmp's bytes hit
/// disk before we commit it via rename, so a process or kernel crash between
/// the write and the rename can't leave a torn save behind.
fn atomic_save(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let tmp = with_extension_suffix(path, "tmp");
    let bak = with_extension_suffix(path, "bak");

    {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("create {tmp:?}"))?;
        file.write_all(bytes)
            .with_context(|| format!("write {tmp:?}"))?;
        file.sync_all()
            .with_context(|| format!("fsync {tmp:?}"))?;
    }

    if path.exists() {
        std::fs::rename(path, &bak)
            .with_context(|| format!("rotate {path:?} -> {bak:?}"))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(())
}

fn with_extension_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
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
                    if let Err(e) = save_world(&path, &save_state) {
                        warn!("autosave failed: {e:#}");
                    }
                }
            });
            info!(autosave_secs = args.autosave_secs, "autosave enabled");
        }
    }

    let sim_state = Arc::clone(&state);
    tokio::spawn(async move {
        run_sim_loop(sim_state).await;
    });
    info!(tick_hz = args.tick_hz, "sim loop started");

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

    // Inert showcase row (parent: None, children: 0). Existing cells are
    // visually distinct but disconnected from any plant tree.
    let entries: Vec<(i32, i32, Occupant)> = vec![
        (
            10,
            20,
            Occupant::Leaf {
                plant,
                energy,
                facing: Direction::East,
                parent: None,
            },
        ),
        (
            12,
            20,
            Occupant::Leaf {
                plant,
                energy,
                facing: Direction::North,
                parent: None,
            },
        ),
        (
            14,
            20,
            Occupant::Root {
                plant,
                energy,
                parent: None,
            },
        ),
        (
            16,
            20,
            Occupant::Stem {
                plant,
                energy,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: None,
                children: 0,
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
                parent: None,
                children: 0,
            },
        ),
        (
            20,
            20,
            Occupant::Stem {
                plant,
                energy,
                connections: STEM_CONNECT_EAST | STEM_CONNECT_SOUTH,
                parent: None,
                children: 0,
            },
        ),
        (
            22,
            20,
            Occupant::Antenna {
                plant,
                energy,
                parent: None,
            },
        ),
        (
            24,
            20,
            Occupant::Sprout {
                plant,
                energy,
                facing: Direction::North,
                genome: bare_genome(),
                parent: None,
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
                parent: None,
            },
        ),
    ];
    for (x, y, occupant) in entries {
        place_at(chunks, chunks_x, x, y, occupant);
    }

    // Viable mini-plant centered around (50, 50). A trunk stem with a leaf
    // on its east side (production source) and a sprout above it (growth
    // sink). Energy: leaf -> trunk -> sprout -> trunk (cycle), with leaf
    // photosynthesis as the source.
    let plant2 = 2u32;
    let mini_plant: Vec<(i32, i32, Occupant)> = vec![
        (
            50,
            51,
            Occupant::Stem {
                plant: plant2,
                energy: 100,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_EAST,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        ),
        (
            50,
            50,
            Occupant::Sprout {
                plant: plant2,
                energy: 100,
                facing: Direction::North,
                genome: bare_genome(),
                parent: Some(Direction::South),
            },
        ),
        (
            51,
            51,
            Occupant::Leaf {
                plant: plant2,
                energy: 100,
                facing: Direction::East,
                parent: Some(Direction::West),
            },
        ),
    ];
    for (x, y, occupant) in mini_plant {
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

enum SimAction {
    TickNow,
    TickAfter(Duration),
    Wait,
}

async fn run_sim_loop(state: Arc<SimState>) {
    loop {
        let action = {
            let mut ctrl = state.control.lock().expect("control poisoned");
            let dur = Duration::from_millis(1000 / ctrl.tick_hz.max(1) as u64);
            if ctrl.step_pending > 0 {
                ctrl.step_pending -= 1;
                SimAction::TickNow
            } else if !ctrl.paused {
                SimAction::TickAfter(dur)
            } else {
                SimAction::Wait
            }
        };

        match action {
            SimAction::TickNow => {}
            SimAction::TickAfter(dur) => tokio::time::sleep(dur).await,
            SimAction::Wait => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        }

        let snapshot_chunks = {
            let mut chunks = state.world.lock().expect("sim lock poisoned");
            mutate_world(&mut chunks, state.chunks_x, state.chunks_y);
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

const LEAF_PHOTOSYNTHESIS: Energy = 5;
const UPKEEP_DEFAULT: Energy = 1;
const UPKEEP_SEED: Energy = 0;
const UPKEEP_SPROUT: Energy = 3;

const ROOT_PULL_KERNEL: [[u16; 3]; 3] = [
    [0, 1, 0],
    [1, 2, 1],
    [0, 1, 0],
];
const ANTENNA_PULL_KERNEL: [[u16; 3]; 3] = [
    [0, 1, 0],
    [1, 2, 1],
    [0, 1, 0],
];
const DEATH_DEPOSIT_KERNEL: [[u16; 3]; 3] = [
    [1, 2, 1],
    [2, 4, 2],
    [1, 2, 1],
];

#[derive(Clone, Copy)]
enum SoilField {
    Organic,
    Energy,
}

fn mutate_world(chunks: &mut [Chunk], chunks_x: u32, chunks_y: u32) {
    let edge = CHUNK_EDGE as i32;
    let max_x = chunks_x as i32 * edge;
    let max_y = chunks_y as i32 * edge;

    // Phase 1: photosynthesis (per-cell, in-place).
    for chunk in chunks.iter_mut() {
        for cell in chunk.cells.iter_mut() {
            if cell.sunlit {
                if let Occupant::Leaf { energy, .. } = &mut cell.occupant {
                    *energy = energy.saturating_add(LEAF_PHOTOSYNTHESIS);
                }
            }
        }
    }

    // Phase 2: soil pulls. Serial — multiple roots near the same soil cell
    // each take their share in iteration order until that cell is empty.
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let field = match &chunks[chunk_idx].cells[cell_idx].occupant {
                        Occupant::Root { .. } => Some(SoilField::Organic),
                        Occupant::Antenna { .. } => Some(SoilField::Energy),
                        _ => None,
                    };
                    if let Some(field) = field {
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        apply_soil_pull(chunks, chunks_x, wx, wy, max_x, max_y, field);
                    }
                }
            }
        }
    }

    // Phase 3: upkeep (per-cell, in-place).
    for chunk in chunks.iter_mut() {
        for cell in chunk.cells.iter_mut() {
            if let Some(e) = occupant_energy(&cell.occupant) {
                let cost = upkeep_for(&cell.occupant);
                set_occupant_energy(&mut cell.occupant, e.saturating_sub(cost));
            }
        }
    }

    // Phase 4: directed push. Production cells push surplus to parent, stems
    // split surplus across children, sprouts push to parent. Build a delta
    // array from the current state, then apply atomically — removes any
    // order dependency between cells in the same generation.
    let total_cells = chunks.len() * CHUNK_AREA;
    let mut deltas: Vec<i32> = vec![0; total_cells];
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let cell = &chunks[chunk_idx].cells[cell_idx];

                    let cur_energy = match occupant_energy(&cell.occupant) {
                        Some(e) => e,
                        None => continue,
                    };
                    let buffer = upkeep_for(&cell.occupant);
                    if cur_energy <= buffer {
                        continue;
                    }
                    let pushable = cur_energy - buffer;

                    let targets = push_targets(&cell.occupant);
                    if targets.is_empty() {
                        continue;
                    }
                    let per_target = pushable / targets.len() as Energy;
                    if per_target == 0 {
                        continue;
                    }
                    let total_pushed = per_target * targets.len() as Energy;

                    deltas[linear_idx(chunks_x, wx, wy)] -= total_pushed as i32;
                    for dir in targets {
                        let (dx, dy) = direction_to_delta(dir);
                        let nx = wx + dx;
                        let ny = wy + dy;
                        if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
                            continue;
                        }
                        deltas[linear_idx(chunks_x, nx, ny)] += per_target as i32;
                    }
                }
            }
        }
    }
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let delta = deltas[linear_idx(chunks_x, wx, wy)];
                    if delta == 0 {
                        continue;
                    }
                    let cell = &mut chunks[chunk_idx].cells[cell_idx];
                    if let Some(e) = occupant_energy(&cell.occupant) {
                        let new_e = ((e as i32) + delta)
                            .clamp(0, Energy::MAX as i32) as Energy;
                        set_occupant_energy(&mut cell.occupant, new_e);
                    }
                }
            }
        }
    }

    // Phase 5: death — collect 0-energy occupants, then deposit organic over
    // a 3x3 area and replace the cell with Empty.
    let mut dying: Vec<(i32, i32)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    if matches!(
                        occupant_energy(&chunks[chunk_idx].cells[cell_idx].occupant),
                        Some(0)
                    ) {
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        dying.push((wx, wy));
                    }
                }
            }
        }
    }
    for (wx, wy) in dying {
        deposit_kernel(chunks, chunks_x, wx, wy, max_x, max_y, &DEATH_DEPOSIT_KERNEL);
        if let Some(cell) = cell_at_mut(chunks, chunks_x, wx, wy) {
            cell.occupant = Occupant::Empty;
        }
    }
}

fn push_targets(occ: &Occupant) -> Vec<Direction> {
    match occ {
        // Sprouts and seeds are terminal sinks — they accumulate energy but
        // never push it back. (For sprouts, pushing back to the parent stem
        // would just bounce: the stem would push to its children, putting
        // the energy right back here.)
        Occupant::Empty | Occupant::Seed { .. } | Occupant::Sprout { .. } => Vec::new(),
        Occupant::Leaf { parent, .. }
        | Occupant::Root { parent, .. }
        | Occupant::Antenna { parent, .. } => parent.iter().copied().collect(),
        Occupant::Stem { children, .. } => bitmask_to_dirs(*children),
    }
}

fn bitmask_to_dirs(mask: u8) -> Vec<Direction> {
    let mut dirs = Vec::new();
    if mask & STEM_CONNECT_NORTH != 0 {
        dirs.push(Direction::North);
    }
    if mask & STEM_CONNECT_EAST != 0 {
        dirs.push(Direction::East);
    }
    if mask & STEM_CONNECT_SOUTH != 0 {
        dirs.push(Direction::South);
    }
    if mask & STEM_CONNECT_WEST != 0 {
        dirs.push(Direction::West);
    }
    dirs
}

fn direction_to_delta(dir: Direction) -> (i32, i32) {
    match dir {
        Direction::North => (0, -1),
        Direction::East => (1, 0),
        Direction::South => (0, 1),
        Direction::West => (-1, 0),
    }
}

fn linear_idx(chunks_x: u32, wx: i32, wy: i32) -> usize {
    let edge = CHUNK_EDGE as i32;
    let cx = wx / edge;
    let cy = wy / edge;
    let lx = (wx % edge) as usize;
    let ly = (wy % edge) as usize;
    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
    chunk_idx * CHUNK_AREA + ly * (CHUNK_EDGE as usize) + lx
}

fn apply_soil_pull(
    chunks: &mut [Chunk],
    chunks_x: u32,
    wx: i32,
    wy: i32,
    max_x: i32,
    max_y: i32,
    field: SoilField,
) {
    let kernel = match field {
        SoilField::Organic => &ROOT_PULL_KERNEL,
        SoilField::Energy => &ANTENNA_PULL_KERNEL,
    };
    let mut total_pulled: u32 = 0;
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            let want = kernel[(dy + 1) as usize][(dx + 1) as usize];
            if want == 0 {
                continue;
            }
            let nx = wx + dx;
            let ny = wy + dy;
            if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
                continue;
            }
            if let Some(cell) = cell_at_mut(chunks, chunks_x, nx, ny) {
                let avail = match field {
                    SoilField::Organic => cell.organic,
                    SoilField::Energy => cell.soil_energy,
                };
                let actual = avail.min(want);
                match field {
                    SoilField::Organic => cell.organic -= actual,
                    SoilField::Energy => cell.soil_energy -= actual,
                }
                total_pulled += actual as u32;
            }
        }
    }
    if total_pulled > 0 {
        if let Some(cell) = cell_at_mut(chunks, chunks_x, wx, wy) {
            if let Some(e) = occupant_energy(&cell.occupant) {
                let new_e =
                    (e as u32 + total_pulled).min(Energy::MAX as u32) as Energy;
                set_occupant_energy(&mut cell.occupant, new_e);
            }
        }
    }
}

fn deposit_kernel(
    chunks: &mut [Chunk],
    chunks_x: u32,
    wx: i32,
    wy: i32,
    max_x: i32,
    max_y: i32,
    kernel: &[[u16; 3]; 3],
) {
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            let weight = kernel[(dy + 1) as usize][(dx + 1) as usize];
            if weight == 0 {
                continue;
            }
            let nx = wx + dx;
            let ny = wy + dy;
            if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
                continue;
            }
            if let Some(cell) = cell_at_mut(chunks, chunks_x, nx, ny) {
                cell.organic = cell.organic.saturating_add(weight);
            }
        }
    }
}

fn cell_at_mut(chunks: &mut [Chunk], chunks_x: u32, wx: i32, wy: i32) -> Option<&mut Cell> {
    if wx < 0 || wy < 0 {
        return None;
    }
    let edge = CHUNK_EDGE as i32;
    let cx = wx / edge;
    let cy = wy / edge;
    let lx = (wx % edge) as usize;
    let ly = (wy % edge) as usize;
    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
    chunks.get_mut(chunk_idx)?.cells.get_mut(cell_idx)
}

fn occupant_energy(occ: &Occupant) -> Option<Energy> {
    match occ {
        Occupant::Empty => None,
        Occupant::Leaf { energy, .. }
        | Occupant::Root { energy, .. }
        | Occupant::Stem { energy, .. }
        | Occupant::Antenna { energy, .. }
        | Occupant::Sprout { energy, .. }
        | Occupant::Seed { energy, .. } => Some(*energy),
    }
}

fn set_occupant_energy(occ: &mut Occupant, new_energy: Energy) {
    match occ {
        Occupant::Empty => {}
        Occupant::Leaf { energy, .. }
        | Occupant::Root { energy, .. }
        | Occupant::Stem { energy, .. }
        | Occupant::Antenna { energy, .. }
        | Occupant::Sprout { energy, .. }
        | Occupant::Seed { energy, .. } => *energy = new_energy,
    }
}

fn upkeep_for(occ: &Occupant) -> Energy {
    match occ {
        Occupant::Empty => 0,
        Occupant::Seed { .. } => UPKEEP_SEED,
        Occupant::Sprout { .. } => UPKEEP_SPROUT,
        _ => UPKEEP_DEFAULT,
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
        ClientMessage::SetPaused(paused) => {
            let mut ctrl = state.control.lock().expect("control poisoned");
            ctrl.paused = paused;
            info!(paused, "sim pause state changed");
        }
        ClientMessage::Step => {
            let mut ctrl = state.control.lock().expect("control poisoned");
            ctrl.step_pending = ctrl.step_pending.saturating_add(1);
            debug!(step_pending = ctrl.step_pending, "step requested");
        }
        ClientMessage::SetTickHz(hz) => {
            let hz = hz.max(1);
            let mut ctrl = state.control.lock().expect("control poisoned");
            ctrl.tick_hz = hz;
            info!(tick_hz = hz, "tick rate changed");
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
        parent: None,
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
        ClientMessage::Hello => {
            let (paused, tick_hz) = {
                let ctrl = state.control.lock().expect("control poisoned");
                (ctrl.paused, ctrl.tick_hz)
            };
            Some(ServerMessage::Welcome {
                world_chunks_x: state.chunks_x,
                world_chunks_y: state.chunks_y,
                paused,
                tick_hz,
            })
        }
        ClientMessage::Subscribe => {
            let chunks = state.world.lock().expect("sim lock poisoned").clone();
            Some(ServerMessage::ChunkBatch(chunks))
        }
        ClientMessage::SpawnSprout { .. }
        | ClientMessage::SetPaused(_)
        | ClientMessage::Step
        | ClientMessage::SetTickHz(_) => {
            warn!("control / spawn message arrived on bidi stream; expected on uni");
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
