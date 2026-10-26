#version 450
layout(location = 0) in ivec4 position;
layout(location = 0) out vec3 color;
void main() {
    gl_Position = vec4(vec2(position.xy) / 128.0, 0.0, 1.0);
    color = vec3(gl_VertexIndex == 0, gl_VertexIndex == 1, gl_VertexIndex == 2);
}
