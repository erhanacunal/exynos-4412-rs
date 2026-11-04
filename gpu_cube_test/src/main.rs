//! A textured cube turning in the middle of the screen, in perspective: the CPU makes one
//! matrix a frame, the GP's vertex shader takes the 36 vertices of the 12 triangles through it,
//! and the four PPs render the 320 x 320 frame into the screen, the depth test hiding the far
//! faces. Each face has its own colour, from its cell of a texture atlas, and is lit by one
//! distant light from the upper left front: the vertex shader takes each vertex's brightness
//! from its face's normal, a quarter ambient and three quarters diffuse, and the fragment
//! shader scales the texel by it.
//!
//! The shaders are compiled offline by Mesa 25.3.3's lima compiler (MIT), run on a PC under
//! `drm-shim` with `LIMA_DEBUG=dump`, from
//!
//! ```glsl
//! uniform mat4 mvp; uniform vec3 light; attribute vec4 pos; attribute vec2 uv;
//! attribute vec3 normal; varying vec3 v;
//! void main() { v = vec3(uv, 0.25 + 0.75 * max(dot(normal, light), 0.0)); gl_Position = mvp * pos; }
//! precision mediump float; uniform sampler2D t; varying vec3 v;
//! void main() { vec4 c = texture2D(t, v.xy); gl_FragColor = vec4(c.rgb * v.z, c.a); }
//! ```
//!
//! The light is the cube's own: the CPU turns the light's direction back by the cube's turns
//! each frame, so that the shader needs the normals as they are.
//!
//! with `GL_DEPTH_TEST` and `GL_LESS`. The depth buffer is the PPs' own tile memory, cleared
//! for each tile by the frame (`PpFrame::clear`) and never written back.
//!
//! The first frame's positions and brightnesses from the GP are checked against the CPU's own. As in
//! `gpu_spin_test`, each frame starts at vsync; a line every two seconds gives the frame rate
//! and the GPU's microseconds.
//!
//! The texture is sampled bilinearly; key `b` switches between that and nearest, which differ
//! in the texture's descriptor alone.
//!
//! Keys: `b` bilinear or nearest, `v` vsync on or off, `p` pause, `r` restart, `f` restart into
//! fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::f32::consts::TAU;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, Commands, Filter, GpFrame, IdentityMap, Mali, PpFrame, TileEnd};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const WIDTH: usize = 1280;
const HEIGHT: usize = 800;
/// The frame's side: 400 tiles, within one polygon list block a tile (`PLB_MAX_BLOCKS`).
const SIZE: usize = 320;
/// The frame's corner on the screen, on a tile column and a 64-byte line.
const LEFT: usize = (WIDTH - SIZE) / 2 / mali::TILE * mali::TILE;
const TOP: usize = (HEIGHT - SIZE) / 2;
const TILES: usize = (SIZE / mali::TILE) * (SIZE / mali::TILE);
const BLOCK_WORDS: usize = mali::PLB_BLOCK / 4;
const HEAP_WORDS: usize = 16 * 1024;
const STREAM_WORDS: usize = TILES * 4 + 4;
const TIMEOUT_US: u64 = 100_000;
const GREY: u32 = 0xFF33_3333;
/// Six faces of two triangles.
const VERTICES: usize = 36;
/// A turn about the vertical axis every 8 seconds, about the horizontal every 13.
const TURN_Y_US: u64 = 8_000_000;
const TURN_X_US: u64 = 13_000_000;
/// The camera's distance from the cube's centre, and the perspective's focal length: the
/// cube, of half side 1, stays within 0.75 of the frame's centre.
const DISTANCE: f32 = 4.0;
const FOCAL: f32 = 1.6;
const NEAR: f32 = 1.0;
const FAR: f32 = 10.0;
/// Towards the light, in the camera's space: up, left and in front; of length 1.
const LIGHT: [f32; 3] = [-0.408_248_3, 0.408_248_3, 0.816_496_6];
const AMBIENT: f32 = 0.25;
const DIFFUSE: f32 = 0.75;
const REPORT_US: u64 = 2_000_000;

/// The atlas: 4 x 2 cells of 32 x 32 texels, a face to a cell.
const ATLAS_WIDTH: usize = 128;
const ATLAS_HEIGHT: usize = 64;
const CELL: usize = 32;
const CELLS_ACROSS: usize = ATLAS_WIDTH / CELL;
/// The faces' colours, red, green, blue from the top byte: +x, -x, +y, -y, +z, -z.
const FACE_COLORS: [u32; 6] = [0xE0_3030, 0x30_C040, 0x30_60E0, 0xE0_D030, 0xC0_40C0, 0x30_C0C0];

