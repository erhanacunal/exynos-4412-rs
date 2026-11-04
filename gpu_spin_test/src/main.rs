//! A textured square turning and pulsing in the middle of the screen, drawn by the Mali-400
//! every frame: the CPU turns the four corners, the GP runs the vertex shader over the two
//! triangles and bins them, and the four PPs render the 320 x 320 frame straight into the
//! screen, each taking every fourth tile.
//!
//! The shaders are `gpu_triangle_test`'s textured pair, compiled offline by Mesa 25.3.3's lima
//! compiler (MIT); the texture is 64 x 64 RGBA8888, sampled nearest.
//!
//! Each frame starts at the start of vsync. The frame is on the screen's middle lines, which
//! the FIMD scans some milliseconds later; the GPU is done well before, so the screen shows
//! whole frames without a second buffer. Every two seconds a line: frames a second, and the
//! microseconds of the GP, of the PPs and of the whole frame's GPU work, mean and largest.
//!
//! Keys: `v` vsync on or off (off: as fast as the GPU goes), `p` pause, `r` restart, `f`
//! restart into fastboot.

#![no_std]
#![no_main]

use core::cell::UnsafeCell;
use core::f32::consts::TAU;

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
/// The frame's side: 400 tiles, within one polygon list block a tile (`PLB_MAX_BLOCKS`).
const SIZE: usize = 320;
/// The frame's corner on the screen: on a tile column and a 64-byte line, as the PP writes
/// fast (README, "First triangle").
const LEFT: usize = (WIDTH - SIZE) / 2 / mali::TILE * mali::TILE;
const TOP: usize = (HEIGHT - SIZE) / 2;
const TILES: usize = (SIZE / mali::TILE) * (SIZE / mali::TILE);
const BLOCK_WORDS: usize = mali::PLB_BLOCK / 4;
const HEAP_WORDS: usize = 16 * 1024;
const STREAM_WORDS: usize = TILES * 4 + 4;
const TIMEOUT_US: u64 = 100_000;
/// The background, the screen's and the frame's clear colour.
const GREY: u32 = 0xFF33_3333;
/// Two triangles, six vertices.
const VERTICES: usize = 6;
/// A turn every 6 seconds; a pulse every 2.
const TURN_US: u64 = 6_000_000;
const PULSE_US: u64 = 2_000_000;
/// The square's half side in clip coordinates: from 0.45 to 0.65, and its corners within
/// 0.92 of the centre, inside the frame.
const HALF_SIDE: f32 = 0.55;
const PULSE: f32 = 0.10;
const REPORT_US: u64 = 2_000_000;

/// Texels on a side of the texture.
const TEXELS: usize = 64;

/// The square's corners, counter-clockwise from the bottom left, and where each is in the
/// texture: its first line is at the top.
const CORNERS: [[f32; 2]; 4] = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
const CORNER_UVS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
/// The two triangles, by corner.
const TRIANGLES: [usize; VERTICES] = [0, 1, 2, 0, 2, 3];

/// `gpu_triangle_test`'s textured vertex shader: the position through the viewport, the
/// texture coordinates to `v1.xy`.
const VERTEX_SHADER: [u32; 36] = [
    0xad4a_d463, 0x4380_02b5, 0x0147_ff80, 0x000a_8d30, //
    0xad41_da56, 0x4380_02b5, 0x0007_ff80, 0x000a_d510, //
    0xad4a_d6b5, 0x0380_02b5, 0x0007_ff80, 0x000a_d500, //
    0x668a_d6b5, 0x0380_82b5, 0x003f_ff80, 0x000a_d500, //
    0x6c0a_d6b5, 0x0380_82b5, 0x0037_ff80, 0x000a_d500, //
    0xb408_0a01, 0x4380_0ab5, 0x0007_ff80, 0x000a_d500, //
    0xb307_4e00, 0x4380_2acd, 0x0247_ff80, 0x0008_4900, //
    0x74c8_4a36, 0x4780_62c0, 0x4647_f080, 0x0000_d100, //
    0x64cb_42d6, 0x0380_41b2, 0x4002_6400, 0x000a_d508,
];
const PREFETCH: u32 = 7;
/// `load.v 0.xy, texld 0, mov.v1 $0 ^texture, sync, stop`.
const FRAGMENT_SHADER: [u32; 6] = [0x0000_11e6, 0x3f00_3c20, 0x0000_0000, 0x3900_1000, 0x0000_0e4e, 0x0000_07cf];
/// The dump's render state, its addresses zero: words 9, 12 and 15 take the fragment
/// shader's, the texture list's and the varyings'.
const RENDER_STATE: [u32; 16] = [
    0x0000_0000, 0x0000_0000, 0xfc3b_1ad2, 0x0000_003e, //
    0xffff_0000, 0xff00_0007, 0xff00_0007, 0x0000_ffff, //
    0x0000_f807, 0x0000_0000, 0x0000_0001, 0x0000_0000, //
    0x0000_0000, 0x0000_5222, 0x0000_2000, 0x0000_0000,
];
/// The vertex shader's constants, after the viewport: the clamp.
const CLAMP: [f32; 4] = [-32768.0, 32768.0, 0.0, 0.0];
/// Where the varyings follow the positions in [`Shared::gp_output`]: 16 bytes a vertex of
/// position, rounded up to 64 (lima_draw.c:983-988).
const VARYINGS_OFFSET: usize = (VERTICES * 16).next_multiple_of(64) / 4;
/// Words of the texture list before its descriptor: `lima_tex_list_size`, 64 bytes.
const DESCRIPTOR_OFFSET: usize = 16;

