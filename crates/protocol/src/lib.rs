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

/// Initial genome size for fresh sprouts. Genomes mutate in size over
/// generations (insert/delete events at copy time), bounded by
/// `GENOME_MIN` and `GENOME_MAX`.
pub const GENOME_LEN: usize = 32;
pub const GENOME_MIN: usize = 1;
pub const GENOME_MAX: usize = 128;

/// Initial per-genome mutation rate. Each lineage's actual rate
/// drifts under selection pressure (mutation_rate is itself
/// mutable per-copy).
pub const DEFAULT_MUTATION_RATE: f32 = 0.05;
pub const MUTATION_RATE_MAX: f32 = 0.2;
/// Hard floor for genome mutation rates. Prevents an absorbing state
/// at zero (where the meta-mutation gate `rng < rate` can never fire
/// again) and gives every lineage at least a tiny pressure toward
/// change. Clamped in `mutate_genome`.
pub const MUTATION_RATE_MIN: f32 = 0.01;

/// Live-tunable simulation scalars. The server owns the authoritative
/// copy in `SimState.params`; the viewer mirrors it via `Welcome` and
/// edits via `ClientMessage::SetSimParams`. Kernel scales are float
/// multipliers applied to fixed bell-curve base kernels — the shape
/// stays a 3×3 [[1,2,1],[2,4,2],[1,2,1]] bell, the scale just dials
/// magnitude up or down (1.0 = stock).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SimParams {
    pub leaf_photosynthesis: u16,
    pub upkeep_default: u16,
    pub upkeep_seed: u16,
    pub upkeep_sprout: u16,
    pub soil_energy_rest: u16,
    pub soil_energy_regulation: u16,
    pub seed_dropoff_threshold: u16,
    pub soil_organic_poison: u16,
    pub soil_energy_poison: u16,
    pub cost_leaf: u16,
    pub cost_root: u16,
    pub cost_antenna: u16,
    pub cost_sprout: u16,
    pub cost_seed: u16,
    pub root_pull_scale: f32,
    pub antenna_pull_scale: f32,
    pub death_deposit_scale: f32,
    /// When true, world edges wrap (toroidal — opposite edges are
    /// neighbors). When false, edges are hard walls. Live-tunable; the
    /// world geometry doesn't change, only the neighbor lookup rule.
    pub world_wrap: bool,
}

/// World-generation knobs. Applied at world-build time only — changing
/// these has no effect until the user regenerates. Bundled in
/// `ClientMessage::RegenerateWorld` and broadcast back via `Welcome` so
/// viewers can populate their regen dialog with the values currently in
/// effect.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct WorldGenParams {
    /// World size in chunks. Total cells = chunks_x * chunks_y *
    /// CHUNK_AREA. Defaults match the CLI defaults at first connect.
    pub chunks_x: u32,
    pub chunks_y: u32,
    /// Number of "boxes" laid out in a `boxes_x × boxes_y` grid. Each
    /// box has its own toxic border + sunlit interior; total clan
    /// count = boxes_x * boxes_y.
    pub boxes_x: u32,
    pub boxes_y: u32,
    /// Per-box sunlit inset (fraction of each box edge). 0.0 = whole
    /// box lit; 0.5 = nothing lit.
    pub sunlit_margin_frac: f32,
    /// Spacing between initial sprouts. Smaller = denser starting
    /// population.
    pub sprout_grid_spacing: u32,
    /// Toxic-border thickness in cells. 0 disables the border (no
    /// box separation).
    pub toxic_border_thickness: u32,
    /// Organic seeded into toxic-border cells. Above
    /// `SimParams::soil_organic_poison` to act as a wall.
    pub toxic_border_organic: u16,
    /// Organic seeded into ordinary cells (everywhere outside the
    /// toxic borders).
    pub default_organic: u16,
    /// Soil-energy seeded into every cell at world build.
    /// `SimParams::soil_energy_regulation` then drifts cells toward
    /// `SimParams::soil_energy_rest` each tick.
    pub default_soil_energy: u16,
    /// Half-width of the log2 spread used to draw initial sprout
    /// mutation rates around DEFAULT. 0 = uniform DEFAULT, 3 = ±3
    /// octaves (rate × 1/8 .. × 8).
    pub initial_mutation_rate_octaves: f32,
}

