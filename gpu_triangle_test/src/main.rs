//! Triangles through the Mali-400: its geometry processor runs the vertex shader and the PLBU,
//! which bins the triangle into the polygon lists of the tiles; a pixel processor then renders
//! the tiles from those lists with the fragment shader. Two scenes: a flat orange triangle, and
//! one whose corners are red, green and blue, the colours interpolated between them.
//!
//! The shaders are compiled offline by Mesa 25.3.3's lima compiler (MIT), run on a PC under
//! `drm-shim` with `LIMA_DEBUG=dump`, from
//!
//! ```glsl
//! // Flat
//! attribute vec4 pos; void main() { gl_Position = pos; }
//! precision mediump float; uniform vec4 color; void main() { gl_FragColor = color; }
//! // Shaded
//! attribute vec4 pos; attribute vec4 col; varying vec4 v; void main() { v = col; gl_Position = pos; }
//! precision mediump float; varying vec4 v; void main() { gl_FragColor = v; }
//! ```
//!
//! The command lists are built here with `mali::Commands`, in the order Mesa's driver builds
//! them.
//!
//! 1. GPU on, the GP and PP0 reset, their MMUs paging over DRAM.
//! 2. For each scene, a 64 x 64 frame of 4 x 4 tiles into a buffer of its own. The GP: both
//!    lists end, without an error; the positions the shader wrote are the viewport's transform
//!    of the three vertices, and the colours it passed on are the vertices' own; the polygon list
//!    blocks of the tiles the triangle covers list it. A pattern marks what the GP left alone.
//!    Then PP0: every pixel checked against the triangle, the clear grey outside; those within a
//!    pixel of an edge are counted apart. On the board the PP's first line is the viewport's
//!    bottom: the apex is up in memory, down on the screen.
//! 3. If all hold: the screen on, and each scene drawn 256 x 256 on it, side by side, checked
//!    the same.
//!
//! Keys afterwards: `g` again, `r` restart, `f` restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::{self, Commands, GpFrame, IdentityMap, Mali, PpFrame, TileEnd};
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const WIDTH: usize = 1280;
const HEIGHT: usize = 800;
/// The frames: 64 into [`Shared::small`], then on the screen.
const SMALL: usize = 64;
const LARGE: usize = 256;
const MAX_TILES: usize = (LARGE / mali::TILE) * (LARGE / mali::TILE);
const BLOCK_WORDS: usize = mali::PLB_BLOCK / 4;
const HEAP_WORDS: usize = 16 * 1024;
const STREAM_WORDS: usize = MAX_TILES * 4 + 4;
const TIMEOUT_US: u64 = 100_000;
/// What the GP did not write still holds this.
const PATTERN: u32 = 0xDEAD_BEEF;

/// The clear colour, as Mesa's dump had it.
const GREY: u32 = 0xFF33_3333;
/// The flat triangle's colour as the PP writes it: its green, 0.5, is 127.5 of 255, which the
/// PP's dithering (`flip_dithering_matrix`) writes as 0x7F on some pixels and 0x80 on others.
const ORANGE: u32 = 0xFFFF_8000;
const ORANGE_DITHERED: u32 = 0xFFFF_7F00;
/// How far a shaded pixel's channel may be from the exact interpolation, in 1/255: half a step
/// of rounding, one of dithering, and the fragment shader's half floats.
const TOLERANCE: f32 = 2.0;

/// The triangle, in clip coordinates.
const VERTICES: [f32; 6] = [-0.75, -0.75, 0.75, -0.75, 0.0, 0.75];
/// The shaded triangle's corners: red, green, blue, opaque.
const COLORS: [f32; 12] = [1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0];

/// A pair of shaders from Mesa, and what Mesa's driver put around them.
struct Shaders {
    name: &'static str,
    /// 16 bytes an instruction.
    vertex: &'static [u32],
    /// Instructions the GP fetches ahead: `SHADER_INFO` in the dump.
    prefetch: u32,
    fragment: &'static [u32],
    /// The render state of the dump, its addresses zero: word 9 takes the fragment shader's,
    /// with the length of its first instruction (lima_draw.c:752-753); words 11 and 15 those of
    /// the uniforms and the varyings, when there are.
    render_state: [u32; 16],
    shaded: bool,
}