/// Each face's corners, counter-clockwise seen from outside, from its bottom left.
const FACES: [[[f32; 3]; 4]; 6] = [
    [[1.0, -1.0, 1.0], [1.0, -1.0, -1.0], [1.0, 1.0, -1.0], [1.0, 1.0, 1.0]],
    [[-1.0, -1.0, -1.0], [-1.0, -1.0, 1.0], [-1.0, 1.0, 1.0], [-1.0, 1.0, -1.0]],
    [[-1.0, 1.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, -1.0], [-1.0, 1.0, -1.0]],
    [[-1.0, -1.0, -1.0], [1.0, -1.0, -1.0], [1.0, -1.0, 1.0], [-1.0, -1.0, 1.0]],
    [[-1.0, -1.0, 1.0], [1.0, -1.0, 1.0], [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0]],
    [[1.0, -1.0, -1.0], [-1.0, -1.0, -1.0], [-1.0, 1.0, -1.0], [1.0, 1.0, -1.0]],
];
/// Each face's normal, outwards.
const NORMALS: [[f32; 3]; 6] =
    [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
/// Where the corners are in a cell, its first line at the top.
const CORNER_UVS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
/// A face's two triangles, by corner.
const FACE_TRIANGLES: [usize; 6] = [0, 1, 2, 0, 2, 3];

/// The vertex shader: `mvp * pos` from the first four uniforms, then the viewport; the texture
/// coordinates and the brightness, from the normal and the light's uniform, to `v1.xyz`.
const VERTEX_SHADER: [u32; 84] = [
    0xb30a_d6b5, 0x0381_4ab5, 0x0007_ff80, 0x000a_d500, //
    0xb34b_3eb5, 0x0380_ead0, 0x0007_ff80, 0x000a_d500, //
    0xb341_c2d1, 0x4380_6ad3, 0x0007_ff80, 0x000a_d500, //
    0xb4c1_3430, 0x4380_aad2, 0x0007_fd80, 0x0008_d500, //
    0xb3c0_b02e, 0x4380_6ad4, 0x0247_ff80, 0x0008_c100, //
    0xb480_3c30, 0x4380_2ada, 0x0247_ff80, 0x000b_3100, //
    0xb6c1_3c16, 0x4380_aad9, 0x0247_fb80, 0x000b_9100, //
    0xd6c0_380d, 0x4380_22d8, 0x0647_ea00, 0x000c_dd00, //
    0xdc01_e6d7, 0x4381_831a, 0x0247_ff00, 0x0006_5900, //
    0x9c01_384c, 0x4380_833b, 0x0244_cb80, 0x0008_9500, //
    0x9c41_ba10, 0x4380_e2d7, 0x0147_ff80, 0x000d_4130, //
    0x9c0c_5a56, 0x0780_c001, 0x0247_ff00, 0x0006_1110, //
    0x3447_09a1, 0x4b81_00a7, 0x0247_ff80, 0x000c_dd00, //
    0x7e8b_62d7, 0x4381_c2b5, 0x027f_ff80, 0x0001_d900, //
    0xd6cb_32d9, 0x0381_0098, 0x0247_ff80, 0x000b_d900, //
    0x6609_82fa, 0x4b82_02b5, 0x0277_ff80, 0x000d_d900, //
    0x9e08_66cc, 0x0381_c25a, 0x0247_ff80, 0x000b_d900, //
    0xb60c_4717, 0x0381_ead6, 0x0247_ff80, 0x0006_d100, //
    0xd607_49bb, 0x4781_42b5, 0x467f_fb80, 0x000b_0500, //
    0x74c8_5f3b, 0x4781_81b2, 0x4646_1e00, 0x0000_6108, //
    0x9b8b_46b5, 0x0381_c2f2, 0xc007_0c80, 0x000a_d508,
];
const PREFETCH: u32 = 19;
/// `load.v $0.xyz 0, mov.v1 $1.xy $0.xyxx`; `load.v $1.xyxx, texld 0, mov.v1 $1 ^texture, sync`;
/// `load.v $2.xyz 0, mul.v0 $0.xyz $1.xyzx $2.zzzz, mov.s1 $0.w $1.w, stop`.
const FRAGMENT_SHADER: [u32; 15] = [
    0x0230_1084, 0x7000_3c60, 0x4000_0100, 0x0000_1f0c, //
    0x0228_11c6, 0x0f04_0404, 0x0000_0000, 0x3900_1000, //
    0x1000_0e4e, 0x0000_07cf, 0x0000_24a5, 0x7200_3c60, //
    0x0aa2_0904, 0x6000_e01c, 0x0000_07c8,
];
/// The dump's render state, its addresses zero: words 9, 12 and 15 take the fragment
/// shader's, the texture list's and the varyings'. Word 3 is the textured triangle's with the
/// depth test on, compare `Less`; word 10, varying 0 a vec4.
const RENDER_STATE: [u32; 16] = [
    0x0000_0000, 0x0000_0000, 0xfc3b_1ad2, 0x0000_0033, //
    0xffff_0000, 0xff00_0007, 0xff00_0007, 0x0000_ffff, //
    0x0000_f807, 0x0000_0000, 0x0000_0000, 0x0000_0000, //
    0x0000_0000, 0x0000_5222, 0x0000_2000, 0x0000_0000,
];
/// The uniforms: the matrix, 16; towards the light, 4; the viewport's scale and translation,
/// 8; then the shader's constants, the clamp among them, as the dump had them.
const MATRIX_UNIFORMS: usize = 16;
const LIGHT_UNIFORM: usize = MATRIX_UNIFORMS;
const VIEWPORT_UNIFORMS: usize = LIGHT_UNIFORM + 4;
const CONSTANTS: [f32; 8] = [0.0, DIFFUSE, AMBIENT, -32768.0, 32768.0, 0.0, 0.0, 0.0];
const UNIFORMS: usize = VIEWPORT_UNIFORMS + 8 + CONSTANTS.len();
/// Where the varyings follow the positions in [`Shared::gp_output`]: 16 bytes a vertex of
/// position, rounded up to 64 (lima_draw.c:983-988).
const VARYINGS_OFFSET: usize = (VERTICES * 16).next_multiple_of(64) / 4;
/// Words of the texture list before its descriptor: `lima_tex_list_size`, 64 bytes.
const DESCRIPTOR_OFFSET: usize = 16;

/// The atlas's texel at column `x`, line `y`, alpha, red, green, blue from the top byte: in
/// each cell a white border, then its face's colour on a checkerboard of 8 x 8 texel squares,
/// bright and dim. The two cells past the sixth are white, as the borders are, so that bilinear
/// filtering at a cell's edge takes nothing else from its neighbour.
fn texel(x: usize, y: usize) -> u32 {
    let face = (y / CELL) * CELLS_ACROSS + x / CELL;
    let Some(color) = FACE_COLORS.get(face) else { return 0xFFFF_FFFF };
    let (u, v) = (x % CELL, y % CELL);
    if u < 2 || v < 2 || u >= CELL - 2 || v >= CELL - 2 {
        return 0xFFFF_FFFF;
    }
    let dim = ((u / 8) ^ (v / 8)) & 1 == 1;
    let color = if dim { (color >> 1) & 0x7F_7F7F } else { *color };
    0xFF00_0000 | color
}

/// Sine and cosine: `core` has none without `std`. Taylor series to the 13th power after
/// bringing the angle into -pi..pi, within 3e-5 there.
fn sin_cos(angle: f32) -> (f32, f32) {
    let turns = angle / TAU;
    let whole = if turns >= 0.0 { (turns + 0.5) as i32 } else { (turns - 0.5) as i32 };
    let x = (turns - whole as f32) * TAU;
    let x2 = x * x;
    let (mut sin, mut cos) = (x, 1.0);
    let (mut sin_term, mut cos_term) = (x, 1.0);
    for n in 1..=6 {
        let n = n as f32;
        sin_term *= -x2 / ((2.0 * n) * (2.0 * n + 1.0));
        cos_term *= -x2 / ((2.0 * n - 1.0) * (2.0 * n));
        sin += sin_term;
        cos += cos_term;
    }
    (sin, cos)
}

/// A 4 x 4 matrix as GL keeps one: column after column, `m[column * 4 + row]`.
type Matrix = [f32; 16];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    core::array::from_fn(|index| {
        let (column, row) = (index / 4, index % 4);
        (0..4).map(|k| a[k * 4 + row] * b[column * 4 + k]).sum()
    })
}