impl Default for WorldGenParams {
    fn default() -> Self {
        Self {
            chunks_x: 36,
            chunks_y: 24,
            boxes_x: 3,
            boxes_y: 2,
            sunlit_margin_frac: 0.10,
            sprout_grid_spacing: 6,
            toxic_border_thickness: 2,
            toxic_border_organic: 1000,
            default_organic: 0,
            default_soil_energy: 10,
            initial_mutation_rate_octaves: 3.0,
        }
    }
}

impl Default for SimParams {
    fn default() -> Self {
        Self {
            leaf_photosynthesis: 20,
            upkeep_default: 2,
            upkeep_seed: 1,
            upkeep_sprout: 5,
            soil_energy_rest: 10,
            soil_energy_regulation: 1,
            seed_dropoff_threshold: 500,
            soil_organic_poison: 400,
            soil_energy_poison: 1000,
            cost_leaf: 30,
            cost_root: 30,
            cost_antenna: 30,
            cost_sprout: 60,
            cost_seed: 90,
            root_pull_scale: 1.0,
            antenna_pull_scale: 1.0,
            death_deposit_scale: 1.0,
            world_wrap: true,
        }
    }
}

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

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Genome {
    pub genes: Vec<Gene>,
    /// Per-genome mutation rate. Mutates with each copy, so a lineage
    /// can evolve toward stability or chaos under selection. Bounded
    /// by [0.0, MUTATION_RATE_MAX].
    pub mutation_rate: f32,
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
        Self {
            genes,
            mutation_rate: DEFAULT_MUTATION_RATE,
        }
    }
}

