struct Camera {
    transform: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var splash_texture: texture_2d<f32>;
@group(1) @binding(1) var splash_sampler: sampler;

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.clip_position = camera.transform * vec4<f32>(input.position, 1.0);
    output.uv = input.uv;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Both faces sample the same UVs. The inside is therefore the direct,
    // physical horizontal mirror of the exterior when viewed through the wall.
    return textureSample(splash_texture, splash_sampler, input.uv);
}
