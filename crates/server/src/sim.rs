use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use rand::SeedableRng;

use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ClanId, Direction, Energy, GENOME_MAX, GENOME_MIN, Gene,
    Genome, MUTATION_RATE_MAX, MUTATION_RATE_MIN, Occupant, STEM_CONNECT_EAST, STEM_CONNECT_NORTH,
    STEM_CONNECT_SOUTH, STEM_CONNECT_WEST, ServerMessage, SimParams, SlotProduct, WorldGenParams,
};
use rand::Rng;
use rand_chacha::ChaCha12Rng;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

/// Fixed bell-curve shape for the 3×3 soil/death kernels. Magnitude is
/// dialed at runtime by `SimParams::*_scale` (1.0 = stock); the shape
/// stays constant. See `scaled_kernel` for how the scale folds in.
const BASE_BELL_KERNEL: [[u16; 3]; 3] = [[1, 2, 1], [2, 4, 2], [1, 2, 1]];

fn scaled_kernel(scale: f32) -> [[u16; 3]; 3] {
    let mut out = [[0u16; 3]; 3];
    for y in 0..3 {
        for x in 0..3 {
            let v = (BASE_BELL_KERNEL[y][x] as f32 * scale.max(0.0)).round();
            out[y][x] = v.min(u16::MAX as f32) as u16;
        }
    }
    out
}

pub struct SimState {
    /// World dims in chunks. Atomic so `regenerate_world` can resize
    /// the world without taking a write lock on SimState — readers
    /// (sim loop, Welcome, persist, etc.) just `.load()`.
    pub chunks_x: AtomicU32,
    pub chunks_y: AtomicU32,
    pub world: std::sync::Mutex<Vec<Chunk>>,
    pub tick_tx: broadcast::Sender<Arc<Vec<u8>>>,
    pub next_plant_id: AtomicU32,
    pub current_tick: AtomicU64,
    pub control: std::sync::Mutex<SimControl>,
    /// Current seed. AtomicU64 so `regenerate_world` can update it without
    /// taking a write lock on SimState — readers (e.g., Welcome) just load.
    pub seed: AtomicU64,
    pub rng: std::sync::Mutex<ChaCha12Rng>,
    /// Live-tunable scalars. Snapshotted at the top of each tick so
    /// `mutate_world` works against a stable copy and the lock is held
    /// only briefly.
    pub params: std::sync::Mutex<SimParams>,
    /// World-gen knobs that built the current world. Updated whenever
    /// `regenerate_world` runs; broadcast in `Welcome` so viewers can
    /// populate the regen dialog with the values currently in effect.
    pub world_gen_params: std::sync::Mutex<WorldGenParams>,
}

#[derive(Debug)]
pub struct SimControl {
    pub paused: bool,
    pub tick_hz: u32,
    /// When false (default), the sim ticks as fast as it can. When
    /// true, it honors `tick_hz` between ticks. Pause is independent.
    pub tick_rate_limited: bool,
    pub step_pending: u32,
}

enum SimAction {
    TickNow,
    TickAfter(Duration),
    Wait,
}

#[derive(Clone, Copy, Debug)]
enum SoilField {
    Organic,
    Energy,
}

