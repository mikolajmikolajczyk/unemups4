#version 450
layout(location = 0) in vec2 inUV;
layout(location = 0) out vec4 outColor;
layout(binding = 0) uniform sampler2D texSampler;

void main() {
    // OLD (Debug):
    // outColor = vec4(1.0, 0.0, 0.0, 1.0);

    // NEW (Real):
    outColor = texture(texSampler, inUV);
}
