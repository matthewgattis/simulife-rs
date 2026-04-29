use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use rand::SeedableRng;

use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, Direction, Energy, Gene, Genome, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
    ServerMessage, SlotProduct,
};
use rand::Rng;
use rand_chacha::ChaCha12Rng;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const LEAF_PHOTOSYNTHESIS: Energy = 5;
const UPKEEP_DEFAULT: Energy = 1;
const UPKEEP_SEED: Energy = 1;
const UPKEEP_SPROUT: Energy = 3;

/// Soil energy "rest level". Each tick every cell's soil_energy drifts by
/// SOIL_ENERGY_REGULATION toward this value.
const SOIL_ENERGY_REST: u16 = 100;
const SOIL_ENERGY_REGULATION: u16 = 1;

/// Once a seed has accumulated this much energy from its parent stem, it
/// disconnects: parent stem drops the children-bit pointing at the seed,
/// and the seed clears its own parent. The seed then lives off its
/// reserves (upkeep ticks it down) until starvation or germination.
const SEED_DROPOFF_THRESHOLD: Energy = 100;

/// Per-slot spawn cost. Sprout drains the sum of these for whatever it
/// produces in a generation. Each new cell starts with its slot's cost as
/// its initial energy.
const COST_SPROUT: Energy = 20;
const COST_LEAF: Energy = 5;
const COST_ROOT: Energy = 5;
const COST_ANTENNA: Energy = 5;
const COST_SEED: Energy = 30;

/// Per-field probability of mutating a single field at any copy site.
const MUTATION_RATE: f32 = 0.02;

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

pub struct SimState {
    pub chunks_x: u32,
    pub chunks_y: u32,
    pub world: std::sync::Mutex<Vec<Chunk>>,
    pub tick_tx: broadcast::Sender<Arc<Vec<u8>>>,
    pub next_plant_id: AtomicU32,
    pub current_tick: AtomicU64,
    pub control: std::sync::Mutex<SimControl>,
    /// Current seed. AtomicU64 so `regenerate_world` can update it without
    /// taking a write lock on SimState — readers (e.g., Welcome) just load.
    pub seed: AtomicU64,
    pub rng: std::sync::Mutex<ChaCha12Rng>,
}

#[derive(Debug)]
pub struct SimControl {
    pub paused: bool,
    pub tick_hz: u32,
    pub step_pending: u32,
}

enum SimAction {
    TickNow,
    TickAfter(Duration),
    Wait,
}

#[derive(Clone, Copy)]
enum SoilField {
    Organic,
    Energy,
}

pub async fn run_sim_loop(state: Arc<SimState>) {
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

        let tick = state.current_tick.load(Ordering::Relaxed) + 1;
        let _tick_span = tracing::info_span!("tick", tick).entered();

        let snapshot_chunks: Vec<protocol::WireChunk> = {
            let mut chunks = state.world.lock().expect("sim lock poisoned");
            let mut rng = state.rng.lock().expect("rng lock poisoned");
            let _mutate = tracing::info_span!("mutate_world").entered();
            mutate_world(&mut chunks, state.chunks_x, state.chunks_y, &mut *rng);
            drop(_mutate);
            // Build the wire view directly from the locked world. Avoids
            // cloning the full Chunks (with their Box<Genome>s) just to
            // serialize a stripped version.
            let _wire = tracing::info_span!("to_wire_chunks").entered();
            chunks.iter().map(protocol::WireChunk::from).collect()
        };
        state.current_tick.store(tick, Ordering::Relaxed);

        let msg = ServerMessage::ChunkBatch {
            tick,
            chunks: snapshot_chunks,
        };
        let encode = tracing::info_span!("encode_msg").entered();
        match protocol::encode_server_message(&msg) {
            Ok(bytes) => {
                drop(encode);
                let _ = tracing::info_span!("broadcast", bytes = bytes.len()).entered();
                let _ = state.tick_tx.send(Arc::new(bytes));
            }
            Err(e) => error!("serialize tick failed: {e:#}"),
        }
    }
}

/// Wipe the world, reseed the RNG, reset tick + plant id, and broadcast a
/// fresh Welcome + ChunkBatch so connected viewers refresh in place. Holds
/// the world + rng mutexes for the swap; safe to call between sim ticks.
pub fn regenerate_world(state: &SimState, seed: u64) {
    let chunks_x = state.chunks_x;
    let chunks_y = state.chunks_y;

    let mut new_chunks = crate::world::build_world(chunks_x, chunks_y);
    let mut new_rng = ChaCha12Rng::seed_from_u64(seed);
    let count = crate::world::place_random_sprout_grid(
        &mut new_chunks,
        chunks_x,
        chunks_y,
        &mut new_rng,
    );

    {
        let mut world = state.world.lock().expect("sim lock poisoned");
        let mut rng = state.rng.lock().expect("rng lock poisoned");
        *world = new_chunks.clone();
        *rng = new_rng;
    }
    state.seed.store(seed, Ordering::Relaxed);
    state.next_plant_id.store(count + 1, Ordering::Relaxed);
    state.current_tick.store(0, Ordering::Relaxed);
    info!(seed, "world regenerated");

    let (paused, tick_hz) = {
        let ctrl = state.control.lock().expect("control poisoned");
        (ctrl.paused, ctrl.tick_hz)
    };
    let welcome = ServerMessage::Welcome {
        world_chunks_x: chunks_x,
        world_chunks_y: chunks_y,
        paused,
        tick_hz,
        tick: 0,
        seed,
    };
    if let Ok(bytes) = protocol::encode_server_message(&welcome) {
        let _ = state.tick_tx.send(Arc::new(bytes));
    }
    let wire_chunks: Vec<protocol::WireChunk> =
        new_chunks.iter().map(protocol::WireChunk::from).collect();
    let batch = ServerMessage::ChunkBatch {
        tick: 0,
        chunks: wire_chunks,
    };
    if let Ok(bytes) = protocol::encode_server_message(&batch) {
        let _ = state.tick_tx.send(Arc::new(bytes));
    }
}

pub fn spawn_sprout(state: &SimState, x: i32, y: i32, facing: Direction) {
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
        genome: Box::new(Genome::default_vine()),
        parent: None,
        current_gene: 0,
    };
    info!(x, y, plant, ?facing, "sprout spawned");
}