/// The texture's texel at column `x`, line `y`, alpha, red, green, blue from the top byte: a
/// white border; inside, red growing to the right and green down, on a checkerboard of 8 x 8
/// texel squares, bright on blue and dim.
fn texel(x: usize, y: usize) -> u32 {
    let last = TEXELS - 2;
    if x < 2 || y < 2 || x >= last || y >= last {
        return 0xFFFF_FFFF;
    }
    let (red, green) = ((x * 4) as u32, (y * 4) as u32);
    let (red, green, blue) = if ((x / 8) ^ (y / 8)) & 1 == 0 { (red, green, 0xC0) } else { (red / 2, green / 2, 0x30) };
    0xFF00_0000 | (red << 16) | (green << 8) | blue
}

/// Sine and cosine: `core` has none without `std`. Taylor series to the 13th power after
/// bringing the angle into -pi..pi, within 3e-5 there, ample for pixels.
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
    texels: Lines<[u32; TEXELS * TEXELS]>,
    vertex_shader: Lines<[u32; 36]>,
    fragment_shader: Lines<[u32; 8]>,
    /// The viewport's scale and translation (lima_draw.c:877-882), then [`CLAMP`].
    uniforms: Lines<[f32; 12]>,
    attributes: Lines<[u32; 4]>,
    varyings: Lines<[u32; 4]>,
    /// The six vertices' x and y, rewritten each frame.
    positions: Lines<[f32; VERTICES * 2]>,
    uvs: Lines<[f32; VERTICES * 2]>,
    /// The PP's texture list, then the descriptor at [`DESCRIPTOR_OFFSET`].
    textures: Lines<[u32; DESCRIPTOR_OFFSET + 16]>,
    /// What the vertex shader writes: the positions, then from [`VARYINGS_OFFSET`] the
    /// texture coordinates, 16 bytes a vertex.
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
    texels: Lines([0; TEXELS * TEXELS]),
    vertex_shader: Lines([0; 36]),
    fragment_shader: Lines([0; 8]),
    uniforms: Lines([0.0; 12]),
    attributes: Lines([0; 4]),
    varyings: Lines([0; 4]),
    positions: Lines([0.0; VERTICES * 2]),
    uvs: Lines([0.0; VERTICES * 2]),
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