/// The flat scene. The vertex shader: a reciprocal of `w`, a clamp to the constants, the
/// viewport's scale and translation. The fragment shader: `load.u 0, mov.v1 $0 ^uniform, stop`.
const FLAT: Shaders = Shaders {
    name: "flat",
    vertex: &[
        0xad4a_d463, 0x4380_02b5, 0x0147_ff80, 0x000a_8d30, //
        0xad41_da56, 0x4380_02b5, 0x0007_ff80, 0x000a_d510, //
        0xad4a_d6b5, 0x0380_02b5, 0x0007_ff80, 0x000a_d500, //
        0x668a_d6b5, 0x0380_82b5, 0x003f_ff80, 0x000a_d500, //
        0x6c0a_d6b5, 0x0380_82b5, 0x0037_ff80, 0x000a_d500, //
        0xb408_0a01, 0x4380_0ab5, 0x0007_ff80, 0x000a_d500, //
        0xb307_4e00, 0x4380_2acd, 0x0247_ff80, 0x0008_4900, //
        0x74c8_4a36, 0x0380_62d4, 0x0007_ff80, 0x000a_d500, //
        0x64cb_42d1, 0x0380_41b2, 0x4002_6400, 0x000a_d508,
    ],
    prefetch: 6,
    fragment: &[0x0000_1224, 0x0000_0800, 0x001c_9e00, 0x000f_9e00],
    render_state: [
        0x0000_0000, 0x0000_0000, 0xfc3b_1ad2, 0x0000_003e, //
        0xffff_0000, 0xff00_0007, 0xff00_0007, 0x0000_ffff, //
        0x0000_f807, 0x0000_0000, 0x0000_0000, 0x0000_0003, //
        0x0000_0000, 0x0000_1280, 0x0001_2000, 0x0000_0000,
    ],
    shaded: false,
};

/// The shaded scene. The vertex shader also moves the colour attribute to the varying,
/// `v1.xyzw`. The fragment shader: `load.v $0 0, stop`, the varying as the colour.
const SHADED: Shaders = Shaders {
    name: "shaded",
    vertex: &[
        0xad4a_d463, 0x4380_02b5, 0x0147_ff80, 0x000a_8d30, //
        0xad41_da56, 0x4380_02b5, 0x0007_ff80, 0x000a_d510, //
        0xad4a_d6b5, 0x0380_02b5, 0x0007_ff80, 0x000a_d500, //
        0x668a_d6b5, 0x0380_82b5, 0x003f_ff80, 0x000a_d500, //
        0x6c0b_0eb5, 0x4780_82b5, 0x0037_ff80, 0x000a_d500, //
        0xb4c8_0602, 0x4380_2ad0, 0x0007_ff80, 0x000a_d500, //
        0xb307_4a20, 0x4380_2acd, 0x8246_ff80, 0x0008_c108, //
        0x74c8_4a3b, 0x4780_62c0, 0xc647_d080, 0x0000_8908, //
        0x64cb_42d7, 0x0380_41b2, 0x4002_6400, 0x000a_d508,
    ],
    prefetch: 7,
    fragment: &[0x0000_00a3, 0xf000_3c60, 0x0000_0000],
    render_state: [
        0x0000_0000, 0x0000_0000, 0xfc3b_1ad2, 0x0000_003e, //
        0xffff_0000, 0xff00_0007, 0xff00_0007, 0x0000_ffff, //
        0x0000_f807, 0x0000_0000, 0x0000_0000, 0x0000_0000, //
        0x0000_0000, 0x0000_1202, 0x0000_2000, 0x0000_0000,
    ],
    shaded: true,
};

