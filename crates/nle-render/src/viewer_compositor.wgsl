@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct ColorCorrection {
    // temperature, tint, saturation, exposure
    color: vec4<f32>,
    // brightness, contrast, highlights, shadows
    light: vec4<f32>,
    // operation (0 = basic, 1 = vignette), amount, midpoint, feather
    effect: vec4<f32>,
    // Vignette center x, center y, or Basic Correction whites, blacks.
    center: vec4<f32>,
};
struct ColorCorrectionStack {
    corrections: array<ColorCorrection, 8>,
    count: u32,
    // 0 = nearest, 1 = bilinear, 2 = manually filtered bicubic.
    sampling_quality: u32,
    _padding_1: u32,
    _padding_2: u32,
};
@group(0) @binding(2) var<uniform> color_stack: ColorCorrectionStack;
struct CurveLutStack {
    // Eight node-major, 256-entry component-then-master RGB lookup tables.
    samples: array<vec4<f32>, 2048>,
};
@group(0) @binding(3) var<storage, read> curve_luts: CurveLutStack;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) opacity: f32,
};
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) opacity: f32,
};

struct MatteVertexInput {
    @location(0) opacity: f32,
    @location(1) color: vec3<f32>,
};

struct MatteVertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) opacity: f32,
    @location(1) color: vec3<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = vec4<f32>(input.position, 0.0, 1.0);
    output.uv = input.uv;
    output.opacity = input.opacity;
    return output;
}

@vertex
fn vs_blit(@builtin(vertex_index) index: u32) -> VertexOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, -1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, -1.0),
    );
    let uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.uv = uvs[index];
    output.opacity = 1.0;
    return output;
}

@vertex
fn vs_matte(
    @builtin(vertex_index) index: u32,
    input: MatteVertexInput,
) -> MatteVertexOutput {
    let positions = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, 1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, -1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, -1.0),
    );
    var output: MatteVertexOutput;
    output.position = vec4<f32>(positions[index], 0.0, 1.0);
    output.opacity = input.opacity;
    output.color = input.color;
    return output;
}

fn linear_to_srgb(linear: vec3<f32>) -> vec3<f32> {
    let clamped = clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0));
    return select(
        12.92 * clamped,
        1.055 * pow(clamped, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055),
        clamped > vec3<f32>(0.0031308),
    );
}

fn srgb_to_linear(encoded: vec3<f32>) -> vec3<f32> {
    return select(
        encoded / vec3<f32>(12.92),
        pow((encoded + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4)),
        encoded > vec3<f32>(0.04045),
    );
}

fn apply_curve_lut(encoded: vec3<f32>, correction_index: u32) -> vec3<f32> {
    // Export's preceding geq stage produces 8-bit RGB, then FFmpeg's curves filter performs a
    // direct 256-entry LUT lookup. Rounding here preserves that boundary behavior in preview.
    let levels = vec3<u32>(round(clamp(encoded, vec3<f32>(0.0), vec3<f32>(1.0)) * 255.0));
    let base = correction_index * 256u;
    return vec3<f32>(
        curve_luts.samples[base + levels.x].x,
        curve_luts.samples[base + levels.y].y,
        curve_luts.samples[base + levels.z].z,
    );
}

fn alpha_safe_premultiplied(sample: vec4<f32>) -> vec4<f32> {
    let alpha = clamp(sample.a, 0.0, 1.0);
    return vec4<f32>(clamp(sample.rgb, vec3<f32>(0.0), vec3<f32>(alpha)), alpha);
}

fn cubic_weight(distance: f32) -> f32 {
    let x = abs(distance);
    if (x <= 1.0) {
        return ((1.5 * x - 2.5) * x * x) + 1.0;
    }
    if (x < 2.0) {
        return ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
    }
    return 0.0;
}

// WebGPU samplers support nearest and linear filtering only. The source is already
// encoded-premultiplied, so filter all four components together and clamp Catmull-Rom's
// overshoot back into the premultiplied-alpha-safe range before color correction.
fn texture_sample_bicubic(uv: vec2<f32>) -> vec4<f32> {
    let dimensions = textureDimensions(source_texture);
    let sample_position = uv * vec2<f32>(dimensions) - vec2<f32>(0.5);
    let base = vec2<i32>(floor(sample_position));
    let fraction = fract(sample_position);
    let maximum = vec2<i32>(dimensions) - vec2<i32>(1);
    var result = vec4<f32>(0.0);
    for (var y = -1; y <= 2; y = y + 1) {
        let weight_y = cubic_weight(f32(y) - fraction.y);
        for (var x = -1; x <= 2; x = x + 1) {
            let texel = clamp(base + vec2<i32>(x, y), vec2<i32>(0), maximum);
            let weight = cubic_weight(f32(x) - fraction.x) * weight_y;
            result += textureLoad(source_texture, texel, 0) * weight;
        }
    }
    return alpha_safe_premultiplied(result);
}

