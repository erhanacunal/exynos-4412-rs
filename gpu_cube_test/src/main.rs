//! Two textured, lit cubes turning side by side, in perspective, drawn with the `gpu` runtime:
//! one mesh, two draws a frame, each with its own matrix and light. The frame is 512 x 256 in
//! the middle of the screen, the 512 tiles one frame takes at most.
//!
//! Each face has its own colour, from its cell of a texture atlas, and is lit by one distant
//! light from the upper left front: the vertex shader takes each vertex's brightness from its
//! face's normal, a quarter ambient and three quarters diffuse, and the fragment shader scales
//! the texel by it. The shaders are `shaders/lit.vert` and `lit.frag`, which
//! `make shaders APP=gpu_cube_test` compiles into `src/shaders.rs` (`tools/shaderc`).
//!
//! The light is each cube's own: the CPU turns the light's direction back by the cube's turns,
//! so that the shader needs the normals as they are. The depth test hides the far faces.
//!
//! Each frame starts at vsync; a line every two seconds gives the frame rate and the GPU's
//! microseconds.
//!
//! Keys: `b` bilinear or nearest, `v` vsync on or off, `p` pause, `r` restart, `f` restart into
//! fastboot.

#![no_std]
#![no_main]

use core::f32::consts::TAU;

use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::l2c::L2c;
use exynos_4412_rs::drivers::mali::Filter;
use exynos_4412_rs::drivers::power::{self, BootMode};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::gpu::{Area, Gpu, Mat4, Mesh, Texture, Times, Vec3};
use exynos_4412_rs::{BootArgs, console, mmu, println, soc};

mod shaders;

use shaders::LIT;

exynos_4412_rs::entry!(main);

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
/// The frame: 32 x 16 tiles.
const AREA: Area = Area::centered(512, 256);
const GREY: u32 = 0xFF33_3333;
const BACKGROUND: u32 = 0xFF20_2020;
/// Six faces of two triangles.
const VERTICES: usize = 36;
/// A turn about the vertical axis every 8 seconds, about the horizontal every 13.
const TURN_Y_US: u64 = 8_000_000;
const TURN_X_US: u64 = 13_000_000;
/// The cubes' centres, of half side 1, in front of the camera.
const CENTRES: [Vec3; 2] = [[-1.8, 0.0, -5.0], [1.8, 0.0, -5.0]];
/// The perspective: `2 atan(1 / 1.6)` from bottom to top.
const FOV_Y: f32 = 1.117;
const NEAR: f32 = 1.0;
const FAR: f32 = 10.0;
/// Towards the light, in the camera's space: up, left and in front; of length 1.
const LIGHT: Vec3 = [-0.408_248_3, 0.408_248_3, 0.816_496_6];
const REPORT_US: u64 = 2_000_000;

/// The atlas: 4 x 2 cells of 32 x 32 texels, a face to a cell.
const ATLAS_WIDTH: usize = 128;
const ATLAS_HEIGHT: usize = 64;
const CELL: usize = 32;
const CELLS_ACROSS: usize = ATLAS_WIDTH / CELL;
/// The faces' colours, red, green, blue from the top byte: +x, -x, +y, -y, +z, -z.
const FACE_COLORS: [u32; 6] = [0xE0_3030, 0x30_C040, 0x30_60E0, 0xE0_D030, 0xC0_40C0, 0x30_C0C0];

/// Each face's corners, counter-clockwise seen from outside, from its bottom left.
const FACES: [[Vec3; 4]; 6] = [
    [[1.0, -1.0, 1.0], [1.0, -1.0, -1.0], [1.0, 1.0, -1.0], [1.0, 1.0, 1.0]],
    [[-1.0, -1.0, -1.0], [-1.0, -1.0, 1.0], [-1.0, 1.0, 1.0], [-1.0, 1.0, -1.0]],
    [[-1.0, 1.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, -1.0], [-1.0, 1.0, -1.0]],
    [[-1.0, -1.0, -1.0], [1.0, -1.0, -1.0], [1.0, -1.0, 1.0], [-1.0, -1.0, 1.0]],
    [[-1.0, -1.0, 1.0], [1.0, -1.0, 1.0], [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0]],
    [[1.0, -1.0, -1.0], [-1.0, -1.0, -1.0], [-1.0, 1.0, -1.0], [1.0, 1.0, -1.0]],
];
/// Each face's normal, outwards.
const NORMALS: [Vec3; 6] =
    [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
/// Where the corners are in a cell, its first line at the top.
const CORNER_UVS: [[f32; 2]; 4] = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]];
/// A face's two triangles, by corner.
const FACE_TRIANGLES: [usize; 6] = [0, 1, 2, 0, 2, 3];

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