/// `glFrustum`'s matrix for a square frame of focal length `focal`.
fn perspective(focal: f32, near: f32, far: f32) -> Matrix {
    let mut m = [0.0; 16];
    m[0] = focal;
    m[5] = focal;
    m[10] = (far + near) / (near - far);
    m[11] = -1.0;
    m[14] = 2.0 * far * near / (near - far);
    m
}

fn rotation_y(angle: f32) -> Matrix {
    let (sin, cos) = sin_cos(angle);
    let mut m = IDENTITY;
    (m[0], m[2], m[8], m[10]) = (cos, -sin, sin, cos);
    m
}

fn rotation_x(angle: f32) -> Matrix {
    let (sin, cos) = sin_cos(angle);
    let mut m = IDENTITY;
    (m[5], m[6], m[9], m[10]) = (cos, sin, -sin, cos);
    m
}

/// The frame's matrix: the cube turned by `y` about the vertical axis, then by `x` about the
/// horizontal, [`DISTANCE`] in front of the camera, in perspective.
fn matrix(y: f32, x: f32) -> (Matrix, Matrix) {
    let mut view = IDENTITY;
    view[14] = -DISTANCE;
    let model = multiply(&rotation_x(x), &rotation_y(y));
    (multiply(&perspective(FOCAL, NEAR, FAR), &multiply(&view, &model)), model)
}

