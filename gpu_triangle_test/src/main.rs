//! One triangle through the Mali-400: its geometry processor runs the vertex shader and the
//! PLBU, which bins the triangle into the polygon lists of the tiles; a pixel processor then
//! renders the tiles from those lists with the fragment shader.
//!
//! The shaders are compiled offline by Mesa 25.3.3's lima compiler (MIT), run on a PC under
//! `drm-shim` with `LIMA_DEBUG=dump`, from
//!
//! ```glsl
//! attribute vec4 pos; void main() { gl_Position = pos; }
//! precision mediump float; uniform vec4 color; void main() { gl_FragColor = color; }
//! ```
//!
//! The command lists are built here with `mali::Commands`, in the order Mesa's driver builds
//! them.
//!
//! 1. GPU on, the GP and PP0 reset, their MMUs paging over DRAM.
//! 2. A 64 x 64 frame of 4 x 4 tiles into a buffer of its own. The GP: both lists end, without an
//!    error; the positions the shader wrote are the viewport's transform of the three vertices;
//!    the polygon list blocks of the tiles the triangle covers were written. A pattern marks
//!    what the GP left alone. Then PP0: every pixel checked, orange inside the triangle, the
//!    clear grey outside; those within a pixel of an edge are counted apart. On the board the
//!    PP's first line is the viewport's bottom: the apex is up in memory, down on the screen.
//! 3. If all hold: the screen on, and a 256 x 256 frame drawn in its middle, checked the same.
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

/// The clear colour, as Mesa's dump had it, and the triangle's, as the PP writes them: its green,
/// 0.5, is 127.5 of 255, which the PP's dithering (`flip_dithering_matrix`) writes as 0x7F on
/// some pixels and 0x80 on others.
const GREY: u32 = 0xFF33_3333;
const ORANGE: u32 = 0xFFFF_8000;
const ORANGE_DITHERED: u32 = 0xFFFF_7F00;

fn is_orange(pixel: u32) -> bool {
    pixel == ORANGE || pixel == ORANGE_DITHERED
}

/// The triangle, in clip coordinates.
const VERTICES: [f32; 6] = [-0.75, -0.75, 0.75, -0.75, 0.0, 0.75];

/// The vertex shader: 9 instructions of 16 bytes, fetched 6 ahead. Mesa's disassembly: a
/// reciprocal of `w`, a clamp to the constants, the viewport's scale and translation.
const VERTEX_SHADER: [u32; 36] = [
    0xad4a_d463, 0x4380_02b5, 0x0147_ff80, 0x000a_8d30, //
    0xad41_da56, 0x4380_02b5, 0x0007_ff80, 0x000a_d510, //
    0xad4a_d6b5, 0x0380_02b5, 0x0007_ff80, 0x000a_d500, //
    0x668a_d6b5, 0x0380_82b5, 0x003f_ff80, 0x000a_d500, //
    0x6c0a_d6b5, 0x0380_82b5, 0x0037_ff80, 0x000a_d500, //
    0xb408_0a01, 0x4380_0ab5, 0x0007_ff80, 0x000a_d500, //
    0xb307_4e00, 0x4380_2acd, 0x0247_ff80, 0x0008_4900, //
    0x74c8_4a36, 0x0380_62d4, 0x0007_ff80, 0x000a_d500, //
    0x64cb_42d1, 0x0380_41b2, 0x4002_6400, 0x000a_d508,
];
const VERTEX_PREFETCH: u32 = 6;

/// The fragment shader: `load.u 0, mov.v1 $0 ^uniform, stop`.
const FRAGMENT_SHADER: [u32; 4] = [0x0000_1224, 0x0000_0800, 0x001c_9e00, 0x000f_9e00];

/// The render state Mesa made for the draw, its two addresses to be filled in: word 9, the
/// fragment shader's with the length of its first instruction, and word 11, the uniforms'
/// with their count (lima_draw.c:752-775).
const RENDER_STATE: [u32; 16] = [
    0x0000_0000, 0x0000_0000, 0xfc3b_1ad2, 0x0000_003e, //
    0xffff_0000, 0xff00_0007, 0xff00_0007, 0x0000_ffff, //
    0x0000_f807, 0x0000_0000, 0x0000_0000, 0x0000_0003, //
    0x0000_0000, 0x0000_1280, 0x0001_2000, 0x0000_0000,
];

/// The colour uniform, orange, in half floats: 1.0, 0.5, 0.0, 1.0.
const COLOR: [u32; 2] = [0x3800_3c00, 0x3c00_0000];

/// The vertex shader's constants, after the viewport: the clamp.
const CLAMP: [f32; 4] = [-32768.0, 32768.0, 0.0, 0.0];

