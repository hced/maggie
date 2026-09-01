// Overlay fragment shader: applies UV offset for pan-without-rerender.
// Translated from OVERLAY_FRAGMENT_SHADER in gpu.rs.
@group(0) @binding(1) var overlay_tex: texture_2d<f32>;
@group(0) @binding(2) var overlay_sampler: sampler;

struct Uniforms {
    rect: vec4<f32>,
    uv_offset: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv + uniforms.uv_offset;
    if (uv.x < 0.0 || uv.x >= 1.0 || uv.y < 0.0 || uv.y >= 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    return textureSample(overlay_tex, overlay_sampler, uv);
}
