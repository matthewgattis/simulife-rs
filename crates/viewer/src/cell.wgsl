struct Camera {
    view_proj: mat4x4<f32>,
}

struct World {
    layer_flags: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

struct Cell {
    organic: u32,
    soil_energy: u32,
    sunlit: u32,
    kind: u32,
    plant: u32,
    energy: u32,
    facing: u32,
    connections: u32,
    clan: u32,
    mutation_rate: f32,
    _pad0: u32,
    _pad1: u32,
}

@group(0) @binding(0) var<uniform> camera: Camera;
@group(0) @binding(1) var<uniform> world: World;
@group(0) @binding(2) var<storage, read> cells: array<Cell>;

struct VsIn {
    @location(0) corner: vec2<f32>,
}

struct InstanceIn {
    @location(1) chunk_pos: vec2<f32>,
    @location(2) chunk_first_cell: u32,
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) chunk_uv: vec2<f32>,
    @location(1) @interpolate(flat) chunk_first_cell: u32,
}

const CHUNK_EDGE: f32 = 32.0;
const CHUNK_EDGE_U: u32 = 32u;

const KIND_EMPTY: u32 = 0u;
const KIND_LEAF: u32 = 1u;
const KIND_ROOT: u32 = 2u;
const KIND_STEM: u32 = 3u;
const KIND_ANTENNA: u32 = 4u;
const KIND_SPROUT: u32 = 5u;
const KIND_SEED: u32 = 6u;

const FACING_N: u32 = 0u;
const FACING_S: u32 = 2u;

const STEM_N: u32 = 1u;
const STEM_E: u32 = 2u;
const STEM_S: u32 = 4u;
const STEM_W: u32 = 8u;

const LAYER_ORGANIC: u32 = 1u;
const LAYER_FG: u32 = 2u;
const LAYER_ENERGY: u32 = 4u;
const LAYER_CLAN: u32 = 8u;
const LAYER_MUTATION_RATE: u32 = 16u;

const CLEAR_COLOR: vec3<f32> = vec3<f32>(0.05, 0.07, 0.10);
const OUTLINE_COLOR: vec3<f32> = vec3<f32>(0.0, 0.0, 0.0);

@vertex
fn vs_main(v: VsIn, i: InstanceIn) -> VsOut {
    var out: VsOut;
    let world_pos = i.chunk_pos + v.corner * CHUNK_EDGE;
    out.clip_pos = camera.view_proj * vec4<f32>(world_pos, 0.0, 1.0);
    out.chunk_uv = v.corner;
    out.chunk_first_cell = i.chunk_first_cell;
    return out;
}

fn rect_sdf(uv: vec2<f32>, lo: vec2<f32>, hi: vec2<f32>) -> f32 {
    let center = (lo + hi) * 0.5;
    let half_size = (hi - lo) * 0.5;
    let q = abs(uv - center) - half_size;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
}

fn soil_color(cell: Cell, show_organic: bool, show_energy: bool) -> vec3<f32> {
    // Dark-mode palette: empty soil sits near the clear color so the
    // canvas reads as background; organic/energy add saturated tints
    // on top so plant cells and rich soil pop against the darkness.
    let base = vec3<f32>(0.10, 0.11, 0.14);
    let brown = vec3<f32>(0.55, 0.40, 0.25);
    let blue = vec3<f32>(0.20, 0.45, 0.75);
    var color = base;
    if (show_organic) {
        color = mix(color, brown, f32(cell.organic) / 400.0);
    }
    if (show_energy) {
        color = mix(color, blue, f32(cell.soil_energy) / 1000.0);
    }
    // Shadow zones are dimmer than sunlit so the lit playfield is
    // visually obvious. Wider gap than the original (0.8) since the
    // dark base needs more dimming to read as shadow.
    return color * select(0.55, 1.0, cell.sunlit != 0u);
}

