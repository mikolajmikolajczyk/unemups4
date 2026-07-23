#version 450
layout(location = 0) in vec2 inUV;
layout(location = 0) out vec4 outColor;
layout(binding = 0) uniform sampler2D texSampler;

// Present-shader flags decoded per-flip by the backend (task-154 residual #2,
// task-175). swap_rb R<->B-swaps a BGRA guest scanout (A8R8G8B8_SRGB, e.g.
// Celeste) so the guest's channel order reaches the swapchain correctly; it is
// set ONLY when the present sources pixels from guest memory, since an embedded
// draw already wrote shader-space RGBA into texture_image.
//
// texture_image is _UNORM, so this sample yields the guest's own GAMMA-SPACE
// value byte for byte. decode_srgb is set when the swapchain is an _SRGB format,
// which encodes linear->sRGB on store: the shader decodes first so the two
// cancel and the guest's bytes reach the display unaltered. A non-_SRGB
// swapchain stores raw, so no correction is applied.
layout(push_constant) uniform PC { uint swap_rb; uint decode_srgb; } pc;

// Per-channel sRGB -> linear (IEC 61966-2-1) EOTF. Pre-compensates the _SRGB
// swapchain's encode-on-store.
vec3 srgb_to_linear(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    bvec3 lo = lessThanEqual(c, vec3(0.04045));
    vec3 hi = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(hi, c / 12.92, lo);
}

void main() {
    vec4 t = texture(texSampler, inUV);
    vec4 c = (pc.swap_rb != 0u) ? t.bgra : t.rgba;
    if (pc.decode_srgb != 0u) {
        c.rgb = srgb_to_linear(c.rgb);
    }
    outColor = c;
}
