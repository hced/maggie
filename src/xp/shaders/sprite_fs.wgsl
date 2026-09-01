// Sprite fragment shader: samples a sprite texture with alpha blending.
// Translated from the sprite rendering passes in gpu.rs draw().
@group(0) @binding(1) var sprite_tex: texture_2d<f32>;
@group(0) @binding(2) var sprite_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(sprite_tex, sprite_sampler, in.uv);
}
