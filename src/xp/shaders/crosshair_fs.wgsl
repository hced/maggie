// Crosshair fragment shader: outputs luminance (white on arms, black elsewhere).
// The blend mode ONE_MINUS_DST_COLOR / ONE_MINUS_SRC_COLOR inverts the
// destination under the arms. Translated from INVERT_CURSOR_FRAGMENT_SHADER
// in gpu.rs.
@group(0) @binding(1) var crosshair_tex: texture_2d<f32>;
@group(0) @binding(2) var crosshair_sampler: sampler;

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
    // Center origin: 0.5 = center of quad, range is -0.5..0.5.
    let c = in.uv - 0.5;
    // Anti-aliasing width: ~1 sub-pixel.
    let aa = 0.004;
    // Crosshair arm half-thickness and gap (empty center).
    let hw = 0.04;   // arm half-thickness (8% of size)
    let gap = 0.08;  // center gap radius

    // Horizontal arm: |y| near 0, |x| in [gap, 0.5].
    let y_in = 1.0 - smoothstep(hw - aa, hw + aa, abs(c.y));
    let x_arm = smoothstep(gap - aa, gap + aa, abs(c.x))
              * (1.0 - smoothstep(0.5 - aa, 0.5 + aa, abs(c.x)));
    let horizontal = y_in * x_arm;

    // Vertical arm: |x| near 0, |y| in [gap, 0.5].
    let x_in = 1.0 - smoothstep(hw - aa, hw + aa, abs(c.x));
    let y_arm = smoothstep(gap - aa, gap + aa, abs(c.y))
              * (1.0 - smoothstep(0.5 - aa, 0.5 + aa, abs(c.y)));
    let vertical = x_in * y_arm;

    let cov = clamp(horizontal + vertical, 0.0, 1.0);
    // Output luminance: white on arms, black elsewhere.
    return vec4<f32>(vec3<f32>(cov), 1.0);
}
