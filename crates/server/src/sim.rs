use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, Direction, Energy, Gene, Genome, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
    ServerMessage, SlotProduct,
};
use rand::Rng;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const LEAF_PHOTOSYNTHESIS: Energy = 5;
const UPKEEP_DEFAULT: Energy = 1;
const UPKEEP_SEED: Energy = 0;
const UPKEEP_SPROUT: Energy = 3;

/// Per-slot spawn cost. Sprout drains the sum of these for whatever it
/// produces in a generation. Each new cell starts with its slot's cost as
/// its initial energy.
const COST_SPROUT: Energy = 20;
const COST_LEAF: Energy = 5;
const COST_ROOT: Energy = 5;
const COST_ANTENNA: Energy = 5;
const COST_SEED: Energy = 30;

/// Per-field probability of mutating a single field at any copy site.
const MUTATION_RATE: f32 = 0.0;

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

        let snapshot_chunks = {
            let mut chunks = state.world.lock().expect("sim lock poisoned");
            mutate_world(&mut chunks, state.chunks_x, state.chunks_y);
            chunks.clone()
        };
        let tick = state.current_tick.fetch_add(1, Ordering::Relaxed) + 1;

        let msg = ServerMessage::ChunkBatch {
            tick,
            chunks: snapshot_chunks,
        };
        match rmp_serde::to_vec(&msg) {
            Ok(bytes) => {
                let _ = state.tick_tx.send(Arc::new(bytes));
            }
            Err(e) => error!("serialize tick failed: {e:#}"),
        }
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

    // Phase 5: growth — sprouts execute their current gene if energy covers
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
        Occupant::Sprout { .. } => true,
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
            genome: Box::new(mutate_genome(parent_genome)),
            parent: parent_back,
        },
        SlotProduct::Sprout => Occupant::Sprout {
            plant,
            energy: COST_SPROUT,
            facing,
            genome: Box::new(mutate_genome(parent_genome)),
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

    // Walk the plan and figure out which slots are *actually* growable: the
    // slot has to be a real product (not Nothing) AND its target cell has to
    // be in-bounds AND Empty.
    let mut viable: [bool; 3] = [false; 3];
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
        let cleared = matches!(
            cell_at_mut(chunks, chunks_x, nx, ny).map(|c| &c.occupant),
            Some(Occupant::Empty)
        );
        if cleared {
            viable[i] = true;
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
    if sprout_energy <= effective_cost {
        return; // wait for more energy
    }

    let mut connections = 0u8;
    let mut children = 0u8;
    let mut grew = false;

    for (i, (dir, slot)) in plan.iter().enumerate() {
        if !viable[i] {
            continue;
        }
        let Some(occ) = make_slot_occupant(*slot, plant, *dir, *dir, genome, next_gene) else {
            continue;
        };
        let (dx, dy) = direction_to_delta(*dir);
        let nx = wx + dx;
        let ny = wy + dy;
        if let Some(target) = cell_at_mut(chunks, chunks_x, nx, ny) {
            target.occupant = occ;
            connections |= dir_to_bitmask(*dir);
            if matches!(slot, SlotProduct::Sprout) {
                children |= dir_to_bitmask(*dir);
            }
            grew = true;
        }
    }

    if grew {
        if let Some(parent_dir) = parent {
            connections |= dir_to_bitmask(parent_dir);
        }
        let new_energy = sprout_energy.saturating_sub(effective_cost);
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

/// Per-field mutation pass over a genome. Each gene's slots and `next` each
/// roll independently against `MUTATION_RATE`. Called at every copy site:
/// sprout-produces-sub-sprouts, sprout-produces-seed, seed-germinates.
pub fn mutate_genome(g: &Genome) -> Genome {
    let mut rng = rand::thread_rng();
    let len = g.genes.len();
    let mut new_genes: Vec<Gene> = g.genes.clone();
    for gene in new_genes.iter_mut() {
        if rng.r#gen::<f32>() < MUTATION_RATE {
            gene.front = random_slot(&mut rng);
        }
        if rng.r#gen::<f32>() < MUTATION_RATE {
            gene.left = random_slot(&mut rng);
        }
        if rng.r#gen::<f32>() < MUTATION_RATE {
            gene.right = random_slot(&mut rng);
        }
        if rng.r#gen::<f32>() < MUTATION_RATE {
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
        let blocker = || Occupant::Leaf {
            plant: 99,
            energy: 50,
            facing: Direction::North,
            parent: None,
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
        );

        assert!(matches!(
            cell_at(&chunks, chunks_x, 10, 10).occupant,
            Occupant::Empty
        ));
        // Center weight of DEATH_DEPOSIT_KERNEL is 4.
        assert!(cell_at(&chunks, chunks_x, 10, 10).organic >= 4);
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
        assert!(is_valid_child(&sprout));
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
        let leaf =
            make_slot_occupant(SlotProduct::Leaf, 7, Direction::East, Direction::East, &parent_genome, 0)
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
        );
        assert!(nothing.is_none());

        let sprout = make_slot_occupant(
            SlotProduct::Sprout,
            5,
            Direction::North,
            Direction::North,
            &parent_genome,
            3,
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
        // MUTATION_RATE is currently 0.0, so this is a no-op clone. If the
        // const ever becomes nonzero, this test will start to flake — at
        // which point mutation should be tested via a seeded RNG instead.
        assert_eq!(MUTATION_RATE, 0.0);
        let g = Genome::default_vine();
        let copied = mutate_genome(&g);
        assert_eq!(copied, g);
    }
}
