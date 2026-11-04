// A vertex lit by one distant light: a quarter ambient, three quarters diffuse.
uniform mat4 mvp;
uniform vec3 light;
attribute vec4 pos;
attribute vec2 uv;
attribute vec3 normal;
varying vec3 v;

void main() {
    v = vec3(uv, 0.25 + 0.75 * max(dot(normal, light), 0.0));
    gl_Position = mvp * pos;
}
