use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, Direction, Genome, Occupant,
};
use rand::Rng;

/// Inset (per side, as fraction of each box's dimension) where sunlight
/// switches off. 0.10 → central 80% × 80% lit per box, dark frame around.
const SUNLIT_MARGIN_FRAC: f32 = 0.10;

/// Spacing between initial sprouts in `place_random_sprout_grid`. Tight
/// enough to seed thousands of competing lineages — most random genomes
/// die or stall in the first few ticks, so dense packing is fine.
const SPROUT_GRID_SPACING: i32 = 6;

/// Thickness in cells of the toxic-organic border ringing each box.
const TOXIC_BORDER_THICKNESS: u32 = 2;

/// Organic value seeded into the toxic-border cells. Above
/// SOIL_ORGANIC_POISON, so any non-Root that ventures in dies. Acts as
/// a hard wall that plants can't expand across — lineages take many
/// ticks of root-pull to chew through.
const TOXIC_BORDER_ORGANIC: u16 = 1000;

/// Organic seeded everywhere else. Below the poison threshold; just
/// some baseline soil chemistry.
const DEFAULT_ORGANIC: u16 = 0;

/// World layout: BOXES_PER_DIMENSION × BOXES_PER_DIMENSION isolated
/// "boxes," each with its own sunlit interior and toxic-border ring.
/// World wrap connects opposite edges, so each box's outer borders are
/// the same wall as its neighbors' through the wrap.
pub const BOXES_PER_DIMENSION: u32 = 2;

