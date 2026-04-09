#version 450
layout(push_constant) uniform PC { vec4 geom; };
layout(location = 0) out vec2 v_tc;
void main() {
    // Generate unit quad from vertex index (0-3, triangle strip).
    vec2 pos = vec2(gl_VertexIndex & 1, (gl_VertexIndex >> 1) & 1);
    // Map [0,1] to clip-space position via push constants.
    gl_Position = vec4(geom.xy + pos * geom.zw, 0.0, 1.0);
    v_tc = pos;
}