/// Inherited "clan" identifier — set on initial sprouts based on which
/// box they were placed in, propagates through every descendant
/// (growth, eating, germination). Lets the viewer color cells by where
/// the lineage originally came from, even after it has invaded another
/// box.
pub type ClanId = u8;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Occupant {
    Empty,
    Leaf {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        facing: Direction,
        parent: Option<Direction>,
    },
    Root {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Stem {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        connections: u8,
        parent: Option<Direction>,
        children: u8,
    },
    Antenna {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Sprout {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        facing: Direction,
        genome: Box<Genome>,
        parent: Option<Direction>,
        current_gene: u8,
    },
    Seed {
        plant: PlantId,
        clan: ClanId,
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
    /// mutation_rate of the lineage that occupies this cell, copied
    /// directly from the parent sprout's `genome.mutation_rate` whenever
    /// a plant cell is written. Stale when the cell is Empty — viewer
    /// only renders it when an occupant is present.
    pub lineage_mutation_rate: f32,
    pub occupant: Occupant,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Chunk {
    pub coord: ChunkCoord,
    pub cells: Vec<Cell>,
}

/// Lightweight occupant for the live wire feed. Same shape as `Occupant`
/// minus the genome, which dominates msgpack work for sprouts/seeds. The
/// viewer doesn't need genomes to render, so we strip them at emit time
/// and surface them via a separate request when the user inspects a cell.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum WireOccupant {
    Empty,
    Leaf {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        facing: Direction,
        parent: Option<Direction>,
    },
    Root {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Stem {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        connections: u8,
        parent: Option<Direction>,
        children: u8,
    },
    Antenna {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        parent: Option<Direction>,
    },
    Sprout {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        facing: Direction,
        parent: Option<Direction>,
        current_gene: u8,
    },
    Seed {
        plant: PlantId,
        clan: ClanId,
        energy: Energy,
        facing: Direction,
        parent: Option<Direction>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WireCell {
    pub organic: u16,
    pub soil_energy: u16,
    pub sunlit: bool,
    pub lineage_mutation_rate: f32,
    pub occupant: WireOccupant,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WireChunk {
    pub coord: ChunkCoord,
    pub cells: Vec<WireCell>,
}

impl From<&Occupant> for WireOccupant {
    fn from(o: &Occupant) -> Self {
        match o {
            Occupant::Empty => WireOccupant::Empty,
            Occupant::Leaf {
                plant,
                clan,
                energy,
                facing,
                parent,
            } => WireOccupant::Leaf {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                facing: *facing,
                parent: *parent,
            },
            Occupant::Root {
                plant,
                clan,
                energy,
                parent,
            } => WireOccupant::Root {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                parent: *parent,
            },
            Occupant::Stem {
                plant,
                clan,
                energy,
                connections,
                parent,
                children,
            } => WireOccupant::Stem {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                connections: *connections,
                parent: *parent,
                children: *children,
            },
            Occupant::Antenna {
                plant,
                clan,
                energy,
                parent,
            } => WireOccupant::Antenna {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                parent: *parent,
            },
            Occupant::Sprout {
                plant,
                clan,
                energy,
                facing,
                parent,
                current_gene,
                ..
            } => WireOccupant::Sprout {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                facing: *facing,
                parent: *parent,
                current_gene: *current_gene,
            },
            Occupant::Seed {
                plant,
                clan,
                energy,
                facing,
                parent,
                ..
            } => WireOccupant::Seed {
                plant: *plant,
                clan: *clan,
                energy: *energy,
                facing: *facing,
                parent: *parent,
            },
        }
    }
}

impl From<&Cell> for WireCell {
    fn from(c: &Cell) -> Self {
        WireCell {
            organic: c.organic,
            soil_energy: c.soil_energy,
            sunlit: c.sunlit,
            lineage_mutation_rate: c.lineage_mutation_rate,
            occupant: WireOccupant::from(&c.occupant),
        }
    }
}

impl From<&Chunk> for WireChunk {
    fn from(c: &Chunk) -> Self {
        WireChunk {
            coord: c.coord,
            cells: c.cells.iter().map(WireCell::from).collect(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMessage {
    Hello,
    Subscribe,
    SpawnSprout {
        x: i32,
        y: i32,
        facing: Direction,
    },
    SetPaused(bool),
    Step,
    SetTickHz(u32),
    /// Toggle whether the server respects `tick_hz`. When false the
    /// server runs as fast as it can; when true it sleeps between
    /// ticks to honor the requested rate.
    SetTickRateLimited(bool),
    /// Replace the server's live tunable scalars wholesale. Server
    /// rebroadcasts a fresh `Welcome` so any other connected viewers
    /// pick up the new values.
    SetSimParams(SimParams),
    /// Rebuild the world from scratch with the given seed and
    /// generation params. World dims, box layout, sunlight, etc. all
    /// take effect on the new build. Server broadcasts a fresh
    /// `Welcome` so viewers refresh their mirror.
    RegenerateWorld {
        seed: u64,
        params: WorldGenParams,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMessage {
    Welcome {
        world_chunks_x: u32,
        world_chunks_y: u32,
        paused: bool,
        tick_hz: u32,
        tick_rate_limited: bool,
        tick: u64,
        seed: u64,
        sim_params: SimParams,
        world_gen_params: WorldGenParams,
    },
    ChunkBatch {
        tick: u64,
        chunks: Vec<WireChunk>,
    },
}

/// zstd compression level for ServerMessage payloads on the wire. Level 1
/// is ~500 MB/s with ratios in the 5–20× range on the mostly-empty chunk
/// data we send.
const SERVER_MSG_ZSTD_LEVEL: i32 = 1;

/// Worker threads zstd uses internally for ChunkBatch encoding. zstd
/// splits the input into independently-compressed jobs across this many
/// worker threads — most of the per-tick zstd cost is parallelizable on
/// our 14 MB+ inputs. 0 = single-threaded (no extra threads spawned).
const SERVER_MSG_ZSTD_WORKERS: u32 = 4;

/// Encode a `ServerMessage` for the wire: msgpack, then zstd (with
/// libzstd's internal multi-threading). Symmetric with
/// [`decode_server_message`].
pub fn encode_server_message(msg: &ServerMessage) -> std::io::Result<Vec<u8>> {
    use std::io::Write;

    let raw = {
        let _span = tracing::info_span!("encode_msgpack").entered();
        rmp_serde::to_vec(msg).map_err(std::io::Error::other)?
    };
    let _span = tracing::info_span!("encode_zstd", raw_bytes = raw.len()).entered();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), SERVER_MSG_ZSTD_LEVEL)?;
    if SERVER_MSG_ZSTD_WORKERS > 0 {
        // Best-effort: if the linked libzstd was built without
        // ZSTD_MULTITHREAD support this errors silently and we fall
        // back to single-threaded compression.
        let _ = encoder.multithread(SERVER_MSG_ZSTD_WORKERS);
    }
    encoder.write_all(&raw)?;
    encoder.finish()
}

/// Decode a `ServerMessage` from the wire: zstd, then msgpack.
pub fn decode_server_message(buf: &[u8]) -> std::io::Result<ServerMessage> {
    let raw = {
        let _span = tracing::info_span!("decode_zstd", wire_bytes = buf.len()).entered();
        zstd::decode_all(buf)?
    };
    let _span = tracing::info_span!("decode_msgpack", raw_bytes = raw.len()).entered();
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
            lineage_mutation_rate: 0.0,
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
            lineage_mutation_rate: 0.0,
            occupant: Occupant::Sprout {
                plant: 1,
                clan: 0,
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
            lineage_mutation_rate: 0.0,
            occupant: Occupant::Seed {
                plant: 1,
                clan: 0,
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
            lineage_mutation_rate: 0.0,
            occupant: Occupant::Leaf {
                plant: 1,
                clan: 0,
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
            clan: _,
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
            tick_rate_limited: false,
            tick: 42,
            seed: 0xCAFE_BABE_DEAD_BEEF,
            sim_params: SimParams::default(),
            world_gen_params: WorldGenParams::default(),
        };
        let bytes = rmp_serde::to_vec(&msg).expect("encode");
        let decoded: ServerMessage = rmp_serde::from_slice(&bytes).expect("decode");
        match decoded {
            ServerMessage::Welcome {
                world_chunks_x,
                world_chunks_y,
                paused,
                tick_hz,
                tick_rate_limited,
                tick,
                seed,
                sim_params: _,
                world_gen_params: _,
            } => {
                assert_eq!(world_chunks_x, 16);
                assert_eq!(world_chunks_y, 16);
                assert!(!paused);
                assert_eq!(tick_hz, 10);
                assert!(!tick_rate_limited);
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
        assert_client_msg_roundtrips(ClientMessage::RegenerateWorld {
            seed: 0xDEAD_BEEF_CAFE_BABE,
            params: WorldGenParams::default(),
        });
    }

    fn roundtrip_occupant(occ: Occupant) -> Occupant {
        let cell = Cell {
            organic: 0,
            soil_energy: 0,
            sunlit: false,
            lineage_mutation_rate: 0.0,
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
            clan: 0,
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
            clan: 0,
            energy: 50,
            parent: None,
        });
        assert!(matches!(root, Occupant::Root { parent: None, .. }));

        let antenna = roundtrip_occupant(Occupant::Antenna {
            plant: 2,
            clan: 0,
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
            clan: 0,
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
            clan: 0,
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
            clan: 0,
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

    fn empty_wire_cell(sunlit: bool) -> WireCell {
        WireCell {
            organic: 0,
            soil_energy: 0,
            sunlit,
            lineage_mutation_rate: 0.0,
            occupant: WireOccupant::Empty,
        }
    }

    #[test]
    fn chunk_batch_roundtrip() {
        let cells = vec![empty_wire_cell(true); CHUNK_AREA];
        let chunk = WireChunk {
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
        let cells = vec![empty_wire_cell(true); CHUNK_AREA];
        let chunk = WireChunk {
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