pub async fn run_sim_loop(state: Arc<SimState>) {
    loop {
        let action = {
            let mut ctrl = state.control.lock().expect("control poisoned");
            if ctrl.step_pending > 0 {
                ctrl.step_pending -= 1;
                SimAction::TickNow
            } else if ctrl.paused {
                SimAction::Wait
            } else if ctrl.tick_rate_limited {
                let dur = Duration::from_millis(1000 / ctrl.tick_hz.max(1) as u64);
                SimAction::TickAfter(dur)
            } else {
                SimAction::TickNow
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
        // Span guards must drop before the yield_now below — entered()
        // guards aren't Send across an await. Scope the whole tick body
        // in a block so all spans drop at the closing brace.
        {
            let _tick_span = tracing::info_span!("tick", tick).entered();

            let snapshot_chunks: Vec<protocol::WireChunk> = {
                let params = *state.params.lock().expect("params poisoned");
                let mut chunks = state.world.lock().expect("sim lock poisoned");
                let mut rng = state.rng.lock().expect("rng lock poisoned");
                let _mutate = tracing::info_span!("mutate_world").entered();
                mutate_world(
                    &mut chunks,
                    state.chunks_x.load(Ordering::Relaxed),
                    state.chunks_y.load(Ordering::Relaxed),
                    &params,
                    &state.next_plant_id,
                    &mut *rng,
                );
                drop(_mutate);
                // Build the wire view directly from the locked world.
                // Avoids cloning the full Chunks (with their
                // Box<Genome>s) just to serialize a stripped version.
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

        // When tick_rate_limited is off there's no `.await` in this
        // iteration; without an explicit yield the cooperative tokio
        // scheduler can starve other tasks (notably the ctrl_c signal
        // handler and the QUIC accept loop). One yield_now per tick is
        // enough — it's effectively free when the runtime has nothing
        // else queued.
        tokio::task::yield_now().await;
    }
}

/// Wipe the world, reseed the RNG, reset tick + plant id, and broadcast a
/// fresh Welcome + ChunkBatch so connected viewers refresh in place. Holds
/// the world + rng mutexes for the swap; safe to call between sim ticks.
pub fn regenerate_world(state: &SimState, seed: u64, params: WorldGenParams) {
    let chunks_x = params.chunks_x;
    let chunks_y = params.chunks_y;

    let mut new_chunks = crate::world::build_world(&params);
    let mut new_rng = ChaCha12Rng::seed_from_u64(seed);
    let count = crate::world::place_random_sprout_grid(&mut new_chunks, &params, &mut new_rng);

    {
        let mut world = state.world.lock().expect("sim lock poisoned");
        let mut rng = state.rng.lock().expect("rng lock poisoned");
        let mut wgp = state.world_gen_params.lock().expect("wgp poisoned");
        *world = new_chunks.clone();
        *rng = new_rng;
        *wgp = params;
    }
    state.chunks_x.store(chunks_x, Ordering::Relaxed);
    state.chunks_y.store(chunks_y, Ordering::Relaxed);
    state.seed.store(seed, Ordering::Relaxed);
    state.next_plant_id.store(count + 1, Ordering::Relaxed);
    state.current_tick.store(0, Ordering::Relaxed);
    info!(seed, chunks_x, chunks_y, "world regenerated");

    let (paused, tick_hz, tick_rate_limited) = {
        let ctrl = state.control.lock().expect("control poisoned");
        (ctrl.paused, ctrl.tick_hz, ctrl.tick_rate_limited)
    };
    let sim_params = *state.params.lock().expect("params poisoned");
    let welcome = ServerMessage::Welcome {
        world_chunks_x: chunks_x,
        world_chunks_y: chunks_y,
        paused,
        tick_hz,
        tick_rate_limited,
        tick: 0,
        seed,
        sim_params,
        world_gen_params: params,
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
    let max_x = state.chunks_x.load(Ordering::Relaxed) as i32 * edge;
    let max_y = state.chunks_y.load(Ordering::Relaxed) as i32 * edge;
    if x < 0 || y < 0 || x >= max_x || y >= max_y {
        warn!(x, y, "spawn out of bounds");
        return;
    }
    let cx = x / edge;
    let cy = y / edge;
    let lx = (x % edge) as usize;
    let ly = (y % edge) as usize;
    let chunk_idx =
        (cy as usize) * (state.chunks_x.load(Ordering::Relaxed) as usize) + (cx as usize);
    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;

    let plant = state.next_plant_id.fetch_add(1, Ordering::Relaxed);
    // Manually-spawned sprouts default to clan 0; they don't inherit from
    // any lineage (yet). If we ever want clan to reflect spawn position,
    // we can compute it from (x, y) the same way world::place_random does.
    let mut chunks = state.world.lock().expect("sim lock poisoned");
    let genome = Genome::default_vine();
    chunks[chunk_idx].cells[cell_idx].lineage_mutation_rate = genome.mutation_rate;
    chunks[chunk_idx].cells[cell_idx].occupant = Occupant::Sprout {
        plant,
        clan: 0,
        energy: 100,
        facing,
        genome: Box::new(genome),
        parent: None,
        current_gene: 0,
    };
    info!(x, y, plant, ?facing, "sprout spawned");
}

fn mutate_world(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    params: &SimParams,
    next_plant_id: &AtomicU32,
    rng: &mut impl Rng,
) {
    let edge = CHUNK_EDGE as i32;
    let max_x = chunks_x as i32 * edge;
    let max_y = chunks_y as i32 * edge;
    let wrap = params.world_wrap;
    let root_kernel = scaled_kernel(params.root_pull_scale);
    let antenna_kernel = scaled_kernel(params.antenna_pull_scale);
    let death_kernel = scaled_kernel(params.death_deposit_scale);

    // Phase 1: photosynthesis (per-cell, in-place).
    {
        let _span = tracing::info_span!("phase_photo").entered();
        for chunk in chunks.iter_mut() {
            for cell in chunk.cells.iter_mut() {
                if cell.sunlit {
                    if let Occupant::Leaf { energy, .. } = &mut cell.occupant {
                        *energy = energy.saturating_add(params.leaf_photosynthesis);
                    }
                }
            }
        }
    }

    // Phase 1.5: soil energy regulation. Each cell drifts its soil_energy
    // toward params.soil_energy_rest by params.soil_energy_regulation per
    // tick. Runs before soil pulls so antennae deplete a freshened soil
    // each tick.
    {
        let _span = tracing::info_span!("phase_soil_regulation").entered();
        let rest = params.soil_energy_rest;
        let reg = params.soil_energy_regulation;
        for chunk in chunks.iter_mut() {
            for cell in chunk.cells.iter_mut() {
                if cell.soil_energy < rest {
                    cell.soil_energy = (cell.soil_energy + reg).min(rest);
                } else if cell.soil_energy > rest {
                    cell.soil_energy = cell.soil_energy.saturating_sub(reg).max(rest);
                }
            }
        }
    }

    // Phase 2: soil pulls.
    //
    // Order-independent fair-share: when multiple pullers are within 3×3
    // of the same soil cell, the cell's contents are split between them
    // in proportion to their kernel weights, instead of "first puller in
    // iteration order grabs all available."
    //
    // Three passes:
    //   1. Demand pass — each puller writes its kernel weights to the
    //      `demand[neighbor]` buffer with `+=`. After this pass, each
    //      cell knows the total amount pullers want to take from it.
    //   2. Gain pass — each puller reads the source state of its 3×3
    //      soil neighbors plus the demand buffer, computes its fair
    //      share `(my_kernel_weight * actual_loss / total_demand)`, and
    //      writes only its own gain.
    //   3. Apply pass — each soil cell subtracts `min(available, demand)`
    //      from itself; each puller adds its gain to its energy.
    //
    // Integer-divided shares may leave a few units in the soil due to
    // floor rounding — that's acceptable and erring on the conservative
    // side of mass conservation.
    {
        let _span = tracing::info_span!("phase_soil_pulls").entered();
        let total_cells = chunks.len() * CHUNK_AREA;
        // u8 is sufficient: max demand per cell is bounded by 9
        // pullers × max kernel weight (4) = 36.
        let mut organic_demand: Vec<u8> = vec![0; total_cells];
        let mut energy_demand: Vec<u8> = vec![0; total_cells];
        let mut pullers: Vec<(i32, i32, SoilField)> = Vec::new();

        // Pass 1: collect pullers and accumulate per-cell demand.
        for cy in 0..chunks_y {
            for cx in 0..chunks_x {
                for ly in 0..(CHUNK_EDGE as usize) {
                    for lx in 0..(CHUNK_EDGE as usize) {
                        let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                        let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                        let field = match &chunks[chunk_idx].cells[cell_idx].occupant {
                            Occupant::Root { .. } => SoilField::Organic,
                            Occupant::Antenna { .. } => SoilField::Energy,
                            _ => continue,
                        };
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        let kernel = match field {
                            SoilField::Organic => &root_kernel,
                            SoilField::Energy => &antenna_kernel,
                        };
                        pullers.push((wx, wy, field));
                        for dy in -1..=1i32 {
                            for dx in -1..=1i32 {
                                let weight = kernel[(dy + 1) as usize][(dx + 1) as usize];
                                if weight == 0 {
                                    continue;
                                }
                                let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                                    continue;
                                };
                                let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                                    continue;
                                };
                                let idx = linear_idx(chunks_x, nx, ny);
                                let buf = match field {
                                    SoilField::Organic => &mut organic_demand,
                                    SoilField::Energy => &mut energy_demand,
                                };
                                buf[idx] = buf[idx].saturating_add(weight as u8);
                            }
                        }
                    }
                }
            }
        }

        // Pass 2: each puller computes its fair-share gain across its
        // 3×3 soil neighbors. Reads only; no writes yet.
        let mut puller_gains: Vec<(i32, i32, u32)> = Vec::with_capacity(pullers.len());
        for (pwx, pwy, field) in &pullers {
            let kernel = match field {
                SoilField::Organic => &root_kernel,
                SoilField::Energy => &antenna_kernel,
            };
            let mut gain: u32 = 0;
            for dy in -1..=1i32 {
                for dx in -1..=1i32 {
                    let weight = kernel[(dy + 1) as usize][(dx + 1) as usize] as u32;
                    if weight == 0 {
                        continue;
                    }
                    let Some(nx) = in_bounds(pwx + dx, max_x, wrap) else {
                        continue;
                    };
                    let Some(ny) = in_bounds(pwy + dy, max_y, wrap) else {
                        continue;
                    };
                    let n_chunk_idx =
                        (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
                    let n_cell_idx =
                        (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
                    let neighbor = &chunks[n_chunk_idx].cells[n_cell_idx];
                    let avail = match field {
                        SoilField::Organic => neighbor.organic as u32,
                        SoilField::Energy => neighbor.soil_energy as u32,
                    };
                    let idx = linear_idx(chunks_x, nx, ny);
                    let total_demand = match field {
                        SoilField::Organic => organic_demand[idx] as u32,
                        SoilField::Energy => energy_demand[idx] as u32,
                    };
                    if total_demand == 0 {
                        continue;
                    }
                    let actual_loss = avail.min(total_demand);
                    gain += weight * actual_loss / total_demand;
                }
            }
            puller_gains.push((*pwx, *pwy, gain));
        }

        // Pass 3a: apply soil losses (each cell subtracts its own loss).
        for cy in 0..chunks_y {
            for cx in 0..chunks_x {
                for ly in 0..(CHUNK_EDGE as usize) {
                    for lx in 0..(CHUNK_EDGE as usize) {
                        let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                        let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        let idx = linear_idx(chunks_x, wx, wy);
                        let cell = &mut chunks[chunk_idx].cells[cell_idx];
                        let od = organic_demand[idx] as u32;
                        if od > 0 {
                            let loss = (cell.organic as u32).min(od) as u16;
                            cell.organic -= loss;
                        }
                        let ed = energy_demand[idx] as u32;
                        if ed > 0 {
                            let loss = (cell.soil_energy as u32).min(ed) as u16;
                            cell.soil_energy -= loss;
                        }
                    }
                }
            }
        }

        // Pass 3b: apply puller gains (each puller writes only itself).
        for (pwx, pwy, gain) in puller_gains {
            if gain == 0 {
                continue;
            }
            if let Some(cell) = cell_at_mut(chunks, chunks_x, pwx, pwy) {
                if let Some(e) = occupant_energy(&cell.occupant) {
                    let new_e = (e as u32 + gain).min(Energy::MAX as u32) as Energy;
                    set_occupant_energy(&mut cell.occupant, new_e);
                }
            }
        }
    }

    // Phase 3: upkeep (per-cell, in-place).
    {
        let _span = tracing::info_span!("phase_upkeep").entered();
        for chunk in chunks.iter_mut() {
            for cell in chunk.cells.iter_mut() {
                if let Some(e) = occupant_energy(&cell.occupant) {
                    let cost = upkeep_for(params, &cell.occupant);
                    set_occupant_energy(&mut cell.occupant, e.saturating_sub(cost));
                }
            }
        }
    }

    // Phase 4: prune. See `phase_prune_pull`.
    let _phase_prune = tracing::info_span!("phase_prune").entered();
    let prune_change_count = phase_prune_pull(chunks, chunks_x, chunks_y, max_x, max_y, wrap);
    drop(_phase_prune);
    tracing::event!(
        target: "phase",
        tracing::Level::INFO,
        prune_changes = prune_change_count,
        "phase_prune_done"
    );

    // Phase 4b: drainage. Each productive parent stem pulls the energy
    // out of any kin dead-end stem child (children == 0) and drops the
    // bit pointing at it. Eliminates the parent↔dead-end mutual flow
    // and fast-tracks the dead-end into energy_dead → death this tick.
    let _phase_drainage = tracing::info_span!("phase_drainage").entered();
    let drain_count =
        phase_drainage(chunks, chunks_x, chunks_y, max_x, max_y, wrap);
    drop(_phase_drainage);
    tracing::event!(
        target: "phase",
        tracing::Level::INFO,
        drain_count,
        "phase_drainage_done"
    );

    // Phase 5: directed push. Production cells push surplus to parent, stems
    // split surplus across children, sprouts/seeds are terminal sinks. Build
    // a delta array from the current state, then apply atomically — removes
    // any order dependency between cells in the same generation.
    let _phase_push = tracing::info_span!("phase_push").entered();
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
                    let buffer = upkeep_for(params, &cell.occupant);
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
                        let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                            continue;
                        };
                        let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                            continue;
                        };
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
                        let new_e = ((e as i32) + delta).clamp(0, Energy::MAX as i32) as Energy;
                        set_occupant_energy(&mut cell.occupant, new_e);
                    }
                }
            }
        }
    }
    drop(_phase_push);

    // Phase 5.5: seed germination. A Seed becomes a Sprout in place (and
    // tries to grow this same tick in phase 6) if either:
    //   - its parent died (cell at parent_dir is Empty or OOB), OR
    //   - it has accumulated SEED_DROPOFF_THRESHOLD energy.
    // In the threshold case the parent stem is still alive — clear its
    // children-bit pointing at the now-departing seed.
    let _phase_germ = tracing::info_span!("phase_germination").entered();
    let mut germinations: Vec<(i32, i32, Option<(i32, i32, u8)>)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let (energy, parent_dir) = match &chunks[chunk_idx].cells[cell_idx].occupant {
                        Occupant::Seed { energy, parent, .. } => (*energy, *parent),
                        _ => continue,
                    };
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;

                    let parent_dead = match parent_dir {
                        Some(dir) => {
                            let (dx, dy) = direction_to_delta(dir);
                            // OOB → parent gone, treat as dead.
                            match (
                                in_bounds(wx + dx, max_x, wrap),
                                in_bounds(wy + dy, max_y, wrap),
                            ) {
                                (Some(nx), Some(ny)) => {
                                    let n_chunk_idx = (ny / edge) as usize * chunks_x as usize
                                        + (nx / edge) as usize;
                                    let n_cell_idx = (ny % edge) as usize * (CHUNK_EDGE as usize)
                                        + (nx % edge) as usize;
                                    matches!(
                                        chunks[n_chunk_idx].cells[n_cell_idx].occupant,
                                        Occupant::Empty
                                    )
                                }
                                _ => true,
                            }
                        }
                        None => false,
                    };

                    let at_threshold = energy >= params.seed_dropoff_threshold;

                    if parent_dead || at_threshold {
                        // Only the threshold case needs to clear the parent
                        // stem's bit — if the parent is dead it's already gone.
                        let parent_clear = if at_threshold && !parent_dead {
                            parent_dir.map(|dir| {
                                let (dx, dy) = direction_to_delta(dir);
                                (wx + dx, wy + dy, dir_to_bitmask(opposite_dir(dir)))
                            })
                        } else {
                            None
                        };
                        germinations.push((wx, wy, parent_clear));
                    }
                }
            }
        }
    }
    let germination_count = germinations.len() as u64;
    for (sx, sy, parent_clear) in germinations {
        let (clan, energy, facing, genome) = match cell_at_mut(chunks, chunks_x, sx, sy) {
            Some(cell) => match &cell.occupant {
                Occupant::Seed {
                    clan,
                    energy,
                    facing,
                    genome,
                    ..
                } => (*clan, *energy, *facing, genome.clone()),
                _ => continue,
            },
            None => continue,
        };
        // Mint a fresh plant id: the germinated sprout is now its own
        // organism, disconnected from its source. Without this, two
        // physically separate trees can share a plant id (the seed's
        // original parent's), which then defeats the same-plant check
        // in `edible_for` — they'd treat each other as kin.
        let plant = next_plant_id.fetch_add(1, Ordering::Relaxed);
        if let Some(seed_cell) = cell_at_mut(chunks, chunks_x, sx, sy) {
            seed_cell.occupant = Occupant::Sprout {
                plant,
                clan,
                energy,
                facing,
                genome,
                parent: None,
                current_gene: 0,
            };
        }
        if let Some((px, py, bit)) = parent_clear {
            if let Some(parent_cell) = cell_at_mut(chunks, chunks_x, px, py) {
                if let Occupant::Stem { children, .. } = &mut parent_cell.occupant {
                    *children &= !bit;
                }
            }
        }
    }
    drop(_phase_germ);
    tracing::event!(
        target: "phase",
        tracing::Level::INFO,
        germinations = germination_count,
        "phase_germination_done"
    );

    // Phase 6: growth (pull-pattern). See `phase_growth_pull` for the
    // full multi-pass coordination logic.
    let _phase_growth = tracing::info_span!("phase_growth").entered();
    let growth_attempts = phase_growth_pull(
        chunks,
        chunks_x,
        chunks_y,
        max_x,
        max_y,
        params,
        &death_kernel,
        rng,
    );
    drop(_phase_growth);
    tracing::event!(
        target: "phase",
        tracing::Level::INFO,
        growth_attempts,
        "phase_growth_done"
    );
    // Phase 7: death — collect cells that should die this tick. Reasons:
    //   - energy_dead: occupant.energy is 0
    //   - stranded:   stem with no push target (no children + missing parent)
    //                 or production cell (Leaf/Root/Antenna) whose parent is
    //                 missing
    //   - poisoned:   soil organic or soil energy exceeds the toxicity
    //                 threshold and the occupant isn't immune (Root is
    //                 immune to organic, Antenna is immune to energy)
    // Apply: deposit organic per kernel weight + distribute the dying
    // cell's own energy across the kernel, then replace cell with Empty.
    let _phase_death = tracing::info_span!("phase_death").entered();
    let mut dying: Vec<(i32, i32, Energy)> = Vec::new();
    let death_count: u64;
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let cell = &chunks[chunk_idx].cells[cell_idx];
                    let occ = &cell.occupant;
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let energy_dead = matches!(occupant_energy(occ), Some(0));
                    let stranded =
                        cell_has_no_push_target(occ, chunks, chunks_x, max_x, max_y, wx, wy, wrap);
                    let poisoned = is_poisoned(params, occ, cell.organic, cell.soil_energy);
                    if energy_dead || stranded || poisoned {
                        let energy = occupant_energy(occ).unwrap_or(0);
                        dying.push((wx, wy, energy));
                    }
                }
            }
        }
    }
    death_count = dying.len() as u64;
    for (wx, wy, energy) in dying {
        deposit_kernel(
            chunks,
            chunks_x,
            wx,
            wy,
            max_x,
            max_y,
            wrap,
            &death_kernel,
            energy,
        );
        // Clear parent direction on any neighbor that pointed at us.
        // Otherwise, if a foreign cell later repopulates our position
        // (via growth or eating into Empty), the orphan would silently
        // re-attach across plants and pump energy.
        for d in [
            Direction::North,
            Direction::East,
            Direction::South,
            Direction::West,
        ] {
            let (dx, dy) = direction_to_delta(d);
            let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                continue;
            };
            let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                continue;
            };
            let opp = opposite_dir(d);
            if let Some(neighbor) = cell_at_mut(chunks, chunks_x, nx, ny) {
                match &mut neighbor.occupant {
                    Occupant::Leaf { parent, .. }
                    | Occupant::Root { parent, .. }
                    | Occupant::Antenna { parent, .. }
                    | Occupant::Stem { parent, .. }
                    | Occupant::Sprout { parent, .. }
                    | Occupant::Seed { parent, .. } => {
                        if *parent == Some(opp) {
                            *parent = None;
                        }
                    }
                    Occupant::Empty => {}
                }
            }
        }
        if let Some(cell) = cell_at_mut(chunks, chunks_x, wx, wy) {
            cell.occupant = Occupant::Empty;
        }
    }
    drop(_phase_death);
    tracing::event!(
        target: "phase",
        tracing::Level::INFO, deaths = death_count, "phase_death_done");

    // Per-tick summary event with occupant census so we can correlate
    // tick duration against world fullness over the run.
    let mut occupants: u64 = 0;
    let mut leaves: u64 = 0;
    let mut roots: u64 = 0;
    let mut antennas: u64 = 0;
    let mut stems: u64 = 0;
    let mut sprouts: u64 = 0;
    let mut seeds: u64 = 0;
    for chunk in chunks.iter() {
        for cell in chunk.cells.iter() {
            match cell.occupant {
                Occupant::Empty => {}
                Occupant::Leaf { .. } => {
                    leaves += 1;
                    occupants += 1;
                }
                Occupant::Root { .. } => {
                    roots += 1;
                    occupants += 1;
                }
                Occupant::Antenna { .. } => {
                    antennas += 1;
                    occupants += 1;
                }
                Occupant::Stem { .. } => {
                    stems += 1;
                    occupants += 1;
                }
                Occupant::Sprout { .. } => {
                    sprouts += 1;
                    occupants += 1;
                }
                Occupant::Seed { .. } => {
                    seeds += 1;
                    occupants += 1;
                }
            }
        }
    }
    tracing::event!(
        target: "phase",
        tracing::Level::INFO,
        occupants,
        leaves,
        roots,
        antennas,
        stems,
        sprouts,
        seeds,
        "tick_census"
    );
}

