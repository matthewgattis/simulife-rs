struct Camera {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> camera: Camera;

struct VsIn {
    @location(0) corner: vec2<f32>,
}

struct InstanceIn {
    @location(1) cell_pos: vec2<f32>,
    @location(2) bg_color: vec3<f32>,
    @location(3) fg_color: vec3<f32>,
    @location(4) shape: u32,
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) bg_color: vec3<f32>,
    @location(2) fg_color: vec3<f32>,
    @location(3) @interpolate(flat) shape: u32,
}

@vertex
fn vs_main(v: VsIn, i: InstanceIn) -> VsOut {
    var out: VsOut;
    let world_pos = i.cell_pos + v.corner;
    out.clip_pos = camera.view_proj * vec4<f32>(world_pos, 0.0, 1.0);
    out.uv = v.corner;
    out.bg_color = i.bg_color;
    out.fg_color = i.fg_color;
    out.shape = i.shape;
    return out;
}

const KIND_NONE: u32 = 0u;
const KIND_CIRCLE: u32 = 1u;
const KIND_SQUARE: u32 = 2u;
const KIND_OVAL_H: u32 = 3u;
const KIND_OVAL_V: u32 = 4u;
const KIND_STEM: u32 = 5u;

const STEM_N: u32 = 1u;
const STEM_E: u32 = 2u;
const STEM_S: u32 = 4u;
const STEM_W: u32 = 8u;

fn aa_inside(d: f32) -> f32 {
    let aw = fwidth(d);
    return 1.0 - smoothstep(-aw, aw, d);
}

fn rect_mask(uv: vec2<f32>, lo: vec2<f32>, hi: vec2<f32>) -> f32 {
    let aw = fwidth(uv);
    let lx = smoothstep(lo.x - aw.x, lo.x + aw.x, uv.x);
    let hx = 1.0 - smoothstep(hi.x - aw.x, hi.x + aw.x, uv.x);
    let ly = smoothstep(lo.y - aw.y, lo.y + aw.y, uv.y);
    let hy = 1.0 - smoothstep(hi.y - aw.y, hi.y + aw.y, uv.y);
    return lx * hx * ly * hy;
}

fn shape_alpha(uv: vec2<f32>, kind: u32, param: u32) -> f32 {
    if (kind == KIND_CIRCLE) {
        let d = length(uv - vec2<f32>(0.5)) - 0.3;
        return aa_inside(d);
    }
    if (kind == KIND_SQUARE) {
        let m = max(abs(uv.x - 0.5), abs(uv.y - 0.5)) - 0.3;
        return aa_inside(m);
    }
    if (kind == KIND_OVAL_H) {
        let p = (uv - vec2<f32>(0.5)) / vec2<f32>(0.45, 0.25);
        let d = length(p) - 1.0;
        return aa_inside(d);
    }
    if (kind == KIND_OVAL_V) {
        let p = (uv - vec2<f32>(0.5)) / vec2<f32>(0.25, 0.45);
        let d = length(p) - 1.0;
        return aa_inside(d);
    }
    if (kind == KIND_STEM) {
        var alpha = 0.0;
        if ((param & STEM_N) != 0u) {
            alpha = max(alpha, rect_mask(uv, vec2<f32>(0.4, 0.0), vec2<f32>(0.6, 0.5)));
        }
        if ((param & STEM_E) != 0u) {
            alpha = max(alpha, rect_mask(uv, vec2<f32>(0.5, 0.4), vec2<f32>(1.0, 0.6)));
        }
        if ((param & STEM_S) != 0u) {
            alpha = max(alpha, rect_mask(uv, vec2<f32>(0.4, 0.5), vec2<f32>(0.6, 1.0)));
        }
        if ((param & STEM_W) != 0u) {
            alpha = max(alpha, rect_mask(uv, vec2<f32>(0.0, 0.4), vec2<f32>(0.5, 0.6)));
        }
        return alpha;
    }
    return 0.0;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let kind = in.shape & 0xffu;
    let param = (in.shape >> 8u) & 0xffu;
    let alpha = shape_alpha(in.uv, kind, param);
    let color = mix(in.bg_color, in.fg_color, alpha);
    return vec4<f32>(color, 1.0);
}
