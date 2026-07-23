#version 450

// Embedded VS id 0 — the firmware fullscreen-quad vertex shader (doc-1 §3.4).
//
// No vertex input: the fullscreen triangle is generated from gl_VertexIndex, so
// there is no vertex buffer / vertex-fetch to model (task-24 keeps the resource
// cache empty for this draw). A 3-vertex oversized triangle covers the whole
// clip-space viewport; the rasterizer clips it to the render target. The two UV
// components pass a normalized 0..1 screen coordinate to the pixel shader so the
// R/G-export PS (id 1) can produce the fill the embedded pair is defined to draw.
//
// gl_VertexIndex mapping (0,1,2) -> clip xy:
//   0 -> (-1,-1)   1 -> ( 3,-1)   2 -> (-1, 3)
// which is the standard "single big triangle" fullscreen pass, Vulkan-portable
// (no capability beyond core 1.0), MoltenVK/Metal safe (decision-3).

layout(location = 0) out vec2 v_uv;

void main() {
    vec2 pos = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    v_uv = pos;
    gl_Position = vec4(pos * 2.0 - 1.0, 0.0, 1.0);
}
