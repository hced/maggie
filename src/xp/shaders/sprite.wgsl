// Sprite vertex shader: positions a quad at u_rect in normalized screen coords.
// Translated from SPRITE_VERTEX_SHADER in gpu.rs.
struct Uniforms {
    rect: vec4<f32>,  // (x, y, w, h) in normalized surface coords
    uv_offset: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) a_pos: vec2<f32>) -> VertexOutput {
    var out: VertexOutput;
    // Map a_pos [0,1] to the sprite rect in normalized screen coords.
    let screen = uniforms.rect.xy + a_pos * uniforms.rect.zw;
    // Convert to NDC: x [0,1] -> [-1,1], y [0,1] -> [1,-1] (y-flip for screen coords).
    let ndc = vec2<f32>(screen.x * 2.0 - 1.0, 1.0 - screen.y * 2.0);
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = a_pos;
    return out;
}
