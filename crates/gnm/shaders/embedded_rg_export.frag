#version 450

// Embedded PS id 1 — the firmware "export 32-bit R and G" pixel shader (doc-1
// §3.4). The embedded PS carries no user color; it writes the two exported
// channels (R, G) and leaves B/A at their fixed values. Driving R and G from the
// interpolated screen coordinate makes the genuine GPU draw visible as an R/G
// gradient fill (the constrained output doc-1 describes), rather than a flat
// color that is indistinguishable from a clear.
//
// Core GLSL 4.5 -> SPIR-V, no extension/capability beyond a single RGBA color
// export — inside the Vulkan portability subset (MoltenVK/Metal, decision-3).

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 o_color;

void main() {
    o_color = vec4(v_uv.x, v_uv.y, 0.0, 1.0);
}
