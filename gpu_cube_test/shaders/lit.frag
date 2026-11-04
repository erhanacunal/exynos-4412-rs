// The texel, scaled by the vertex's brightness.
precision mediump float;
uniform sampler2D t;
varying vec3 v;

void main() {
    vec4 c = texture2D(t, v.xy);
    gl_FragColor = vec4(c.rgb * v.z, c.a);
}