/// The flat colour uniform, orange, in half floats: 1.0, 0.5, 0.0, 1.0.
const COLOR: [u32; 2] = [0x3800_3c00, 0x3c00_0000];

/// The vertex shaders' constants, after the viewport: the clamp.
const CLAMP: [f32; 4] = [-32768.0, 32768.0, 0.0, 0.0];

/// Where the varyings follow the positions in [`Shared::gp_output`], as Mesa lays them out:
/// 16 bytes a vertex of position, rounded up to 64 (lima_draw.c:983-988).
const VARYINGS_OFFSET: usize = 16;

/// Cache-line aligned, so that the GP's outputs can be dropped from the caches alone.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Lines<T>(T);

/// Page aligned, as Mesa's buffers are.
#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct Page<T>(T);

#[repr(C, align(4096))]
struct Shared {
    /// The 64 x 64 target.
    small: [u32; SMALL * SMALL],
    /// The polygon list blocks, one a tile.
    plb: [u32; MAX_TILES * BLOCK_WORDS],
    heap: [u32; HEAP_WORDS],
    stream: Lines<[u32; STREAM_WORDS]>,
    block_addresses: Lines<[u32; MAX_TILES]>,
    vertex_shader: Lines<[u32; 36]>,
    fragment_shader: Lines<[u32; 4]>,
    /// The viewport's scale and translation (lima_draw.c:877-882), then [`CLAMP`].
    uniforms: Lines<[f32; 12]>,
    attributes: Lines<[u32; 4]>,
    varyings: Lines<[u32; 4]>,
    vertices: Lines<[f32; 6]>,
    colors: Lines<[f32; 12]>,
    /// What the vertex shader writes: x, y, z and 1/w of each vertex, then from
    /// [`VARYINGS_OFFSET`] its colour, 16 bytes a vertex.
    gp_output: Page<[f32; 32]>,
    render_state: Lines<[u32; 16]>,
    /// The frame's own render state, for tiles without polygons: zeros.
    frame_state: Lines<[u32; 16]>,
    color: Lines<[u32; 2]>,
    color_address: Lines<[u32; 1]>,
    vs_list: Lines<[u32; 32]>,
    plbu_list: Lines<[u32; 64]>,
}

struct Store(UnsafeCell<Shared>);

// SAFETY: `main` is the only user, and takes the one reference to it once.
unsafe impl Sync for Store {}