/// Towards the light in the cube's own space: [`LIGHT`] turned back by `model`'s turns, its
/// transpose, as the turns are a rotation.
fn light(model: &Matrix) -> [f32; 3] {
    core::array::from_fn(|axis| (0..3).map(|k| model[axis * 4 + k] * LIGHT[k]).sum())
}

/// Cache-line aligned.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Lines<T>(T);

/// Page aligned, as Mesa's buffers are.
#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct Page<T>(T);

#[repr(C, align(4096))]
struct Shared {
    /// The polygon list blocks, one a tile.
    plb: [u32; TILES * BLOCK_WORDS],
    heap: [u32; HEAP_WORDS],
    streams: [Lines<[u32; STREAM_WORDS]>; mali::PP_COUNT],
    block_addresses: Lines<[u32; TILES]>,
    texels: Lines<[u32; ATLAS_WIDTH * ATLAS_HEIGHT]>,
    vertex_shader: Lines<[u32; VERTEX_SHADER.len()]>,
    fragment_shader: Lines<[u32; 16]>,
    /// The matrix and the light, rewritten each frame, then the viewport and the constants.
    uniforms: Lines<[f32; UNIFORMS]>,
    attributes: Lines<[u32; 6]>,
    varyings: Lines<[u32; 4]>,
    /// The cube's vertices, x, y and z.
    positions: Lines<[f32; VERTICES * 3]>,
    uvs: Lines<[f32; VERTICES * 2]>,
    normals: Lines<[f32; VERTICES * 3]>,
    /// The PP's texture list, then the descriptor at [`DESCRIPTOR_OFFSET`].
    textures: Lines<[u32; DESCRIPTOR_OFFSET + 16]>,
    /// What the vertex shader writes: x, y, z and 1/w of each vertex, then from
    /// [`VARYINGS_OFFSET`] its texture coordinates and brightness, 16 bytes a vertex.
    gp_output: Page<[f32; VARYINGS_OFFSET + VERTICES * 4]>,
    render_state: Lines<[u32; 16]>,
    /// The frame's own render state, for tiles without polygons: zeros.
    frame_state: Lines<[u32; 16]>,
    vs_list: Lines<[u32; 32]>,
    plbu_list: Lines<[u32; 64]>,
}

