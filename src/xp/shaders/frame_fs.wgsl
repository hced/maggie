// Frame fragment shader: samples the captured frame texture.
// Texels outside [0,1] UV square are painted black (OOB handling).
// Translated from FRAGMENT_SHADER in gpu.rs.
@group(0) @binding(1) var frame_tex: texture_2d<f32>;
@group(0) @binding(2) var frame_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let uv = in.uv;
    if (uv.x < 0.0 || uv.x >= 1.0 || uv.y < 0.0 || uv.y >= 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return textureSample(frame_tex, frame_sampler, uv);
}
