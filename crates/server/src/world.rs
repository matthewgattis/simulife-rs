use protocol::{
    CHUNK_AREA, CHUNK_EDGE, Cell, Chunk, ChunkCoord, Direction, Genome, Occupant, WorldGenParams,
};
use rand::Rng;

pub fn build_world(params: &WorldGenParams) -> Vec<Chunk> {
    let chunks_x = params.chunks_x;
    let chunks_y = params.chunks_y;
    let boxes_x = params.boxes_x.max(1);
    let boxes_y = params.boxes_y.max(1);
    let toxic_thickness = params.toxic_border_thickness;
    let toxic_organic = params.toxic_border_organic;
    let default_organic = params.default_organic;
    let mut chunks = Vec::with_capacity((chunks_x * chunks_y) as usize);
    let total_w = chunks_x * CHUNK_EDGE as u32;
    let total_h = chunks_y * CHUNK_EDGE as u32;
    // Per-box dimensions and per-box sunlit margin.
    let box_w = (total_w / boxes_x).max(1);
    let box_h = (total_h / boxes_y).max(1);
    let margin_x = (box_w as f32 * params.sunlit_margin_frac) as u32;
    let margin_y = (box_h as f32 * params.sunlit_margin_frac) as u32;
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            let cells = (0..CHUNK_AREA)
                .map(|i| {
                    let local_x = (i % CHUNK_EDGE as usize) as u32;
                    let local_y = (i / CHUNK_EDGE as usize) as u32;
                    let world_x = cx * CHUNK_EDGE as u32 + local_x;
                    let world_y = cy * CHUNK_EDGE as u32 + local_y;
                    let bx = world_x % box_w;
                    let by = world_y % box_h;
                    let sunlit = bx >= margin_x
                        && bx < box_w.saturating_sub(margin_x)
                        && by >= margin_y
                        && by < box_h.saturating_sub(margin_y);
                    let in_toxic_border = toxic_thickness > 0
                        && (bx < toxic_thickness
                            || by < toxic_thickness
                            || bx >= box_w.saturating_sub(toxic_thickness)
                            || by >= box_h.saturating_sub(toxic_thickness));
                    let organic = if in_toxic_border {
                        toxic_organic
                    } else {
                        default_organic
                    };
                    Cell {
                        organic,
                        soil_energy: params.default_soil_energy,
                        sunlit,
                        lineage_mutation_rate: 0.0,
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
    params: &WorldGenParams,
    rng: &mut impl Rng,
) -> u32 {
    let chunks_x = params.chunks_x;
    let chunks_y = params.chunks_y;
    let boxes_x = params.boxes_x.max(1);
    let boxes_y = params.boxes_y.max(1);
    let total_w = chunks_x as i32 * CHUNK_EDGE as i32;
    let total_h = chunks_y as i32 * CHUNK_EDGE as i32;
    let box_w = (total_w / boxes_x as i32).max(1);
    let box_h = (total_h / boxes_y as i32).max(1);
    let spacing = params.sprout_grid_spacing.max(1) as i32;
    let octaves = params.initial_mutation_rate_octaves.max(0.0);
    let mut count = 0u32;
    let mut y = spacing;
    while y < total_h - spacing {
        let mut x = spacing;
        while x < total_w - spacing {
            if let Some(cell) = cell_at(chunks, chunks_x, x, y)
                && cell.sunlit
            {
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
                // Log-uniform spread around DEFAULT controlled by
                // params.initial_mutation_rate_octaves. 0 = no
                // spread (everyone starts at DEFAULT).
                let oct = if octaves > 0.0 {
                    rng.gen_range(-octaves..octaves)
                } else {
                    0.0
                };
                let rate = protocol::DEFAULT_MUTATION_RATE * 2f32.powf(oct);
                genome.mutation_rate =
                    rate.clamp(protocol::MUTATION_RATE_MIN, protocol::MUTATION_RATE_MAX);
                let stamp_rate = genome.mutation_rate;
                // Clan: which 2D box this sprout starts in. Encoded
                // row-major: clan = box_y * boxes_x + box_x.
                let bx = (x / box_w) as u32;
                let by = (y / box_h) as u32;
                let clan = (by * boxes_x + bx) as protocol::ClanId;
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
                    cell.lineage_mutation_rate = stamp_rate;
                }
            }
            x += spacing;
        }
        y += spacing;
    }
    count
}

fn cell_at(chunks: &[Chunk], chunks_x: u32, x: i32, y: i32) -> Option<&Cell> {
    if x < 0 || y < 0 {
        return None;
    }
    let edge = CHUNK_EDGE as i32;
    let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
    let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
    chunks.get(chunk_idx)?.cells.get(cell_idx)
}

fn cell_at_mut(chunks: &mut [Chunk], chunks_x: u32, x: i32, y: i32) -> Option<&mut Cell> {
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
    if let Some(chunk) = chunks.get_mut(chunk_idx)
        && let Some(cell) = chunk.cells.get_mut(cell_idx)
    {
        cell.occupant = occupant;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    // Test fixtures pin to the production defaults so existing
    // assertions about box layout / borders / sunlight still hold.
    const BOXES_X: u32 = 3;
    const BOXES_Y: u32 = 2;
    const TOXIC_BORDER_THICKNESS: u32 = 2;
    const TOXIC_BORDER_ORGANIC: u16 = 1000;
    const DEFAULT_ORGANIC: u16 = 40;
    const SUNLIT_MARGIN_FRAC: f32 = 0.10;

    fn params(chunks_x: u32, chunks_y: u32) -> WorldGenParams {
        WorldGenParams {
            chunks_x,
            chunks_y,
            boxes_x: BOXES_X,
            boxes_y: BOXES_Y,
            sunlit_margin_frac: SUNLIT_MARGIN_FRAC,
            sprout_grid_spacing: 6,
            toxic_border_thickness: TOXIC_BORDER_THICKNESS,
            toxic_border_organic: TOXIC_BORDER_ORGANIC,
            default_organic: DEFAULT_ORGANIC,
            default_soil_energy: 100,
            initial_mutation_rate_octaves: 3.0,
        }
    }

    fn cell_at_test(chunks: &[Chunk], chunks_x: u32, x: i32, y: i32) -> &Cell {
        let edge = CHUNK_EDGE as i32;
        let chunk_idx = (y / edge) as usize * chunks_x as usize + (x / edge) as usize;
        let cell_idx = (y % edge) as usize * (CHUNK_EDGE as usize) + (x % edge) as usize;
        &chunks[chunk_idx].cells[cell_idx]
    }

    #[test]
    fn build_world_lays_out_chunks_in_row_major_order() {
        let chunks = build_world(&params(3, 2));
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
        // 6x4 chunk world = 192×128. With BOXES_X=3, BOXES_Y=2, each box
        // is 64×64. We want a cell well inside box (0,0)'s interior.
        let chunks = build_world(&params(6, 4));
        for chunk in &chunks {
            for cell in &chunk.cells {
                assert_eq!(cell.soil_energy, 100);
                assert!(matches!(cell.occupant, Occupant::Empty));
            }
        }
        // Box (0,0) corner → in toxic border.
        assert_eq!(cell_at_test(&chunks, 6, 0, 0).organic, TOXIC_BORDER_ORGANIC);
        // Center of box (0,0) at (32, 32) → default soil organic.
        assert_eq!(cell_at_test(&chunks, 6, 32, 32).organic, DEFAULT_ORGANIC);
    }

    #[test]
    fn build_world_toxic_border_rings_each_box() {
        // 6x4 chunks = 192×128. With BOXES_X=3, BOXES_Y=2 → each box 64×64.
        let chunks = build_world(&params(6, 4));
        let box_w = 6 * CHUNK_EDGE as i32 / BOXES_X as i32;
        let box_h = 4 * CHUNK_EDGE as i32 / BOXES_Y as i32;
        let t = TOXIC_BORDER_THICKNESS as i32;
        // Box (0,0): all four sides have a toxic border.
        assert_eq!(
            cell_at_test(&chunks, 6, 0, box_h / 2).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) left edge"
        );
        assert_eq!(
            cell_at_test(&chunks, 6, box_w - 1, box_h / 2).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) right edge (interior wall)"
        );
        assert_eq!(
            cell_at_test(&chunks, 6, box_w / 2, 0).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) top edge"
        );
        assert_eq!(
            cell_at_test(&chunks, 6, box_w / 2, box_h - 1).organic,
            TOXIC_BORDER_ORGANIC,
            "box (0,0) bottom edge (interior wall)"
        );
        // Just inside box (0,0)'s left border.
        assert_eq!(
            cell_at_test(&chunks, 6, t, box_h / 2).organic,
            DEFAULT_ORGANIC
        );
        // Last toxic cell on box (0,0)'s left border.
        assert_eq!(
            cell_at_test(&chunks, 6, t - 1, box_h / 2).organic,
            TOXIC_BORDER_ORGANIC
        );
        // Box (2,1) (bottom-right) has its own border — local (0,0) of
        // that box is at world (2*box_w, box_h).
        assert_eq!(
            cell_at_test(&chunks, 6, 2 * box_w, box_h).organic,
            TOXIC_BORDER_ORGANIC
        );
        // Center of box (2,1).
        assert_eq!(
            cell_at_test(&chunks, 6, 2 * box_w + box_w / 2, box_h + box_h / 2).organic,
            DEFAULT_ORGANIC
        );
    }

    #[test]
    fn build_world_sunlit_is_central_rectangle_per_box() {
        // 6x4 chunks = 192×128. Each box 64×64. Per-box margin = 12.
        let chunks = build_world(&params(6, 4));
        let box_w = 6 * CHUNK_EDGE as i32 / BOXES_X as i32;
        let box_h = 4 * CHUNK_EDGE as i32 / BOXES_Y as i32;
        let margin_x = (box_w as f32 * SUNLIT_MARGIN_FRAC) as i32;
        // Inside box (0,0)'s lit center.
        assert!(cell_at_test(&chunks, 6, box_w / 2, box_h / 2).sunlit);
        // Just inside the per-box margin.
        assert!(cell_at_test(&chunks, 6, margin_x, box_h / 2).sunlit);
        // Just outside the per-box margin → dark.
        assert!(!cell_at_test(&chunks, 6, margin_x - 1, box_h / 2).sunlit);
        // Box (0,0) corner → dark.
        assert!(!cell_at_test(&chunks, 6, 0, 0).sunlit);
        // The cell at the boundary between box (0,0) and box (1,0) is in
        // box (0,0)'s right border → dark.
        assert!(!cell_at_test(&chunks, 6, box_w - 1, box_h / 2).sunlit);
        // Box (2,1) (bottom-right) also has its own lit center.
        assert!(cell_at_test(&chunks, 6, 2 * box_w + box_w / 2, box_h + box_h / 2).sunlit);
    }

    #[test]
    fn place_random_sprout_grid_places_sprouts_only_in_lit_region() {
        let mut chunks = build_world(&params(6, 4));
        let mut rng = rand_chacha::ChaCha12Rng::seed_from_u64(123);
        let count = place_random_sprout_grid(&mut chunks, &params(6, 4), &mut rng);
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
        let mut chunks = build_world(&params(1, 1));
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
        let mut chunks = build_world(&params(2, 1));
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
