// Frame vertex shader: samples u_src rect from the texture.
// Translated from VERTEX_SHADER in gpu.rs.
struct Uniforms {
    src: vec4<f32>, // (x, y, w, h) in texture space
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) a_pos: vec2<f32>) -> VertexOutput {
    var out: VertexOutput;
    let ndc = vec2<f32>(a_pos.x * 2.0 - 1.0, a_pos.y * 2.0 - 1.0);
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    // Sample from the src rect, flipping Y (texture is top-left origin).
    out.uv = uniforms.src.xy + vec2<f32>(a_pos.x, 1.0 - a_pos.y) * uniforms.src.zw;
    return out;
}
