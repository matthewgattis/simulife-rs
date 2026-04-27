struct Camera {
    view_proj: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> camera: Camera;

struct VsIn {
    @location(0) corner: vec2<f32>,
}

struct InstanceIn {
    @location(1) cell_pos: vec2<f32>,
    @location(2) color: vec3<f32>,
}

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec3<f32>,
}

@vertex
fn vs_main(v: VsIn, i: InstanceIn) -> VsOut {
    let world_pos = i.cell_pos + v.corner;
    var out: VsOut;
    out.clip_pos = camera.view_proj * vec4<f32>(world_pos, 0.0, 1.0);
    out.color = i.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