fn mutate_world(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    rng: &mut impl Rng,
) {
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

    // Phase 1.5: soil energy regulation. Each cell drifts its soil_energy
    // toward SOIL_ENERGY_REST by SOIL_ENERGY_REGULATION per tick. Runs
    // before soil pulls so antennae deplete a freshened soil each tick.
    for chunk in chunks.iter_mut() {
        for cell in chunk.cells.iter_mut() {
            if cell.soil_energy < SOIL_ENERGY_REST {
                cell.soil_energy = (cell.soil_energy + SOIL_ENERGY_REGULATION)
                    .min(SOIL_ENERGY_REST);
            } else if cell.soil_energy > SOIL_ENERGY_REST {
                cell.soil_energy = cell
                    .soil_energy
                    .saturating_sub(SOIL_ENERGY_REGULATION)
                    .max(SOIL_ENERGY_REST);
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

    // Phase 4: prune stem children. For each stem with a children bitmask,
    // remove bits whose neighbor is not a valid energy sink (a sprout, or a
    // stem that itself has children). Pre-prune state is read into a parallel
    // array first, so cascading happens one level per tick.
    let total_cells = chunks.len() * CHUNK_AREA;
    let mut pruned_children: Vec<Option<u8>> = vec![None; total_cells];
    let bits = [
        STEM_CONNECT_NORTH,
        STEM_CONNECT_EAST,
        STEM_CONNECT_SOUTH,
        STEM_CONNECT_WEST,
    ];
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let current_children =
                        match &chunks[chunk_idx].cells[cell_idx].occupant {
                            Occupant::Stem { children, .. } if *children != 0 => *children,
                            _ => continue,
                        };
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let mut kept = 0u8;
                    for bit in bits {
                        if current_children & bit == 0 {
                            continue;
                        }
                        let dir = bit_to_dir(bit);
                        let (dx, dy) = direction_to_delta(dir);
                        let nx = wx + dx;
                        let ny = wy + dy;
                        if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
                            continue;
                        }
                        let n_chunk_idx = (ny / edge) as usize * chunks_x as usize
                            + (nx / edge) as usize;
                        let n_cell_idx = (ny % edge) as usize * (CHUNK_EDGE as usize)
                            + (nx % edge) as usize;
                        let neighbor = &chunks[n_chunk_idx].cells[n_cell_idx];
                        if is_valid_child(&neighbor.occupant) {
                            kept |= bit;
                        }
                    }
                    if kept != current_children {
                        pruned_children[linear_idx(chunks_x, wx, wy)] = Some(kept);
                    }
                }
            }
        }
    }
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let Some(new_c) = pruned_children[linear_idx(chunks_x, wx, wy)] else {
                        continue;
                    };
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    if let Occupant::Stem { children, .. } =
                        &mut chunks[chunk_idx].cells[cell_idx].occupant
                    {
                        *children = new_c;
                    }
                }
            }
        }
    }

    // Phase 5: directed push. Production cells push surplus to parent, stems
    // split surplus across children, sprouts/seeds are terminal sinks. Build
    // a delta array from the current state, then apply atomically — removes
    // any order dependency between cells in the same generation.
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

    // Phase 5.5: seed dropoff. After push, any seed at or above the
    // threshold disconnects from its parent stem — clearing the stem's
    // children-bit pointing at it and zeroing the seed's own parent. The
    // seed then lives off accumulated energy until starvation.
    let mut dropoffs: Vec<(i32, i32, i32, i32, u8)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    if let Occupant::Seed {
                        energy,
                        parent: Some(parent_dir),
                        ..
                    } = &chunks[chunk_idx].cells[cell_idx].occupant
                    {
                        if *energy >= SEED_DROPOFF_THRESHOLD {
                            let wx = cx as i32 * edge + lx as i32;
                            let wy = cy as i32 * edge + ly as i32;
                            let (dx, dy) = direction_to_delta(*parent_dir);
                            let parent_x = wx + dx;
                            let parent_y = wy + dy;
                            let parent_bit = dir_to_bitmask(opposite_dir(*parent_dir));
                            dropoffs.push((wx, wy, parent_x, parent_y, parent_bit));
                        }
                    }
                }
            }
        }
    }
    for (sx, sy, px, py, bit) in dropoffs {
        if let Some(seed_cell) = cell_at_mut(chunks, chunks_x, sx, sy) {
            if let Occupant::Seed { parent, .. } = &mut seed_cell.occupant {
                *parent = None;
            }
        }
        if let Some(parent_cell) = cell_at_mut(chunks, chunks_x, px, py) {
            if let Occupant::Stem { children, .. } = &mut parent_cell.occupant {
                *children &= !bit;
            }
        }
    }

    // Phase 6: growth — sprouts execute their current gene if energy covers
    // the slot costs and all desired targets are Empty.
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let info = match &chunks[chunk_idx].cells[cell_idx].occupant {
                        Occupant::Sprout {
                            plant,
                            energy,
                            facing,
                            parent,
                            current_gene,
                            genome,
                        } => Some((
                            *plant,
                            *energy,
                            *facing,
                            *parent,
                            *current_gene,
                            genome.clone(),
                        )),
                        _ => None,
                    };
                    if let Some((plant, energy, facing, parent, current_gene, genome)) = info {
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        attempt_growth(
                            chunks,
                            chunks_x,
                            max_x,
                            max_y,
                            wx,
                            wy,
                            plant,
                            energy,
                            facing,
                            parent,
                            current_gene,
                            &genome,
                            rng,
                        );
                    }
                }
            }
        }
    }

    // Phase 7: death — collect 0-energy occupants and stems with no push
    // target (children == 0 AND parent is None or points at an empty cell),
    // then deposit organic over a 3x3 area and replace the cell with Empty.
    let mut dying: Vec<(i32, i32)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let occ = &chunks[chunk_idx].cells[cell_idx].occupant;
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let energy_dead = matches!(occupant_energy(occ), Some(0));
                    let stranded =
                        cell_has_no_push_target(occ, chunks, chunks_x, max_x, max_y, wx, wy);
                    if energy_dead || stranded {
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
        // never push it back.
        Occupant::Empty | Occupant::Seed { .. } | Occupant::Sprout { .. } => Vec::new(),
        Occupant::Leaf { parent, .. }
        | Occupant::Root { parent, .. }
        | Occupant::Antenna { parent, .. } => parent.iter().copied().collect(),
        // Stem with children: push to them. Stem with no children: fall back
        // to parent (leaf-like) — happens after pruning has stripped dead /
        // dead-end children.
        Occupant::Stem {
            children, parent, ..
        } => {
            if *children != 0 {
                bitmask_to_dirs(*children)
            } else {
                parent.iter().copied().collect()
            }
        }
    }
}