struct Store(UnsafeCell<Shared>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Store {}

static STORE: Store = Store(UnsafeCell::new(Shared {
    plb: [0; TILES * BLOCK_WORDS],
    heap: [0; HEAP_WORDS],
    streams: [Lines([0; STREAM_WORDS]); mali::PP_COUNT],
    block_addresses: Lines([0; TILES]),
    texels: Lines([0; ATLAS_WIDTH * ATLAS_HEIGHT]),
    vertex_shader: Lines([0; VERTEX_SHADER.len()]),
    fragment_shader: Lines([0; 16]),
    uniforms: Lines([0.0; UNIFORMS]),
    attributes: Lines([0; 6]),
    varyings: Lines([0; 4]),
    positions: Lines([0.0; VERTICES * 3]),
    uvs: Lines([0.0; VERTICES * 2]),
    normals: Lines([0.0; VERTICES * 3]),
    textures: Lines([0; DESCRIPTOR_OFFSET + 16]),
    gp_output: Page([0.0; VARYINGS_OFFSET + VERTICES * 4]),
    render_state: Lines([0; 16]),
    frame_state: Lines([0; 16]),
    vs_list: Lines([0; 32]),
    plbu_list: Lines([0; 64]),
}));

#[repr(C, align(4096))]
struct Pixels(UnsafeCell<[u32; WIDTH * HEIGHT]>);

// SAFETY: `main` takes the one reference to it once, for the framebuffer.
unsafe impl Sync for Pixels {}

static PIXELS: Pixels = Pixels(UnsafeCell::new([0; WIDTH * HEIGHT]));

struct Tables(UnsafeCell<IdentityMap<3>>);

// SAFETY: as for `Store`.
unsafe impl Sync for Tables {}

static TABLES: Tables = Tables(UnsafeCell::new(IdentityMap::new()));

fn address<T>(value: &T) -> u32 {
    value as *const T as u32
}

/// Everything but the matrix, once; returns the GP's frame and the PPs'.
fn prepare(shared: &mut Shared, screen: &Framebuffer) -> (GpFrame, PpFrame) {
    let plb = address(&shared.plb);
    for (tile, slot) in shared.block_addresses.0.iter_mut().enumerate() {
        *slot = plb + (tile * mali::PLB_BLOCK) as u32;
    }
    for pp in 0..mali::PP_COUNT {
        mali::clear_stream(&mut shared.streams[pp].0, SIZE, SIZE, pp, mali::PP_COUNT, TileEnd::Blocks(plb));
    }
    for (index, slot) in shared.texels.0.iter_mut().enumerate() {
        let [alpha, red, green, blue] = texel(index % ATLAS_WIDTH, index / ATLAS_WIDTH).to_be_bytes();
        *slot = u32::from_le_bytes([red, green, blue, alpha]);
    }
    for (face, corners) in FACES.iter().enumerate() {
        let cell = [(face % CELLS_ACROSS) as f32, (face / CELLS_ACROSS) as f32];
        let scale = [(CELL as f32) / ATLAS_WIDTH as f32, (CELL as f32) / ATLAS_HEIGHT as f32];
        for (index, corner) in FACE_TRIANGLES.iter().enumerate() {
            let vertex = face * 6 + index;
            shared.positions.0[vertex * 3..vertex * 3 + 3].copy_from_slice(&corners[*corner]);
            shared.normals.0[vertex * 3..vertex * 3 + 3].copy_from_slice(&NORMALS[face]);
            let uv = CORNER_UVS[*corner];
            shared.uvs.0[vertex * 2] = (cell[0] + uv[0]) * scale[0];
            shared.uvs.0[vertex * 2 + 1] = (cell[1] + uv[1]) * scale[1];
        }
    }
    shared.vertex_shader.0 = VERTEX_SHADER;
    shared.fragment_shader.0[..FRAGMENT_SHADER.len()].copy_from_slice(&FRAGMENT_SHADER);
    let half = SIZE as f32 / 2.0;
    // y scaled by minus half: the scene's top on the frame's first line.
    shared.uniforms.0[VIEWPORT_UNIFORMS..VIEWPORT_UNIFORMS + 8].copy_from_slice(&[half, -half, 0.5, 0.0, half, half, 0.5, 0.0]);
    shared.uniforms.0[VIEWPORT_UNIFORMS + 8..].copy_from_slice(&CONSTANTS);
    // `stride << 11 | type << 2 | (components - 1)`, type 0 float: lima_draw.c:850-855.
    shared.attributes.0 = [
        address(&shared.positions),
        (12 << 11) | (3 - 1),
        address(&shared.uvs),
        (8 << 11) | (2 - 1),
        address(&shared.normals),
        (12 << 11) | (3 - 1),
    ];
    let varyings = address(&shared.gp_output) + (VARYINGS_OFFSET * 4) as u32;
    // The position; the texture coordinates and brightness, 16 bytes a vertex, three 32-bit
    // components.
    shared.varyings.0 = [address(&shared.gp_output), 0x8020, varyings, (16 << 11) | (3 - 1)];
    let descriptor = address(&shared.textures) + (DESCRIPTOR_OFFSET * 4) as u32;
    shared.textures.0[0] = descriptor;
    set_filter(shared, Filter::Bilinear);
    shared.render_state.0 = RENDER_STATE;
    shared.render_state.0[9] = address(&shared.fragment_shader) | (FRAGMENT_SHADER[0] & 0x1F);
    shared.render_state.0[12] = address(&shared.textures);
    shared.render_state.0[15] = varyings;

    // lima_draw.c:287-334.
    let vs_start = address(&shared.vs_list);
    let mut vs = Commands::new(&mut shared.vs_list.0);
    vs.vs_arrays_semaphore_begin();
    vs.vs_uniforms(address(&shared.uniforms), (UNIFORMS * 4) as u32);
    vs.vs_shader(address(&shared.vertex_shader), (VERTEX_SHADER.len() * 4) as u32, PREFETCH);
    vs.vs_counts(2, 3);
    vs.vs_attributes(address(&shared.attributes), 3);
    vs.vs_varyings(address(&shared.varyings), 2);
    vs.vs_draw(VERTICES as u32);
    let vs_end = vs_start + vs.bytes();

    // lima_job.c:406-434, then lima_draw.c:336-439.
    let tiles_x = (SIZE / mali::TILE) as u32;
    let plbu_start = address(&shared.plbu_list);
    let mut plbu = Commands::new(&mut shared.plbu_list.0);
    plbu.plbu_head(tiles_x, tiles_x, address(&shared.block_addresses));
    plbu.plbu_viewport(0.0, SIZE as f32, 0.0, SIZE as f32);
    plbu.plbu_arrays_semaphore_begin();
    plbu.plbu_primitive_setup(0);
    plbu.plbu_render_state(address(&shared.render_state), address(&shared.gp_output));
    plbu.plbu_scissor(0, SIZE as u32, 0, SIZE as u32);
    plbu.plbu_depth_range(0.0, 1.0);
    plbu.plbu_draw_arrays(mali::MODE_TRIANGLES, 0, VERTICES as u32);
    plbu.end();
    let plbu_end = plbu_start + plbu.bytes();

    let heap_start = address(&shared.heap);
    let gp = GpFrame { vs_start, vs_end, plbu_start, plbu_end, heap_start, heap_end: heap_start + (HEAP_WORDS * 4) as u32 };
    let corner = screen.address() + (TOP * WIDTH + LEFT) * 4;
    let pp = PpFrame::clear(SIZE, SIZE, GREY, corner, screen.stride(), address(&shared.frame_state) as usize);
    (gp, pp)
}

/// The texture's descriptor for `filter`, cleaned to DRAM.
fn set_filter(shared: &mut Shared, filter: Filter) {
    let descriptor = mali::texture_rgba8888(address(&shared.texels), ATLAS_WIDTH as u32, ATLAS_HEIGHT as u32, filter);
    shared.textures.0[DESCRIPTOR_OFFSET..].copy_from_slice(&descriptor);
    mmu::clean_dcache_range(shared.textures.0.as_ptr() as usize, size_of_val(&shared.textures));
}

/// The frame's matrix and light into the uniforms, cleaned to DRAM.
fn place(shared: &mut Shared, m: &Matrix, light: &[f32; 3]) {
    shared.uniforms.0[..MATRIX_UNIFORMS].copy_from_slice(m);
    shared.uniforms.0[LIGHT_UNIFORM..LIGHT_UNIFORM + 3].copy_from_slice(light);
    mmu::clean_dcache_range(shared.uniforms.0.as_ptr() as usize, size_of_val(&shared.uniforms));
}

/// What the vertex shader should write for vertex `vertex` under `m`: the viewport's x and y,
/// y counted down from the frame's first line, its depth, and 1/w.
fn expected(shared: &Shared, m: &Matrix, vertex: usize) -> [f32; 4] {
    let p = &shared.positions.0[vertex * 3..vertex * 3 + 3];
    let clip: [f32; 4] = core::array::from_fn(|row| (0..3).map(|k| m[k * 4 + row] * p[k]).sum::<f32>() + m[12 + row]);
    let (half, w) = (SIZE as f32 / 2.0, clip[3]);
    [clip[0] / w * half + half, -clip[1] / w * half + half, clip[2] / w * 0.5 + 0.5, 1.0 / w]
}

/// The brightness the vertex shader should give vertex `vertex` under the light `toward`.
fn brightness(shared: &Shared, toward: &[f32; 3], vertex: usize) -> f32 {
    let normal = &shared.normals.0[vertex * 3..vertex * 3 + 3];
    AMBIENT + DIFFUSE * normal.iter().zip(toward).map(|(n, l)| n * l).sum::<f32>().max(0.0)
}

/// The first frame's check: every vertex the GP wrote against [`expected`], and its texture
/// coordinates and brightness.
fn check_gp(shared: &Shared, m: &Matrix, toward: &[f32; 3]) -> bool {
    mmu::invalidate_dcache_range(address(&shared.gp_output) as usize, size_of_val(&shared.gp_output));
    let mut worst = [0.0f32; 4];
    let mut first_wrong = None;
    let (mut worst_varying, mut first_varying) = (0.0f32, None);
    let lit = (0..VERTICES).filter(|vertex| brightness(shared, toward, *vertex) > AMBIENT).count();
    for vertex in 0..VERTICES {
        let got = &shared.gp_output.0[vertex * 4..vertex * 4 + 4];
        let want = expected(shared, m, vertex);
        // Pixels for x and y; depth and 1/w relative.
        let error = [(got[0] - want[0]).abs(), (got[1] - want[1]).abs(), (got[2] - want[2]).abs(), ((got[3] - want[3]) / want[3]).abs()];
        for (slot, value) in worst.iter_mut().zip(error) {
            *slot = slot.max(value);
        }
        let right = error[0] < 0.05 && error[1] < 0.05 && error[2] < 1e-4 && error[3] < 1e-4;
        if !right && first_wrong.is_none() {
            first_wrong = Some((vertex, [got[0], got[1], got[2], got[3]], want));
        }
        let varying = &shared.gp_output.0[VARYINGS_OFFSET + vertex * 4..VARYINGS_OFFSET + vertex * 4 + 3];
        let want = [shared.uvs.0[vertex * 2], shared.uvs.0[vertex * 2 + 1], brightness(shared, toward, vertex)];
        let error = varying.iter().zip(want).map(|(got, want)| (got - want).abs()).fold(0.0, f32::max);
        worst_varying = worst_varying.max(error);
        if error > 1e-4 && first_varying.is_none() {
            first_varying = Some((vertex, [varying[0], varying[1], varying[2]], want));
        }
    }
    let good = first_wrong.is_none() && first_varying.is_none();
    println!(
        "GP positions of {} vertices: largest error x {:.4}, y {:.4}, depth {:.6}, 1/w {:.6} relative: {}",
        VERTICES,
        worst[0],
        worst[1],
        worst[2],
        worst[3],
        if good { "OK" } else { "MISMATCH" }
    );
    println!("  texture coordinates and brightness: largest error {:.6}; {} vertices lit", worst_varying, lit);
    if let Some((vertex, got, want)) = first_wrong {
        println!("  vertex {}: {:.3?}, want {:.3?}", vertex, got, want);
    }
    if let Some((vertex, got, want)) = first_varying {
        println!("  vertex {} varying: {:.3?}, want {:.3?}", vertex, got, want);
    }
    good
}

/// Microseconds of one frame's parts.
#[derive(Clone, Copy, Default)]
struct Times {
    gp: u64,
    pps: u64,
    /// From the first reset to the last PP's end.
    all: u64,
}

/// One frame: resets, the GP, then the four PPs.
fn draw(gpu: Mali, mct: Mct, gp: &GpFrame, pp: &PpFrame, shared: &Shared) -> Result<Times, &'static str> {
    let begin = mct.count();
    // A soft reset of each before each frame, as Linux gives them after each job.
    gpu.reset_gp(mct, false).map_err(|_| "GP reset")?;
    for index in 0..mali::PP_COUNT {
        gpu.reset_pp(index, mct, false).map_err(|_| "PP reset")?;
    }
    // The GPU's L2 may hold the last frame's matrix.
    gpu.clear_l2(mct).map_err(|_| "L2 clear")?;
    gpu.start_gp(gp).map_err(|_| "GP start")?;
    let (gp_us, _) = match gpu.wait_gp(gp, mct, TIMEOUT_US) {
        Ok(done) => done,
        Err(error) => {
            println!("GP ERROR {:?}; hard reset {:?}", error, gpu.hard_reset_gp(mct));
            return Err("GP");
        }
    };
    let pps = mct.count();
    for index in 0..mali::PP_COUNT {
        gpu.start_pp(index, pp, address(&shared.streams[index])).map_err(|_| "PP start")?;
    }
    for index in 0..mali::PP_COUNT {
        if let Err(error) = gpu.wait_pp(index, mct, TIMEOUT_US) {
            println!("PP{} ERROR {:?}; hard reset {:?}", index, error, gpu.hard_reset_pp(index, mct));
            return Err("PP");
        }
    }
    let end = mct.count();
    Ok(Times { gp: gp_us, pps: (end - pps) / COUNTS_PER_US, all: (end - begin) / COUNTS_PER_US })
}