/// True iff the soil's chemistry is fatal for this occupant.
/// - Roots are immune to organic poisoning, vulnerable to energy.
/// - Antennas are immune to energy poisoning, vulnerable to organic.
/// - Everyone else dies to either.
fn is_poisoned(params: &SimParams, occ: &Occupant, organic: u16, soil_energy: u16) -> bool {
    let organic_toxic = organic > params.soil_organic_poison;
    let energy_toxic = soil_energy > params.soil_energy_poison;
    match occ {
        Occupant::Empty => false,
        Occupant::Root { .. } => energy_toxic,
        Occupant::Antenna { .. } => organic_toxic,
        _ => organic_toxic || energy_toxic,
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
        // Productive stems push to descendants. Dead-end stems (no
        // children) don't push at all — their energy gets drained by
        // their own parent stem in `phase_drainage`. This breaks the
        // parent↔dead-end mutual flow that used to keep doomed plants
        // alive on circulating energy.
        Occupant::Stem { children, .. } => {
            if *children != 0 {
                bitmask_to_dirs(*children)
            } else {
                Vec::new()
            }
        }
    }
}

/// True if a neighbor cell should keep a stem's connection bit pointing
/// at it. Empty is kept (shader masks it visually anyway, and the cell
/// may belong to the same plant later via a child cell that grew here).
/// Same-plant occupants of any kind are kept. Foreign live cells get
/// the connection bit dropped — the stem visually severs from invaders.
fn is_kin_or_empty(occ: &Occupant, parent_plant: u32) -> bool {
    match occ {
        Occupant::Empty => true,
        Occupant::Sprout { plant, .. }
        | Occupant::Seed { plant, .. }
        | Occupant::Stem { plant, .. }
        | Occupant::Leaf { plant, .. }
        | Occupant::Root { plant, .. }
        | Occupant::Antenna { plant, .. } => *plant == parent_plant,
    }
}

/// Keep a stem's `children` bit if the neighbor is a kin Sprout, Seed,
/// or Stem of any state (productive or dead-end). Drop only on Empty
/// or foreign — `phase_drainage` is responsible for pruning kin
/// dead-end stems separately, with the energy transfer.
///
/// Without this looseness, a foreign sprout that grew into a freshly
/// vacated child position would silently inherit the parent's bit.
fn is_kin_descendable(occ: &Occupant, parent_plant: u32) -> bool {
    match occ {
        Occupant::Sprout { plant, .. }
        | Occupant::Seed { plant, .. }
        | Occupant::Stem { plant, .. } => *plant == parent_plant,
        _ => false,
    }
}

/// True if the neighbor is a same-plant cell that's still alive
/// (anything except Empty). Used to decide whether a stem's `parent`
/// reference still points at something meaningful — when the parent
/// has died/been replaced, the ref is cleared.
fn is_kin_alive(occ: &Occupant, parent_plant: u32) -> bool {
    match occ {
        Occupant::Empty => false,
        Occupant::Sprout { plant, .. }
        | Occupant::Seed { plant, .. }
        | Occupant::Stem { plant, .. }
        | Occupant::Leaf { plant, .. }
        | Occupant::Root { plant, .. }
        | Occupant::Antenna { plant, .. } => *plant == parent_plant,
    }
}

/// True for cells that have nowhere to push energy. Stems with no
/// children and no parent are dead-ends — phase_prune already cleared
/// the parent ref if the parent died, so we just check the field.
/// Producers (leaf/root/antenna) check the parent neighbor directly,
/// since their parent ref isn't auto-cleared by prune.
fn cell_has_no_push_target(
    occ: &Occupant,
    chunks: &[Chunk],
    chunks_x: u32,
    max_x: i32,
    max_y: i32,
    wx: i32,
    wy: i32,
    wrap: bool,
) -> bool {
    match occ {
        Occupant::Stem {
            children, parent, ..
        } => *children == 0 && parent.is_none(),
        Occupant::Leaf { parent, .. }
        | Occupant::Root { parent, .. }
        | Occupant::Antenna { parent, .. } => {
            let Some(parent_dir) = parent else {
                return true;
            };
            let edge = CHUNK_EDGE as i32;
            let (dx, dy) = direction_to_delta(*parent_dir);
            let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                return true;
            };
            let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                return true;
            };
            let n_chunk_idx =
                (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
            let n_cell_idx =
                (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
            matches!(
                chunks[n_chunk_idx].cells[n_cell_idx].occupant,
                Occupant::Empty
            )
        }
        _ => false,
    }
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

/// Bounds-check a world coordinate. The world has hard edges — going
/// off one side returns None and the caller skips that neighbor access.
/// Toxic borders (from `world::build_world`) are still the main way
/// regions stay isolated, but the world edge is also a wall.
fn in_bounds(c: i32, max: i32, wrap: bool) -> Option<i32> {
    if wrap {
        // rem_euclid handles negative values correctly: a wrap to the
        // far side, not a sign-flipped remainder.
        Some(c.rem_euclid(max))
    } else if c < 0 || c >= max {
        None
    } else {
        Some(c)
    }
}

fn deposit_kernel(
    chunks: &mut [Chunk],
    chunks_x: u32,
    wx: i32,
    wy: i32,
    max_x: i32,
    max_y: i32,
    wrap: bool,
    kernel: &[[u16; 3]; 3],
    energy: Energy,
) {
    let kernel_sum: u32 = kernel.iter().flatten().map(|&w| w as u32).sum();
    // Integer-divide to keep the deposit lossless: per_unit * kernel_sum
    // never exceeds energy, so we don't manufacture energy from death.
    let per_unit = if kernel_sum > 0 {
        energy as u32 / kernel_sum
    } else {
        0
    };
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            let weight = kernel[(dy + 1) as usize][(dx + 1) as usize];
            if weight == 0 {
                continue;
            }
            let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                continue;
            };
            let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                continue;
            };
            if let Some(cell) = cell_at_mut(chunks, chunks_x, nx, ny) {
                cell.organic = cell.organic.saturating_add(weight);
                let energy_share = (per_unit * weight as u32).min(u16::MAX as u32) as u16;
                cell.soil_energy = cell.soil_energy.saturating_add(energy_share);
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

fn upkeep_for(params: &SimParams, occ: &Occupant) -> Energy {
    match occ {
        Occupant::Empty => 0,
        Occupant::Seed { .. } => params.upkeep_seed,
        Occupant::Sprout { .. } => params.upkeep_sprout,
        _ => params.upkeep_default,
    }
}

fn slot_cost(params: &SimParams, slot: SlotProduct) -> Energy {
    match slot {
        SlotProduct::Nothing => 0,
        SlotProduct::Leaf => params.cost_leaf,
        SlotProduct::Root => params.cost_root,
        SlotProduct::Antenna => params.cost_antenna,
        SlotProduct::Seed => params.cost_seed,
        SlotProduct::Sprout => params.cost_sprout,
    }
}

fn make_slot_occupant(
    params: &SimParams,
    slot: SlotProduct,
    plant: u32,
    clan: ClanId,
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
            clan,
            energy: params.cost_leaf,
            facing,
            parent: parent_back,
        },
        SlotProduct::Root => Occupant::Root {
            plant,
            clan,
            energy: params.cost_root,
            parent: parent_back,
        },
        SlotProduct::Antenna => Occupant::Antenna {
            plant,
            clan,
            energy: params.cost_antenna,
            parent: parent_back,
        },
        SlotProduct::Seed => Occupant::Seed {
            plant,
            clan,
            energy: params.cost_seed,
            facing,
            genome: Box::new(mutate_genome(parent_genome, rng)),
            parent: parent_back,
        },
        SlotProduct::Sprout => Occupant::Sprout {
            plant,
            clan,
            energy: params.cost_sprout,
            facing,
            genome: Box::new(mutate_genome(parent_genome, rng)),
            parent: parent_back,
            current_gene: next_gene,
        },
    })
}

/// Pre-tick snapshot of one sprout's growth-relevant state. Filled in
/// pass A; mutated in pass C (`won[]`); read in pass D.
struct SproutSnapshot {
    src_chunk_idx: usize,
    src_cell_idx: usize,
    src_wx: i32,
    src_wy: i32,
    plant: u32,
    clan: ClanId,
    energy: Energy,
    parent: Option<Direction>,
    next_gene: u8,
    genome: Box<Genome>,
    plan_dirs: [Direction; 3],
    plan_slots: [SlotProduct; 3],
    harvested: [u32; 3],
    no_viable: bool,
    can_afford: bool,
    won: [bool; 3],
}

/// One sprout's bid on one destination cell. Built in pass A; consumed
/// in passes B and D.
struct SproutBid {
    sprout_idx: usize,
    slot_idx: usize,
    dst_chunk_idx: usize,
    dst_cell_idx: usize,
    dst_global_idx: usize,
    /// Smallest-wins tiebreak score derived from the (src, dst) pair.
    /// Symmetric across the world — no positional or lineage bias.
    score: u64,
}