fn is_valid_child(occ: &Occupant) -> bool {
    match occ {
        // Seeds and sprouts are terminal sinks — both legitimately receive
        // pushed energy. Stems with at least one valid child also count;
        // stems with no children are dead-ends and get pruned.
        Occupant::Sprout { .. } | Occupant::Seed { .. } => true,
        Occupant::Stem { children, .. } => *children != 0,
        _ => false,
    }
}

/// True for cells that have nowhere to push energy: stems with no children
/// AND a missing/empty parent, plus any production cell (leaf, root, antenna)
/// whose parent is missing/empty. Sprouts and seeds are sinks — they don't
/// push, so this rule doesn't apply to them.
fn cell_has_no_push_target(
    occ: &Occupant,
    chunks: &[Chunk],
    chunks_x: u32,
    max_x: i32,
    max_y: i32,
    wx: i32,
    wy: i32,
) -> bool {
    let parent = match occ {
        Occupant::Stem {
            children, parent, ..
        } => {
            if *children != 0 {
                return false;
            }
            *parent
        }
        Occupant::Leaf { parent, .. }
        | Occupant::Root { parent, .. }
        | Occupant::Antenna { parent, .. } => *parent,
        _ => return false,
    };

    let Some(parent_dir) = parent else {
        return true;
    };
    let edge = CHUNK_EDGE as i32;
    let (dx, dy) = direction_to_delta(parent_dir);
    let nx = wx + dx;
    let ny = wy + dy;
    if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
        return true;
    }
    let n_chunk_idx = (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
    let n_cell_idx = (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
    matches!(
        chunks[n_chunk_idx].cells[n_cell_idx].occupant,
        Occupant::Empty
    )
}

fn bit_to_dir(bit: u8) -> Direction {
    match bit {
        STEM_CONNECT_NORTH => Direction::North,
        STEM_CONNECT_EAST => Direction::East,
        STEM_CONNECT_SOUTH => Direction::South,
        _ => Direction::West,
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

fn slot_cost(slot: SlotProduct) -> Energy {
    match slot {
        SlotProduct::Nothing => 0,
        SlotProduct::Leaf => COST_LEAF,
        SlotProduct::Root => COST_ROOT,
        SlotProduct::Antenna => COST_ANTENNA,
        SlotProduct::Seed => COST_SEED,
        SlotProduct::Sprout => COST_SPROUT,
    }
}

fn make_slot_occupant(
    slot: SlotProduct,
    plant: u32,
    facing: Direction,
    parent: Direction,
    parent_genome: &Genome,
    next_gene: u8,
    rng: &mut impl Rng,
) -> Option<Occupant> {
    let parent_back = Some(opposite_dir(parent));
    let _ = parent;
    Some(match slot {
        SlotProduct::Nothing => return None,
        SlotProduct::Leaf => Occupant::Leaf {
            plant,
            energy: COST_LEAF,
            facing,
            parent: parent_back,
        },
        SlotProduct::Root => Occupant::Root {
            plant,
            energy: COST_ROOT,
            parent: parent_back,
        },
        SlotProduct::Antenna => Occupant::Antenna {
            plant,
            energy: COST_ANTENNA,
            parent: parent_back,
        },
        SlotProduct::Seed => Occupant::Seed {
            plant,
            energy: COST_SEED,
            facing,
            genome: Box::new(mutate_genome(parent_genome, MUTATION_RATE, rng)),
            parent: parent_back,
        },
        SlotProduct::Sprout => Occupant::Sprout {
            plant,
            energy: COST_SPROUT,
            facing,
            genome: Box::new(mutate_genome(parent_genome, MUTATION_RATE, rng)),
            parent: parent_back,
            current_gene: next_gene,
        },
    })
}

fn attempt_growth(
    chunks: &mut [Chunk],
    chunks_x: u32,
    max_x: i32,
    max_y: i32,
    wx: i32,
    wy: i32,
    plant: u32,
    sprout_energy: Energy,
    facing: Direction,
    parent: Option<Direction>,
    current_gene: u8,
    genome: &Genome,
    rng: &mut impl Rng,
) {
    if genome.genes.is_empty() {
        return;
    }
    let gene = genome.genes[(current_gene as usize) % genome.genes.len()];
    let next_gene = (gene.next as usize % genome.genes.len()) as u8;

    let plan = [
        (facing, gene.front),
        (rotate_left(facing), gene.left),
        (rotate_right(facing), gene.right),
    ];

    // Walk the plan and figure out which slots are growable. A target is
    // viable if (a) the slot is a real product, (b) the cell is in-bounds,
    // and (c) the cell is Empty OR an edible foreign cell. Edible cells get
    // their energy harvested into the eater's pool.
    let mut viable: [bool; 3] = [false; 3];
    let mut harvested: [u32; 3] = [0; 3];
    for (i, (dir, slot)) in plan.iter().enumerate() {
        if matches!(slot, SlotProduct::Nothing) {
            continue;
        }
        let (dx, dy) = direction_to_delta(*dir);
        let nx = wx + dx;
        let ny = wy + dy;
        if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
            continue;
        }
        let Some(cell) = cell_at_mut(chunks, chunks_x, nx, ny) else {
            continue;
        };
        match edible_for(&cell.occupant, plant) {
            EdibleStatus::Empty => {
                viable[i] = true;
            }
            EdibleStatus::Edible(e) => {
                viable[i] = true;
                harvested[i] = e as u32;
            }
            EdibleStatus::Blocked => {}
        }
    }

    // No slot can produce anything — sprout has nowhere to grow. Die in
    // place: deposit organic and become Empty. Keeps trapped sprouts (e.g.
    // pinned at the world edge with all sides blocked) from accumulating
    // energy forever.
    if !viable.iter().any(|v| *v) {
        deposit_kernel(chunks, chunks_x, wx, wy, max_x, max_y, &DEATH_DEPOSIT_KERNEL);
        if let Some(self_cell) = cell_at_mut(chunks, chunks_x, wx, wy) {
            self_cell.occupant = Occupant::Empty;
        }
        return;
    }

    // Cost = sum over the slots that will actually spawn.
    let effective_cost: Energy = plan
        .iter()
        .zip(viable.iter())
        .filter(|(_, v)| **v)
        .map(|((_, slot), _)| slot_cost(*slot))
        .sum();
    let total_harvested: u32 = harvested.iter().sum();
    let pool: u32 = sprout_energy as u32 + total_harvested;
    if pool <= effective_cost as u32 {
        return; // wait for more energy
    }

    let mut connections = 0u8;
    let mut children = 0u8;
    let mut grew = false;

    for (i, (dir, slot)) in plan.iter().enumerate() {
        if !viable[i] {
            continue;
        }
        let Some(occ) = make_slot_occupant(*slot, plant, *dir, *dir, genome, next_gene, rng)
        else {
            continue;
        };
        let (dx, dy) = direction_to_delta(*dir);
        let nx = wx + dx;
        let ny = wy + dy;
        if let Some(target) = cell_at_mut(chunks, chunks_x, nx, ny) {
            target.occupant = occ;
            connections |= dir_to_bitmask(*dir);
            // Both sprouts and seeds need energy from the parent stem to
            // function, so include them in the children mask.
            if matches!(slot, SlotProduct::Sprout | SlotProduct::Seed) {
                children |= dir_to_bitmask(*dir);
            }
            grew = true;
        }
    }

    if grew {
        if let Some(parent_dir) = parent {
            connections |= dir_to_bitmask(parent_dir);
        }
        // Pool already accounts for harvested energy from edible targets.
        let new_energy = pool
            .saturating_sub(effective_cost as u32)
            .min(Energy::MAX as u32) as Energy;
        if let Some(self_cell) = cell_at_mut(chunks, chunks_x, wx, wy) {
            self_cell.occupant = Occupant::Stem {
                plant,
                energy: new_energy,
                connections,
                parent,
                children,
            };
        }
    }
}

/// Outcome of inspecting a growth target.
enum EdibleStatus {
    /// Cell is Empty — grow normally, no energy harvested.
    Empty,
    /// Cell is a foreign edible cell — replace it and harvest its energy.
    Edible(Energy),
    /// Cell is one of: own-plant, Root, Stem, OOB. Cannot grow into it.
    Blocked,
}

fn edible_for(occ: &Occupant, eater_plant: u32) -> EdibleStatus {
    match occ {
        Occupant::Empty => EdibleStatus::Empty,
        Occupant::Leaf { plant, energy, .. }
        | Occupant::Antenna { plant, energy, .. }
        | Occupant::Sprout { plant, energy, .. }
        | Occupant::Seed { plant, energy, .. }
            if *plant != eater_plant =>
        {
            EdibleStatus::Edible(*energy)
        }
        _ => EdibleStatus::Blocked,
    }
}

/// Per-field mutation pass over a genome. Each gene's slots and `next` each
/// roll independently against `rate`. Called at every copy site:
/// sprout-produces-sub-sprouts, sprout-produces-seed, seed-germinates.
///
/// Takes rate explicitly (instead of always reading MUTATION_RATE) so tests
/// can drive a non-zero rate even while the live constant is 0.
pub fn mutate_genome(g: &Genome, rate: f32, rng: &mut impl Rng) -> Genome {
    let len = g.genes.len();
    let mut new_genes: Vec<Gene> = g.genes.clone();
    for gene in new_genes.iter_mut() {
        if rng.r#gen::<f32>() < rate {
            gene.front = random_slot(rng);
        }
        if rng.r#gen::<f32>() < rate {
            gene.left = random_slot(rng);
        }
        if rng.r#gen::<f32>() < rate {
            gene.right = random_slot(rng);
        }
        if rng.r#gen::<f32>() < rate {
            // Always wraps via modulo at read time, but keep it tidy.
            gene.next = (rng.r#gen::<usize>() % len.max(1)) as u8;
        }
    }
    Genome { genes: new_genes }
}