fn sampled_compositor_texture(uv: vec2<f32>) -> vec4<f32> {
    if (color_stack.sampling_quality == 2u) {
        return texture_sample_bicubic(uv);
    }
    return alpha_safe_premultiplied(textureSample(source_texture, source_sampler, uv));
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = sampled_compositor_texture(input.uv);
    let alpha = sample.a;
    // Composition textures contain premultiplied encoded-sRGB values. Color operations remain
    // defined on straight encoded color until the Phase 4 linear working-space migration.
    var encoded = select(vec3<f32>(0.0), sample.rgb / alpha, alpha > 0.0);
    for (var index = 0u; index < min(color_stack.count, 8u); index = index + 1u) {
        let correction = color_stack.corrections[index];
        if (correction.effect.x == 1.0) {
            let dx = 2.0 * (input.uv.x - (0.5 + correction.center.x * 0.5));
            let dy = 2.0 * (input.uv.y - (0.5 + correction.center.y * 0.5));
            let radius = sqrt(dx * dx + dy * dy) / sqrt(2.0);
            let outer = correction.effect.z + correction.effect.w * (1.0 - correction.effect.z);
            let t = clamp(
                (radius - correction.effect.z) / max(0.0001, outer - correction.effect.z),
                0.0,
                1.0,
            );
            let smooth_factor = t * t * (3.0 - 2.0 * t);
            encoded *= 1.0 - correction.effect.y * smooth_factor;
        } else {
            let temperature = correction.color.x;
            let tint = correction.color.y;
            encoded += vec3<f32>(
                0.10 * temperature + 0.05 * tint,
                -0.05 * tint,
                -0.10 * temperature + 0.05 * tint,
            );
            encoded *= exp2(vec3<f32>(correction.color.w));
            var luma = dot(encoded, vec3<f32>(0.2126, 0.7152, 0.0722));
            encoded = vec3<f32>(luma) + (encoded - vec3<f32>(luma)) * correction.color.z;
            encoded = (encoded - vec3<f32>(0.5)) * correction.light.y
                + vec3<f32>(0.5 + correction.light.x);
            luma = dot(encoded, vec3<f32>(0.2126, 0.7152, 0.0722));
            let tonal_luma = clamp(luma, 0.0, 1.0);
            // Highlights/Shadows use broad quadratic masks. Whites/Blacks use eighth-power
            // masks, concentrating their equal-strength adjustment nearer the tonal endpoints.
            let tonal = 0.25 * correction.light.z * tonal_luma * tonal_luma
                + 0.25 * correction.light.w * (1.0 - tonal_luma) * (1.0 - tonal_luma)
                + 0.20 * correction.center.z * pow(tonal_luma, 8.0)
                + 0.20 * correction.center.w * pow(1.0 - tonal_luma, 8.0);
            encoded = clamp(encoded + vec3<f32>(tonal), vec3<f32>(0.0), vec3<f32>(1.0));
            encoded = apply_curve_lut(encoded, index);
        }
    }
    let output_alpha = alpha * input.opacity;
    return vec4<f32>(encoded * output_alpha, output_alpha);
}

@fragment
fn fs_premultiply(input: VertexOutput) -> @location(0) vec4<f32> {
    // This pass is a one-to-one upload prepass. textureLoad preserves exact texels instead of
    // allowing monitor sampling quality to alter encoded source pixels before premultiplication.
    let dimensions = textureDimensions(source_texture);
    let texel = clamp(
        vec2<i32>(input.position.xy),
        vec2<i32>(0),
        vec2<i32>(dimensions) - vec2<i32>(1),
    );
    let straight = textureLoad(source_texture, texel, 0);
    return vec4<f32>(straight.rgb * straight.a, straight.a);
}

@fragment
fn fs_blit_srgb(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(source_texture, source_sampler, input.uv);
    // Decode before an sRGB target re-encodes so presentation preserves the compositor bytes.
    return vec4<f32>(srgb_to_linear(sample.rgb), sample.a);
}

@fragment
fn fs_blit_encoded(input: VertexOutput) -> @location(0) vec4<f32> {
    // A non-sRGB fallback target performs no transfer conversion.
    return textureSample(source_texture, source_sampler, input.uv);
}

@fragment
fn fs_matte(input: MatteVertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(input.color * input.opacity, input.opacity);
}