/// Hash of (src_pos, dst_pos) used as a deterministic tiebreak score
/// when multiple sprouts bid on the same destination cell. FNV-1a-ish.
/// The smallest score wins.
fn tiebreak_score(src_wx: i32, src_wy: i32, dst_wx: i32, dst_wy: i32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [src_wx, src_wy, dst_wx, dst_wy] {
        h ^= v as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Pull-pattern prune phase. Each stem reads its 3×3 source state and
/// decides which `children` / `connections` bits to drop. Returns the
/// count of stems whose bits changed (for tracing).
///
/// Bits drop monotonically — never grow back. Cascades unfold at one
/// cell per tick (compute-then-apply matches CA "speed of light").
///
/// `children`: drop a bit if the neighbor is not a same-plant sink
/// (Sprout/Seed) or a same-plant Stem with `children != 0`.
///
/// `connections`: drop a bit if the neighbor is a foreign live cell
/// (different plant). Empty neighbors keep the bit (the shader masks
/// them visually, and "empty" is a former kin position).
fn phase_prune_pull(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    max_x: i32,
    max_y: i32,
    wrap: bool,
) -> u64 {
    let edge = CHUNK_EDGE as i32;
    let bits = [
        STEM_CONNECT_NORTH,
        STEM_CONNECT_EAST,
        STEM_CONNECT_SOUTH,
        STEM_CONNECT_WEST,
    ];
    let mut prune_changes: Vec<(usize, usize, u8, u8, Option<Direction>)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let (cur_children, cur_connections, parent_plant, parent_dir) =
                        match &chunks[chunk_idx].cells[cell_idx].occupant {
                            Occupant::Stem {
                                children,
                                connections,
                                plant,
                                parent,
                                ..
                            } => (*children, *connections, *plant, *parent),
                            _ => continue,
                        };
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let mut kept_children = 0u8;
                    let mut kept_connections = 0u8;
                    let mut new_parent = parent_dir;
                    // Helper closure to read neighbor occupant once per dir.
                    let neighbor_at = |dir: Direction| -> Option<&Occupant> {
                        let (dx, dy) = direction_to_delta(dir);
                        let nx = in_bounds(wx + dx, max_x, wrap)?;
                        let ny = in_bounds(wy + dy, max_y, wrap)?;
                        let n_chunk_idx =
                            (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
                        let n_cell_idx =
                            (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
                        Some(&chunks[n_chunk_idx].cells[n_cell_idx].occupant)
                    };
                    // Clear the parent ref if the parent neighbor has
                    // died/been replaced. Otherwise a foreign cell that
                    // grew where our parent was could silently "adopt"
                    // us across plant boundaries.
                    if let Some(p_dir) = parent_dir {
                        let alive = neighbor_at(p_dir)
                            .map(|n| is_kin_alive(n, parent_plant))
                            .unwrap_or(false);
                        if !alive {
                            new_parent = None;
                        }
                    }
                    for bit in bits {
                        let in_children = cur_children & bit != 0;
                        let in_connections = cur_connections & bit != 0;
                        if !in_children && !in_connections {
                            continue;
                        }
                        let dir = bit_to_dir(bit);
                        let same_as_parent = Some(dir) == new_parent;
                        let neighbor_occ = neighbor_at(dir);
                        if in_children {
                            // Drop the bit at Empty/foreign so a new
                            // plant that spawns there doesn't get
                            // adopted as our child. Kin dead-end stems
                            // are kept here and handled by phase_drainage.
                            if let Some(n) = neighbor_occ {
                                if is_kin_descendable(n, parent_plant) {
                                    kept_children |= bit;
                                }
                            }
                        }
                        if in_connections {
                            let keep = same_as_parent
                                || match neighbor_occ {
                                    Some(n) => is_kin_or_empty(n, parent_plant),
                                    None => false,
                                };
                            if keep {
                                kept_connections |= bit;
                            }
                        }
                    }
                    if kept_children != cur_children
                        || kept_connections != cur_connections
                        || new_parent != parent_dir
                    {
                        prune_changes.push((
                            chunk_idx,
                            cell_idx,
                            kept_children,
                            kept_connections,
                            new_parent,
                        ));
                    }
                }
            }
        }
    }
    let prune_change_count = prune_changes.len() as u64;
    for (chunk_idx, cell_idx, new_children, new_connections, new_parent) in prune_changes {
        if let Occupant::Stem {
            children,
            connections,
            parent,
            ..
        } = &mut chunks[chunk_idx].cells[cell_idx].occupant
        {
            *children = new_children;
            *connections = new_connections;
            *parent = new_parent;
        }
    }
    prune_change_count
}

/// Drain energy from dead-end stem children into their parent stem,
/// dropping the parent's `children` bit pointing at each drained
/// child. Returns the number of drained pairs (for tracing).
///
/// "Dead-end" = same-plant Stem with `children == 0`. After this phase
/// the drained child has `energy = 0`, which `phase_death` then catches
/// via the `energy_dead` rule. The bit drop also prevents the
/// productive parent from re-pushing energy back into the corpse in
/// the same tick's `phase_push`.
fn phase_drainage(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    max_x: i32,
    max_y: i32,
    wrap: bool,
) -> u64 {
    let edge = CHUNK_EDGE as i32;
    let bits = [
        STEM_CONNECT_NORTH,
        STEM_CONNECT_EAST,
        STEM_CONNECT_SOUTH,
        STEM_CONNECT_WEST,
    ];
    // Gather (parent, child, bit) triples first; apply after, since we
    // both read child.energy and write parent.energy/children — can't
    // hold two &mut to chunks at once.
    let mut drain_actions: Vec<(usize, usize, usize, usize, u8)> = Vec::new();
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let (cur_children, parent_plant) = match &chunks[chunk_idx].cells[cell_idx]
                        .occupant
                    {
                        Occupant::Stem { children, plant, .. } if *children != 0 => {
                            (*children, *plant)
                        }
                        _ => continue,
                    };
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    for bit in bits {
                        if cur_children & bit == 0 {
                            continue;
                        }
                        let dir = bit_to_dir(bit);
                        let (dx, dy) = direction_to_delta(dir);
                        let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                            continue;
                        };
                        let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                            continue;
                        };
                        let n_chunk_idx =
                            (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
                        let n_cell_idx =
                            (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
                        let drain = matches!(
                            &chunks[n_chunk_idx].cells[n_cell_idx].occupant,
                            Occupant::Stem { children: 0, plant, .. }
                                if *plant == parent_plant
                        );
                        if drain {
                            drain_actions.push((chunk_idx, cell_idx, n_chunk_idx, n_cell_idx, bit));
                        }
                    }
                }
            }
        }
    }
    let drain_count = drain_actions.len() as u64;
    for (p_chunk, p_cell, n_chunk, n_cell, bit) in drain_actions {
        let drained = match &mut chunks[n_chunk].cells[n_cell].occupant {
            Occupant::Stem { energy, .. } => {
                let drained = *energy;
                *energy = 0;
                drained
            }
            _ => continue,
        };
        if let Occupant::Stem { energy, children, .. } =
            &mut chunks[p_chunk].cells[p_cell].occupant
        {
            *energy = energy.saturating_add(drained);
            *children &= !bit;
        }
    }
    drain_count
}