/// The cube's mesh and the atlas.
fn make_cube(gpu: &mut Gpu) -> Result<(Mesh, Texture), exynos_4412_rs::gpu::Error> {
    // Written straight into the GPU's memory: the 32 KiB atlas would not fit on the stack.
    let atlas = gpu.texture_with(ATLAS_WIDTH, ATLAS_HEIGHT, Filter::Bilinear, texel)?;

    let mut positions = [[0.0; 3]; VERTICES];
    let mut uvs = [[0.0; 2]; VERTICES];
    let mut normals = [[0.0; 3]; VERTICES];
    let scale = [CELL as f32 / ATLAS_WIDTH as f32, CELL as f32 / ATLAS_HEIGHT as f32];
    for (face, corners) in FACES.iter().enumerate() {
        let cell = [(face % CELLS_ACROSS) as f32, (face / CELLS_ACROSS) as f32];
        for (index, corner) in FACE_TRIANGLES.iter().enumerate() {
            let vertex = face * 6 + index;
            positions[vertex] = corners[*corner];
            normals[vertex] = NORMALS[face];
            let uv = CORNER_UVS[*corner];
            uvs[vertex] = [(cell[0] + uv[0]) * scale[0], (cell[1] + uv[1]) * scale[1]];
        }
    }
    let mesh = gpu.mesh(&LIT).attribute("pos", &positions).attribute("uv", &uvs).attribute("normal", &normals).build()?;
    Ok((mesh, atlas))
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
        self.sum.gp_us += times.gp_us;
        self.sum.pps_us += times.pps_us;
        self.sum.total_us += times.total_us;
        self.most.gp_us = self.most.gp_us.max(times.gp_us);
        self.most.pps_us = self.most.pps_us.max(times.pps_us);
        self.most.total_us = self.most.total_us.max(times.total_us);
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
            self.sum.gp_us / frames,
            self.most.gp_us,
            self.sum.pps_us / frames,
            self.most.pps_us,
            self.sum.total_us / frames,
            self.most.total_us
        );
    }
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

fn spin(gpu: &mut Gpu, mct: Mct, mesh: &Mesh, atlas: &Texture) -> ! {
    println!("Turning: keys b bilinear/nearest, v vsync, p pause, r restart, f fastboot");
    console::flush();
    let uart = console::uart();
    let projection = Mat4::perspective(FOV_Y, AREA.width as f32 / AREA.height as f32, NEAR, FAR);
    let (mut vsync, mut paused) = (true, false);
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
                gpu.set_filter(atlas, filter);
            }
            Some(b'v') => {
                vsync = !vsync;
                gpu.set_vsync(vsync);
            }
            Some(b'p') => paused = !paused,
            Some(b'r') => power::restart(BootMode::Normal),
            Some(b'f') => power::restart(BootMode::Fastboot),
            _ => {}
        }
        let now = mct.count();
        if !paused {
            shown_us += (now - last) / COUNTS_PER_US;
        }
        last = now;
        let y = (shown_us % TURN_Y_US) as f32 / TURN_Y_US as f32 * TAU;
        let x = (shown_us % TURN_X_US) as f32 / TURN_X_US as f32 * TAU;

        let mut frame = gpu.frame(AREA, GREY);
        // The second cube turns the other way.
        for (centre, sign) in CENTRES.iter().zip([1.0, -1.0]) {
            let model = Mat4::translation(centre[0], centre[1], centre[2]) * Mat4::rotation_x(sign * x) * Mat4::rotation_y(sign * y);
            let light = model.transform_direction_back(LIGHT);
            frame.draw(mesh).uniform("mvp", &(projection * model)).uniform("light", &light).texture("t", atlas).depth_test(true);
        }
        match frame.show() {
            Ok(times) => stats.add(times),
            Err(error) => {
                println!("Frame {} failed: {:?}", stats.frames, error);
                wait_keys();
            }
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
    println!("\nGPU cubes: two textured, lit cubes, two draws a frame, {}x{} at {},{}", AREA.width, AREA.height, AREA.x, AREA.y);
    let mct = Mct::new();
    mct.start();
    mmu::enable();
    L2c::new().enable();
    Gic::new().init();

    let mut gpu = match Gpu::start(mct, BACKGROUND) {
        Ok(gpu) => gpu,
        Err(error) => {
            println!("GPU start ERROR {:?}", error);
            wait_keys();
        }
    };
    let (mesh, atlas) = match make_cube(&mut gpu) {
        Ok(made) => made,
        Err(error) => {
            println!("Cube ERROR {:?}", error);
            wait_keys();
        }
    };
    spin(&mut gpu, mct, &mesh, &atlas);
}
