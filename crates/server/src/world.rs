use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, Direction, Genome, Occupant,
    STEM_CONNECT_EAST, STEM_CONNECT_NORTH, STEM_CONNECT_SOUTH, STEM_CONNECT_WEST,
};

pub fn build_world(chunks_x: u32, chunks_y: u32) -> Vec<Chunk> {
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

pub fn place_showcase(chunks: &mut [Chunk], chunks_x: u32) {
    let plant = 1u32;
    let energy = 200u16;
    let default_genome = || Box::new(Genome::default_vine());

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
                genome: default_genome(),
                parent: None,
                current_gene: 0,
            },
        ),
        (
            26,
            20,
            Occupant::Seed {
                plant,
                energy,
                facing: Direction::East,
                genome: default_genome(),
                parent: None,
            },
        ),
    ];
    for (x, y, occupant) in entries {
        place_at(chunks, chunks_x, x, y, occupant);
    }

    // Viable mini-plant centered around (50, 50). A trunk stem with a leaf
    // on its east side (production source) and a sprout above it (growth
    // sink). Energy: leaf -> trunk -> sprout, with leaf photosynthesis as
    // the source and the sprout as a terminal sink.
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
                genome: default_genome(),
                parent: Some(Direction::South),
                current_gene: 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_at<'a>(chunks: &'a [Chunk], chunks_x: u32, x: i32, y: i32) -> &'a Cell {
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
        // Spot-check the deterministic per-cell formulas at a few coords.
        // organic = (x ^ y) & 0xff, sunlit = (x + y) % 3 != 0.
        let c = cell_at(&chunks, 2, 0, 0);
        assert_eq!(c.organic, 0);
        assert!(!c.sunlit); // (0+0) % 3 == 0 → not sunlit
        let c = cell_at(&chunks, 2, 5, 7);
        assert_eq!(c.organic, ((5u32 ^ 7) & 0xff) as u16);
        // (5 + 7) % 3 == 0 → not sunlit.
        assert!(!c.sunlit);
    }

    #[test]
    fn build_world_sunlit_pattern_follows_formula() {
        let chunks = build_world(2, 1);
        for y in 0..(CHUNK_EDGE as i32) {
            for x in 0..(2 * CHUNK_EDGE as i32) {
                let expected =
                    (x as u32).wrapping_add(y as u32) % 3 != 0;
                assert_eq!(cell_at(&chunks, 2, x, y).sunlit, expected);
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

    #[test]
    fn place_showcase_lays_out_inert_row_and_mini_plant() {
        // World big enough for both: showcase row uses x up to 26, mini
        // plant uses (50, 50). Need 2 chunks wide × 2 chunks tall (each 32).
        let mut chunks = build_world(2, 2);
        place_showcase(&mut chunks, 2);

        // Spot-check the inert showcase row at y=20.
        assert!(matches!(
            cell_at(&chunks, 2, 10, 20).occupant,
            Occupant::Leaf { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 14, 20).occupant,
            Occupant::Root { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 16, 20).occupant,
            Occupant::Stem { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 22, 20).occupant,
            Occupant::Antenna { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 24, 20).occupant,
            Occupant::Sprout { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 26, 20).occupant,
            Occupant::Seed { .. }
        ));

        // Mini-plant trio.
        assert!(matches!(
            cell_at(&chunks, 2, 50, 50).occupant,
            Occupant::Sprout { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 50, 51).occupant,
            Occupant::Stem { .. }
        ));
        assert!(matches!(
            cell_at(&chunks, 2, 51, 51).occupant,
            Occupant::Leaf { .. }
        ));
    }
}