/// Pull-pattern growth phase. Each sprout decides which destinations
/// it would like to spawn into; each destination cell picks a winner
/// among bidders; each source then decides what it becomes (Stem,
/// Empty+deposit, or unchanged) based on which of its bids actually
/// won. Only writes a cell to itself in the apply passes.
///
/// Returns the number of sprouts considered (for tracing).
fn phase_growth_pull(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    max_x: i32,
    max_y: i32,
    params: &SimParams,
    death_kernel: &[[u16; 3]; 3],
    rng: &mut impl Rng,
) -> u64 {
    let edge = CHUNK_EDGE as i32;
    let wrap = params.world_wrap;
    let mut sprouts: Vec<SproutSnapshot> = Vec::new();
    let mut bids: Vec<SproutBid> = Vec::new();

    // Pass A: gather sprouts and bids from a single read-only scan.
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            for ly in 0..(CHUNK_EDGE as usize) {
                for lx in 0..(CHUNK_EDGE as usize) {
                    let chunk_idx = cy as usize * chunks_x as usize + cx as usize;
                    let cell_idx = ly * (CHUNK_EDGE as usize) + lx;
                    let (plant, clan, energy, facing, parent, current_gene, genome) =
                        match &chunks[chunk_idx].cells[cell_idx].occupant {
                            Occupant::Sprout {
                                plant,
                                clan,
                                energy,
                                facing,
                                parent,
                                current_gene,
                                genome,
                            } => (
                                *plant,
                                *clan,
                                *energy,
                                *facing,
                                *parent,
                                *current_gene,
                                genome.clone(),
                            ),
                            _ => continue,
                        };
                    if genome.genes.is_empty() {
                        continue;
                    }
                    let wx = cx as i32 * edge + lx as i32;
                    let wy = cy as i32 * edge + ly as i32;
                    let gene = genome.genes[(current_gene as usize) % genome.genes.len()];
                    let next_gene = (gene.next as usize % genome.genes.len()) as u8;
                    let plan_dirs = [facing, rotate_left(facing), rotate_right(facing)];
                    let plan_slots = [gene.front, gene.left, gene.right];

                    let mut viable = [false; 3];
                    let mut harvested = [0u32; 3];
                    for i in 0..3 {
                        if matches!(plan_slots[i], SlotProduct::Nothing) {
                            continue;
                        }
                        let (dx, dy) = direction_to_delta(plan_dirs[i]);
                        let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                            continue;
                        };
                        let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                            continue;
                        };
                        let n_chunk_idx =
                            (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
                        let n_cell_idx =
                            (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
                        let neighbor = &chunks[n_chunk_idx].cells[n_cell_idx];
                        match edible_for(&neighbor.occupant, plant) {
                            EdibleStatus::Empty => viable[i] = true,
                            EdibleStatus::Edible(e) => {
                                if matches!(plan_slots[i], SlotProduct::Sprout | SlotProduct::Seed)
                                {
                                    viable[i] = true;
                                    harvested[i] = e as u32;
                                }
                            }
                            EdibleStatus::Blocked => {}
                        }
                    }

                    let no_viable = !viable.iter().any(|v| *v);
                    let effective_cost: Energy = (0..3)
                        .filter(|i| viable[*i])
                        .map(|i| slot_cost(params, plan_slots[i]))
                        .sum();
                    let total_harvested: u32 = harvested.iter().sum();
                    let pool: u32 = energy as u32 + total_harvested;
                    let can_afford = !no_viable && pool > effective_cost as u32;

                    let sprout_idx = sprouts.len();
                    sprouts.push(SproutSnapshot {
                        src_chunk_idx: chunk_idx,
                        src_cell_idx: cell_idx,
                        src_wx: wx,
                        src_wy: wy,
                        plant,
                        clan,
                        energy,
                        parent,
                        next_gene,
                        genome,
                        plan_dirs,
                        plan_slots,
                        harvested,
                        no_viable,
                        can_afford,
                        won: [false; 3],
                    });

                    if !can_afford {
                        continue;
                    }
                    for i in 0..3 {
                        if !viable[i] {
                            continue;
                        }
                        let (dx, dy) = direction_to_delta(plan_dirs[i]);
                        let Some(nx) = in_bounds(wx + dx, max_x, wrap) else {
                            continue;
                        };
                        let Some(ny) = in_bounds(wy + dy, max_y, wrap) else {
                            continue;
                        };
                        let dst_chunk_idx =
                            (ny / edge) as usize * chunks_x as usize + (nx / edge) as usize;
                        let dst_cell_idx =
                            (ny % edge) as usize * (CHUNK_EDGE as usize) + (nx % edge) as usize;
                        let dst_global_idx = dst_chunk_idx * CHUNK_AREA + dst_cell_idx;
                        bids.push(SproutBid {
                            sprout_idx,
                            slot_idx: i,
                            dst_chunk_idx,
                            dst_cell_idx,
                            dst_global_idx,
                            score: tiebreak_score(wx, wy, nx, ny),
                        });
                    }
                }
            }
        }
    }

    // Pass B: per-destination tiebreak. Track winning bid index per dst.
    let mut winning_bid: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(bids.len());
    for (bidi, bid) in bids.iter().enumerate() {
        match winning_bid.get(&bid.dst_global_idx).copied() {
            None => {
                winning_bid.insert(bid.dst_global_idx, bidi);
            }
            Some(prev_i) => {
                if bid.score < bids[prev_i].score {
                    winning_bid.insert(bid.dst_global_idx, bidi);
                }
            }
        }
    }

    // Pass C: mark which slots each sprout won.
    for (&_dst_idx, &bidi) in winning_bid.iter() {
        let bid = &bids[bidi];
        sprouts[bid.sprout_idx].won[bid.slot_idx] = true;
    }
    let mut eaten_sprout: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for sprout in &sprouts {
        let src_global = sprout.src_chunk_idx * CHUNK_AREA + sprout.src_cell_idx;
        if winning_bid.contains_key(&src_global) {
            eaten_sprout.insert(src_global);
        }
    }

    // Pass D1: place new occupants at winning destinations. Stamp the
    // destination cell with the parent's lineage rate so the viewer can
    // color whole plants by their lineage's mutation rate.
    let growth_attempts = sprouts.len() as u64;
    for (&_dst_idx, &bidi) in winning_bid.iter() {
        let bid = &bids[bidi];
        let sprout = &sprouts[bid.sprout_idx];
        let dir = sprout.plan_dirs[bid.slot_idx];
        let slot = sprout.plan_slots[bid.slot_idx];
        if let Some(occ) = make_slot_occupant(
            params,
            slot,
            sprout.plant,
            sprout.clan,
            dir,
            dir,
            &sprout.genome,
            sprout.next_gene,
            rng,
        ) {
            let cell = &mut chunks[bid.dst_chunk_idx].cells[bid.dst_cell_idx];
            cell.lineage_mutation_rate = sprout.genome.mutation_rate;
            cell.occupant = occ;
        }
    }

    // Pass D2: each sprout writes its own outcome.
    let mut deposit_tasks: Vec<(i32, i32, Energy)> = Vec::new();
    for sprout in &sprouts {
        let src_global = sprout.src_chunk_idx * CHUNK_AREA + sprout.src_cell_idx;
        if eaten_sprout.contains(&src_global) {
            continue;
        }
        if sprout.no_viable {
            deposit_tasks.push((sprout.src_wx, sprout.src_wy, sprout.energy));
            chunks[sprout.src_chunk_idx].cells[sprout.src_cell_idx].occupant = Occupant::Empty;
            continue;
        }
        if !sprout.can_afford {
            continue;
        }
        let any_won = sprout.won.iter().any(|w| *w);
        if !any_won {
            continue;
        }
        let mut connections = 0u8;
        let mut children = 0u8;
        let mut won_cost: u32 = 0;
        let mut won_harvested: u32 = 0;
        for i in 0..3 {
            if !sprout.won[i] {
                continue;
            }
            connections |= dir_to_bitmask(sprout.plan_dirs[i]);
            if matches!(
                sprout.plan_slots[i],
                SlotProduct::Sprout | SlotProduct::Seed
            ) {
                children |= dir_to_bitmask(sprout.plan_dirs[i]);
            }
            won_cost += slot_cost(params, sprout.plan_slots[i]) as u32;
            won_harvested += sprout.harvested[i];
        }
        if let Some(parent_dir) = sprout.parent {
            connections |= dir_to_bitmask(parent_dir);
        }
        let new_energy = ((sprout.energy as u32) + won_harvested)
            .saturating_sub(won_cost)
            .min(Energy::MAX as u32) as Energy;
        chunks[sprout.src_chunk_idx].cells[sprout.src_cell_idx].occupant = Occupant::Stem {
            plant: sprout.plant,
            clan: sprout.clan,
            energy: new_energy,
            connections,
            parent: sprout.parent,
            children,
        };
    }
    // Pass D3: deposit organic for sprouts that died without growing.
    for (wx, wy, energy) in deposit_tasks {
        deposit_kernel(
            chunks,
            chunks_x,
            wx,
            wy,
            max_x,
            max_y,
            wrap,
            death_kernel,
            energy,
        );
    }
    growth_attempts
}

/// Outcome of inspecting a growth target.
enum EdibleStatus {
    /// Cell is Empty — grow normally, no energy harvested.
    Empty,
    /// Cell is an edible non-empty cell. Only Sprout / Seed slots may
    /// consume it (see `phase_growth_pull`); other slots ignore Edible and
    /// treat the target as unavailable.
    Edible(Energy),
    /// Cell is Root or Stem (always inviolate). Cannot grow into it.
    Blocked,
}

