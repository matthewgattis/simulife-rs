use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, Direction, Genome, Occupant,
};
use rand::Rng;

/// Inset (per side, as fraction of total dimension) where sunlight switches
/// off. 0.10 → central 80% × 80% lit, dark frame around.
const SUNLIT_MARGIN_FRAC: f32 = 0.10;

/// Spacing between initial sprouts in `place_random_sprout_grid`.
const SPROUT_GRID_SPACING: i32 = 32;

pub fn build_world(chunks_x: u32, chunks_y: u32) -> Vec<Chunk> {
    let mut chunks = Vec::with_capacity((chunks_x * chunks_y) as usize);
    let total_w = chunks_x * CHUNK_EDGE as u32;
    let total_h = chunks_y * CHUNK_EDGE as u32;
    let margin_x = (total_w as f32 * SUNLIT_MARGIN_FRAC) as u32;
    let margin_y = (total_h as f32 * SUNLIT_MARGIN_FRAC) as u32;
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            let cells = (0..CHUNK_AREA)
                .map(|i| {
                    let local_x = (i % CHUNK_EDGE as usize) as u32;
                    let local_y = (i / CHUNK_EDGE as usize) as u32;
                    let world_x = cx * CHUNK_EDGE as u32 + local_x;
                    let world_y = cy * CHUNK_EDGE as u32 + local_y;
                    let sunlit = world_x >= margin_x
                        && world_x < total_w - margin_x
                        && world_y >= margin_y
                        && world_y < total_h - margin_y;
                    Cell {
                        organic: ((world_x ^ world_y) & 0xff) as u16,
                        soil_energy: 100,
                        sunlit,
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
                    let genome =
                        crate::sim::mutate_genome(&Genome::default_vine(), 1.0, rng);
                    place_at(
                        chunks,
                        chunks_x,
                        x,
                        y,
                        Occupant::Sprout {
                            plant: count,
                            energy: 100,
                            facing,
                            genome: Box::new(genome),
                            parent: None,
                            current_gene: 0,
                        },
                    );
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
        let chunks = build_world(2, 1);
        // Soil energy is constant 100 everywhere.
        for chunk in &chunks {
            for cell in &chunk.cells {
                assert_eq!(cell.soil_energy, 100);
                assert!(matches!(cell.occupant, Occupant::Empty));
            }
        }
        // Spot-check organic formula. (Sunlit is checked in its own test.)
        let c = cell_at_test(&chunks, 2, 0, 0);
        assert_eq!(c.organic, 0);
        let c = cell_at_test(&chunks, 2, 5, 7);
        assert_eq!(c.organic, ((5u32 ^ 7) & 0xff) as u16);
    }

    #[test]
    fn build_world_sunlit_is_central_rectangle() {
        // 2x2 chunk world = 64×64. SUNLIT_MARGIN_FRAC=0.10 → margin 6.
        let chunks = build_world(2, 2);
        let total = 2 * CHUNK_EDGE as i32;
        let margin = (total as f32 * SUNLIT_MARGIN_FRAC) as i32;
        // Inside the lit center.
        assert!(cell_at_test(&chunks, 2, total / 2, total / 2).sunlit);
        // Just inside the margin on each axis.
        assert!(cell_at_test(&chunks, 2, margin, total / 2).sunlit);
        assert!(cell_at_test(&chunks, 2, total / 2, margin).sunlit);
        // Just outside the margin → dark.
        assert!(!cell_at_test(&chunks, 2, margin - 1, total / 2).sunlit);
        assert!(!cell_at_test(&chunks, 2, total / 2, margin - 1).sunlit);
        // Far corners → dark.
        assert!(!cell_at_test(&chunks, 2, 0, 0).sunlit);
        assert!(!cell_at_test(&chunks, 2, total - 1, total - 1).sunlit);
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