static STORE: Store = Store(UnsafeCell::new(Shared {
    small: [0; SMALL * SMALL],
    plb: [0; MAX_TILES * BLOCK_WORDS],
    heap: [0; HEAP_WORDS],
    stream: Lines([0; STREAM_WORDS]),
    block_addresses: Lines([0; MAX_TILES]),
    vertex_shader: Lines([0; 36]),
    fragment_shader: Lines([0; 4]),
    uniforms: Lines([0.0; 12]),
    attributes: Lines([0; 4]),
    varyings: Lines([0; 4]),
    vertices: Lines([0.0; 6]),
    colors: Lines([0.0; 12]),
    gp_output: Page([0.0; 32]),
    render_state: Lines([0; 16]),
    frame_state: Lines([0; 16]),
    color: Lines([0; 2]),
    color_address: Lines([0; 1]),
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

fn verdict(good: bool) -> &'static str {
    if good { "OK" } else { "MISMATCH" }
}

fn address<T>(value: &T) -> u32 {
    value as *const T as u32
}

/// Where a frame goes: its side, its corner and the distance between its lines, in bytes.
#[derive(Clone, Copy)]
struct Target {
    size: usize,
    corner: usize,
    stride: usize,
}

/// Everything the GP reads for a `size` frame of `scene`, and the pattern where it writes;
/// returns its frame.
fn prepare(shared: &mut Shared, scene: &Shaders, size: usize) -> GpFrame {
    let tiles_x = size / mali::TILE;
    shared.plb.fill(PATTERN);
    shared.heap.fill(PATTERN);
    shared.gp_output.0.fill(f32::from_bits(PATTERN));
    let plb = address(&shared.plb);
    for (tile, slot) in shared.block_addresses.0.iter_mut().enumerate() {
        *slot = plb + (tile * mali::PLB_BLOCK) as u32;
    }
    shared.vertex_shader.0[..scene.vertex.len()].copy_from_slice(scene.vertex);
    shared.fragment_shader.0[..scene.fragment.len()].copy_from_slice(scene.fragment);
    let half = size as f32 / 2.0;
    shared.uniforms.0[..8].copy_from_slice(&[half, half, 0.5, 0.0, half, half, 0.5, 0.0]);
    shared.uniforms.0[8..].copy_from_slice(&CLAMP);
    shared.vertices.0 = VERTICES;
    shared.colors.0 = COLORS;
    // `stride << 11 | type << 2 | (components - 1)`, type 0 float: lima_draw.c:850-855.
    shared.attributes.0 = [address(&shared.vertices), (8 << 11) | (2 - 1), address(&shared.colors), (16 << 11) | (4 - 1)];
    let varyings = address(&shared.gp_output) + (VARYINGS_OFFSET * 4) as u32;
    // The position; the colour, 16 bytes a vertex, four 32-bit components: lima_draw.c:1006-1020.
    shared.varyings.0 = [address(&shared.gp_output), 0x8020, varyings, (16 << 11) | (4 - 1)];
    shared.color.0 = COLOR;
    shared.color_address.0 = [address(&shared.color)];
    shared.render_state.0 = scene.render_state;
    shared.render_state.0[9] = address(&shared.fragment_shader) | (scene.fragment[0] & 0x1F);
    if scene.shaded {
        shared.render_state.0[15] = varyings;
    } else {
        shared.render_state.0[11] |= address(&shared.color_address);
    }
    let (attributes, outputs) = if scene.shaded { (2, 2) } else { (1, 1) };

    // lima_draw.c:287-334.
    let vs_start = address(&shared.vs_list);
    let mut vs = Commands::new(&mut shared.vs_list.0);
    vs.vs_arrays_semaphore_begin();
    vs.vs_uniforms(address(&shared.uniforms), 48);
    vs.vs_shader(address(&shared.vertex_shader), (scene.vertex.len() * 4) as u32, scene.prefetch);
    vs.vs_counts(outputs, attributes);
    vs.vs_attributes(address(&shared.attributes), attributes);
    vs.vs_varyings(address(&shared.varyings), outputs);
    vs.vs_draw(3);
    let vs_end = vs_start + vs.bytes();

    // lima_job.c:406-434, then lima_draw.c:336-439.
    let plbu_start = address(&shared.plbu_list);
    let mut plbu = Commands::new(&mut shared.plbu_list.0);
    plbu.plbu_head(tiles_x as u32, tiles_x as u32, address(&shared.block_addresses));
    plbu.plbu_viewport(0.0, size as f32, 0.0, size as f32);
    plbu.plbu_arrays_semaphore_begin();
    plbu.plbu_primitive_setup(0);
    plbu.plbu_render_state(address(&shared.render_state), address(&shared.gp_output));
    plbu.plbu_scissor(0, size as u32, 0, size as u32);
    plbu.plbu_depth_range(0.0, 1.0);
    plbu.plbu_draw_arrays(mali::MODE_TRIANGLES, 0, 3);
    plbu.end();
    let plbu_end = plbu_start + plbu.bytes();

    let heap_start = address(&shared.heap);
    GpFrame { vs_start, vs_end, plbu_start, plbu_end, heap_start, heap_end: heap_start + (HEAP_WORDS * 4) as u32 }
}

/// Where the viewport puts a vertex: what the shader should write.
fn expected(vertex: usize, size: usize) -> [f32; 4] {
    let half = size as f32 / 2.0;
    let (x, y) = (VERTICES[vertex * 2], VERTICES[vertex * 2 + 1]);
    [x * half + half, y * half + half, 0.5, 1.0]
}

/// A square root by Newton's method: `core` has none without `std`.
fn sqrt(value: f32) -> f32 {
    let mut root = value.max(1.0);
    for _ in 0..20 {
        root = 0.5 * (root + value / root);
    }
    root
}

/// How far inside the triangle a point is, in pixels: negative outside.
fn inside(size: usize, x: f32, y: f32) -> f32 {
    let corners = [expected(0, size), expected(1, size), expected(2, size)];
    let mut nearest = f32::MAX;
    for i in 0..3 {
        let (a, b) = (corners[i], corners[(i + 1) % 3]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        // The corners go counter-clockwise with y up, so the inside is on the left.
        let distance = (dx * (y - a[1]) - dy * (x - a[0])) / sqrt(dx * dx + dy * dy);
        nearest = nearest.min(distance);
    }
    nearest
}

/// The weights of the three corners at a point: its barycentric coordinates.
fn weights(size: usize, x: f32, y: f32) -> [f32; 3] {
    let [a, b, c] = [expected(0, size), expected(1, size), expected(2, size)];
    let area = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
    let wa = ((b[0] - x) * (c[1] - y) - (b[1] - y) * (c[0] - x)) / area;
    let wb = ((c[0] - x) * (a[1] - y) - (c[1] - y) * (a[0] - x)) / area;
    [wa, wb, 1.0 - wa - wb]
}

/// The interpolated colour at a point, red, green and blue in 0..=255.
fn shade(size: usize, x: f32, y: f32) -> [f32; 3] {
    let w = weights(size, x, y);
    core::array::from_fn(|channel| (0..3).map(|corner| w[corner] * COLORS[corner * 4 + channel]).sum::<f32>() * 255.0)
}

/// Whether the triangle covers the centre of any pixel of tile `tile`.
fn covered(size: usize, tile: usize) -> bool {
    let tiles_x = size / mali::TILE;
    let (left, top) = ((tile % tiles_x) * mali::TILE, (tile / tiles_x) * mali::TILE);
    (top..top + mali::TILE).any(|y| (left..left + mali::TILE).any(|x| inside(size, x as f32 + 0.5, y as f32 + 0.5) >= 0.0))
}

fn written(words: &[u32]) -> usize {
    words.iter().filter(|word| **word != PATTERN).count()
}

/// Step 2's checks of what the GP wrote.
fn check_gp(shared: &Shared, scene: &Shaders, size: usize) -> bool {
    let mut good = true;
    for vertex in 0..3 {
        let got = &shared.gp_output.0[vertex * 4..vertex * 4 + 4];
        let want = expected(vertex, size);
        let mut right = got.iter().zip(want).all(|(got, want)| (got - want).abs() < 0.01);
        let color = &shared.gp_output.0[VARYINGS_OFFSET + vertex * 4..VARYINGS_OFFSET + vertex * 4 + 4];
        if scene.shaded {
            right &= color.iter().zip(&COLORS[vertex * 4..]).all(|(got, want)| (got - want).abs() < 0.001);
        }
        good &= right;
        println!(
            "Vertex {}: {:.3} {:.3} {:.3} {:.3}, want {:.1} {:.1} {:.1} {:.1}{}: {}",
            vertex,
            got[0],
            got[1],
            got[2],
            got[3],
            want[0],
            want[1],
            want[2],
            want[3],
            if scene.shaded { "; colour" } else { "" },
            verdict(right)
        );
        if scene.shaded {
            println!("  colour {:.3} {:.3} {:.3} {:.3}", color[0], color[1], color[2], color[3]);
        }
    }
    let tiles = (size / mali::TILE) * (size / mali::TILE);
    let (mut covered_tiles, mut written_tiles, mut listed, mut missing) = (0, 0, 0, 0);
    for tile in 0..tiles {
        let words = written(&shared.plb[tile * BLOCK_WORDS..(tile + 1) * BLOCK_WORDS]);
        // Every block gets the list's end, 2 words: more is a polygon.
        let polygon = words > 2;
        let covers = covered(size, tile);
        covered_tiles += covers as usize;
        written_tiles += (words > 0) as usize;
        listed += polygon as usize;
        missing += (covers && !polygon) as usize;
    }
    println!(
        "Tiles {}: covered {}, blocks written {}, with the triangle {}, covered without it {}: {}",
        tiles,
        covered_tiles,
        written_tiles,
        listed,
        missing,
        verdict(missing == 0)
    );
    good &= missing == 0;
    if let Some(tile) = (0..tiles).find(|tile| written(&shared.plb[tile * BLOCK_WORDS..(tile + 1) * BLOCK_WORDS]) > 2) {
        let block = &shared.plb[tile * BLOCK_WORDS..];
        println!(
            "Block {} at {:#x}: {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}; RSW {:#x}, output {:#x}",
            tile,
            address(&shared.plb[tile * BLOCK_WORDS]),
            block[0],
            block[1],
            block[2],
            block[3],
            block[4],
            block[5],
            address(&shared.render_state),
            address(&shared.gp_output)
        );
    }
    let heap_used = shared.heap.iter().rposition(|word| *word != PATTERN).map_or(0, |last| (last + 1) * 4);
    println!("Heap: {} words written, up to byte {}", written(&shared.heap), heap_used);
    good
}

/// Counts of a check of the PP's pixels.
#[derive(Clone, Copy, Debug, Default)]
struct Count {
    inside_wrong: usize,
    outside_wrong: usize,
    edge: usize,
    /// Flat: pixels inside written with green 0x7F.
    dithered: usize,
    /// Shaded: the largest channel error inside, in 1/255.
    worst: f32,
    first_wrong: Option<(usize, usize, u32)>,
}

impl Count {
    fn wrong(&self) -> usize {
        self.inside_wrong + self.outside_wrong
    }
}

fn channels(pixel: u32) -> [f32; 3] {
    [(pixel >> 16) & 0xFF, (pixel >> 8) & 0xFF, pixel & 0xFF].map(|value| value as f32)
}

/// `pixels` against the triangle, the frame's line `y` being the viewport's `size - 1 - y` with
/// `flipped`, `y` without.
fn count(pixels: &[u32], line: usize, size: usize, scene: &Shaders, flipped: bool) -> Count {
    let mut count = Count::default();
    for y in 0..size {
        let up = if flipped { size - 1 - y } else { y };
        for x in 0..size {
            let pixel = pixels[y * line + x];
            let (cx, cy) = (x as f32 + 0.5, up as f32 + 0.5);
            let depth = inside(size, cx, cy);
            if depth.abs() < 1.0 {
                count.edge += 1;
                continue;
            }
            let right = if depth < 0.0 {
                pixel == GREY
            } else if scene.shaded {
                let want = shade(size, cx, cy);
                let error = channels(pixel).iter().zip(want).map(|(got, want)| (got - want).abs()).fold(0.0, f32::max);
                count.worst = count.worst.max(error);
                error <= TOLERANCE && pixel >> 24 == 0xFF
            } else {
                count.dithered += (pixel == ORANGE_DITHERED) as usize;
                pixel == ORANGE || pixel == ORANGE_DITHERED
            };
            if !right {
                if depth > 0.0 {
                    count.inside_wrong += 1;
                } else {
                    count.outside_wrong += 1;
                }
                count.first_wrong.get_or_insert((x, y, pixel));
            }
        }
    }
    count
}

/// Step 2's check of what the PP wrote: `pixels` from the frame's corner, `line` pixels apart.
/// Both ways up are tried.
fn check_pixels(pixels: &[u32], line: usize, size: usize, scene: &Shaders) -> bool {
    let (down, up) = (count(pixels, line, size, scene, true), count(pixels, line, size, scene, false));
    let (best, way) = if down.wrong() <= up.wrong() { (down, "apex down in memory") } else { (up, "apex up in memory") };
    let good = best.wrong() == 0;
    println!(
        "Pixels {}x{}, {}: inside wrong {}, outside wrong {}, first {:x?}",
        size, size, way, best.inside_wrong, best.outside_wrong, best.first_wrong
    );
    let detail = if scene.shaded { "largest channel error" } else { "green 0x7f on" };
    let value = if scene.shaded { best.worst } else { best.dithered as f32 };
    println!(
        "  {} {:.1} inside; {} at the edges not checked; the other way up {} wrong: {}",
        detail,
        value,
        best.edge,
        down.wrong().max(up.wrong()),
        verdict(good)
    );
    if scene.shaded {
        // The corners as drawn: a pixel inside near each, and the centre.
        let points = [(0.25, 0.2), (0.75, 0.2), (0.5, 0.75), (0.5, 0.4)];
        let mut text = [0u32; 4];
        for (slot, (fx, fy)) in text.iter_mut().zip(points) {
            let (x, y) = ((fx * size as f32) as usize, (fy * size as f32) as usize);
            *slot = pixels[y * line + x];
        }
        println!("  near the corners {:08x} {:08x} {:08x}, centre {:08x}", text[0], text[1], text[2], text[3]);
    }
    good
}

/// A frame of `scene` through the GP, then PP0. Returns whether the GP's checks held and both
/// ended.
fn draw(gpu: Mali, mct: Mct, shared: &mut Shared, scene: &Shaders, target: Target) -> bool {
    let size = target.size;
    let frame = prepare(shared, scene, size);
    let words = mali::clear_stream(&mut shared.stream.0, size, size, 0, 1, TileEnd::Blocks(address(&shared.plb)));
    mmu::clean_dcache_range(shared as *const Shared as usize, size_of::<Shared>());
    // A soft reset of both before each frame, as Linux gives them after each job
    // (lima_gp.c:68-101, lima_pp.c:104-135): what they hold from the frame before goes.
    for (name, reset) in [("GP", gpu.reset_gp(mct, false)), ("PP0", gpu.reset_pp(0, mct, false))] {
        if let Err(error) = reset {
            println!("{} reset ERROR {:?}", name, error);
            return false;
        }
    }
    if let Err(error) = gpu.clear_l2(mct) {
        println!("L2 ERROR {:?}", error);
        return false;
    }
    if let Err(error) = gpu.start_gp(&frame) {
        println!("GP start ERROR {:?}", error);
        return false;
    }
    let result = gpu.wait_gp(&frame, mct, TIMEOUT_US);
    for region in [
        (shared.plb.as_ptr() as usize, size_of_val(&shared.plb)),
        (shared.heap.as_ptr() as usize, size_of_val(&shared.heap)),
        (address(&shared.gp_output) as usize, size_of_val(&shared.gp_output)),
    ] {
        mmu::invalidate_dcache_range(region.0, region.1);
    }
    let (gp_us, rawstat) = match result {
        Ok(done) => done,
        Err(error) => {
            println!("GP ERROR {:?}", error);
            println!("GP hard reset {:?}", gpu.hard_reset_gp(mct));
            return false;
        }
    };
    println!(
        "{} {}x{}: GP lists ended in {} us, INT_RAWSTAT {:#x}; PP stream {} words",
        scene.name, size, size, gp_us, rawstat, words
    );
    let good = check_gp(shared, scene, size);
    console::flush();

    let pp_frame = PpFrame::clear(size, size, GREY, target.corner, target.stride, address(&shared.frame_state) as usize);
    if let Err(error) = gpu.start_pp(0, &pp_frame, address(&shared.stream)) {
        println!("PP start ERROR {:?}", error);
        return false;
    }
    match gpu.wait_pp(0, mct, TIMEOUT_US) {
        Ok(us) => println!("PP0 frame in {} us", us),
        Err(error) => {
            println!("PP0 ERROR {:?}, state {:x?}", error, gpu.pp_state(0));
            println!("PP0 hard reset {:?}", gpu.hard_reset_pp(0, mct));
            return false;
        }
    }
    good
}

/// Step 2, for one scene.
fn small(gpu: Mali, mct: Mct, shared: &mut Shared, scene: &Shaders) -> bool {
    shared.small.fill(PATTERN);
    let corner = shared.small.as_ptr() as usize;
    let target = Target { size: SMALL, corner, stride: SMALL * 4 };
    let mut good = draw(gpu, mct, shared, scene, target);
    if good {
        mmu::invalidate_dcache_range(corner, size_of_val(&shared.small));
        good = check_pixels(&shared.small, SMALL, SMALL, scene);
    }
    println!("Triangle {} {}x{}: {}", scene.name, SMALL, SMALL, verdict(good));
    console::flush();
    good
}

/// Step 3 for one scene, onto the uncached screen at column `x`: nothing to keep in step with
/// the caches.
fn large(gpu: Mali, mct: Mct, shared: &mut Shared, scene: &Shaders, screen: &Framebuffer, x: usize) -> bool {
    let y = (HEIGHT - LARGE) / 2;
    let corner = screen.address() + (y * WIDTH + x) * 4;
    let target = Target { size: LARGE, corner, stride: screen.stride() };
    let mut good = draw(gpu, mct, shared, scene, target);
    if good {
        good = check_pixels(&screen.pixels()[y * WIDTH + x..], WIDTH, LARGE, scene);
    }
    println!("Triangle {} {}x{} on the screen: {}", scene.name, LARGE, LARGE, verdict(good));
    console::flush();
    good
}

/// Steps 2 and 3, each scene; the screen's step only if every small frame was right.
fn run(gpu: Mali, mct: Mct, shared: &mut Shared, screen: Option<&Framebuffer>) -> bool {
    let mut good = small(gpu, mct, shared, &FLAT);
    good &= small(gpu, mct, shared, &SHADED);
    if good && let Some(screen) = screen {
        // Two columns, even for the write-back: a quarter and three quarters across.
        good &= large(gpu, mct, shared, &FLAT, screen, WIDTH / 4 - LARGE / 2);
        good &= large(gpu, mct, shared, &SHADED, screen, WIDTH * 3 / 4 - LARGE / 2);
    }
    println!("Triangles: {}", verdict(good));
    good
}

fn start_screen(mct: Mct) -> Option<Framebuffer> {
    let fimd = Fimd::new();
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
    println!("Screen on, drawn {}", if screen.is_uncached() { "uncached" } else { "cached" });
    Some(screen)
}

fn wait_keys(gpu: Mali, mct: Mct, shared: &mut Shared, screen: Option<&Framebuffer>) -> ! {
    println!("Keys: g again, r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    loop {
        match uart.try_read_byte() {
            Some(b'g') => {
                run(gpu, mct, shared, screen);
                println!("Keys: g again, r restart, f fastboot");
                console::flush();
            }
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => core::hint::spin_loop(),
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPU triangles: Mali-400 GP and PP0, flat and shaded");
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
            wait_keys(gpu, mct, shared, None);
        }
    }
    if let Err(error) = gpu.pmu_power_up(mali::PMU_ALL, mct) {
        println!("GPU PMU ERROR {:?}", error);
    }
    if tables.map(soc::DRAM_BASE, 12 << 20).is_err() {
        println!("Page tables too few");
        wait_keys(gpu, mct, shared, None);
    }
    let gp = (gpu.reset_gp(mct, false), gpu.enable_mmu(None, tables.directory(), mct));
    let pp = (gpu.reset_pp(0, mct, false), gpu.enable_mmu(Some(0), tables.directory(), mct));
    if gp.0.is_err() || gp.1.is_err() || pp.0.is_err() || pp.1.is_err() {
        println!("GP reset, MMU {:?}; PP0 reset, MMU {:?}", gp, pp);
        wait_keys(gpu, mct, shared, None);
    }
    println!("GP and PP0 reset, their MMUs paging; buffers at {:#x}", shared as *const Shared as usize);
    console::flush();
    let screen = start_screen(mct);
    run(gpu, mct, shared, screen.as_ref());
    wait_keys(gpu, mct, shared, screen.as_ref());
}
