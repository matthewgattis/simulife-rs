use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, Direction, Energy, Genome, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
    ServerMessage,
};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

const LEAF_PHOTOSYNTHESIS: Energy = 5;
const UPKEEP_DEFAULT: Energy = 1;
const UPKEEP_SEED: Energy = 0;
const UPKEEP_SPROUT: Energy = 3;
const GROWTH_THRESHOLD: Energy = 50;
const GROWTH_CHILD_ENERGY: Energy = 10;

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
        genome: Box::new(Genome { bytes: Vec::new() }),
        parent: None,
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

    // Phase 5: growth — sprouts above GROWTH_THRESHOLD differentiate into
    // a stem and spawn new cells in front (sprout) + left/right (leaves).
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
                            ..
                        } if *energy > GROWTH_THRESHOLD => {
                            Some((*plant, *energy, *facing, *parent))
                        }
                        _ => None,
                    };
                    if let Some((plant, energy, facing, parent)) = info {
                        let wx = cx as i32 * edge + lx as i32;
                        let wy = cy as i32 * edge + ly as i32;
                        attempt_growth(
                            chunks, chunks_x, max_x, max_y, wx, wy, plant, energy, facing,
                            parent,
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

#[derive(Clone, Copy)]
enum GrowthChild {
    Sprout,
    Leaf,
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
) {
    let plan = [
        (facing, GrowthChild::Sprout),
        (rotate_left(facing), GrowthChild::Leaf),
        (rotate_right(facing), GrowthChild::Leaf),
    ];

    let mut connections = 0u8;
    let mut children = 0u8;
    let mut grew = false;

    for (dir, kind) in plan {
        let (dx, dy) = direction_to_delta(dir);
        let nx = wx + dx;
        let ny = wy + dy;
        if nx < 0 || ny < 0 || nx >= max_x || ny >= max_y {
            continue;
        }
        let target = match cell_at_mut(chunks, chunks_x, nx, ny) {
            Some(c) if matches!(c.occupant, Occupant::Empty) => c,
            _ => continue,
        };
        let opposite = opposite_dir(dir);
        target.occupant = match kind {
            GrowthChild::Sprout => Occupant::Sprout {
                plant,
                energy: GROWTH_CHILD_ENERGY,
                facing: dir,
                genome: Box::new(Genome { bytes: Vec::new() }),
                parent: Some(opposite),
            },
            GrowthChild::Leaf => Occupant::Leaf {
                plant,
                energy: GROWTH_CHILD_ENERGY,
                facing: dir,
                parent: Some(opposite),
            },
        };
        connections |= dir_to_bitmask(dir);
        if matches!(kind, GrowthChild::Sprout) {
            children |= dir_to_bitmask(dir);
        }
        grew = true;
    }

    if grew {
        // Visually connect back to the parent stem too, so the new stem reads
        // as part of the existing trunk rather than a disconnected hub.
        if let Some(parent_dir) = parent {
            connections |= dir_to_bitmask(parent_dir);
        }
        let new_energy = sprout_energy.saturating_sub(GROWTH_THRESHOLD);
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