/// Sums and largest values over a report's frames.
#[derive(Default)]
struct Stats {
    frames: u64,
    sum: Times,
    most: Times,
}

impl Stats {
    fn add(&mut self, times: Times) {
        self.frames += 1;
        self.sum.gp += times.gp;
        self.sum.pps += times.pps;
        self.sum.all += times.all;
        self.most.gp = self.most.gp.max(times.gp);
        self.most.pps = self.most.pps.max(times.pps);
        self.most.all = self.most.all.max(times.all);
    }

    fn report(&self, us: u64, vsync: bool, filter: Filter) {
        let frames = self.frames.max(1);
        let fps_tenths = self.frames * 10_000_000 / us.max(1);
        println!(
            "{}.{} fps, vsync {}, {:?}: GP {}/{} us, PPs {}/{} us, all {}/{} us (mean/max)",
            fps_tenths / 10,
            fps_tenths % 10,
            if vsync { "on" } else { "off" },
            filter,
            self.sum.gp / frames,
            self.most.gp,
            self.sum.pps / frames,
            self.most.pps,
            self.sum.all / frames,
            self.most.all
        );
    }
}

fn start_screen(fimd: &Fimd, mct: Mct) -> Option<Framebuffer> {
    if let Err(error) = fimd.power_on() {
        println!("Display power on ERROR {:?}", error);
        return None;
    }
    display::configure_pins();
    // SAFETY: the only reference to these, taken once: see `Pixels`.
    let pixels: &'static mut [u32] = unsafe { &mut *PIXELS.0.get() };
    let mut screen = Framebuffer::new(pixels, WIDTH, HEIGHT)?;
    screen.fill_rect(0, 0, WIDTH, HEIGHT, GREY);
    screen.flush();
    if let Err(error) = fimd.start(&LTN101AL03, &screen) {
        println!("Display start ERROR {:?}", error);
        return None;
    }
    display::set_lvds(true);
    let settle = mct.count();
    while mct.count() - settle < u64::from(LVDS_SETTLE_MS) * 1000 * COUNTS_PER_US {}
    display::set_backlight(true);
    if !screen.is_uncached() {
        // The PPs write DRAM behind the caches: the screen must be read through no cache.
        println!("Screen cached: the GPU's frames would not show");
        return None;
    }
    Some(screen)
}

