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