/// Everything but the positions, once; returns the GP's frame and the PPs'.
fn prepare(shared: &mut Shared, screen: &Framebuffer) -> (GpFrame, PpFrame) {
    let plb = address(&shared.plb);
    for (tile, slot) in shared.block_addresses.0.iter_mut().enumerate() {
        *slot = plb + (tile * mali::PLB_BLOCK) as u32;
    }
    for pp in 0..mali::PP_COUNT {
        mali::clear_stream(&mut shared.streams[pp].0, SIZE, SIZE, pp, mali::PP_COUNT, TileEnd::Blocks(plb));
    }
    for (index, slot) in shared.texels.0.iter_mut().enumerate() {
        let [alpha, red, green, blue] = texel(index % TEXELS, index / TEXELS).to_be_bytes();
        *slot = u32::from_le_bytes([red, green, blue, alpha]);
    }
    shared.vertex_shader.0 = VERTEX_SHADER;
    shared.fragment_shader.0[..FRAGMENT_SHADER.len()].copy_from_slice(&FRAGMENT_SHADER);
    let half = SIZE as f32 / 2.0;
    // y scaled by minus half: the scene's top on the frame's first line.
    shared.uniforms.0[..8].copy_from_slice(&[half, -half, 0.5, 0.0, half, half, 0.5, 0.0]);
    shared.uniforms.0[8..].copy_from_slice(&CLAMP);
    for (vertex, corner) in TRIANGLES.iter().enumerate() {
        shared.uvs.0[vertex * 2..vertex * 2 + 2].copy_from_slice(&CORNER_UVS[*corner]);
    }
    // `stride << 11 | type << 2 | (components - 1)`, type 0 float: lima_draw.c:850-855.
    shared.attributes.0 = [address(&shared.positions), (8 << 11) | (2 - 1), address(&shared.uvs), (8 << 11) | (2 - 1)];
    let varyings = address(&shared.gp_output) + (VARYINGS_OFFSET * 4) as u32;
    // The position; the texture coordinates, 16 bytes a vertex, two 32-bit components.
    shared.varyings.0 = [address(&shared.gp_output), 0x8020, varyings, (16 << 11) | (2 - 1)];
    let descriptor = address(&shared.textures) + (DESCRIPTOR_OFFSET * 4) as u32;
    shared.textures.0[0] = descriptor;
    shared.textures.0[DESCRIPTOR_OFFSET..]
        .copy_from_slice(&mali::texture_rgba8888(address(&shared.texels), TEXELS as u32, TEXELS as u32));
    shared.render_state.0 = RENDER_STATE;
    shared.render_state.0[9] = address(&shared.fragment_shader) | (FRAGMENT_SHADER[0] & 0x1F);
    shared.render_state.0[12] = address(&shared.textures);
    shared.render_state.0[15] = varyings;

    // lima_draw.c:287-334.
    let vs_start = address(&shared.vs_list);
    let mut vs = Commands::new(&mut shared.vs_list.0);
    vs.vs_arrays_semaphore_begin();
    vs.vs_uniforms(address(&shared.uniforms), 48);
    vs.vs_shader(address(&shared.vertex_shader), (VERTEX_SHADER.len() * 4) as u32, PREFETCH);
    vs.vs_counts(2, 2);
    vs.vs_attributes(address(&shared.attributes), 2);
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

/// The square turned by `angle` and scaled by `scale`, into the positions; cleaned to DRAM.
fn place(shared: &mut Shared, angle: f32, scale: f32) {
    let (sin, cos) = sin_cos(angle);
    for (vertex, corner) in TRIANGLES.iter().enumerate() {
        let [x, y] = CORNERS[*corner];
        shared.positions.0[vertex * 2] = scale * (x * cos - y * sin);
        shared.positions.0[vertex * 2 + 1] = scale * (x * sin + y * cos);
    }
    mmu::clean_dcache_range(shared.positions.0.as_ptr() as usize, size_of_val(&shared.positions));
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
    // The GPU's L2 may hold the last frame's positions.
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

    fn report(&self, us: u64, vsync: bool) {
        let frames = self.frames.max(1);
        let fps_tenths = self.frames * 10_000_000 / us.max(1);
        println!(
            "{}.{} fps, vsync {}: GP {}/{} us, PPs {}/{} us, all {}/{} us (mean/max)",
            fps_tenths / 10,
            fps_tenths % 10,
            if vsync { "on" } else { "off" },
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
    println!("Turning: keys v vsync, p pause, r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    let (mut vsync, mut paused) = (true, false);
    // Time of the animation, which a pause stops.
    let mut shown_us = 0;
    let mut last = mct.count();
    let mut report_start = last;
    let mut stats = Stats::default();
    loop {
        match uart.try_read_byte() {
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
        let angle = (shown_us % TURN_US) as f32 / TURN_US as f32 * TAU;
        let (pulse, _) = sin_cos((shown_us % PULSE_US) as f32 / PULSE_US as f32 * TAU);
        place(shared, angle, HALF_SIDE + PULSE * pulse);
        match draw(gpu, mct, gp, pp, shared) {
            Ok(times) => stats.add(times),
            Err(what) => {
                println!("Frame {} failed: {}", stats.frames, what);
                wait_keys();
            }
        }
        let since = (mct.count() - report_start) / COUNTS_PER_US;
        if since >= REPORT_US {
            stats.report(since, vsync);
            stats = Stats::default();
            report_start = mct.count();
        }
    }
}

fn main(_args: BootArgs) -> ! {
    println!("\nGPU spin: a textured square, two triangles, {}x{} at {},{} on four PPs", SIZE, SIZE, LEFT, TOP);
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