fn wait_keys() -> ! {
    println!("Keys: r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    loop {
        match uart.read_byte() {
            b'r' => power::restart(BootMode::Normal),
            b'f' => power::restart(BootMode::Fastboot),
            _ => {}
        }
    }
}

fn spin(gpu: Mali, mct: Mct, fimd: &Fimd, shared: &mut Shared, gp: &GpFrame, pp: &PpFrame) -> ! {
    println!("Turning: keys b bilinear/nearest, v vsync, p pause, r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    let (mut vsync, mut paused, mut first) = (true, false, true);
    let mut filter = Filter::Bilinear;
    // Time of the animation, which a pause stops.
    let mut shown_us = 0;
    let mut last = mct.count();
    let mut report_start = last;
    let mut stats = Stats::default();
    loop {
        match uart.try_read_byte() {
            Some(b'b') => {
                filter = if filter == Filter::Bilinear { Filter::Nearest } else { Filter::Bilinear };
                set_filter(shared, filter);
            }
            Some(b'v') => vsync = !vsync,
            Some(b'p') => paused = !paused,
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
        if vsync && let Err(error) = fimd.wait_vsync() {
            println!("Vsync ERROR {:?}; going on without", error);
            vsync = false;
        }
        let now = mct.count();
        if !paused {
            shown_us += (now - last) / COUNTS_PER_US;
        }
        last = now;
        let y = (shown_us % TURN_Y_US) as f32 / TURN_Y_US as f32 * TAU;
        let x = (shown_us % TURN_X_US) as f32 / TURN_X_US as f32 * TAU;
        let (m, model) = matrix(y, x);
        let toward = light(&model);
        place(shared, &m, &toward);
        match draw(gpu, mct, gp, pp, shared) {
            Ok(times) => stats.add(times),
            Err(what) => {
                println!("Frame {} failed: {}", stats.frames, what);
                wait_keys();
            }
        }
        if first {
            first = false;
            check_gp(shared, &m, &toward);
            console::flush();
        }
        let since = (mct.count() - report_start) / COUNTS_PER_US;
        if since >= REPORT_US {
            stats.report(since, vsync, filter);
            stats = Stats::default();
            report_start = mct.count();
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPU cube: 12 textured, lit triangles in perspective, depth tested, {}x{} at {},{} on four PPs", SIZE, SIZE, LEFT, TOP);
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    // SAFETY: the only references to these, taken once, here.
    let (shared, tables) = unsafe { (&mut *STORE.0.get(), &mut *TABLES.0.get()) };

    let gpu = Mali::new();
    match gpu.power_on(mali::SAFE_HZ, mct) {
        Ok(hz) => println!("GPU on: sclk_g3d {} Hz", hz),
        Err(error) => {
            println!("Power on ERROR {:?}", error);
            wait_keys();
        }
    }
    if let Err(error) = gpu.pmu_power_up(mali::PMU_ALL, mct) {
        println!("GPU PMU ERROR {:?}", error);
    }
    if tables.map(soc::DRAM_BASE, 12 << 20).is_err() {
        println!("Page tables too few");
        wait_keys();
    }
    let gp_ready = (gpu.reset_gp(mct, false), gpu.enable_mmu(None, tables.directory(), mct));
    if gp_ready.0.is_err() || gp_ready.1.is_err() {
        println!("GP reset, MMU {:?}", gp_ready);
        wait_keys();
    }
    for index in 0..mali::PP_COUNT {
        let ready = (gpu.reset_pp(index, mct, false), gpu.enable_mmu(Some(index), tables.directory(), mct));
        if ready.0.is_err() || ready.1.is_err() {
            println!("PP{} reset, MMU {:?}", index, ready);
            wait_keys();
        }
    }
    let fimd = Fimd::new();
    let Some(screen) = start_screen(&fimd, mct) else { wait_keys() };
    let (gp, pp) = prepare(shared, &screen);
    mmu::clean_dcache_range(shared as *const Shared as usize, size_of::<Shared>());
    spin(gpu, mct, &fimd, shared, &gp, &pp);
}
