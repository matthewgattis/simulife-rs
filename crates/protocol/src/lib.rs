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
        seed: u64,
    },
    ChunkSnapshot(Chunk),
    ChunkBatch {
        tick: u64,
        chunks: Vec<Chunk>,
    },
}

/// zstd compression level for ServerMessage payloads on the wire. Level 1
/// is ~500 MB/s with ratios in the 5–20× range on the mostly-empty chunk
/// data we send.
const SERVER_MSG_ZSTD_LEVEL: i32 = 1;

/// Encode a `ServerMessage` for the wire: msgpack, then zstd. Symmetric
/// with [`decode_server_message`].
pub fn encode_server_message(msg: &ServerMessage) -> std::io::Result<Vec<u8>> {
    let raw = rmp_serde::to_vec(msg).map_err(std::io::Error::other)?;
    zstd::encode_all(&raw[..], SERVER_MSG_ZSTD_LEVEL)
}

/// Decode a `ServerMessage` from the wire: zstd, then msgpack.
pub fn decode_server_message(buf: &[u8]) -> std::io::Result<ServerMessage> {
    let raw = zstd::decode_all(buf)?;
    rmp_serde::from_slice(&raw).map_err(std::io::Error::other)
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
            seed: 0xCAFE_BABE_DEAD_BEEF,
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
                seed,
            } => {
                assert_eq!(world_chunks_x, 16);
                assert_eq!(world_chunks_y, 16);
                assert!(!paused);
                assert_eq!(tick_hz, 10);
                assert_eq!(tick, 42);
                assert_eq!(seed, 0xCAFE_BABE_DEAD_BEEF);
            }
            _ => panic!("expected Welcome"),
        }
    }

    fn assert_client_msg_roundtrips(msg: ClientMessage) {
        let bytes = rmp_serde::to_vec(&msg).expect("encode");
        let decoded: ClientMessage = rmp_serde::from_slice(&bytes).expect("decode");
        // Compare via Debug because ClientMessage doesn't impl PartialEq.
        assert_eq!(format!("{msg:?}"), format!("{decoded:?}"));
    }

    #[test]
    fn client_message_variants_roundtrip() {
        assert_client_msg_roundtrips(ClientMessage::Hello);
        assert_client_msg_roundtrips(ClientMessage::Subscribe);
        assert_client_msg_roundtrips(ClientMessage::SpawnSprout {
            x: -3,
            y: 17,
            facing: Direction::West,
        });
        assert_client_msg_roundtrips(ClientMessage::SetPaused(true));
        assert_client_msg_roundtrips(ClientMessage::SetPaused(false));
        assert_client_msg_roundtrips(ClientMessage::Step);
        assert_client_msg_roundtrips(ClientMessage::SetTickHz(60));
    }

    fn roundtrip_occupant(occ: Occupant) -> Occupant {
        let cell = Cell {
            organic: 0,
            soil_energy: 0,
            sunlit: false,
            occupant: occ,
        };
        let bytes = rmp_serde::to_vec(&cell).expect("encode");
        let decoded: Cell = rmp_serde::from_slice(&bytes).expect("decode");
        decoded.occupant
    }

    #[test]
    fn every_occupant_variant_roundtrips() {
        assert!(matches!(
            roundtrip_occupant(Occupant::Empty),
            Occupant::Empty
        ));

        let leaf = roundtrip_occupant(Occupant::Leaf {
            plant: 9,
            energy: 100,
            facing: Direction::East,
            parent: Some(Direction::West),
        });
        match leaf {
            Occupant::Leaf {
                plant,
                facing,
                parent,
                ..
            } => {
                assert_eq!(plant, 9);
                assert_eq!(facing, Direction::East);
                assert_eq!(parent, Some(Direction::West));
            }
            _ => panic!("leaf"),
        }

        let root = roundtrip_occupant(Occupant::Root {
            plant: 1,
            energy: 50,
            parent: None,
        });
        assert!(matches!(root, Occupant::Root { parent: None, .. }));

        let antenna = roundtrip_occupant(Occupant::Antenna {
            plant: 2,
            energy: 7,
            parent: Some(Direction::North),
        });
        assert!(matches!(
            antenna,
            Occupant::Antenna {
                parent: Some(Direction::North),
                ..
            }
        ));

        let stem = roundtrip_occupant(Occupant::Stem {
            plant: 3,
            energy: 0,
            connections: STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH,
            parent: Some(Direction::South),
            children: STEM_CONNECT_NORTH,
        });
        match stem {
            Occupant::Stem {
                connections,
                children,
                ..
            } => {
                assert_eq!(connections, STEM_CONNECT_NORTH | STEM_CONNECT_SOUTH);
                assert_eq!(children, STEM_CONNECT_NORTH);
            }
            _ => panic!("stem"),
        }

        let sprout = roundtrip_occupant(Occupant::Sprout {
            plant: 4,
            energy: 25,
            facing: Direction::South,
            genome: Box::new(Genome::default_vine()),
            parent: Some(Direction::North),
            current_gene: 5,
        });
        match sprout {
            Occupant::Sprout {
                current_gene,
                genome,
                ..
            } => {
                assert_eq!(current_gene, 5);
                assert_eq!(genome.genes.len(), GENOME_LEN);
            }
            _ => panic!("sprout"),
        }

        let seed = roundtrip_occupant(Occupant::Seed {
            plant: 5,
            energy: 30,
            facing: Direction::West,
            genome: Box::new(Genome::default_vine()),
            parent: None,
        });
        assert!(matches!(seed, Occupant::Seed { parent: None, .. }));
    }

    #[test]
    fn default_vine_genome_has_active_first_gene_and_dormant_rest() {
        let g = Genome::default_vine();
        assert_eq!(g.genes.len(), GENOME_LEN);

        // Gene 0: active vine — front sprout, two leaf side branches.
        assert_eq!(g.genes[0].front, SlotProduct::Sprout);
        assert_eq!(g.genes[0].left, SlotProduct::Leaf);
        assert_eq!(g.genes[0].right, SlotProduct::Leaf);
        assert_eq!(g.genes[0].next, 0);

        // The rest are dormant.
        for gene in &g.genes[1..] {
            assert_eq!(gene.front, SlotProduct::Nothing);
            assert_eq!(gene.left, SlotProduct::Nothing);
            assert_eq!(gene.right, SlotProduct::Nothing);
        }
    }

    #[test]
    fn gene_default_is_all_nothing() {
        let gene = Gene::default();
        assert_eq!(gene.front, SlotProduct::Nothing);
        assert_eq!(gene.left, SlotProduct::Nothing);
        assert_eq!(gene.right, SlotProduct::Nothing);
        assert_eq!(gene.next, 0);
    }

    #[test]
    fn chunk_batch_roundtrip() {
        let cells = vec![empty_cell(true); CHUNK_AREA];
        let chunk = Chunk {
            coord: ChunkCoord { x: 0, y: 0 },
            cells,
        };
        let msg = ServerMessage::ChunkBatch {
            tick: 1234,
            chunks: vec![chunk],
        };
        let bytes = rmp_serde::to_vec(&msg).expect("encode");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("decode");
        match decoded {
            ServerMessage::ChunkBatch { tick, chunks } => {
                assert_eq!(tick, 1234);
                assert_eq!(chunks.len(), 1);
            }
            _ => panic!("expected ChunkBatch"),
        }
    }

    #[test]
    fn server_message_zstd_roundtrips_and_compresses() {
        // Build a non-trivial payload: a full chunk of empty cells, which
        // should compress strongly.
        let cells = vec![empty_cell(true); CHUNK_AREA];
        let chunk = Chunk {
            coord: ChunkCoord { x: 0, y: 0 },
            cells,
        };
        let msg = ServerMessage::ChunkBatch {
            tick: 7,
            chunks: vec![chunk],
        };

        let raw = rmp_serde::to_vec(&msg).expect("raw msgpack");
        let wire = encode_server_message(&msg).expect("encode");

        // Compression should actually shrink the payload meaningfully.
        assert!(
            wire.len() < raw.len() / 2,
            "expected ≥2× compression, got {} → {}",
            raw.len(),
            wire.len()
        );

        let decoded = decode_server_message(&wire).expect("decode");
        match decoded {
            ServerMessage::ChunkBatch { tick, chunks } => {
                assert_eq!(tick, 7);
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].cells.len(), CHUNK_AREA);
            }
            _ => panic!("expected ChunkBatch"),
        }
    }

    #[test]
    fn server_message_decode_rejects_garbage() {
        // Empty buffer can't even be a zstd frame.
        assert!(decode_server_message(&[]).is_err());
        // Random bytes that aren't a valid zstd frame.
        assert!(decode_server_message(&[0, 1, 2, 3, 4, 5]).is_err());
    }
}