pub fn build_world(chunks_x: u32, chunks_y: u32) -> Vec<Chunk> {
    let mut chunks = Vec::with_capacity((chunks_x * chunks_y) as usize);
    let total_w = chunks_x * CHUNK_EDGE as u32;
    let total_h = chunks_y * CHUNK_EDGE as u32;
    // Per-box dimensions and per-box sunlit margin.
    let box_w = total_w / BOXES_PER_DIMENSION;
    let box_h = total_h / BOXES_PER_DIMENSION;
    let margin_x = (box_w as f32 * SUNLIT_MARGIN_FRAC) as u32;
    let margin_y = (box_h as f32 * SUNLIT_MARGIN_FRAC) as u32;
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            let cells = (0..CHUNK_AREA)
                .map(|i| {
                    let local_x = (i % CHUNK_EDGE as usize) as u32;
                    let local_y = (i / CHUNK_EDGE as usize) as u32;
                    let world_x = cx * CHUNK_EDGE as u32 + local_x;
                    let world_y = cy * CHUNK_EDGE as u32 + local_y;
                    // Each box checks margin/border in its own local
                    // coords. With wrap on, opposite world edges connect
                    // to each other, so the outer borders of corner
                    // boxes meet through the wrap and form a continuous
                    // wall on the torus.
                    let bx = world_x % box_w;
                    let by = world_y % box_h;
                    let sunlit = bx >= margin_x
                        && bx < box_w - margin_x
                        && by >= margin_y
                        && by < box_h - margin_y;
                    let in_toxic_border = bx < TOXIC_BORDER_THICKNESS
                        || by < TOXIC_BORDER_THICKNESS
                        || bx >= box_w - TOXIC_BORDER_THICKNESS
                        || by >= box_h - TOXIC_BORDER_THICKNESS;
                    let organic = if in_toxic_border {
                        TOXIC_BORDER_ORGANIC
                    } else {
                        DEFAULT_ORGANIC
                    };
                    Cell {
                        organic,
                        soil_energy: 100,
                        sunlit,
                        lineage_mutation_rate: 0,
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

/// Place sprouts on a regular grid across the lit region of the world,
/// each carrying a freshly-randomized genome. Returns the count placed so
/// the caller can advance `next_plant_id`.
pub fn place_random_sprout_grid(
    chunks: &mut [Chunk],
    chunks_x: u32,
    chunks_y: u32,
    rng: &mut impl Rng,
) -> u32 {
    let total_w = chunks_x as i32 * CHUNK_EDGE as i32;
    let total_h = chunks_y as i32 * CHUNK_EDGE as i32;
    let box_w = total_w / BOXES_PER_DIMENSION as i32;
    let box_h = total_h / BOXES_PER_DIMENSION as i32;
    let mut count = 0u32;
    let mut y = SPROUT_GRID_SPACING;
    while y < total_h - SPROUT_GRID_SPACING {
        let mut x = SPROUT_GRID_SPACING;
        while x < total_w - SPROUT_GRID_SPACING {
            if let Some(cell) = cell_at(chunks, chunks_x, x, y) {
                if cell.sunlit {
                    count += 1;
                    let facing = match rng.r#gen::<u8>() % 4 {
                        0 => Direction::North,
                        1 => Direction::East,
                        2 => Direction::South,
                        _ => Direction::West,
                    };
                    let mut starter = Genome::default_vine();
                    starter.mutation_rate = 1.0;
                    let mut genome = crate::sim::mutate_genome(&starter, rng);
                    // Restore the standard initial mutation rate so all
                    // fresh sprouts start at the same rate; only the
                    // gene contents differ between them.
                    genome.mutation_rate = protocol::DEFAULT_MUTATION_RATE;
                    // Clan: which 2D box this sprout starts in. Encoded
                    // row-major: clan = box_y * BOXES_PER_DIMENSION + box_x.
                    let bx = (x / box_w) as u32;
                    let by = (y / box_h) as u32;
                    let clan = (by * BOXES_PER_DIMENSION + bx) as protocol::ClanId;
                    let rate_q = protocol::quantize_mutation_rate(genome.mutation_rate);
                    place_at(
                        chunks,
                        chunks_x,
                        x,
                        y,
                        Occupant::Sprout {
                            plant: count,
                            clan,
                            energy: 100,
                            facing,
                            genome: Box::new(genome),
                            parent: None,
                            current_gene: 0,
                        },
                    );
                    if let Some(cell) = cell_at_mut(chunks, chunks_x, x, y) {
                        cell.lineage_mutation_rate = rate_q;
                    }
                }
            }
            x += SPROUT_GRID_SPACING;
        }
        y += SPROUT_GRID_SPACING;
    }
    count
}

fn cell_at<'a>(chunks: &'a [Chunk], chunks_x: u32, x: i32, y: i32) -> Option<&'a Cell> {
    if x < 0 || y < 0 {
        return None;
    }
    let edge = CHUNK_EDGE as i32;
    let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
    let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
    chunks.get(chunk_idx)?.cells.get(cell_idx)
}

fn cell_at_mut<'a>(
    chunks: &'a mut [Chunk],
    chunks_x: u32,
    x: i32,
    y: i32,
) -> Option<&'a mut Cell> {
    if x < 0 || y < 0 {
        return None;
    }
    let edge = CHUNK_EDGE as i32;
    let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
    let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
    chunks.get_mut(chunk_idx)?.cells.get_mut(cell_idx)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn cell_at_test<'a>(chunks: &'a [Chunk], chunks_x: u32, x: i32, y: i32) -> &'a Cell {
        let edge = CHUNK_EDGE as i32;
        let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
        let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
        &chunks[chunk_idx].cells[cell_idx]
    }

    #[test]
    fn build_world_lays_out_chunks_in_row_major_order() {
        let chunks = build_world(3, 2);
        assert_eq!(chunks.len(), 6);
        // Row-major: idx = cy * chunks_x + cx.
        assert_eq!(chunks[0].coord, ChunkCoord { x: 0, y: 0 });
        assert_eq!(chunks[1].coord, ChunkCoord { x: 1, y: 0 });
        assert_eq!(chunks[2].coord, ChunkCoord { x: 2, y: 0 });
        assert_eq!(chunks[3].coord, ChunkCoord { x: 0, y: 1 });
        assert_eq!(chunks[5].coord, ChunkCoord { x: 2, y: 1 });
        for chunk in &chunks {
            assert_eq!(chunk.cells.len(), CHUNK_AREA);
        }
    }

    #[test]
    fn build_world_initializes_per_cell_fields() {
        // 4x4 chunk world = 128×128. With BOXES_PER_DIMENSION=2, each box
        // is 64×64. We want a cell well inside box (0,0)'s interior.
        let chunks = build_world(4, 4);
        for chunk in &chunks {
            for cell in &chunk.cells {
                assert_eq!(cell.soil_energy, 100);
                assert!(matches!(cell.occupant, Occupant::Empty));
            }
        }
        // Box (0,0) corner → in toxic border.
        assert_eq!(
            cell_at_test(&chunks, 4, 0, 0).organic,
            TOXIC_BORDER_ORGANIC
        );
        // Center of box (0,0) at (32, 32) → default soil organic.
        assert_eq!(
            cell_at_test(&chunks, 4, 32, 32).organic,
            DEFAULT_ORGANIC
        );
    }

    #[test]
    fn build_world_toxic_border_rings_each_box() {
        // 4x4 chunks = 128×128. Two boxes per dimension → each box 64×64.
        let chunks = build_world(4, 4);
        let box_dim = 4 * CHUNK_EDGE as i32 / BOXES_PER_DIMENSION as i32;
        let t = TOXIC_BORDER_THICKNESS as i32;
        // Box (0,0): all four sides have a toxic border.
        assert_eq!(
            cell_at_test(&chunks, 4, 0, box_dim / 2).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) left edge"
        );
        assert_eq!(
            cell_at_test(&chunks, 4, box_dim - 1, box_dim / 2).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) right edge (interior wall)"
        );
        assert_eq!(
            cell_at_test(&chunks, 4, box_dim / 2, 0).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) top edge"
        );
        assert_eq!(
            cell_at_test(&chunks, 4, box_dim / 2, box_dim - 1).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) bottom edge (interior wall)"
        );
        // Just inside box (0,0)'s left border.
        assert_eq!(
            cell_at_test(&chunks, 4, t, box_dim / 2).organic,
            DEFAULT_ORGANIC
        );
        // Last toxic cell on box (0,0)'s left border.
        assert_eq!(
            cell_at_test(&chunks, 4, t - 1, box_dim / 2).organic,
            TOXIC_BORDER_ORGANIC
        );
        // Box (1,1) has its own border too — local (0,0) of that box is
        // at world (box_dim, box_dim).
        assert_eq!(
            cell_at_test(&chunks, 4, box_dim, box_dim).organic,
            TOXIC_BORDER_ORGANIC
        );
        // Center of box (1,1).
        assert_eq!(
            cell_at_test(&chunks, 4, box_dim + box_dim / 2, box_dim + box_dim / 2).organic,
            DEFAULT_ORGANIC
        );
    }

    #[test]
    fn build_world_sunlit_is_central_rectangle_per_box() {
        // 4x4 chunks = 128×128. Each box 64×64. Per-box margin = 6.
        let chunks = build_world(4, 4);
        let box_dim = 4 * CHUNK_EDGE as i32 / BOXES_PER_DIMENSION as i32;
        let margin = (box_dim as f32 * SUNLIT_MARGIN_FRAC) as i32;
        // Inside box (0,0)'s lit center.
        assert!(cell_at_test(&chunks, 4, box_dim / 2, box_dim / 2).sunlit);
        // Just inside the per-box margin.
        assert!(cell_at_test(&chunks, 4, margin, box_dim / 2).sunlit);
        // Just outside the per-box margin → dark.
        assert!(!cell_at_test(&chunks, 4, margin - 1, box_dim / 2).sunlit);
        // Box (0,0) corner → dark.
        assert!(!cell_at_test(&chunks, 4, 0, 0).sunlit);
        // The cell at the boundary between box (0,0) and box (1,0) is in
        // box (0,0)'s right border → dark.
        assert!(!cell_at_test(&chunks, 4, box_dim - 1, box_dim / 2).sunlit);
        // Box (1,1) also has its own lit center.
        assert!(cell_at_test(
            &chunks,
            4,
            box_dim + box_dim / 2,
            box_dim + box_dim / 2
        )
        .sunlit);
    }

    #[test]
    fn place_random_sprout_grid_places_sprouts_only_in_lit_region() {
        let mut chunks = build_world(4, 4);
        let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(123);
        let count = place_random_sprout_grid(&mut chunks, 4, 4, &mut rng);
        assert!(count > 0, "expected at least one sprout in the lit region");
        // Every placed sprout sits on a sunlit cell.
        for chunk in &chunks {
            for cell in &chunk.cells {
                if matches!(cell.occupant, Occupant::Sprout { .. }) {
                    assert!(cell.sunlit, "sprout placed on dark cell");
                }
            }
        }
    }

    #[test]
    fn place_at_silently_ignores_negative_and_oob() {
        let mut chunks = build_world(1, 1);
        // Negative — must not panic.
        place_at(&mut chunks, 1, -1, 5, Occupant::Empty);
        place_at(&mut chunks, 1, 5, -1, Occupant::Empty);
        // Beyond chunks — must not panic.
        let beyond = (CHUNK_EDGE as i32) * 2;
        place_at(
            &mut chunks,
            1,
            beyond,
            0,
            Occupant::Leaf {
                plant: 1,
                clan: 0,
                energy: 0,
                facing: Direction::North,
                parent: None,
            },
        );
        // No cells modified.
        for chunk in &chunks {
            for cell in &chunk.cells {
                assert!(matches!(cell.occupant, Occupant::Empty));
            }
        }
    }

    #[test]
    fn place_at_writes_into_correct_chunk_and_cell() {
        // 2x1 chunk world. Place at world (CHUNK_EDGE, 0) → chunk (1, 0),
        // local cell (0, 0).
        let mut chunks = build_world(2, 1);
        let edge = CHUNK_EDGE as i32;
        place_at(
            &mut chunks,
            2,
            edge,
            0,
            Occupant::Leaf {
                plant: 7,
                clan: 0,
                energy: 0,
                facing: Direction::East,
                parent: None,
            },
        );
        let target = &chunks[1].cells[0];
        match &target.occupant {
            Occupant::Leaf { plant, .. } => assert_eq!(*plant, 7),
            other => panic!("expected leaf, got {other:?}"),
        }
        // Other chunks untouched.
        for cell in &chunks[0].cells {
            assert!(matches!(cell.occupant, Occupant::Empty));
        }
    }

}
