struct TargetSize {
    width: f32,
    height: f32,
    _padding: vec2<f32>,
};

@group(0) @binding(0) var<uniform> target_size: TargetSize;

struct VertexInput {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @builtin(vertex_index) vertex_index: u32,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

fn unit_quad(index: u32) -> vec2<f32> {
    // Two triangles, generated without a static vertex buffer.
    let points = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    return points[index];
}

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    let pixel = input.rect.xy + unit_quad(input.vertex_index) * input.rect.zw;
    let ndc = vec2<f32>(
        pixel.x / target_size.width * 2.0 - 1.0,
        1.0 - pixel.y / target_size.height * 2.0,
    );
    var output: VertexOutput;
    output.position = vec4<f32>(ndc, 0.0, 1.0);
    output.color = input.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return input.color;
}
