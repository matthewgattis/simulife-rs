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
    let brown = vec3<f32>(0.8, 0.6, 0.4);
    let blue = vec3<f32>(0.4, 0.6, 1.0);
    var color = vec3<f32>(1.0, 1.0, 1.0);
    if (show_organic) {
        color = mix(color, brown, f32(cell.organic) / 255.0);
    }
    if (show_energy) {
        color = mix(color, blue, f32(cell.soil_energy) / 255.0);
    }
    return color;
}

fn occupant_color(cell: Cell) -> vec3<f32> {
    if (cell.kind == KIND_LEAF) { return vec3<f32>(0.20, 0.75, 0.30); }
    if (cell.kind == KIND_ROOT) { return vec3<f32>(0.50, 0.30, 0.10); }
    if (cell.kind == KIND_STEM) { return vec3<f32>(0.55, 0.45, 0.25); }
    if (cell.kind == KIND_ANTENNA) { return vec3<f32>(0.30, 0.55, 0.95); }
    if (cell.kind == KIND_SPROUT) { return vec3<f32>(1.00, 0.85, 0.20); }
    if (cell.kind == KIND_SEED) { return vec3<f32>(0.80, 0.70, 0.35); }
    return vec3<f32>(0.0);
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

    var color = soil_color(cell, show_organic, show_energy);
    if (show_fg && cell.kind != KIND_EMPTY) {
        let s = shape_sdf(cell, cell_uv, aa_axis);
        let d = s.x;
        let aa_w = s.y;
        let aa_pixel = max(aa_axis.x, aa_axis.y);
        let outline_fade = 1.0 - smoothstep(0.05, 0.15, aa_pixel);
        let outline_w = aa_w * 1.0 * outline_fade;
        let alpha_outer = 1.0 - smoothstep(outline_w - aa_w, outline_w + aa_w, d);
        let alpha_inner = 1.0 - smoothstep(-outline_w - aa_w, -outline_w + aa_w, d);
        let fg = occupant_color(cell);
        //color = mix(color, OUTLINE_COLOR, alpha_outer);
        color = mix(color, fg, alpha_inner);
    }

    return vec4<f32>(color, 1.0);
}