fn occupant_color(cell: Cell) -> vec3<f32> {
    if (cell.kind == KIND_LEAF) { return vec3<f32>(0.20, 0.75, 0.30); }
    if (cell.kind == KIND_ROOT) { return vec3<f32>(0.50, 0.30, 0.10); }
    if (cell.kind == KIND_STEM) { return vec3<f32>(0.55, 0.45, 0.25); }
    if (cell.kind == KIND_ANTENNA) { return vec3<f32>(0.30, 0.55, 0.95); }
    if (cell.kind == KIND_SPROUT) { return vec3<f32>(1.00, 1.00, 1.00); }
    if (cell.kind == KIND_SEED) { return vec3<f32>(0.80, 0.70, 0.35); }
    return vec3<f32>(0.0);
}

// Mutation-rate gradient: blue → cyan → green → yellow → red. Uses a
// log2 scale centered on DEFAULT_MUTATION_RATE so the typical
// initial-population rate lands at green (t=0.5). Each 2× drift away
// from default moves the color one quarter of the gradient. Takes the
// raw f32 rate (no quantization) — wire format already carries f32 so
// we color directly.
fn mutation_rate_color(rate: f32) -> vec3<f32> {
    // Floor at MUTATION_RATE_MIN (0.0005) so log2 stays finite —
    // matches the server-side clamp in mutate_genome.
    let safe = max(rate, 0.0005);
    let default_rate: f32 = 0.04;
    // Half-width of the log2 axis the gradient covers, in octaves on
    // each side of DEFAULT. ±4.5 octaves reaches MUTATION_RATE_MAX
    // (log2(0.2/0.04) ≈ 2.32) plus margin below the floor.
    let half_range: f32 = 4.5;
    let t = clamp(0.5 + log2(safe / default_rate) / (2.0 * half_range), 0.0, 1.0);

    let blue = vec3<f32>(0.20, 0.40, 0.95);
    let cyan = vec3<f32>(0.20, 0.85, 0.95);
    let green = vec3<f32>(0.30, 0.85, 0.30);
    let yellow = vec3<f32>(0.95, 0.85, 0.20);
    let red = vec3<f32>(0.95, 0.25, 0.25);
    if (t < 0.25) {
        return mix(blue, cyan, t / 0.25);
    } else if (t < 0.50) {
        return mix(cyan, green, (t - 0.25) / 0.25);
    } else if (t < 0.75) {
        return mix(green, yellow, (t - 0.50) / 0.25);
    } else {
        return mix(yellow, red, (t - 0.75) / 0.25);
    }
}

// Distinct palette for the 6 boxes in a 3×2 layout, row-major:
// 0 top-left, 1 top-mid, 2 top-right, 3 bottom-left, 4 bottom-mid,
// 5 bottom-right. Beyond 6 clans we hash the id into HSL hue space.
fn clan_color(clan: u32) -> vec3<f32> {
    if (clan == 0u) { return vec3<f32>(0.95, 0.30, 0.30); } // red
    if (clan == 1u) { return vec3<f32>(0.95, 0.80, 0.20); } // amber
    if (clan == 2u) { return vec3<f32>(0.30, 0.85, 0.45); } // green
    if (clan == 3u) { return vec3<f32>(0.45, 0.55, 0.95); } // blue
    if (clan == 4u) { return vec3<f32>(0.80, 0.40, 0.95); } // purple
    if (clan == 5u) { return vec3<f32>(0.95, 0.55, 0.20); } // orange
    let h = f32(clan) * 0.6180339;
    let frac = h - floor(h);
    return vec3<f32>(
        0.5 + 0.5 * cos(6.2831853 * (frac + 0.0)),
        0.5 + 0.5 * cos(6.2831853 * (frac + 0.33)),
        0.5 + 0.5 * cos(6.2831853 * (frac + 0.66)),
    );
}