fn random_slot(rng: &mut impl Rng) -> SlotProduct {
    match rng.gen_range(0u8..6) {
        0 => SlotProduct::Nothing,
        1 => SlotProduct::Leaf,
        2 => SlotProduct::Root,
        3 => SlotProduct::Antenna,
        4 => SlotProduct::Seed,
        _ => SlotProduct::Sprout,
    }
}

fn rotate_left(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::West,
        Direction::West => Direction::South,
        Direction::South => Direction::East,
        Direction::East => Direction::North,
    }
}

fn rotate_right(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::East,
        Direction::East => Direction::South,
        Direction::South => Direction::West,
        Direction::West => Direction::North,
    }
}

fn opposite_dir(d: Direction) -> Direction {
    match d {
        Direction::North => Direction::South,
        Direction::East => Direction::West,
        Direction::South => Direction::North,
        Direction::West => Direction::East,
    }
}

fn dir_to_bitmask(d: Direction) -> u8 {
    match d {
        Direction::North => STEM_CONNECT_NORTH,
        Direction::East => STEM_CONNECT_EAST,
        Direction::South => STEM_CONNECT_SOUTH,
        Direction::West => STEM_CONNECT_WEST,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ChunkCoord;
    use rand::SeedableRng;

    fn det_rng() -> ChaCha12Rng {
        ChaCha12Rng::seed_from_u64(0)
    }

    fn empty_world(chunks_x: u32, chunks_y: u32) -> Vec<Chunk> {
        let mut v = Vec::with_capacity((chunks_x * chunks_y) as usize);
        for cy in 0..chunks_y as i32 {
            for cx in 0..chunks_x as i32 {
                let cells = (0..CHUNK_AREA)
                    .map(|_| Cell {
                        organic: 0,
                        soil_energy: 0,
                        sunlit: false,
                        occupant: Occupant::Empty,
                    })
                    .collect();
                v.push(Chunk {
                    coord: ChunkCoord { x: cx, y: cy },
                    cells,
                });
            }
        }
        v
    }

    fn cell_at<'a>(chunks: &'a [Chunk], chunks_x: u32, x: i32, y: i32) -> &'a Cell {
        let edge = CHUNK_EDGE as i32;
        let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
        let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
        &chunks[chunk_idx].cells[cell_idx]
    }

    fn place(chunks: &mut [Chunk], chunks_x: u32, x: i32, y: i32, occ: Occupant) {
        let edge = CHUNK_EDGE as i32;
        let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
        let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
        chunks[chunk_idx].cells[cell_idx].occupant = occ;
    }

    fn vine_sprout(energy: Energy) -> (Occupant, Genome) {
        let genome = Genome::default_vine();
        let occ = Occupant::Sprout {
            plant: 1,
            energy,
            facing: Direction::North,
            genome: Box::new(genome.clone()),
            parent: None,
            current_gene: 0,
        };
        (occ, genome)
    }

    #[test]
    fn growth_with_clear_sides_spawns_sprout_and_two_leaves() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        let (sprout, genome) = vine_sprout(100);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            10,
            1,
            100,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Stem { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 9).occupant,
            Occupant::Sprout { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 9, 10).occupant,
            Occupant::Leaf { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 10).occupant,
            Occupant::Leaf { .. }
        ));
    }

    #[test]
    fn growth_at_top_edge_grows_only_side_leaves() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        let (sprout, genome) = vine_sprout(100);
        // y=0 with facing North → front cell is OOB.
        place(&mut chunks, chunks_x, 10, 0, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            0,
            1,
            100,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        match &cell_at(&chunks, chunks_x, 10, 0).occupant {
            Occupant::Stem { children, .. } => assert_eq!(*children, 0),
            other => panic!("expected children-less stem, got {other:?}"),
        }
        assert!(matches!(
            cell_at(&chunks, chunks_x, 9, 0).occupant,
            Occupant::Leaf { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 0).occupant,
            Occupant::Leaf { .. }
        ));
    }

    #[test]
    fn growth_dies_when_all_targets_blocked() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        // Stems are inedible — they actually block. (Leaves and the like
        // would just get eaten and turn into food.)
        let blocker = || Occupant::Stem {
            plant: 99,
            energy: 50,
            connections: 0,
            parent: None,
            children: 0,
        };
        place(&mut chunks, chunks_x, 10, 9, blocker());
        place(&mut chunks, chunks_x, 9, 10, blocker());
        place(&mut chunks, chunks_x, 11, 10, blocker());

        let (sprout, genome) = vine_sprout(100);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            10,
            1,
            100,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
        // Center weight of DEATH_DEPOSIT_KERNEL is 4.
        assert!(cell_at(&chunks, chunks_x, 10, 10).organic >= 4);
    }

    #[test]
    fn growth_eats_foreign_leaves_and_pools_their_energy() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        // Foreign leaves on all three growth targets — edible, not blocking.
        let foreign_leaf = || Occupant::Leaf {
            plant: 99,
            energy: 50,
            facing: Direction::North,
            parent: None,
        };
        place(&mut chunks, chunks_x, 10, 9, foreign_leaf()); // front
        place(&mut chunks, chunks_x, 9, 10, foreign_leaf()); // left
        place(&mut chunks, chunks_x, 11, 10, foreign_leaf()); // right

        let (sprout, genome) = vine_sprout(30);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            10,
            1,
            30,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        // Sprout's pool: 30 own + 3*50 harvested = 180.
        // Effective cost (Sprout=20 + Leaf=5 + Leaf=5) = 30.
        // Resulting stem energy = 180 - 30 = 150.
        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Stem { plant, energy, .. } => {
                assert_eq!(plant, 1, "stem belongs to eater plant");
                assert_eq!(energy, 150, "pool minus cost");
            }
            ref other => panic!("expected stem, got {other:?}"),
        }
        // The eaten cells now belong to the eater plant.
        match cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Sprout { plant, .. } => assert_eq!(plant, 1),
            ref other => panic!("expected eater sprout in front, got {other:?}"),
        }
        match cell_at(&chunks, chunks_x, 9, 10).occupant {
            Occupant::Leaf { plant, .. } => assert_eq!(plant, 1),
            ref other => panic!("expected eater leaf at left, got {other:?}"),
        }
    }

    #[test]
    fn growth_skips_own_plant_cells() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        // Block the front with an own-plant leaf — cannot eat your own.
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Leaf {
                plant: 1,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        let (sprout, genome) = vine_sprout(100);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            10,
            1,
            100,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        // Front (own leaf) untouched; sides grow normally.
        match cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Leaf { plant, energy, .. } => {
                assert_eq!(plant, 1);
                assert_eq!(energy, 50, "own leaf still intact");
            }
            ref other => panic!("expected own leaf preserved, got {other:?}"),
        }
        // Sides got the leaves.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 9, 10).occupant,
            Occupant::Leaf { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 10).occupant,
            Occupant::Leaf { .. }
        ));
    }

    #[test]
    fn growth_waits_when_energy_below_cost() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        // Vine cost = sprout(20) + leaf(5) + leaf(5) = 30. 5 is well below.
        let (sprout, genome) = vine_sprout(5);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        attempt_growth(
            &mut chunks,
            chunks_x,
            max,
            max,
            10,
            10,
            1,
            5,
            Direction::North,
            None,
            0,
            &genome,
            &mut det_rng(),
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Sprout { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 9).occupant,
            Occupant::Empty
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 9, 10).occupant,
            Occupant::Empty
        ));
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 10).occupant,
            Occupant::Empty
        ));
    }

    // ---------- pure-helper tests ----------

    #[test]
    fn dir_bitmask_round_trip() {
        for d in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ] {
            let mask = dir_to_bitmask(d);
            let dirs = bitmask_to_dirs(mask);
            assert_eq!(dirs, vec![d]);
            assert_eq!(bit_to_dir(mask), d);
        }
    }

    #[test]
    fn bitmask_to_dirs_decodes_combined_mask() {
        let mask = STEM_CONNECT_NORTH | STEM_CONNECT_EAST | STEM_CONNECT_SOUTH | STEM_CONNECT_WEST;
        assert_eq!(
            bitmask_to_dirs(mask),
            vec![
                Direction::North,
                Direction::East,
                Direction::South,
                Direction::West,
            ]
        );
        assert!(bitmask_to_dirs(0).is_empty());
    }

    #[test]
    fn rotate_left_cycles_through_all_dirs() {
        let mut d = Direction::North;
        d = rotate_left(d);
        assert_eq!(d, Direction::West);
        d = rotate_left(d);
        assert_eq!(d, Direction::South);
        d = rotate_left(d);
        assert_eq!(d, Direction::East);
        d = rotate_left(d);
        assert_eq!(d, Direction::North);
    }

    #[test]
    fn rotate_right_cycles_through_all_dirs() {
        let mut d = Direction::North;
        d = rotate_right(d);
        assert_eq!(d, Direction::East);
        d = rotate_right(d);
        assert_eq!(d, Direction::South);
        d = rotate_right(d);
        assert_eq!(d, Direction::West);
        d = rotate_right(d);
        assert_eq!(d, Direction::North);
    }

    #[test]
    fn opposite_dir_is_involution() {
        for d in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ] {
            assert_eq!(opposite_dir(opposite_dir(d)), d);
        }
        assert_eq!(opposite_dir(Direction::North), Direction::South);
        assert_eq!(opposite_dir(Direction::East), Direction::West);
    }

    #[test]
    fn direction_to_delta_matches_screen_axes() {
        assert_eq!(direction_to_delta(Direction::North), (0, -1));
        assert_eq!(direction_to_delta(Direction::East), (1, 0));
        assert_eq!(direction_to_delta(Direction::South), (0, 1));
        assert_eq!(direction_to_delta(Direction::West), (-1, 0));
    }

    #[test]
    fn linear_idx_packs_chunks_then_cells() {
        // 2x1 chunk world: idx 0 is chunk(0,0)'s first cell.
        assert_eq!(linear_idx(2, 0, 0), 0);
        // last cell of chunk(0,0)
        let edge = CHUNK_EDGE as i32;
        assert_eq!(
            linear_idx(2, edge - 1, edge - 1),
            CHUNK_AREA - 1
        );
        // first cell of chunk(1,0)
        assert_eq!(linear_idx(2, edge, 0), CHUNK_AREA);
    }

    #[test]
    fn is_valid_child_only_for_sinks() {
        let sprout = Occupant::Sprout {
            plant: 1,
            energy: 10,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
            current_gene: 0,
        };
        let stem_with_kids = Occupant::Stem {
            plant: 1,
            energy: 10,
            connections: STEM_CONNECT_NORTH,
            parent: None,
            children: STEM_CONNECT_NORTH,
        };
        let stem_no_kids = Occupant::Stem {
            plant: 1,
            energy: 10,
            connections: 0,
            parent: None,
            children: 0,
        };
        let leaf = Occupant::Leaf {
            plant: 1,
            energy: 10,
            facing: Direction::North,
            parent: None,
        };
        let seed = Occupant::Seed {
            plant: 1,
            energy: 10,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
        };
        assert!(is_valid_child(&sprout));
        assert!(is_valid_child(&seed), "seeds receive energy like sprouts");
        assert!(is_valid_child(&stem_with_kids));
        assert!(!is_valid_child(&stem_no_kids));
        assert!(!is_valid_child(&leaf));
        assert!(!is_valid_child(&Occupant::Empty));
    }

    #[test]
    fn push_targets_match_role() {
        let leaf = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: Some(Direction::South),
        };
        assert_eq!(push_targets(&leaf), vec![Direction::South]);

        let leaf_orphan = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        assert!(push_targets(&leaf_orphan).is_empty());

        let stem_kids = Occupant::Stem {
            plant: 1,
            energy: 0,
            connections: 0,
            parent: Some(Direction::South),
            children: STEM_CONNECT_NORTH | STEM_CONNECT_EAST,
        };
        assert_eq!(
            push_targets(&stem_kids),
            vec![Direction::North, Direction::East]
        );

        let stem_no_kids = Occupant::Stem {
            plant: 1,
            energy: 0,
            connections: 0,
            parent: Some(Direction::South),
            children: 0,
        };
        assert_eq!(push_targets(&stem_no_kids), vec![Direction::South]);

        let sprout = Occupant::Sprout {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: Some(Direction::South),
            current_gene: 0,
        };
        assert!(push_targets(&sprout).is_empty(), "sprouts are sinks");

        assert!(push_targets(&Occupant::Empty).is_empty());
    }

    #[test]
    fn cell_has_no_push_target_for_orphans_and_oob() {
        let chunks_x = 1u32;
        let max = CHUNK_EDGE as i32;
        let mut chunks = empty_world(chunks_x, 1);

        // Stem with children=0 and parent=None → orphan.
        let stem_orphan = Occupant::Stem {
            plant: 1,
            energy: 0,
            connections: 0,
            parent: None,
            children: 0,
        };
        assert!(cell_has_no_push_target(
            &stem_orphan, &chunks, chunks_x, max, max, 5, 5
        ));

        // Stem with children present → not orphan.
        let stem_kid = Occupant::Stem {
            plant: 1,
            energy: 0,
            connections: 0,
            parent: None,
            children: STEM_CONNECT_NORTH,
        };
        assert!(!cell_has_no_push_target(
            &stem_kid, &chunks, chunks_x, max, max, 5, 5
        ));

        // Leaf whose parent direction points at an Empty cell → orphan.
        let leaf = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: Some(Direction::South),
        };
        assert!(cell_has_no_push_target(
            &leaf, &chunks, chunks_x, max, max, 5, 5
        ));

        // Same leaf, but place a stem in the parent direction → not orphan.
        place(
            &mut chunks,
            chunks_x,
            5,
            6,
            Occupant::Stem {
                plant: 1,
                energy: 0,
                connections: 0,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        assert!(!cell_has_no_push_target(
            &leaf, &chunks, chunks_x, max, max, 5, 5
        ));

        // Leaf at top edge with parent=North → OOB, orphan.
        let leaf_top = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::South,
            parent: Some(Direction::North),
        };
        assert!(cell_has_no_push_target(
            &leaf_top, &chunks, chunks_x, max, max, 5, 0
        ));

        // Sprout / seed / empty are not subject to orphan death.
        assert!(!cell_has_no_push_target(
            &Occupant::Empty, &chunks, chunks_x, max, max, 0, 0
        ));
    }

    #[test]
    fn slot_cost_per_product() {
        assert_eq!(slot_cost(SlotProduct::Nothing), 0);
        assert_eq!(slot_cost(SlotProduct::Leaf), COST_LEAF);
        assert_eq!(slot_cost(SlotProduct::Root), COST_ROOT);
        assert_eq!(slot_cost(SlotProduct::Antenna), COST_ANTENNA);
        assert_eq!(slot_cost(SlotProduct::Seed), COST_SEED);
        assert_eq!(slot_cost(SlotProduct::Sprout), COST_SPROUT);
    }

    #[test]
    fn make_slot_occupant_sets_parent_back_toward_creator() {
        // The parent direction passed to make_slot_occupant is the spawn
        // direction; the new cell's `parent` field should point the OPPOSITE
        // way (back at the producing sprout).
        let parent_genome = Genome::default_vine();
        let leaf = make_slot_occupant(
            SlotProduct::Leaf,
            7,
            Direction::East,
            Direction::East,
            &parent_genome,
            0,
            &mut det_rng(),
        )
        .unwrap();
        match leaf {
            Occupant::Leaf {
                plant,
                facing,
                parent,
                energy,
            } => {
                assert_eq!(plant, 7);
                assert_eq!(facing, Direction::East);
                assert_eq!(parent, Some(Direction::West));
                assert_eq!(energy, COST_LEAF);
            }
            _ => panic!("expected leaf"),
        }

        let nothing = make_slot_occupant(
            SlotProduct::Nothing,
            1,
            Direction::North,
            Direction::North,
            &parent_genome,
            0,
            &mut det_rng(),
        );
        assert!(nothing.is_none());

        let sprout = make_slot_occupant(
            SlotProduct::Sprout,
            5,
            Direction::North,
            Direction::North,
            &parent_genome,
            3,
            &mut det_rng(),
        )
        .unwrap();
        match sprout {
            Occupant::Sprout {
                current_gene,
                parent,
                ..
            } => {
                assert_eq!(current_gene, 3);
                assert_eq!(parent, Some(Direction::South));
            }
            _ => panic!("expected sprout"),
        }
    }

    #[test]
    fn occupant_energy_get_set_round_trip() {
        let mut occ = Occupant::Stem {
            plant: 1,
            energy: 50,
            connections: 0,
            parent: None,
            children: STEM_CONNECT_NORTH,
        };
        assert_eq!(occupant_energy(&occ), Some(50));
        set_occupant_energy(&mut occ, 99);
        assert_eq!(occupant_energy(&occ), Some(99));

        let empty = Occupant::Empty;
        assert_eq!(occupant_energy(&empty), None);
    }

    #[test]
    fn upkeep_for_each_occupant() {
        let leaf = Occupant::Leaf {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        let sprout = Occupant::Sprout {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
            current_gene: 0,
        };
        let seed = Occupant::Seed {
            plant: 1,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
        };
        assert_eq!(upkeep_for(&leaf), UPKEEP_DEFAULT);
        assert_eq!(upkeep_for(&sprout), UPKEEP_SPROUT);
        assert_eq!(upkeep_for(&seed), UPKEEP_SEED);
        assert_eq!(upkeep_for(&Occupant::Empty), 0);
    }

    #[test]
    fn mutate_genome_at_rate_zero_clones_exactly() {
        let g = Genome::default_vine();
        let copied = mutate_genome(&g, 0.0, &mut det_rng());
        assert_eq!(copied, g);
    }

    #[test]
    fn mutate_genome_with_same_seed_is_deterministic() {
        let g = Genome::default_vine();
        let a = mutate_genome(&g, 0.5, &mut ChaCha12Rng::seed_from_u64(42));
        let b = mutate_genome(&g, 0.5, &mut ChaCha12Rng::seed_from_u64(42));
        assert_eq!(a, b, "same seed should produce identical mutations");
        let c = mutate_genome(&g, 0.5, &mut ChaCha12Rng::seed_from_u64(43));
        assert_ne!(a, c, "different seed should diverge at rate 0.5");
    }

    // ---------- phase tests via mutate_world ----------
    //
    // Strategy: build a tiny, complete world where the phase under test has
    // an observable, deterministic effect. Energy numbers were hand-traced
    // through all 7 phases so the post-tick assertions are exact.

    fn fill_organic(chunks: &mut [Chunk], v: u16) {
        for chunk in chunks.iter_mut() {
            for cell in chunk.cells.iter_mut() {
                cell.organic = v;
            }
        }
    }

    fn fill_soil_energy(chunks: &mut [Chunk], v: u16) {
        for chunk in chunks.iter_mut() {
            for cell in chunk.cells.iter_mut() {
                cell.soil_energy = v;
            }
        }
    }

    #[test]
    fn phase_photosynthesis_credits_sunlit_leaves() {
        // Leaf (sunlit, e=10) → sprout sink (e=0). One tick should funnel
        // photo+pre-existing energy into the sprout.
        //   photo: leaf 10→15
        //   upkeep: leaf 15→14, sprout 0→0
        //   push: leaf surplus 13 → sprout
        //   final: leaf 1, sprout 13
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let edge = CHUNK_EDGE as i32;
        let leaf_idx = 10 * (CHUNK_EDGE as usize) + 10;
        chunks[0].cells[leaf_idx].sunlit = true;
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                energy: 10,
                facing: Direction::North,
                parent: Some(Direction::South),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            11,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());
        let _ = edge;

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 1),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Sprout { energy, .. } => assert_eq!(*energy, 13),
            other => panic!("sprout gone: {other:?}"),
        }
    }

    #[test]
    fn phase_soil_pulls_organic_around_root() {
        // Root → stem → sprout chain. One tick should subtract per the
        // ROOT_PULL_KERNEL ([[0,1,0],[1,2,1],[0,1,0]]) from the soil cells
        // around the root.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        fill_organic(&mut chunks, 100);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Root {
                plant: 1,
                energy: 0,
                parent: Some(Direction::North),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Stem {
                plant: 1,
                energy: 50,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            8,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        // Center weight 2.
        assert_eq!(cell_at(&chunks, chunks_x, 10, 10).organic, 98);
        // Cardinals weight 1.
        assert_eq!(cell_at(&chunks, chunks_x, 9, 10).organic, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 10).organic, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 9).organic, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 11).organic, 99);
        // Corners weight 0 → untouched.
        assert_eq!(cell_at(&chunks, chunks_x, 9, 9).organic, 100);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 11).organic, 100);
    }

    #[test]
    fn phase_soil_pulls_energy_around_antenna() {
        // Same kernel, but for soil_energy via antenna.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        fill_soil_energy(&mut chunks, 100);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Antenna {
                plant: 1,
                energy: 0,
                parent: Some(Direction::North),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Stem {
                plant: 1,
                energy: 50,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            8,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        assert_eq!(cell_at(&chunks, chunks_x, 10, 10).soil_energy, 98);
        assert_eq!(cell_at(&chunks, chunks_x, 9, 10).soil_energy, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 10).soil_energy, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 9).soil_energy, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 11).soil_energy, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 9, 9).soil_energy, 100);
    }

    #[test]
    fn phase_upkeep_decreases_total_system_energy() {
        // Leaf (not sunlit) → sprout sink. No photo, no soil.
        // Pre: leaf=4, sprout=10. Total 14.
        // upkeep: leaf -1, sprout -3. Total 10.
        // push: leaf surplus 2 → sprout. Total still 10.
        // Post: leaf=1, sprout=9. Total 10.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                energy: 4,
                facing: Direction::North,
                parent: Some(Direction::South),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            11,
            Occupant::Sprout {
                plant: 1,
                energy: 10,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 1),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Sprout { energy, .. } => assert_eq!(*energy, 9),
            other => panic!("sprout gone: {other:?}"),
        }
    }

    #[test]
    fn phase_prune_clears_invalid_child_bits() {
        // Stem with children = N | S.
        //   N points at Empty → invalid, drops.
        //   S points at sprout → valid, keeps.
        // After tick: stem.children == S only. Stem still alive (has child).
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Stem {
                plant: 1,
                energy: 50,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: None,
                children: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            11,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::West, // grows W/S/N — corners of single sprout
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        match &cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Stem { children, .. } => {
                assert_eq!(*children, STEM_CONNECT_SOUTH, "N child should be pruned");
            }
            other => panic!("expected stem, got {other:?}"),
        }
    }

    #[test]
    fn phase_push_moves_energy_from_leaf_to_parent_stem() {
        // Leaf (e=10, not sunlit) → stem (children=S → sprout).
        // upkeep: leaf 10→9, stem 0→0 (sat), sprout 0→0 (sat)
        // push: leaf surplus 8 → stem; stem cur=0 ≤ buffer → no push
        // post: leaf=1, stem=8, sprout=0
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                energy: 10,
                facing: Direction::North,
                parent: Some(Direction::South),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            11,
            Occupant::Stem {
                plant: 1,
                energy: 0,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: Some(Direction::North),
                children: STEM_CONNECT_SOUTH,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            12,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 1),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Stem { energy, .. } => assert_eq!(*energy, 8),
            other => panic!("stem gone: {other:?}"),
        }
    }

    #[test]
    fn phase_death_orphan_leaf_dies_and_deposits_organic() {
        // Lone leaf with parent=None has no push target. Phase 7 turns it
        // into Empty and deposits organic per DEATH_DEPOSIT_KERNEL.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
        // DEATH_DEPOSIT_KERNEL center weight = 4.
        assert!(cell_at(&chunks, chunks_x, 10, 10).organic >= 4);
        // Cardinals weight 2.
        assert!(cell_at(&chunks, chunks_x, 9, 10).organic >= 2);
        assert!(cell_at(&chunks, chunks_x, 11, 10).organic >= 2);
        // Corners weight 1.
        assert!(cell_at(&chunks, chunks_x, 9, 9).organic >= 1);
    }

    #[test]
    fn phase_death_zero_energy_cell_clears() {
        // Stem with energy=0 and parent (still has push target so doesn't
        // orphan-die). After upkeep the energy stays at 0 and Phase 7
        // catches it via the energy_dead path.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Stem {
                plant: 1,
                energy: 0,
                connections: STEM_CONNECT_SOUTH,
                parent: Some(Direction::South),
                children: STEM_CONNECT_NORTH,
            },
        );
        // Sprout to keep stem from being orphan-dead via children.
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Sprout {
                plant: 1,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
    }

    #[test]
    fn phase_soil_regulation_drifts_toward_rest() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Three sample cells: below, at, above the rest level.
        let edge = CHUNK_EDGE as usize;
        chunks[0].cells[0].soil_energy = 50; // below
        chunks[0].cells[1].soil_energy = SOIL_ENERGY_REST; // at rest
        chunks[0].cells[2].soil_energy = 200; // above
        // No occupants, so other phases are no-ops.

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        assert_eq!(chunks[0].cells[0].soil_energy, 50 + SOIL_ENERGY_REGULATION);
        assert_eq!(chunks[0].cells[1].soil_energy, SOIL_ENERGY_REST);
        assert_eq!(chunks[0].cells[2].soil_energy, 200 - SOIL_ENERGY_REGULATION);
        let _ = edge;
    }

    #[test]
    fn phase_soil_regulation_clamps_at_rest() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Just-below and just-above cases — must not overshoot rest.
        chunks[0].cells[0].soil_energy = SOIL_ENERGY_REST - 1;
        chunks[0].cells[1].soil_energy = SOIL_ENERGY_REST + 1;

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        assert_eq!(chunks[0].cells[0].soil_energy, SOIL_ENERGY_REST);
        assert_eq!(chunks[0].cells[1].soil_energy, SOIL_ENERGY_REST);
    }

    /// Build trunk-stem + middle-stem-pointing-at-seed + seed setup so the
    /// middle stem doesn't orphan-die after dropoff clears its child bit.
    fn place_seed_dropoff_fixture(chunks: &mut [Chunk], seed_energy: Energy) {
        place(
            chunks,
            1,
            10,
            12,
            Occupant::Stem {
                plant: 1,
                energy: 50,
                connections: STEM_CONNECT_NORTH,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        place(
            chunks,
            1,
            10,
            11,
            Occupant::Stem {
                plant: 1,
                energy: 50,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: Some(Direction::South),
                children: STEM_CONNECT_NORTH,
            },
        );
        place(
            chunks,
            1,
            10,
            10,
            Occupant::Seed {
                plant: 1,
                energy: seed_energy,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
            },
        );
    }

    #[test]
    fn phase_seed_dropoff_disconnects_at_threshold() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place_seed_dropoff_fixture(&mut chunks, SEED_DROPOFF_THRESHOLD);

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        match &cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Seed { parent, .. } => assert_eq!(*parent, None),
            other => panic!("expected seed, got {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Stem { children, .. } => {
                assert_eq!(
                    *children & STEM_CONNECT_NORTH,
                    0,
                    "north (seed) bit should be cleared"
                );
            }
            other => panic!("expected stem, got {other:?}"),
        }
    }

    #[test]
    fn phase_seed_dropoff_keeps_below_threshold_connected() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Low enough that even after upkeep + push from the trunk chain,
        // it stays under the dropoff threshold.
        place_seed_dropoff_fixture(&mut chunks, 30);

        mutate_world(&mut chunks, 1, 1, &mut det_rng());

        match &cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Seed { parent, .. } => assert_eq!(*parent, Some(Direction::South)),
            other => panic!("expected seed, got {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Stem { children, .. } => {
                assert_eq!(*children & STEM_CONNECT_NORTH, STEM_CONNECT_NORTH);
            }
            other => panic!("expected stem, got {other:?}"),
        }
    }
}