/// Cache-line aligned, so that the GP's outputs can be dropped from the caches alone.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Lines<T>(T);

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
    attributes: Lines<[u32; 2]>,
    varyings: Lines<[u32; 2]>,
    vertices: Lines<[f32; 6]>,
    /// Where the vertex shader writes the positions: x, y, z and 1/w each.
    positions: Lines<[f32; 16]>,
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
    attributes: Lines([0; 2]),
    varyings: Lines([0; 2]),
    vertices: Lines([0.0; 6]),
    positions: Lines([0.0; 16]),
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

/// Everything the GP reads for a `size` frame, and the pattern where it writes; returns its
/// frame.
fn prepare(shared: &mut Shared, size: usize) -> GpFrame {
    let tiles_x = size / mali::TILE;
    shared.plb.fill(PATTERN);
    shared.heap.fill(PATTERN);
    shared.positions.0.fill(f32::from_bits(PATTERN));
    let plb = address(&shared.plb);
    for (tile, slot) in shared.block_addresses.0.iter_mut().enumerate() {
        *slot = plb + (tile * mali::PLB_BLOCK) as u32;
    }
    shared.vertex_shader.0 = VERTEX_SHADER;
    shared.fragment_shader.0 = FRAGMENT_SHADER;
    let half = size as f32 / 2.0;
    shared.uniforms.0[..8].copy_from_slice(&[half, half, 0.5, 0.0, half, half, 0.5, 0.0]);
    shared.uniforms.0[8..].copy_from_slice(&CLAMP);
    shared.vertices.0 = VERTICES;
    // Two floats a vertex, 8 bytes apart: lima_draw.c:850-855.
    shared.attributes.0 = [address(&shared.vertices), (8 << 11) | (2 - 1)];
    shared.varyings.0 = [address(&shared.positions), 0x8020];
    shared.color.0 = COLOR;
    shared.color_address.0 = [address(&shared.color)];
    shared.render_state.0 = RENDER_STATE;
    shared.render_state.0[9] = address(&shared.fragment_shader) | (FRAGMENT_SHADER[0] & 0x1F);
    shared.render_state.0[11] = RENDER_STATE[11] | address(&shared.color_address);

    // lima_draw.c:287-334.
    let vs_start = address(&shared.vs_list);
    let mut vs = Commands::new(&mut shared.vs_list.0);
    vs.vs_arrays_semaphore_begin();
    vs.vs_uniforms(address(&shared.uniforms), 48);
    vs.vs_shader(address(&shared.vertex_shader), (VERTEX_SHADER.len() * 4) as u32, VERTEX_PREFETCH);
    vs.vs_counts(1, 1);
    vs.vs_attributes(address(&shared.attributes), 1);
    vs.vs_varyings(address(&shared.varyings), 1);
    vs.vs_draw(3);
    let vs_end = vs_start + vs.bytes();

    // lima_job.c:406-434, then lima_draw.c:336-439.
    let plbu_start = address(&shared.plbu_list);
    let mut plbu = Commands::new(&mut shared.plbu_list.0);
    plbu.plbu_head(tiles_x as u32, tiles_x as u32, address(&shared.block_addresses));
    plbu.plbu_viewport(0.0, size as f32, 0.0, size as f32);
    plbu.plbu_arrays_semaphore_begin();
    plbu.plbu_primitive_setup(0);
    plbu.plbu_render_state(address(&shared.render_state), address(&shared.positions));
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

/// How far inside the triangle a point is, in pixels: negative outside.
fn inside(size: usize, x: f32, y: f32) -> f32 {
    let corners = [expected(0, size), expected(1, size), expected(2, size)];
    let mut nearest = f32::MAX;
    for i in 0..3 {
        let (a, b) = (corners[i], corners[(i + 1) % 3]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        // The corners go counter-clockwise with y up, so the inside is on the left.
        let distance = (dx * (y - a[1]) - dy * (x - a[0])) / libm_sqrt(dx * dx + dy * dy);
        nearest = nearest.min(distance);
    }
    nearest
}

/// A square root by Newton's method: `core` has none without `std`.
fn libm_sqrt(value: f32) -> f32 {
    let mut root = value.max(1.0);
    for _ in 0..20 {
        root = 0.5 * (root + value / root);
    }
    root
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
fn check_gp(shared: &Shared, size: usize) -> bool {
    let mut good = true;
    for vertex in 0..3 {
        let got = &shared.positions.0[vertex * 4..vertex * 4 + 4];
        let want = expected(vertex, size);
        let right = got.iter().zip(want).all(|(got, want)| (got - want).abs() < 0.01);
        good &= right;
        println!(
            "Vertex {}: {:.3} {:.3} {:.3} {:.3}, want {:.1} {:.1} {:.1} {:.1}: {}",
            vertex,
            got[0],
            got[1],
            got[2],
            got[3],
            want[0],
            want[1],
            want[2],
            want[3],
            verdict(right)
        );
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
    let block = &shared.plb[..];
    println!(
        "Block 0: {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x} {:08x}",
        block[0], block[1], block[2], block[3], block[4], block[5], block[6], block[7]
    );
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
    edge_orange: usize,
    /// Pixels inside written with green 0x7F.
    dithered: usize,
    first_wrong: Option<(usize, usize, u32)>,
}

impl Count {
    fn wrong(&self) -> usize {
        self.inside_wrong + self.outside_wrong
    }
}

/// `pixels` against the triangle, the frame's line `y` being the viewport's `size - 1 - y` with
/// `flipped`, `y` without.
fn count(pixels: &[u32], line: usize, size: usize, flipped: bool) -> Count {
    let mut count = Count::default();
    for y in 0..size {
        let up = if flipped { size - 1 - y } else { y };
        for x in 0..size {
            let pixel = pixels[y * line + x];
            let depth = inside(size, x as f32 + 0.5, up as f32 + 0.5);
            if depth.abs() < 1.0 {
                count.edge += 1;
                count.edge_orange += is_orange(pixel) as usize;
                continue;
            }
            count.dithered += (depth > 0.0 && pixel == ORANGE_DITHERED) as usize;
            let right = if depth > 0.0 { is_orange(pixel) } else { pixel == GREY };
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
/// Both ways up are tried: which way the PP's lines go is to be seen.
fn check_pixels(pixels: &[u32], line: usize, size: usize) -> bool {
    let (down, up) = (count(pixels, line, size, true), count(pixels, line, size, false));
    let (best, way) = if down.wrong() <= up.wrong() { (down, "apex down in memory") } else { (up, "apex up in memory") };
    let good = best.wrong() == 0;
    println!(
        "Pixels {}x{}, {}: inside wrong {}, outside wrong {}, first {:x?}",
        size, size, way, best.inside_wrong, best.outside_wrong, best.first_wrong
    );
    println!(
        "  green 0x7f on {} inside; at the edges {} of {} orange; the other way up {} wrong: {}",
        best.dithered,
        best.edge_orange,
        best.edge,
        down.wrong().max(up.wrong()),
        verdict(good)
    );
    if !good {
        let (x, y) = (size / 2, size / 2);
        println!("  centre {:#010x}, corner {:#010x}", pixels[y * line + x], pixels[0]);
    }
    good
}

/// A frame through the GP, then PP0. Returns whether every check held.
fn draw(gpu: Mali, mct: Mct, shared: &mut Shared, target: Target) -> bool {
    let size = target.size;
    let frame = prepare(shared, size);
    let words = mali::clear_stream(&mut shared.stream.0, size, size, 0, 1, TileEnd::Blocks(address(&shared.plb)));
    mmu::clean_dcache_range(shared as *const Shared as usize, size_of::<Shared>());
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
        (address(&shared.positions) as usize, size_of_val(&shared.positions)),
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
    println!("{}x{}: GP lists ended in {} us, INT_RAWSTAT {:#x}; PP stream {} words", size, size, gp_us, rawstat, words);
    let good = check_gp(shared, size);
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

/// Step 2.
fn small(gpu: Mali, mct: Mct, shared: &mut Shared) -> bool {
    shared.small.fill(PATTERN);
    let corner = shared.small.as_ptr() as usize;
    let target = Target { size: SMALL, corner, stride: SMALL * 4 };
    let mut good = draw(gpu, mct, shared, target);
    if good {
        mmu::invalidate_dcache_range(corner, size_of_val(&shared.small));
        good = check_pixels(&shared.small, SMALL, SMALL);
    }
    println!("Triangle {}x{}: {}", SMALL, SMALL, verdict(good));
    good
}

/// Step 3, onto the uncached screen: nothing to keep in step with the caches.
fn large(gpu: Mali, mct: Mct, shared: &mut Shared, screen: &Framebuffer) -> bool {
    let (x, y) = ((WIDTH - LARGE) / 2, (HEIGHT - LARGE) / 2);
    let corner = screen.address() + (y * WIDTH + x) * 4;
    let target = Target { size: LARGE, corner, stride: screen.stride() };
    let mut good = draw(gpu, mct, shared, target);
    if good {
        good = check_pixels(&screen.pixels()[y * WIDTH + x..], WIDTH, LARGE);
    }
    println!("Triangle {}x{} on the screen: {}", LARGE, LARGE, verdict(good));
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
                if small(gpu, mct, shared)
                    && let Some(screen) = screen
                {
                    large(gpu, mct, shared, screen);
                }
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
    println!("\nGPU triangle: Mali-400 GP and PP0");
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
    if !small(gpu, mct, shared) {
        wait_keys(gpu, mct, shared, None);
    }
    let screen = start_screen(mct);
    if let Some(screen) = &screen {
        large(gpu, mct, shared, screen);
    }
    wait_keys(gpu, mct, shared, screen.as_ref());
}
