//! 3D on the Mali-400, above `drivers::mali`: what an application draws with.
//!
//! A shader pair is a [`Program`], as `tools/shaderc` generates it from GLSL: the binaries Mesa's
//! lima compiler made of it, and everything needed to feed them, the uniforms by name among it.
//!
//! [`Gpu::start`] powers the GPU and the screen; [`Gpu::texture`] and [`Gpu::mesh`] put textures
//! and triangles in its memory; [`Gpu::frame`] starts a frame on an [`Area`] of the screen,
//! [`Frame::draw`] adds a mesh with its uniforms and textures, and [`Frame::show`] runs it. The
//! memory is one fixed pool (`memory`): no heap. [`Mat4`] and the rest of `math` are the
//! arithmetic for the uniforms.

mod math;
mod memory;
mod runtime;

pub use math::*;
pub use memory::{FRAME_BYTES, LASTING_BYTES, MAP_TABLES, MAX_DRAWS, MAX_FRAME_TILES, MAX_TILES};
pub use runtime::*;

/// Which shader a uniform belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The vertex shader, on the GP: 32-bit floats.
    Vertex,
    /// The fragment shader, on the PPs: half floats.
    Fragment,
}

/// A vertex attribute, in the order the vertex shader declares them: its index is its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// As the vertex shader declares it.
    pub name: &'static str,
    /// 1 to 4: `float` to `vec4`.
    pub components: u32,
}

/// A uniform's place in its stage's uniforms. One the two shaders share is listed for each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Uniform {
    /// As the shaders declare it, an array's without `[0]`.
    pub name: &'static str,
    /// Whose uniforms it is among.
    pub stage: Stage,
    /// Where it starts, in floats (or half floats, [`Stage::Fragment`]).
    pub offset: usize,
    /// How many: 16 for a `mat4`, 3 for a `vec3`; an array's elements one after another.
    pub floats: usize,
}

/// A varying the vertex shader writes and the fragment shader reads, past the position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Varying {
    /// Bytes from the start of a vertex's varyings.
    pub offset: u32,
    /// The varying info's low bits: `components - 1`, and `0x0C` for half floats
    /// (lima_draw.c:1006-1020).
    pub format: u32,
}

/// A vertex and a fragment shader, compiled offline: what `tools/shaderc` writes for a pair of
/// GLSL files.
#[derive(Clone, Copy, Debug)]
pub struct Program {
    /// The GLSL files' name, without `.vert` and `.frag`.
    pub name: &'static str,
    /// The vertex shader, 16 bytes an instruction.
    pub vertex: &'static [u32],
    /// Instructions the GP fetches ahead: the vertex command list's `SHADER_INFO`.
    pub vertex_prefetch: u32,
    /// The fragment shader. The low 5 bits of its first word are its first instruction's
    /// length, which the render state repeats.
    pub fragment: &'static [u32],
    /// The vertex shader's attributes, in its order: attribute `i` is the `i`th the vertex
    /// command list's attribute info lists.
    pub attributes: &'static [Attribute],
    /// Every active uniform but the samplers, by name.
    pub uniforms: &'static [Uniform],
    /// The vertex uniforms' length in floats. The GP's uniform buffer is these, then the
    /// viewport's scale and translation, 8, then [`vertex_constants`](Self::vertex_constants)
    /// (lima_draw.c:867-893).
    pub vertex_uniform_floats: usize,
    /// The vertex shader's own constants, which follow the viewport.
    pub vertex_constants: &'static [f32],
    /// The fragment uniforms' length in half floats; 0 for none.
    pub fragment_uniform_halves: usize,
    /// The varyings past the position, in the vertex shader's order of outputs.
    pub varyings: &'static [Varying],
    /// Bytes of varyings a vertex.
    pub varying_stride: u32,
    /// The samplers by name, a texture unit each, in order.
    pub samplers: &'static [&'static str],
    /// Mesa's render state for the pair, its addresses zero: words 9, 11, 12 and 15 take the
    /// fragment shader's, the fragment uniforms', the textures' and the varyings' above their
    /// low bits. Drawn with no depth test and no blending.
    pub render_state: [u32; 16],
}

impl Program {
    /// The uniform named `name` in `stage`, if the shader uses it.
    pub fn uniform(&self, name: &str, stage: Stage) -> Option<&Uniform> {
        self.uniforms.iter().find(|uniform| uniform.name == name && uniform.stage == stage)
    }

    /// The index of the attribute named `name`.
    pub fn attribute(&self, name: &str) -> Option<usize> {
        self.attributes.iter().position(|attribute| attribute.name == name)
    }
}