// Returns (signed_distance, aa_width). Negative d = inside the shape.
fn shape_sdf(cell: Cell, cell_uv: vec2<f32>, aa_axis: vec2<f32>) -> vec2<f32> {
    let aa_radial = max(aa_axis.x, aa_axis.y);

    if (cell.kind == KIND_LEAF) {
        var scale: vec2<f32>;
        if (cell.facing == FACING_N || cell.facing == FACING_S) {
            scale = vec2<f32>(0.25, 0.45);
        } else {
            scale = vec2<f32>(0.45, 0.25);
        }
        let p = (cell_uv - vec2<f32>(0.5)) / scale;
        let l = max(length(p), 1e-6);
        let d = l - 1.0;
        let grad_uv = vec2<f32>(p.x / l / scale.x, p.y / l / scale.y);
        let aa_d = abs(grad_uv.x) * aa_axis.x + abs(grad_uv.y) * aa_axis.y;
        return vec2<f32>(d, aa_d);
    }
    if (cell.kind == KIND_ROOT) {
        return vec2<f32>(rect_sdf(cell_uv, vec2<f32>(0.2), vec2<f32>(0.8)), aa_radial);
    }
    if (cell.kind == KIND_STEM) {
        var d = rect_sdf(cell_uv, vec2<f32>(0.4, 0.4), vec2<f32>(0.6, 0.6));
        if ((cell.connections & STEM_N) != 0u) {
            d = min(d, rect_sdf(cell_uv, vec2<f32>(0.4, 0.0), vec2<f32>(0.6, 0.5)));
        }
        if ((cell.connections & STEM_E) != 0u) {
            d = min(d, rect_sdf(cell_uv, vec2<f32>(0.5, 0.4), vec2<f32>(1.0, 0.6)));
        }
        if ((cell.connections & STEM_S) != 0u) {
            d = min(d, rect_sdf(cell_uv, vec2<f32>(0.4, 0.5), vec2<f32>(0.6, 1.0)));
        }
        if ((cell.connections & STEM_W) != 0u) {
            d = min(d, rect_sdf(cell_uv, vec2<f32>(0.0, 0.4), vec2<f32>(0.5, 0.6)));
        }
        return vec2<f32>(d, aa_radial);
    }
    // antenna, sprout, seed: circle
    return vec2<f32>(length(cell_uv - vec2<f32>(0.5)) - 0.3, aa_radial);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let xy = in.chunk_uv * vec2<f32>(CHUNK_EDGE);
    let aa_axis = fwidth(in.chunk_uv) * CHUNK_EDGE;

    let lx = clamp(u32(floor(xy.x)), 0u, CHUNK_EDGE_U - 1u);
    let ly = clamp(u32(floor(xy.y)), 0u, CHUNK_EDGE_U - 1u);
    let cell_uv = xy - vec2<f32>(f32(lx), f32(ly));
    let cell_idx = in.chunk_first_cell + ly * CHUNK_EDGE_U + lx;
    let cell = cells[cell_idx];

    let show_organic = (world.layer_flags & LAYER_ORGANIC) != 0u;
    let show_energy = (world.layer_flags & LAYER_ENERGY) != 0u;
    let show_fg = (world.layer_flags & LAYER_FG) != 0u;
    let show_clan = (world.layer_flags & LAYER_CLAN) != 0u;
    let show_mutation = (world.layer_flags & LAYER_MUTATION_RATE) != 0u;

    var color = soil_color(cell, show_organic, show_energy);

    if (show_fg && cell.kind != KIND_EMPTY) {
        let s = shape_sdf(cell, cell_uv, aa_axis);
        let d = s.x;
        let aa_w = s.y;
        let aa_pixel = max(aa_axis.x, aa_axis.y);
        let outline_fade = 1.0 - smoothstep(0.05, 0.15, aa_pixel);
        let outline_w = aa_w * 0.5 * outline_fade;
        let alpha_outer = 1.0 - smoothstep(outline_w - aa_w, outline_w + aa_w, d);
        let alpha_inner = 1.0 - smoothstep(-outline_w - aa_w, -outline_w + aa_w, d);
        // Resolve fill color: mutation > clan > occupant. Mutation
        // gradient takes priority because it's the most visually
        // distinct overlay; clan still beats default occupant colors.
        var fg = occupant_color(cell);
        if (show_clan) { fg = clan_color(cell.clan); }
        if (show_mutation) { fg = mutation_rate_color(cell.mutation_rate); }
        color = mix(color, OUTLINE_COLOR, alpha_outer);
        color = mix(color, fg, alpha_inner);
    }

    return vec4<f32>(color, 1.0);
}