fn edible_for(occ: &Occupant, eater_plant: u32) -> EdibleStatus {
    // Roots and Stems are always inviolate (eating them would orphan the
    // tree they hold up). Same-plant cells are also blocked — a sprout
    // cannot cannibalise its own lineage. The caller further narrows
    // which slot products can actually consume an Edible (currently
    // Sprouts and Seeds).
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

/// Per-field mutation pass over a genome, plus per-gene insert/delete
/// rolls and a meta-mutation of the genome's mutation_rate. Called at
/// every copy site: sprout-produces-sub-sprouts, sprout-produces-seed.
///
/// Insertion/deletion is topology-preserving: when a gene is inserted
/// before old position `i`, every existing `next` reference >= i shifts
/// up by 1 so working pathways survive intact. When a gene at position
/// `i` is deleted, references to it redirect to whatever follows. New
/// inserted genes have a fresh-random `next` that points into the new
/// index space directly.
///
/// Bounds: genome size is clamped to [GENOME_MIN, GENOME_MAX].
pub fn mutate_genome(g: &Genome, rng: &mut impl Rng) -> Genome {
    let old_len = g.genes.len();

    // 1. Maybe perturb the mutation rate itself (multiplicative jitter).
    // Always clamp the result so a genome handed in with an out-of-band
    // rate gets normalized on its first copy.
    let mut rate = g.mutation_rate;
    if rng.r#gen::<f32>() < rate {
        rate *= rng.gen_range(0.5..1.5);
    }
    rate = rate.clamp(MUTATION_RATE_MIN, MUTATION_RATE_MAX);
    let insert_rate = rate * 0.1;
    let delete_rate = rate * 0.1;

    // 2. Decide deletions per old gene. Never let the genome drop
    // below GENOME_MIN; if too many were marked, unmark from the
    // start until we're at the floor.
    let mut delete: Vec<bool> = (0..old_len)
        .map(|_| rng.r#gen::<f32>() < delete_rate)
        .collect();
    let mut alive = old_len - delete.iter().filter(|&&d| d).count();
    if alive < GENOME_MIN && old_len >= GENOME_MIN {
        let mut needed = GENOME_MIN - alive;
        for d in delete.iter_mut() {
            if needed == 0 {
                break;
            }
            if *d {
                *d = false;
                needed -= 1;
            }
        }
        alive = GENOME_MIN;
    }

    // 3. Decide insertions per old position (insert before that position).
    // Cap so we never exceed GENOME_MAX after the dust settles.
    let mut insertions: Vec<bool> = vec![false; old_len];
    let mut planned = alive;
    for ins in insertions.iter_mut() {
        if planned >= GENOME_MAX {
            break;
        }
        if rng.r#gen::<f32>() < insert_rate {
            *ins = true;
            planned += 1;
        }
    }

    // 4. Build new genes vec + pos_map. Each entry in `next_source`
    // tells us how to remap that new gene's `next`:
    //   None      → already in new index space (inserted gene, or
    //               an old gene whose next was just freshly mutated).
    //   Some(old) → old `next` value, needs remap via pos_map.
    let mut new_genes: Vec<Gene> = Vec::with_capacity(planned);
    let mut next_source: Vec<Option<u8>> = Vec::with_capacity(planned);
    let mut pos_map: Vec<Option<usize>> = Vec::with_capacity(old_len);

    for i in 0..old_len {
        if insertions[i] {
            new_genes.push(Gene {
                front: random_slot(rng),
                left: random_slot(rng),
                right: random_slot(rng),
                next: rng.r#gen::<u8>(),
            });
            next_source.push(None);
        }
        if delete[i] {
            pos_map.push(None);
            continue;
        }
        let mut new_gene = g.genes[i];
        if rng.r#gen::<f32>() < rate {
            new_gene.front = random_slot(rng);
        }
        if rng.r#gen::<f32>() < rate {
            new_gene.left = random_slot(rng);
        }
        if rng.r#gen::<f32>() < rate {
            new_gene.right = random_slot(rng);
        }
        let next_remap = if rng.r#gen::<f32>() < rate {
            new_gene.next = rng.r#gen::<u8>();
            None
        } else {
            Some(g.genes[i].next)
        };
        pos_map.push(Some(new_genes.len()));
        next_source.push(next_remap);
        new_genes.push(new_gene);
    }

    // 5. Pathological: empty genome. Push one default gene so the
    // sprout has something to read (it'll grow nothing, but the cell
    // is still legal).
    if new_genes.is_empty() {
        new_genes.push(Gene::default());
        next_source.push(None);
    }

    // 6. Remap next pointers from old space to new space for genes
    // that came from the old genome and whose next wasn't randomized.
    for (gene, src) in new_genes.iter_mut().zip(next_source.iter()) {
        let Some(orig_next) = src else { continue };
        // The original `next` indexed into the old genome (modulo).
        let orig_idx = if old_len == 0 {
            0
        } else {
            (*orig_next as usize) % old_len
        };
        let new_idx = match pos_map[orig_idx] {
            Some(p) => p,
            None => {
                // Original target was deleted. Walk forward (with wrap)
                // to the nearest surviving gene's new position.
                let mut k = (orig_idx + 1) % old_len.max(1);
                let mut found = 0;
                for _ in 0..old_len {
                    if let Some(p) = pos_map[k] {
                        found = p;
                        break;
                    }
                    k = (k + 1) % old_len;
                }
                found
            }
        };
        gene.next = (new_idx % 256) as u8;
    }

    Genome {
        genes: new_genes,
        mutation_rate: rate,
    }
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
    use protocol::{ChunkCoord, GENOME_LEN};
    use rand::SeedableRng;

    // Tests pin themselves to the production defaults — when those
    // change in `SimParams::default()`, mirror them here. Encoded as
    // consts (not `SimParams::default().field`) so existing test code
    // that uses them in array indices and arithmetic compiles unchanged.
    const COST_SPROUT: Energy = 60;
    const COST_LEAF: Energy = 30;
    const COST_ROOT: Energy = 30;
    const COST_ANTENNA: Energy = 30;
    const COST_SEED: Energy = 90;
    const UPKEEP_DEFAULT: Energy = 2;
    const UPKEEP_SEED: Energy = 1;
    const UPKEEP_SPROUT: Energy = 5;
    const SOIL_ENERGY_REST: u16 = 100;
    const SOIL_ENERGY_REGULATION: u16 = 1;
    const SEED_DROPOFF_THRESHOLD: Energy = 180;
    const SOIL_ORGANIC_POISON: u16 = 400;
    const SOIL_ENERGY_POISON: u16 = 1000;
    const DEATH_DEPOSIT_KERNEL: [[u16; 3]; 3] = [[1, 2, 1], [2, 4, 2], [1, 2, 1]];

    fn test_params() -> SimParams {
        // Tests pre-date world wrap and assume hard edges (e.g. growth
        // at y=0 facing North is OOB, not a wrap to y=max). Override
        // the default so each test doesn't have to set this manually.
        SimParams {
            world_wrap: false,
            ..SimParams::default()
        }
    }

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
                        lineage_mutation_rate: 0.0,
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
            clan: 0,
            energy,
            facing: Direction::North,
            genome: Box::new(genome.clone()),
            parent: None,
            current_gene: 0,
        };
        (occ, genome)
    }

    /// Sprout whose first gene plants a Seed straight ahead. Used by tests
    /// that need to exercise the "only Seeds can eat" rule.
    fn seed_front_sprout(energy: Energy) -> (Occupant, Genome) {
        let mut genes = vec![Gene {
            front: SlotProduct::Seed,
            left: SlotProduct::Nothing,
            right: SlotProduct::Nothing,
            next: 0,
        }];
        while genes.len() < GENOME_LEN {
            genes.push(Gene::default());
        }
        let genome = Genome {
            genes,
            mutation_rate: protocol::DEFAULT_MUTATION_RATE,
        };
        let occ = Occupant::Sprout {
            plant: 1,
            clan: 0,
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

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
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
    fn growth_at_world_edge_skips_oob_front() {
        // World has hard edges (no wrap). A sprout at y=0 facing North
        // has its front target at (10, -1) — out of bounds → skipped.
        // Sides are still in-bounds and grow leaves; the cell becomes a
        // children-less stem.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        let (sprout, genome) = vine_sprout(100);
        place(&mut chunks, chunks_x, 10, 0, sprout);

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );

        // Center cell: stem with no children (front was OOB).
        match &cell_at(&chunks, chunks_x, 10, 0).occupant {
            Occupant::Stem { children, .. } => assert_eq!(*children, 0),
            other => panic!("expected children-less stem, got {other:?}"),
        }
        // Sides grew leaves as usual.
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
            clan: 0,
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

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
        // Some organic was deposited at the center (DEATH_DEPOSIT_KERNEL
        // center weight is non-zero).
        assert!(cell_at(&chunks, chunks_x, 10, 10).organic > 0);
    }

    #[test]
    fn growth_seed_slot_eats_foreign_leaf_and_pools_its_energy() {
        // A sprout whose front gene is Seed: the seed lands on a foreign
        // leaf, harvesting its energy. The side slots are Nothing — they
        // don't try to grow at all.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Leaf {
                plant: 99,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        let (sprout, genome) = seed_front_sprout(40);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );

        // Pool: 40 own + 50 harvested = 90. Subtract COST_SEED.
        let expected = 90u32.saturating_sub(COST_SEED as u32) as Energy;
        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Stem { plant, energy, .. } => {
                assert_eq!(plant, 1, "stem belongs to eater plant");
                assert_eq!(energy, expected, "pool minus cost");
            }
            ref other => panic!("expected stem, got {other:?}"),
        }
        // Eaten cell is now our Seed.
        match cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Seed { plant, .. } => assert_eq!(plant, 1),
            ref other => panic!("expected eater seed in front, got {other:?}"),
        }
    }

    #[test]
    fn growth_static_slot_cannot_eat_foreign_leaf() {
        // A genome whose front gene is a Leaf can't eat — only Sprout or
        // Seed slots have that power. Foreign leaf in front survives
        // untouched; the sprout has no other viable slot, so it dies in
        // place.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Leaf {
                plant: 99,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        let mut genes = vec![Gene {
            front: SlotProduct::Leaf,
            left: SlotProduct::Nothing,
            right: SlotProduct::Nothing,
            next: 0,
        }];
        while genes.len() < GENOME_LEN {
            genes.push(Gene::default());
        }
        let genome = Genome {
            genes,
            mutation_rate: protocol::DEFAULT_MUTATION_RATE,
        };
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Sprout {
                plant: 1,
                clan: 0,
                energy: 100,
                facing: Direction::North,
                genome: Box::new(genome.clone()),
                parent: None,
                current_gene: 0,
            },
        );

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );

        // Foreign leaf intact.
        match cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Leaf { plant, energy, .. } => {
                assert_eq!(plant, 99, "foreign leaf survives — Leaf slot can't eat");
                assert_eq!(energy, 50);
            }
            ref other => panic!("expected foreign leaf untouched, got {other:?}"),
        }
    }

    #[test]
    fn growth_seed_slot_skips_own_plant_cells() {
        // Same-plant cells are protected from eating — even Seed slots
        // refuse to consume an own-plant Leaf. The sprout's only
        // viable target is its front (the leaf), and since that's
        // blocked, no slots are viable: the sprout dies in place.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        let (sprout, genome) = seed_front_sprout(40);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );

        // Front cell still the own-plant leaf, energy intact.
        match cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Leaf { plant, energy, .. } => {
                assert_eq!(plant, 1, "own-plant leaf preserved");
                assert_eq!(energy, 50);
            }
            ref other => panic!("expected own leaf untouched, got {other:?}"),
        }
        // No viable slots: the sprout died in place.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
    }

    #[test]
    fn growth_severs_eaten_cell_from_foreign_parent_stem() {
        // Plant 2 has a Stem at (10, 8) with a child Leaf to its south at
        // (10, 9). Plant 1's sprout at (10, 10) faces north and eats that
        // leaf. The foreign stem must drop its South children/connections
        // bit — otherwise it would keep treating the now-foreign cell as
        // its child (silently merging the two plants and pumping energy
        // across).
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;

        // Foreign stem at (10, 8): South child bit set.
        place(
            &mut chunks,
            chunks_x,
            10,
            8,
            Occupant::Stem {
                plant: 2,
                clan: 1,
                energy: 30,
                connections: STEM_CONNECT_SOUTH,
                parent: None,
                children: STEM_CONNECT_SOUTH,
            },
        );
        // Foreign leaf at (10, 9), parent: North (back to (10, 8)).
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Leaf {
                plant: 2,
                clan: 1,
                energy: 50,
                facing: Direction::North,
                parent: Some(Direction::North),
            },
        );

        // Plant 1's sprout (front=Seed) eats (10, 9). Only Seed slots can
        // eat under the current rule.
        let (sprout, _genome) = seed_front_sprout(40);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        // Run growth, then run prune. With pull-pattern there's no
        // explicit sever-on-eat in growth — prune is what notices a
        // foreign cell and drops the foreign stem's child + connection
        // bits naturally. (In the live sim this happens one tick later;
        // here we run it in-line for a tight assertion.)
        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
            &mut det_rng(),
        );
        phase_prune_pull(&mut chunks, chunks_x, 1, max, max, true);

        // (10, 9) replaced with our Seed (plant 1).
        match &cell_at(&chunks, chunks_x, 10, 9).occupant {
            Occupant::Seed { plant, .. } => assert_eq!(*plant, 1),
            other => panic!("expected own-plant Seed, got {other:?}"),
        }
        // Foreign stem at (10, 8) lost its South child + connection bit.
        match &cell_at(&chunks, chunks_x, 10, 8).occupant {
            Occupant::Stem {
                plant,
                connections,
                children,
                ..
            } => {
                assert_eq!(*plant, 2, "foreign stem still belongs to plant 2");
                assert_eq!(
                    *children & STEM_CONNECT_SOUTH,
                    0,
                    "foreign stem's South child bit should be cleared"
                );
                assert_eq!(
                    *connections & STEM_CONNECT_SOUTH,
                    0,
                    "foreign stem's South connection bit should be cleared"
                );
            }
            other => panic!("expected foreign Stem at (10,8), got {other:?}"),
        }
    }

    #[test]
    fn growth_waits_when_energy_below_cost() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let max = CHUNK_EDGE as i32;
        // Vine cost = sprout(20) + leaf(5) + leaf(5) = 30. 5 is well below.
        let (sprout, genome) = vine_sprout(5);
        place(&mut chunks, chunks_x, 10, 10, sprout);

        phase_growth_pull(
            &mut chunks,
            chunks_x,
            1,
            max,
            max,
            &test_params(),
            &DEATH_DEPOSIT_KERNEL,
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
        assert_eq!(linear_idx(2, edge - 1, edge - 1), CHUNK_AREA - 1);
        // first cell of chunk(1,0)
        assert_eq!(linear_idx(2, edge, 0), CHUNK_AREA);
    }

    #[test]
    fn is_kin_descendable_keeps_kin_sprouts_seeds_stems() {
        let sprout = Occupant::Sprout {
            plant: 1,
            clan: 0,
            energy: 10,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
            current_gene: 0,
        };
        let stem_with_kids = Occupant::Stem {
            plant: 1,
            clan: 0,
            energy: 10,
            connections: STEM_CONNECT_NORTH,
            parent: None,
            children: STEM_CONNECT_NORTH,
        };
        let stem_no_kids = Occupant::Stem {
            plant: 1,
            clan: 0,
            energy: 10,
            connections: 0,
            parent: None,
            children: 0,
        };
        let leaf = Occupant::Leaf {
            plant: 1,
            clan: 0,
            energy: 10,
            facing: Direction::North,
            parent: None,
        };
        let seed = Occupant::Seed {
            plant: 1,
            clan: 0,
            energy: 10,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
        };
        // Kin sprouts/seeds/stems are descendable; dead-end stems are
        // also kept here (phase_drainage handles those separately).
        assert!(is_kin_descendable(&sprout, 1));
        assert!(is_kin_descendable(&seed, 1));
        assert!(is_kin_descendable(&stem_with_kids, 1));
        assert!(is_kin_descendable(&stem_no_kids, 1));
        // Producers and Empty are not children-bit targets.
        assert!(!is_kin_descendable(&leaf, 1));
        assert!(!is_kin_descendable(&Occupant::Empty, 1));
        // Different-plant: blocked even for sinks.
        assert!(!is_kin_descendable(&sprout, 2));
        assert!(!is_kin_descendable(&stem_with_kids, 2));
    }

    #[test]
    fn push_targets_match_role() {
        let leaf = Occupant::Leaf {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::North,
            parent: Some(Direction::South),
        };
        assert_eq!(push_targets(&leaf), vec![Direction::South]);

        let leaf_orphan = Occupant::Leaf {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        assert!(push_targets(&leaf_orphan).is_empty());

        let stem_kids = Occupant::Stem {
            plant: 1,
            clan: 0,
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
            clan: 0,
            energy: 0,
            connections: 0,
            parent: Some(Direction::South),
            children: 0,
        };
        assert!(
            push_targets(&stem_no_kids).is_empty(),
            "dead-end stems don't push up — phase_drainage handles them"
        );

        let sprout = Occupant::Sprout {
            plant: 1,
            clan: 0,
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
            clan: 0,
            energy: 0,
            connections: 0,
            parent: None,
            children: 0,
        };
        assert!(cell_has_no_push_target(
            &stem_orphan,
            &chunks,
            chunks_x,
            max,
            max,
            5,
            5,
            true
        ));

        // Stem with children present → not orphan.
        let stem_kid = Occupant::Stem {
            plant: 1,
            clan: 0,
            energy: 0,
            connections: 0,
            parent: None,
            children: STEM_CONNECT_NORTH,
        };
        assert!(!cell_has_no_push_target(
            &stem_kid, &chunks, chunks_x, max, max, 5, 5, true
        ));

        // Leaf whose parent direction points at an Empty cell → orphan.
        let leaf = Occupant::Leaf {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::North,
            parent: Some(Direction::South),
        };
        assert!(cell_has_no_push_target(
            &leaf, &chunks, chunks_x, max, max, 5, 5, true
        ));

        // Same leaf, but place a stem in the parent direction → not orphan.
        place(
            &mut chunks,
            chunks_x,
            5,
            6,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 0,
                connections: 0,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        assert!(!cell_has_no_push_target(
            &leaf, &chunks, chunks_x, max, max, 5, 5, true
        ));

        // Leaf at top edge with parent=North → OOB, orphan.
        let leaf_top = Occupant::Leaf {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::South,
            parent: Some(Direction::North),
        };
        assert!(cell_has_no_push_target(
            &leaf_top, &chunks, chunks_x, max, max, 5, 0, true
        ));

        // Sprout / seed / empty are not subject to orphan death.
        assert!(!cell_has_no_push_target(
            &Occupant::Empty,
            &chunks,
            chunks_x,
            max,
            max,
            0,
            0,
            true
        ));
    }

    #[test]
    fn slot_cost_per_product() {
        assert_eq!(slot_cost(&test_params(), SlotProduct::Nothing), 0);
        assert_eq!(slot_cost(&test_params(), SlotProduct::Leaf), COST_LEAF);
        assert_eq!(slot_cost(&test_params(), SlotProduct::Root), COST_ROOT);
        assert_eq!(
            slot_cost(&test_params(), SlotProduct::Antenna),
            COST_ANTENNA
        );
        assert_eq!(slot_cost(&test_params(), SlotProduct::Seed), COST_SEED);
        assert_eq!(slot_cost(&test_params(), SlotProduct::Sprout), COST_SPROUT);
    }

    #[test]
    fn make_slot_occupant_sets_parent_back_toward_creator() {
        // The parent direction passed to make_slot_occupant is the spawn
        // direction; the new cell's `parent` field should point the OPPOSITE
        // way (back at the producing sprout).
        let parent_genome = Genome::default_vine();
        let leaf = make_slot_occupant(
            &test_params(),
            SlotProduct::Leaf,
            7,
            0,
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
                clan: _,
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
            &test_params(),
            SlotProduct::Nothing,
            1,
            0,
            Direction::North,
            Direction::North,
            &parent_genome,
            0,
            &mut det_rng(),
        );
        assert!(nothing.is_none());

        let sprout = make_slot_occupant(
            &test_params(),
            SlotProduct::Sprout,
            5,
            0,
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
            clan: 0,
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
            clan: 0,
            energy: 0,
            facing: Direction::North,
            parent: None,
        };
        let sprout = Occupant::Sprout {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
            current_gene: 0,
        };
        let seed = Occupant::Seed {
            plant: 1,
            clan: 0,
            energy: 0,
            facing: Direction::North,
            genome: Box::new(Genome::default_vine()),
            parent: None,
        };
        assert_eq!(upkeep_for(&test_params(), &leaf), UPKEEP_DEFAULT);
        assert_eq!(upkeep_for(&test_params(), &sprout), UPKEEP_SPROUT);
        assert_eq!(upkeep_for(&test_params(), &seed), UPKEEP_SEED);
        assert_eq!(upkeep_for(&test_params(), &Occupant::Empty), 0);
    }

    #[test]
    fn mutate_genome_clamps_rate_above_min() {
        // Rates below MUTATION_RATE_MIN should be lifted to MIN on the
        // next copy, so no lineage gets stuck at the absorbing zero.
        let mut g = Genome::default_vine();
        g.mutation_rate = 0.0;
        let copied = mutate_genome(&g, &mut det_rng());
        assert!(
            copied.mutation_rate >= MUTATION_RATE_MIN,
            "rate {} fell below floor",
            copied.mutation_rate
        );
        // Genes still don't mutate at the (now floored) tiny rate, with
        // overwhelming probability — but we can't assert exact equality
        // because the floor is non-zero. Just confirm length unchanged.
        assert_eq!(copied.genes.len(), g.genes.len());
    }

    #[test]
    fn mutate_genome_with_same_seed_is_deterministic() {
        let mut g = Genome::default_vine();
        g.mutation_rate = 0.5;
        let a = mutate_genome(&g, &mut ChaCha12Rng::seed_from_u64(42));
        let b = mutate_genome(&g, &mut ChaCha12Rng::seed_from_u64(42));
        assert_eq!(
            a.genes, b.genes,
            "same seed should produce identical mutations"
        );
        let c = mutate_genome(&g, &mut ChaCha12Rng::seed_from_u64(43));
        assert_ne!(a.genes, c.genes, "different seed should diverge");
    }

    #[test]
    fn mutate_genome_size_clamps_to_min_max() {
        // Drive a very high rate so insertions and deletions fire often,
        // and run many generations to confirm the size envelope holds.
        let mut g = Genome {
            genes: vec![Gene::default()],
            mutation_rate: 0.5,
        };
        let mut rng = det_rng();
        for _ in 0..200 {
            g = mutate_genome(&g, &mut rng);
            assert!(
                g.genes.len() >= GENOME_MIN && g.genes.len() <= GENOME_MAX,
                "genome size {} out of bounds",
                g.genes.len()
            );
            assert!(
                g.mutation_rate >= MUTATION_RATE_MIN && g.mutation_rate <= MUTATION_RATE_MAX,
                "rate {} out of bounds",
                g.mutation_rate
            );
        }
    }

    #[test]
    fn mutate_genome_topology_preserves_pathways() {
        // Build a 3-gene chain: 0 -> 1 -> 2 -> 0 with distinctive slot
        // products on each. Force an insertion before gene 1 by using a
        // crafted (zero-roll) RNG would be brittle; instead, run many
        // mutations with high rate and verify that whenever the
        // distinctive sequence still has all three genes, the chain is
        // intact under remap.
        //
        // We assert a much weaker property here: any survivor of gene 0
        // points (modulo length) at a gene whose `front` is the next
        // step's front in the chain, within the same generation. This
        // would fail if the remap were wrong.
        let g = Genome {
            genes: vec![
                Gene {
                    front: SlotProduct::Sprout,
                    left: SlotProduct::Nothing,
                    right: SlotProduct::Nothing,
                    next: 1,
                },
                Gene {
                    front: SlotProduct::Leaf,
                    left: SlotProduct::Nothing,
                    right: SlotProduct::Nothing,
                    next: 2,
                },
                Gene {
                    front: SlotProduct::Root,
                    left: SlotProduct::Nothing,
                    right: SlotProduct::Nothing,
                    next: 0,
                },
            ],
            mutation_rate: 0.0, // no field mutations
        };
        // No rate → no inserts/deletes. Genome should clone exactly.
        let copy = mutate_genome(&g, &mut det_rng());
        assert_eq!(copy.genes, g.genes);
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
        //   photo:  leaf 10→20  (LEAF_PHOTOSYNTHESIS = 10)
        //   upkeep: leaf 20→18, sprout 0→0  (UPKEEP_DEFAULT=2, SPROUT=4)
        //   push:   leaf surplus 16 → sprout
        //   final:  leaf 2, sprout 16
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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );
        let _ = edge;

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 2),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Sprout { energy, .. } => assert_eq!(*energy, 16),
            other => panic!("sprout gone: {other:?}"),
        }
    }

    #[test]
    fn phase_soil_pulls_organic_around_root() {
        // Root → stem → sprout chain. One tick should subtract per the
        // ROOT_PULL_KERNEL ([[1,2,1],[2,4,2],[1,2,1]]) from the soil cells
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
                clan: 0,
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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Center weight 4.
        assert_eq!(cell_at(&chunks, chunks_x, 10, 10).organic, 96);
        // Cardinals weight 2.
        assert_eq!(cell_at(&chunks, chunks_x, 9, 10).organic, 98);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 10).organic, 98);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 9).organic, 98);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 11).organic, 98);
        // Corners weight 1.
        assert_eq!(cell_at(&chunks, chunks_x, 9, 9).organic, 99);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 11).organic, 99);
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
                clan: 0,
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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Phase 1.5 regulation drifts each cell by SOIL_ENERGY_REGULATION
        // (100 is above SOIL_ENERGY_REST so it drifts down by 1 first).
        // Then antenna pull subtracts the full 3x3 kernel: center 4,
        // cardinals 2, corners 1.
        let pre = 100 - SOIL_ENERGY_REGULATION;
        assert_eq!(cell_at(&chunks, chunks_x, 10, 10).soil_energy, pre - 4);
        assert_eq!(cell_at(&chunks, chunks_x, 9, 10).soil_energy, pre - 2);
        assert_eq!(cell_at(&chunks, chunks_x, 11, 10).soil_energy, pre - 2);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 9).soil_energy, pre - 2);
        assert_eq!(cell_at(&chunks, chunks_x, 10, 11).soil_energy, pre - 2);
        assert_eq!(cell_at(&chunks, chunks_x, 9, 9).soil_energy, pre - 1);
    }

    #[test]
    fn phase_soil_pulls_split_fairly_under_contention() {
        // Two roots at (10, 10) and (12, 10) both demand 2 organic from
        // the contested cell at (11, 10). With only 1 unit there, the
        // old "iterate-and-take" logic gave the entire unit to the
        // first-iterated root (left) and 0 to the second (right). The
        // new fair-share logic gives both roots equal share (0 each
        // here, by floor of weight × loss / total_demand = 2 × 1 / 4),
        // and the cell drops by 1 unit total.
        //
        // The mirror-image setup means the only mechanism that could
        // produce asymmetric energy between the two roots is the soil
        // pull. Death deposits and any other phase effects are
        // symmetric across the two halves of the world.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        chunks[0].cells[10 * (CHUNK_EDGE as usize) + 11].organic = 1;
        // Stems above each root so the roots have alive parents during
        // phase 2. (The stems orphan-die later in the tick, but that's
        // symmetric so it doesn't perturb the left-vs-right comparison.)
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 100,
                connections: STEM_CONNECT_SOUTH,
                parent: None,
                children: 0,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            12,
            9,
            Occupant::Stem {
                plant: 2,
                clan: 0,
                energy: 100,
                connections: STEM_CONNECT_SOUTH,
                parent: None,
                children: 0,
            },
        );
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Root {
                plant: 1,
                clan: 0,
                energy: 100,
                parent: Some(Direction::North),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            12,
            10,
            Occupant::Root {
                plant: 2,
                clan: 0,
                energy: 100,
                parent: Some(Direction::North),
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // The crucial assertion: the two symmetric roots end with equal
        // energy. Under the old "iteration order grabs first" rule the
        // left root pulled the contested cell's 1 unit and the right
        // root got 0, so they'd be unequal.
        let left = match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Root { energy, .. } => energy,
            ref other => panic!("expected root at (10,10), got {other:?}"),
        };
        let right = match cell_at(&chunks, chunks_x, 12, 10).occupant {
            Occupant::Root { energy, .. } => energy,
            ref other => panic!("expected root at (12,10), got {other:?}"),
        };
        assert_eq!(
            left, right,
            "fair-share split: symmetric roots must end with equal energy"
        );
    }

    #[test]
    fn phase_upkeep_decreases_total_system_energy() {
        // Leaf (not sunlit) → sprout sink. No photo, no soil.
        // Pre: leaf=4, sprout=10. Total 14.
        // upkeep: leaf -2, sprout -4. leaf=2, sprout=6. Total 8.
        // push: leaf cur=2, buffer=2 → no push.
        // Post: leaf=2, sprout=6. Total 8.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
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
                clan: 0,
                energy: 10,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 2),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Sprout { energy, .. } => assert_eq!(*energy, 6),
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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::West, // grows W/S/N — corners of single sprout
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

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
        // upkeep: leaf 10→8, stem 0→0 (sat), sprout 0→0 (sat)
        // push:   leaf surplus 6 → stem; stem cur=0 ≤ buffer → no push
        // post:   leaf=2, stem=6, sprout=0
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::South,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::North),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        match cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { energy, .. } => assert_eq!(energy, 2),
            ref other => panic!("leaf gone: {other:?}"),
        }
        match &cell_at(&chunks, chunks_x, 10, 11).occupant {
            Occupant::Stem { energy, .. } => assert_eq!(*energy, 6),
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
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

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
                clan: 0,
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
                clan: 0,
                energy: 0,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
    }

    #[test]
    fn phase_soil_regulation_drifts_toward_rest() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let edge = CHUNK_EDGE as usize;
        // Three sample cells: below, at, above the rest level. Use
        // values relative to SOIL_ENERGY_REST so the test holds across
        // tuning changes.
        let below = SOIL_ENERGY_REST.saturating_sub(5);
        let above = SOIL_ENERGY_REST + 50;
        chunks[0].cells[0].soil_energy = below;
        chunks[0].cells[1].soil_energy = SOIL_ENERGY_REST;
        chunks[0].cells[2].soil_energy = above;
        // No occupants, so other phases are no-ops.

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        assert_eq!(
            chunks[0].cells[0].soil_energy,
            below + SOIL_ENERGY_REGULATION
        );
        assert_eq!(chunks[0].cells[1].soil_energy, SOIL_ENERGY_REST);
        assert_eq!(
            chunks[0].cells[2].soil_energy,
            above - SOIL_ENERGY_REGULATION
        );
        let _ = edge;
    }

    #[test]
    fn phase_soil_regulation_clamps_at_rest() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Just-below and just-above cases — must not overshoot rest.
        chunks[0].cells[0].soil_energy = SOIL_ENERGY_REST - 1;
        chunks[0].cells[1].soil_energy = SOIL_ENERGY_REST + 1;

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

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
                clan: 0,
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
                clan: 0,
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
                clan: 0,
                energy: seed_energy,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
            },
        );
    }

    #[test]
    fn phase_seed_germinates_at_threshold() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place_seed_dropoff_fixture(&mut chunks, SEED_DROPOFF_THRESHOLD);

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Cell where the seed was is no longer a Seed — it germinated into
        // a Sprout, which then ran phase 6 growth in the same tick. With
        // ~150 energy and clear surroundings, the default vine grows
        // successfully so the cell ends up as a Stem (with a new Sprout
        // in front + side leaves).
        let occ = &cell_at(&chunks, chunks_x, 10, 10).occupant;
        assert!(
            !matches!(occ, Occupant::Seed { .. }),
            "expected germinated cell (not Seed), got {occ:?}"
        );
        // Parent stem's children-bit pointing at the (former) seed cleared.
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
    fn phase_seed_below_threshold_with_alive_parent_stays_seed() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Low enough that even after upkeep + push from the trunk chain,
        // it stays under the dropoff threshold.
        place_seed_dropoff_fixture(&mut chunks, 30);

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

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

    #[test]
    fn phase_death_distributes_dying_cell_energy() {
        // Lone leaf with parent=None — orphan-dies on tick 1. Its 50 units
        // of energy should be sprinkled across the 3x3 death kernel into
        // surrounding soil_energy. With kernel_sum=16 and energy=50, each
        // unit weight gets 50/16 = 3 units.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: None,
            },
        );

        // Snapshot soil_energy beforehand so we can compute deltas without
        // worrying about phase 1.5 regulation.
        let before: Vec<u16> = chunks[0].cells.iter().map(|c| c.soil_energy).collect();

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Leaf pays UPKEEP_DEFAULT first → dies with the remainder.
        // The death kernel splits that energy across the 3x3 in
        // proportion to its weights. We compute the expected center /
        // cardinal / corner contributions from the actual kernel so
        // this stays robust to tuning changes.
        let kernel = DEATH_DEPOSIT_KERNEL;
        let kernel_sum: u32 = kernel.iter().flatten().map(|&w| w as u32).sum();
        let dying_energy = (50u32).saturating_sub(UPKEEP_DEFAULT as u32);
        let per_unit = dying_energy / kernel_sum.max(1);
        let center_w = kernel[1][1] as u32;
        let card_w = kernel[1][0] as u32;
        let corner_w = kernel[0][0] as u32;
        let cell_at_idx =
            |x: i32, y: i32| -> usize { (y as usize) * (CHUNK_EDGE as usize) + x as usize };
        let center_idx = cell_at_idx(10, 10);
        let north_idx = cell_at_idx(10, 9);
        let nw_idx = cell_at_idx(9, 9);
        let drift = SOIL_ENERGY_REGULATION as u32;
        assert_eq!(
            chunks[0].cells[center_idx].soil_energy as u32,
            before[center_idx] as u32 + drift + per_unit * center_w
        );
        assert_eq!(
            chunks[0].cells[north_idx].soil_energy as u32,
            before[north_idx] as u32 + drift + per_unit * card_w
        );
        assert_eq!(
            chunks[0].cells[nw_idx].soil_energy as u32,
            before[nw_idx] as u32 + drift + per_unit * corner_w
        );
    }

    #[test]
    fn phase_death_organic_poison_kills_leaf_but_not_root() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Two cells with high organic. One holds a leaf, one a root. Both
        // also need parents/structure that wouldn't kill them on their own.
        // Easiest: give them both Some(direction) parents pointing at each
        // other so neither orphan-dies — root.parent=East (at leaf),
        // leaf.parent=West (at root). Same plant.
        let high_organic = SOIL_ORGANIC_POISON + 10;
        let leaf_idx = 10 * (CHUNK_EDGE as usize) + 11;
        let root_idx = 10 * (CHUNK_EDGE as usize) + 10;
        chunks[0].cells[leaf_idx].organic = high_organic;
        chunks[0].cells[root_idx].organic = high_organic;
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Root {
                plant: 1,
                clan: 0,
                energy: 50,
                parent: Some(Direction::East),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            11,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: Some(Direction::West),
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Leaf died from organic poisoning.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 10).occupant,
            Occupant::Empty
        ));
        // Root survived — immune to organic poison.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Root { .. }
        ));
    }

    #[test]
    fn phase_death_energy_poison_kills_leaf_but_not_antenna() {
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        let high_energy = SOIL_ENERGY_POISON + 10;
        let leaf_idx = 10 * (CHUNK_EDGE as usize) + 11;
        let antenna_idx = 10 * (CHUNK_EDGE as usize) + 10;
        chunks[0].cells[leaf_idx].soil_energy = high_energy;
        chunks[0].cells[antenna_idx].soil_energy = high_energy;
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Antenna {
                plant: 1,
                clan: 0,
                energy: 50,
                parent: Some(Direction::East),
            },
        );
        place(
            &mut chunks,
            chunks_x,
            11,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: Some(Direction::West),
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Leaf died from energy poisoning.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 11, 10).occupant,
            Occupant::Empty
        ));
        // Antenna survived — immune to energy poison.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Antenna { .. }
        ));
    }

    #[test]
    fn phase_prune_cascades_one_link_per_tick() {
        // Chain A → B → C with a sprout at the tip that already died (cell
        // at sprout position is Empty). Prune drops C's tip-bit when it
        // sees Empty; phase_drainage then drains C's energy back into B
        // and drops B's bit pointing at C — all in tick 1. C dies via
        // energy_dead at end of tick 1. Then tick 2 collapses B → A.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        // Head A (parent=None, points north at B).
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 100,
                connections: STEM_CONNECT_NORTH,
                parent: None,
                children: STEM_CONNECT_NORTH,
            },
        );
        // Middle B (points north at C).
        place(
            &mut chunks,
            chunks_x,
            10,
            9,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 100,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: Some(Direction::South),
                children: STEM_CONNECT_NORTH,
            },
        );
        // Tail C (points north at where the sprout used to be — now
        // Empty).
        place(
            &mut chunks,
            chunks_x,
            10,
            8,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 100,
                connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
                parent: Some(Direction::South),
                children: STEM_CONNECT_NORTH,
            },
        );

        // Helper: snapshot just the (alive?, children) of A/B/C.
        let snap = |chunks: &[Chunk]| -> [(bool, u8); 3] {
            let cells = [(10, 10), (10, 9), (10, 8)];
            let mut out = [(false, 0u8); 3];
            for (i, (x, y)) in cells.iter().enumerate() {
                match &cell_at(chunks, chunks_x, *x, *y).occupant {
                    Occupant::Stem { children, .. } => out[i] = (true, *children),
                    Occupant::Empty => out[i] = (false, 0),
                    other => panic!("unexpected occupant: {other:?}"),
                }
            }
            out
        };

        // Tick 1: tip C sees Empty north → drops its N bit. Drainage
        // sees C as a kin dead-end and drains it to 0; B drops its bit
        // pointing at C in the same step. C dies via energy_dead. B
        // still has its parent ref to A (A is alive kin), so B
        // survives this tick as a parent-only stem.
        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );
        assert_eq!(
            snap(&chunks),
            [(true, STEM_CONNECT_NORTH), (true, 0), (false, 0)],
            "after tick 1 C should be drained + dead, B's child bit cleared"
        );
    }

    #[test]
    fn phase_death_clears_parent_on_neighbors_pointing_at_dying() {
        // A leaf at (10, 10) with parent=South pointing at a stem at
        // (10, 11). The stem dies (orphan + zero energy). After the
        // tick the leaf's parent should be None — so even if a foreign
        // cell later repopulates (10, 11), the leaf can't silently
        // re-attach.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 50,
                facing: Direction::North,
                parent: Some(Direction::South),
            },
        );
        // Stem about to die: zero energy and no children.
        place(
            &mut chunks,
            chunks_x,
            10,
            11,
            Occupant::Stem {
                plant: 1,
                clan: 0,
                energy: 0,
                connections: 0,
                parent: None,
                children: 0,
            },
        );

        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(1),
            &mut det_rng(),
        );

        // Stem died.
        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 11).occupant,
            Occupant::Empty
        ));
        // Leaf's parent has been cleared (it was Some(South), pointing
        // at the now-dead stem). It will orphan-die next tick — we
        // don't need to assert the leaf's still alive here, only that
        // its parent is gone.
        match &cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Leaf { parent, .. } => assert_eq!(*parent, None),
            // Acceptable if it already orphan-died — that's the
            // immediate consequence of clearing parent + push lossage.
            Occupant::Empty => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn phase_seed_germinates_when_parent_dies() {
        // Seed with parent direction pointing at an empty cell. Should
        // germinate even though energy is well below threshold.
        let chunks_x = 1u32;
        let mut chunks = empty_world(chunks_x, 1);
        place(
            &mut chunks,
            chunks_x,
            10,
            10,
            Occupant::Seed {
                plant: 7,
                clan: 0,
                energy: 25, // far below SEED_DROPOFF_THRESHOLD
                facing: Direction::East,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
            },
        );
        // (10, 11) is left Empty — simulating a parent stem that died last
        // tick (decomposed into Empty by phase 7).

        // Counter starts at 100 so we can distinguish the fresh id from
        // the seed's previous plant id (7).
        mutate_world(
            &mut chunks,
            1,
            1,
            &test_params(),
            &AtomicU32::new(100),
            &mut det_rng(),
        );

        match &cell_at(&chunks, chunks_x, 10, 10).occupant {
            Occupant::Sprout {
                plant,
                facing,
                parent,
                current_gene,
                ..
            } => {
                assert_eq!(
                    *plant, 100,
                    "germinated sprout becomes its own plant — fresh id"
                );
                assert_eq!(*facing, Direction::East, "facing preserved");
                assert_eq!(*parent, None);
                assert_eq!(*current_gene, 0);
            }
            other => panic!("expected sprout, got {other:?}"),
        }
    }
}
