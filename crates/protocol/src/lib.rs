use serde::{Deserialize, Serialize};

pub const CHUNK_EDGE: u16 = 32;
pub const CHUNK_AREA: usize = (CHUNK_EDGE as usize) * (CHUNK_EDGE as usize);

pub type PlantId = u32;
pub type Energy = u16;

pub const STEM_CONNECT_NORTH: u8 = 1 << 0;
pub const STEM_CONNECT_EAST: u8 = 1 << 1;
pub const STEM_CONNECT_SOUTH: u8 = 1 << 2;
pub const STEM_CONNECT_WEST: u8 = 1 << 3;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChunkCoord {
    pub x: i32,
    pub y: i32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    North,
    East,
    South,
    West,
}

pub const GENOME_LEN: usize = 16;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotProduct {
    Nothing,
    Leaf,
    Root,
    Antenna,
    Seed,
    Sprout,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gene {
    pub front: SlotProduct,
    pub left: SlotProduct,
    pub right: SlotProduct,
    /// Next-gene index. Out-of-range values are taken modulo `GENOME_LEN` so
    /// the gene graph is always traversable.
    pub next: u8,
}

impl Default for Gene {
    fn default() -> Self {
        Self {
            front: SlotProduct::Nothing,
            left: SlotProduct::Nothing,
            right: SlotProduct::Nothing,
            next: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Genome {
    pub genes: Vec<Gene>,
}

impl Genome {
    /// Default starter: gene 0 is the "vine" (front sprout, left/right leaves,
    /// next loops back to 0); remaining genes are dormant Nothing-slots that
    /// also point at gene 0. Mutation activates the dormant genes over time.
    pub fn default_vine() -> Self {
        let mut genes = Vec::with_capacity(GENOME_LEN);
        genes.push(Gene {
            front: SlotProduct::Sprout,
            left: SlotProduct::Leaf,
            right: SlotProduct::Leaf,
            next: 0,
        });
        while genes.len() < GENOME_LEN {
            genes.push(Gene::default());
        }
        Self { genes }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Occupant {
    Empty,
    Leaf {
        plant: PlantId,
        energy: Energy,
        facing: Direction,
        parent: Option<Direction>,
    },
    Root {
        plant: PlantId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Stem {
        plant: PlantId,
        energy: Energy,
        connections: u8,
        parent: Option<Direction>,
        children: u8,
    },
    Antenna {
        plant: PlantId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Sprout {
        plant: PlantId,
        energy: Energy,
        facing: Direction,
        genome: Box<Genome>,
        parent: Option<Direction>,
        current_gene: u8,
    },
    Seed {
        plant: PlantId,
        energy: Energy,
        facing: Direction,
        genome: Box<Genome>,
        parent: Option<Direction>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Cell {
    pub organic: u16,
    pub soil_energy: u16,
    pub sunlit: bool,
    pub occupant: Occupant,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chunk {
    pub coord: ChunkCoord,
    pub cells: Vec<Cell>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMessage {
    Hello,
    Subscribe,
    SpawnSprout { x: i32, y: i32, facing: Direction },
    SetPaused(bool),
    Step,
    SetTickHz(u32),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMessage {
    Welcome {
        world_chunks_x: u32,
        world_chunks_y: u32,
        paused: bool,
        tick_hz: u32,
        tick: u64,
    },
    ChunkSnapshot(Chunk),
    ChunkBatch {
        tick: u64,
        chunks: Vec<Chunk>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cell(sunlit: bool) -> Cell {
        Cell {
            organic: 0,
            soil_energy: 0,
            sunlit,
            occupant: Occupant::Empty,
        }
    }

    #[test]
    fn chunk_roundtrip_through_msgpack() {
        let mut cells = vec![empty_cell(true); CHUNK_AREA];

        cells[5] = Cell {
            organic: 12,
            soil_energy: 7,
            sunlit: false,
            occupant: Occupant::Sprout {
                plant: 1,
                energy: 100,
                facing: Direction::North,
                genome: Box::new(Genome::default_vine()),
                parent: Some(Direction::South),
                current_gene: 3,
            },
        };
        cells[42] = Cell {
            organic: 200,
            soil_energy: 50,
            sunlit: true,
            occupant: Occupant::Seed {
                plant: 1,
                energy: 5,
                facing: Direction::West,
                genome: Box::new(Genome::default_vine()),
                parent: None,
            },
        };
        cells[100] = Cell {
            organic: 0,
            soil_energy: 0,
            sunlit: true,
            occupant: Occupant::Leaf {
                plant: 1,
                energy: 30,
                facing: Direction::East,
                parent: Some(Direction::West),
            },
        };

        let original = Chunk {
            coord: ChunkCoord { x: -3, y: 7 },
            cells,
        };

        let bytes = rmp_serde::to_vec(&original).expect("encode");
        let decoded: Chunk = rmp_serde::from_slice(&bytes).expect("decode");

        assert_eq!(decoded.coord, original.coord);
        assert_eq!(decoded.cells.len(), CHUNK_AREA);

        let Occupant::Sprout {
            plant,
            energy,
            facing,
            ref genome,
            parent,
            current_gene,
        } = decoded.cells[5].occupant
        else {
            panic!("expected sprout at index 5");
        };
        assert_eq!(plant, 1);
        assert_eq!(energy, 100);
        assert_eq!(facing, Direction::North);
        assert_eq!(genome.genes.len(), GENOME_LEN);
        assert_eq!(parent, Some(Direction::South));
        assert_eq!(current_gene, 3);
        assert_eq!(decoded.cells[5].organic, 12);
        assert!(!decoded.cells[5].sunlit);

        let Occupant::Seed { facing, .. } = decoded.cells[42].occupant else {
            panic!("expected seed at index 42");
        };
        assert_eq!(facing, Direction::West);

        let Occupant::Leaf { energy, facing, .. } = decoded.cells[100].occupant else {
            panic!("expected leaf at index 100");
        };
        assert_eq!(energy, 30);
        assert_eq!(facing, Direction::East);
    }

    #[test]
    fn server_message_roundtrip() {
        let msg = ServerMessage::Welcome {
            world_chunks_x: 16,
            world_chunks_y: 16,
            paused: false,
            tick_hz: 10,
            tick: 42,
        };
        let bytes = rmp_serde::to_vec(&msg).expect("encode");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("decode");
        match decoded {
            ServerMessage::Welcome {
                world_chunks_x,
                world_chunks_y,
                paused,
                tick_hz,
                tick,
            } => {
                assert_eq!(world_chunks_x, 16);
                assert_eq!(world_chunks_y, 16);
                assert!(!paused);
                assert_eq!(tick_hz, 10);
                assert_eq!(tick, 42);
            }
            _ => panic!("expected Welcome"),
        }
    }
}
