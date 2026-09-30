// Input: the Bgra8Unorm texture that grafting imported from the VAAPI DMA-BUF.
// Output: an Rgba8Unorm storage texture that the blit pass samples.

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let coord = vec2<i32>(gid.xy);

    let c = textureLoad(input_tex, coord, 0);

    // Saturation boost around BT.709 luma.
    let luma = dot(c.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let saturated = mix(vec3<f32>(luma), c.rgb, 1.4);

    // Gamma lift on midtones.
    let lifted = pow(clamp(saturated, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 1.1));

    textureStore(output_tex, coord, vec4<f32>(lifted, c.a));
}
