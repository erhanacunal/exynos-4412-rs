//! Drawing: [`Gpu`] starts the GPU and the screen, holds meshes and textures, and runs
//! [`Frame`]s of [`Draw`]s. See the module's documentation, `gpu`.
//!
//! [`Gpu::start_offscreen`] starts the GPU alone, drawing into a buffer of the caller's
//! ([`Target`]): for an application that drives the screen itself and copies the frames in.

use core::cell::UnsafeCell;

use super::Program;
use super::Stage;
use super::math::Mat4;
use super::memory::{self, Arena, Fixed, MAX_DRAWS, MAX_TILES, PLBU_DRAW_WORDS, VS_DRAW_WORDS};
use crate::drivers::display::{self, Fimd, Framebuffer, LTN101AL03, LVDS_SETTLE_MS};
use crate::drivers::mali::{self, Commands, Filter, GpFrame, Mali, PpFrame, TileEnd};
use crate::drivers::timer::Mct;
use crate::{mmu, soc};

/// The screen, the LTN101AL03's.
pub const WIDTH: usize = 1280;
/// The screen, the LTN101AL03's.
pub const HEIGHT: usize = 800;

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;
const TIMEOUT_US: u64 = 100_000;
/// The position's varying info: lima_draw.c:1006-1009.
const POSITION_VARYING: u32 = 0x8020;
/// Render state word 3: the depth test (bit 0) and its compare function (bits 1-3), `Less`
/// being 1 (genxml/common.xml, `Render State` and `Compare Func`).
const DEPTH_TEST: u32 = 1;
const DEPTH_FUNC_MASK: u32 = 0x7 << 1;
const DEPTH_FUNC_LESS: u32 = 1 << 1;
/// Descriptor addresses in a texture list: `lima_tex_list_size`, 64 bytes.
const MAX_SAMPLERS: usize = 16;

#[repr(C, align(4096))]
struct Pixels(UnsafeCell<[u32; WIDTH * HEIGHT]>);

// SAFETY: `Gpu::start` takes the one reference to it, once (`memory::take`).
unsafe impl Sync for Pixels {}

static PIXELS: Pixels = Pixels(UnsafeCell::new([0; WIDTH * HEIGHT]));

/// A buffer frames are drawn into, 32 bits a pixel (0xAARRGGBB as the screen shows them), line
/// after line `stride` bytes apart: the screen's pixels, or the caller's own
/// ([`Gpu::start_offscreen`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// The first pixel's address: a multiple of 64.
    pub address: usize,
    /// Bytes from a line to the next: a multiple of 64, at least `width * 4`.
    pub stride: usize,
    /// Pixels in a line: a multiple of 16.
    pub width: usize,
    /// Lines: a multiple of 16.
    pub height: usize,
}

impl Target {
    fn is_good(&self) -> bool {
        self.address % 64 == 0
            && self.stride % 64 == 0
            && self.stride >= self.width * 4
            && self.width % mali::TILE == 0
            && self.height % mali::TILE == 0
            && self.width > 0
            && self.height > 0
    }

    fn bytes(&self) -> usize {
        self.stride * self.height
    }
}

/// What went wrong. The `&'static str`s are the names asked for.
#[derive(Clone, Copy, Debug)]
pub enum Error {
    /// [`Gpu::start`] ran before.
    AlreadyStarted,
    /// [`Gpu::start`] needs the MMU on: `mmu::enable()`.
    MmuOff,
    /// The GPU would not come on.
    Mali(mali::Error),
    /// The screen would not come on.
    Display(display::Error),
    /// The screen cannot be drawn uncached, so the GPU's writes would not show.
    ScreenCached,
    /// The GPU's MMU tables are too few for the memory.
    PageTables,
    /// The pool is full: [`memory::LASTING_BYTES`] for meshes and textures, or
    /// [`memory::FRAME_BYTES`] for a frame's draws.
    OutOfMemory,
    /// More than [`memory::MAX_DRAWS`] draws in a frame.
    TooManyDraws,
    /// The frame's area: its corner and size not multiples of 16, beyond the target, or more
    /// than [`memory::MAX_TILES`] tiles of 16 x 16.
    BadArea,
    /// The program has no attribute of this name.
    UnknownAttribute(&'static str),
    /// The mesh lacks this attribute of the program.
    MissingAttribute(&'static str),
    /// The attributes do not all have as many vertices.
    VertexCounts,
    /// The mesh has no vertices, or a multiple of three it is not: triangles.
    NotTriangles,
    /// The program has no uniform of this name.
    UnknownUniform(&'static str),
    /// The uniform takes `want` floats, not `got`.
    UniformSize {
        /// Its name.
        name: &'static str,
        /// Floats it takes: 16 for a `mat4`.
        want: usize,
        /// Floats given.
        got: usize,
    },
    /// A draw left this uniform unset.
    MissingUniform(&'static str),
    /// The program has no sampler of this name.
    UnknownSampler(&'static str),
    /// A draw left this sampler without a texture.
    MissingTexture(&'static str),
    /// The texels are not `width * height`, or the texture is empty or too large.
    TextureSize,
    /// The screen's vsync did not come.
    Vsync,
    /// [`Gpu::start_offscreen`]'s target: not aligned, or its size not whole tiles.
    BadTarget,
    /// The GP raised an error or did not finish.
    Gp(mali::Error),
    /// A PP raised an error or did not finish.
    Pp(mali::Error),
}

/// Where on the screen a frame goes, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    /// The left column: a multiple of 16.
    pub x: usize,
    /// The top line: a multiple of 16.
    pub y: usize,
    /// A multiple of 16.
    pub width: usize,
    /// A multiple of 16.
    pub height: usize,
}

impl Area {
    /// The area `width` x `height` at column `x`, line `y`.
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }

    /// `width` x `height` in the middle of the screen, its corner moved up and left to a
    /// multiple of 16.
    pub const fn centered(width: usize, height: usize) -> Self {
        Self { x: (WIDTH - width) / 2 / 16 * 16, y: (HEIGHT - height) / 2 / 16 * 16, width, height }
    }

    fn tiles(&self) -> (usize, usize) {
        (self.width / mali::TILE, self.height / mali::TILE)
    }

    fn is_good(&self, target: &Target) -> bool {
        let (across, down) = self.tiles();
        let whole = [self.x, self.y, self.width, self.height].iter().all(|value| value % mali::TILE == 0);
        whole
            && across > 0
            && down > 0
            && across * down <= MAX_TILES
            && self.x + self.width <= target.width
            && self.y + self.height <= target.height
    }
}

/// How long a frame took on the GPU, in microseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Times {
    /// The GP: vertex shaders and binning.
    pub gp_us: u64,
    /// The four PPs, from the first's start to the last's end.
    pub pps_us: u64,
    /// From the resets to the last PP's end.
    pub total_us: u64,
}

/// Values for a uniform: an `f32`, an array of them, or a [`Mat4`].
pub trait Floats {
    /// The values, in the order GLSL takes them.
    fn floats(&self) -> &[f32];
}

impl Floats for f32 {
    fn floats(&self) -> &[f32] {
        core::slice::from_ref(self)
    }
}

impl<const N: usize> Floats for [f32; N] {
    fn floats(&self) -> &[f32] {
        self
    }
}

impl Floats for [f32] {
    fn floats(&self) -> &[f32] {
        self
    }
}

impl Floats for Mat4 {
    fn floats(&self) -> &[f32] {
        &self.0
    }
}

/// A texture in the GPU's memory: see [`Gpu::texture`].
#[derive(Clone, Copy, Debug)]
pub struct Texture {
    descriptor: usize,
    texels: u32,
    width: u32,
    height: u32,
}

/// Triangles for one [`Program`], their attributes in the GPU's memory: see [`Gpu::mesh`].
#[derive(Clone, Copy, Debug)]
pub struct Mesh {
    program: &'static Program,
    vertices: u32,
    vertex_shader: usize,
    fragment_shader: usize,
    attribute_info: usize,
}

impl Mesh {
    /// Its vertices: three a triangle.
    pub fn vertices(&self) -> u32 {
        self.vertices
    }
}

/// The GPU and the screen. One, from [`Gpu::start`] or [`Gpu::start_offscreen`].
pub struct Gpu {
    mali: Mali,
    mct: Mct,
    /// The screen, with [`Gpu::start`]; none offscreen.
    fimd: Option<Fimd>,
    screen: Option<Framebuffer>,
    /// Where frames are drawn: the screen's pixels, or the caller's buffer.
    target: Target,
    fixed: &'static mut Fixed,
    lasting: Arena,
    frame: Arena,
    /// The area the PPs' streams are for.
    streams_for: Option<(usize, usize)>,
    vsync: bool,
}

impl Gpu {
    /// Powers the GPU at 160 MHz, maps the pool for its MMUs, and turns the screen on, all of
    /// it `background`, 0xAARRGGBB. Needs the MMU on (`mmu::enable()`), for an uncached screen.
    pub fn start(mct: Mct, background: u32) -> Result<Self, Error> {
        if !mmu::is_enabled() {
            return Err(Error::MmuOff);
        }
        let pool = memory::take()?;
        let mali = Mali::new();
        mali.power_on(mali::SAFE_HZ, mct).map_err(Error::Mali)?;
        mali.pmu_power_up(mali::PMU_ALL, mct).map_err(Error::Mali)?;

        let fimd = Fimd::new();
        fimd.power_on().map_err(Error::Display)?;
        display::configure_pins();
        // SAFETY: the only reference to the pixels: `memory::take` let this run once.
        let pixels: &'static mut [u32] = unsafe { &mut *PIXELS.0.get() };
        let pixels_end = pixels.as_ptr() as usize + size_of_val(pixels);
        let mut screen = Framebuffer::new(pixels, WIDTH, HEIGHT).ok_or(Error::ScreenCached)?;
        if !screen.is_uncached() {
            return Err(Error::ScreenCached);
        }
        screen.fill_rect(0, 0, WIDTH, HEIGHT, background);
        screen.flush();
        fimd.start(&LTN101AL03, &screen).map_err(Error::Display)?;
        display::set_lvds(true);
        let settle = mct.count();
        while mct.count() - settle < u64::from(LVDS_SETTLE_MS) * 1000 * COUNTS_PER_US {}
        display::set_backlight(true);

        // The GPU sees DRAM from its start to the end of the pool and the pixels.
        let pool_end = pool as *const memory::Pool as usize + size_of::<memory::Pool>();
        let end = pool_end.max(pixels_end);
        pool.tables.map(soc::DRAM_BASE, end - soc::DRAM_BASE).map_err(|_| Error::PageTables)?;
        let target = Target { address: screen.address(), stride: screen.stride(), width: WIDTH, height: HEIGHT };
        Self::bring_up(mali, mct, pool, Some(fimd), Some(screen), target)
    }

    /// Powers the GPU at 160 MHz and maps the pool and `target` for its MMUs, without the
    /// screen: frames are drawn into `target`, [`frame`](Self::frame)s don't wait for vsync, and
    /// [`screen`](Self::screen) has nothing to give. The GPU writes `target` past the CPU's
    /// caches: have no dirty lines of it in them (`mmu::discard_dcache_range` first), read it
    /// after a frame through an uncached view or by a device, and keep the CPU's writes out of
    /// it.
    /// Needs the MMU on (`mmu::enable()`).
    ///
    /// # Safety
    ///
    /// `target` is memory the caller owns for as long as the `Gpu` lives, which nothing else
    /// uses meanwhile but to read what the frames drew.
    pub unsafe fn start_offscreen(mct: Mct, target: Target) -> Result<Self, Error> {
        if !mmu::is_enabled() {
            return Err(Error::MmuOff);
        }
        if !target.is_good() {
            return Err(Error::BadTarget);
        }
        let pool = memory::take()?;
        let mali = Mali::new();
        mali.power_on(mali::SAFE_HZ, mct).map_err(Error::Mali)?;
        mali.pmu_power_up(mali::PMU_ALL, mct).map_err(Error::Mali)?;
        let pool_start = pool as *const memory::Pool as usize;
        pool.tables.map(pool_start, size_of::<memory::Pool>()).map_err(|_| Error::PageTables)?;
        pool.tables.map(target.address, target.bytes()).map_err(|_| Error::PageTables)?;
        Self::bring_up(mali, mct, pool, None, None, target)
    }

    /// The MMUs on the tables `pool` holds, then the pool's fixed parts.
    fn bring_up(
        mali: Mali,
        mct: Mct,
        pool: &'static mut memory::Pool,
        fimd: Option<Fimd>,
        screen: Option<Framebuffer>,
        target: Target,
    ) -> Result<Self, Error> {
        let directory = pool.tables.directory();
        mali.reset_gp(mct, false).map_err(Error::Mali)?;
        mali.enable_mmu(None, directory, mct).map_err(Error::Mali)?;
        for index in 0..mali::PP_COUNT {
            mali.reset_pp(index, mct, false).map_err(Error::Mali)?;
            mali.enable_mmu(Some(index), directory, mct).map_err(Error::Mali)?;
        }

        let fixed = &mut pool.fixed;
        let plb = fixed.plb.as_ptr() as u32;
        for (tile, slot) in fixed.block_addresses.0.iter_mut().enumerate() {
            *slot = plb + (tile * mali::PLB_BLOCK) as u32;
        }
        mmu::clean_dcache_range(fixed as *const Fixed as usize, size_of::<Fixed>());
        let lasting = Arena::new(&mut pool.lasting.0);
        let frame = Arena::new(&mut pool.frame.0);
        Ok(Self { mali, mct, fimd, screen, target, fixed, lasting, frame, streams_for: None, vsync: true })
    }

    /// The screen, for the CPU to draw on beside the frames; uncached, so what is drawn shows.
    ///
    /// # Panics
    ///
    /// Without a screen: [`start_offscreen`](Self::start_offscreen).
    pub fn screen(&mut self) -> &mut Framebuffer {
        self.screen.as_mut().expect("Gpu::start_offscreen has no screen")
    }

    /// Where frames are drawn.
    pub fn target(&self) -> Target {
        self.target
    }

    /// Whether [`frame`](Self::frame) waits for vsync first: on from the start. Off, frames come
    /// as fast as the GPU draws them, and may tear.
    pub fn set_vsync(&mut self, on: bool) {
        self.vsync = on;
    }

    /// A texture of `width` x `height` texels, 0xAARRGGBB as the screen's, line after line from
    /// the top, sampled by `filter`. The texels are copied: `texels` is free after.
    pub fn texture(&mut self, width: usize, height: usize, texels: &[u32], filter: Filter) -> Result<Texture, Error> {
        if texels.len() != width * height {
            return Err(Error::TextureSize);
        }
        self.texture_with(width, height, filter, |x, y| texels[y * width + x])
    }

    /// A texture whose texel at column `x`, line `y` from the top is `texel(x, y)`, 0xAARRGGBB,
    /// written straight into the GPU's memory: no array of texels needed, on the stack or
    /// elsewhere. The stacks are small: CPU0's is 16 KiB, less than a 128 x 64 texture.
    pub fn texture_with(
        &mut self,
        width: usize,
        height: usize,
        filter: Filter,
        texel: impl Fn(usize, usize) -> u32,
    ) -> Result<Texture, Error> {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(Error::TextureSize);
        }
        let count = width * height;
        let address = self.lasting.alloc(count * 4, 64)?;
        // SAFETY: just allocated, `count` words.
        let memory = unsafe { memory::words(address, count) };
        for (index, slot) in memory.iter_mut().enumerate() {
            // The GPU's RGBA8888: red in the first byte.
            let [alpha, red, green, blue] = texel(index % width, index / width).to_be_bytes();
            *slot = u32::from_le_bytes([red, green, blue, alpha]);
        }
        let descriptor = self.lasting.alloc(mali::TEXTURE_DESCRIPTOR_BYTES, 64)?;
        let texture = Texture { descriptor, texels: address as u32, width: width as u32, height: height as u32 };
        self.set_filter(&texture, filter);
        mmu::clean_dcache_range(address, count * 4);
        Ok(texture)
    }

    /// Rewrites `texture`'s texels: the one at column `x`, line `y` from the top becomes
    /// `texel(x, y)`, 0xAARRGGBB. The next frame samples them; frames run to their end in
    /// [`Frame::show`], so none reads them meanwhile.
    pub fn write_texture(&mut self, texture: &Texture, texel: impl Fn(usize, usize) -> u32) {
        let (width, count) = (texture.width as usize, (texture.width * texture.height) as usize);
        let address = texture.texels as usize;
        // SAFETY: the texture's own texels, allocated by `texture_with`.
        let memory = unsafe { memory::words(address, count) };
        for (index, slot) in memory.iter_mut().enumerate() {
            let [alpha, red, green, blue] = texel(index % width, index / width).to_be_bytes();
            *slot = u32::from_le_bytes([red, green, blue, alpha]);
        }
        mmu::clean_dcache_range(address, count * 4);
    }

    /// Samples `texture` by `filter` from the next frame on.
    pub fn set_filter(&mut self, texture: &Texture, filter: Filter) {
        let descriptor = mali::texture_rgba8888(texture.texels, texture.width, texture.height, filter);
        // SAFETY: the descriptor's own words, allocated by `texture`.
        unsafe { memory::words(texture.descriptor, 16) }.copy_from_slice(&descriptor);
        mmu::clean_dcache_range(texture.descriptor, mali::TEXTURE_DESCRIPTOR_BYTES);
    }

    /// Starts a mesh of triangles for `program`: give it every attribute the vertex shader
    /// declares, by name, then [`build`](MeshBuilder::build) it.
    pub fn mesh(&mut self, program: &'static Program) -> MeshBuilder<'_> {
        MeshBuilder { gpu: self, program, attributes: [None; MAX_ATTRIBUTES], vertices: None, error: None }
    }

    /// Starts a frame on `area` of the screen, cleared to `clear`, 0xAARRGGBB; first waits for
    /// vsync, unless [`set_vsync`](Self::set_vsync) turned that off. Draw in it, then
    /// [`show`](Frame::show) it. What it holds lasts until the next frame starts.
    pub fn frame(&mut self, area: Area, clear: u32) -> Frame<'_> {
        let mut error = None;
        if !area.is_good(&self.target) {
            error = Some(Error::BadArea);
        } else if let Some(fimd) = self.fimd.as_ref().filter(|_| self.vsync) {
            if fimd.wait_vsync().is_err() {
                error = Some(Error::Vsync);
            }
        }
        self.frame.reset();
        let (across, down) = area.tiles();
        if error.is_none() && self.streams_for != Some((area.width, area.height)) {
            let plb = self.fixed.plb.as_ptr() as u32;
            for (pp, stream) in self.fixed.streams.iter_mut().enumerate() {
                mali::clear_stream(&mut stream.0, area.width, area.height, pp, mali::PP_COUNT, TileEnd::Blocks(plb));
            }
            mmu::clean_dcache_range(self.fixed.streams.as_ptr() as usize, size_of_val(&self.fixed.streams));
            self.streams_for = Some((area.width, area.height));
        }
        let mut plbu_words = 0;
        if error.is_none() {
            let mut plbu = Commands::new(&mut self.fixed.plbu_list.0);
            plbu.plbu_head(across as u32, down as u32, self.fixed.block_addresses.0.as_ptr() as u32);
            plbu_words = plbu.bytes() as usize / 4;
        }
        Frame { gpu: self, area, clear, vs_words: 0, plbu_words, draws: 0, error }
    }
}

/// The most attributes a vertex shader takes.
const MAX_ATTRIBUTES: usize = 16;

/// A mesh being made: see [`Gpu::mesh`]. A wrong name or count is kept, and
/// [`build`](Self::build) returns it.
pub struct MeshBuilder<'g> {
    gpu: &'g mut Gpu,
    program: &'static Program,
    /// The attribute info of each: the data's address and `stride << 11 | components - 1`.
    attributes: [Option<[u32; 2]>; MAX_ATTRIBUTES],
    vertices: Option<usize>,
    error: Option<Error>,
}

impl MeshBuilder<'_> {
    /// The attribute `name`'s values, a vertex each: `[f32; 3]` for a `vec3`, or for a `vec4`
    /// whose w is 1. Copied: `data` is free after.
    pub fn attribute<const N: usize>(mut self, name: &'static str, data: &[[f32; N]]) -> Self {
        if self.error.is_some() {
            return self;
        }
        let Some(index) = self.program.attribute(name) else {
            self.error = Some(Error::UnknownAttribute(name));
            return self;
        };
        if self.vertices.is_some_and(|count| count != data.len()) {
            self.error = Some(Error::VertexCounts);
            return self;
        }
        self.vertices = Some(data.len());
        match self.gpu.lasting.copy(data, 64) {
            Ok(address) => {
                // `stride << 11 | type << 2 | (components - 1)`, type 0 float: lima_draw.c:850-855.
                let info = (((N * 4) as u32) << 11) | (N as u32).saturating_sub(1);
                mmu::clean_dcache_range(address, size_of_val(data));
                self.attributes[index] = Some([address as u32, info]);
            }
            Err(error) => self.error = Some(error),
        }
        self
    }

    /// The mesh, if every attribute came, each of one count of vertices, triangles.
    pub fn build(self) -> Result<Mesh, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let program = self.program;
        let mut info = [0u32; MAX_ATTRIBUTES * 2];
        for (index, attribute) in program.attributes.iter().enumerate() {
            let words = self.attributes[index].ok_or(Error::MissingAttribute(attribute.name))?;
            info[index * 2..index * 2 + 2].copy_from_slice(&words);
        }
        let vertices = self.vertices.unwrap_or(0);
        if vertices == 0 || !vertices.is_multiple_of(3) {
            return Err(Error::NotTriangles);
        }
        let lasting = &mut self.gpu.lasting;
        let attribute_info = lasting.copy(&info[..program.attributes.len() * 2], 64)?;
        let vertex_shader = lasting.copy(program.vertex, 64)?;
        let fragment_shader = lasting.copy(program.fragment, 64)?;
        mmu::clean_dcache_range(attribute_info, program.attributes.len() * 8);
        mmu::clean_dcache_range(vertex_shader, size_of_val(program.vertex));
        mmu::clean_dcache_range(fragment_shader, size_of_val(program.fragment));
        Ok(Mesh { program, vertices: vertices as u32, vertex_shader, fragment_shader, attribute_info })
    }
}

/// A frame being drawn: see [`Gpu::frame`]. Mistakes in its draws are kept, and
/// [`show`](Self::show) returns the first.
pub struct Frame<'g> {
    gpu: &'g mut Gpu,
    area: Area,
    clear: u32,
    vs_words: usize,
    plbu_words: usize,
    draws: usize,
    error: Option<Error>,
}

impl<'g> Frame<'g> {
    /// Draws `mesh`: set its uniforms and textures on what this returns; it joins the frame at
    /// the end of the statement. Uniforms the program has must all be set.
    pub fn draw<'f>(&'f mut self, mesh: &Mesh) -> Draw<'f, 'g> {
        let place = self.place(mesh);
        if let Err(error) = place {
            self.error.get_or_insert(error);
        }
        Draw { frame: self, mesh: *mesh, place: place.ok(), set: 0, textures: 0, depth_test: false }
    }

    /// Room in the frame's memory for a draw of `mesh`, its uniforms filled but the program's.
    fn place(&mut self, mesh: &Mesh) -> Result<Place, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.draws == MAX_DRAWS {
            return Err(Error::TooManyDraws);
        }
        let program = mesh.program;
        let arena = &mut self.gpu.frame;
        let floats = (program.vertex_uniform_floats + 8 + program.vertex_constants.len()).next_multiple_of(4);
        let gp_uniforms = arena.alloc(floats * 4, 64)?;
        // SAFETY: just allocated, `floats` floats.
        let uniforms = unsafe { memory::floats(gp_uniforms, floats) };
        let (width, height) = (self.area.width as f32 / 2.0, self.area.height as f32 / 2.0);
        let viewport = program.vertex_uniform_floats;
        // y scaled by minus half: the scene's top on the frame's first line.
        uniforms[viewport..viewport + 8].copy_from_slice(&[width, -height, 0.5, 0.0, width, height, 0.5, 0.0]);
        uniforms[viewport + 8..viewport + 8 + program.vertex_constants.len()].copy_from_slice(program.vertex_constants);
        let (pp_uniforms, pp_array) = if program.fragment_uniform_halves > 0 {
            let halves = arena.alloc(program.fragment_uniform_halves * 2, 64)?;
            let array = arena.alloc(4, 64)?;
            // SAFETY: just allocated, a word.
            let word = unsafe { memory::words(array, 1) };
            word[0] = halves as u32;
            (halves, array)
        } else {
            (0, 0)
        };
        let vertices = mesh.vertices as usize;
        let positions = (vertices * 16).next_multiple_of(64);
        let varyings = (program.varying_stride as usize * vertices).next_multiple_of(64);
        let gp_output = arena.alloc(positions + varyings, 4096)?;
        let varying_info = arena.alloc((1 + program.varyings.len()) * 8, 64)?;
        let texture_list = arena.alloc(MAX_SAMPLERS * 4, 64)?;
        let render_state = arena.alloc(64, 64)?;
        Ok(Place { gp_uniforms, floats, pp_uniforms, pp_array, gp_output, positions, varying_info, texture_list, render_state })
    }

    /// Runs the frame: the GP over every draw, then the four PPs, which write the area of the
    /// screen. A frame of no draws shows nothing and leaves the area as it was.
    pub fn show(self) -> Result<Times, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.draws == 0 {
            return Ok(Times::default());
        }
        let gpu = self.gpu;
        let mct = gpu.mct;
        let fixed = &mut *gpu.fixed;
        let mut plbu = Commands::new(&mut fixed.plbu_list.0[self.plbu_words..]);
        plbu.end();
        let plbu_bytes = (self.plbu_words * 4) as u32 + plbu.bytes();
        let vs_start = fixed.vs_list.0.as_ptr() as u32;
        let plbu_start = fixed.plbu_list.0.as_ptr() as u32;
        let heap_start = fixed.heap.as_ptr() as u32;
        let frame = GpFrame {
            vs_start,
            vs_end: vs_start + (self.vs_words * 4) as u32,
            plbu_start,
            plbu_end: plbu_start + plbu_bytes,
            heap_start,
            heap_end: heap_start + size_of_val(&fixed.heap) as u32,
        };
        let (start, length) = gpu.frame.used();
        mmu::clean_dcache_range(start, length);
        mmu::clean_dcache_range(vs_start as usize, self.vs_words * 4);
        mmu::clean_dcache_range(plbu_start as usize, plbu_bytes as usize);

        let mali = gpu.mali;
        let begin = mct.count();
        // A soft reset of each before each frame, as Linux gives them after each job: what they
        // hold from the frame before goes.
        mali.reset_gp(mct, false).map_err(Error::Gp)?;
        for index in 0..mali::PP_COUNT {
            mali.reset_pp(index, mct, false).map_err(Error::Pp)?;
        }
        // The GPU's L2 may hold the last frame's uniforms.
        mali.clear_l2(mct).map_err(Error::Gp)?;
        mali.start_gp(&frame).map_err(Error::Gp)?;
        let (gp_us, _) = mali.wait_gp(&frame, mct, TIMEOUT_US).map_err(|error| {
            let _ = mali.hard_reset_gp(mct);
            Error::Gp(error)
        })?;

        let area = self.area;
        let target = gpu.target;
        let corner = target.address + area.y * target.stride + area.x * 4;
        let frame_state = fixed.frame_state.0.as_ptr() as usize;
        let pp_frame = PpFrame::clear(area.width, area.height, self.clear, corner, target.stride, frame_state);
        let pps = mct.count();
        for index in 0..mali::PP_COUNT {
            mali.start_pp(index, &pp_frame, fixed.streams[index].0.as_ptr() as u32).map_err(Error::Pp)?;
        }
        for index in 0..mali::PP_COUNT {
            mali.wait_pp(index, mct, TIMEOUT_US).map_err(|error| {
                let _ = mali.hard_reset_pp(index, mct);
                Error::Pp(error)
            })?;
        }
        let end = mct.count();
        Ok(Times { gp_us, pps_us: (end - pps) / COUNTS_PER_US, total_us: (end - begin) / COUNTS_PER_US })
    }
}

/// Where a draw's own things are in the frame's memory.
#[derive(Clone, Copy)]
struct Place {
    gp_uniforms: usize,
    floats: usize,
    pp_uniforms: usize,
    pp_array: usize,
    gp_output: usize,
    /// Bytes of positions in `gp_output`, before the varyings.
    positions: usize,
    varying_info: usize,
    texture_list: usize,
    render_state: usize,
}

/// One draw of a mesh in a frame: see [`Frame::draw`]. It joins the frame when dropped, at the
/// end of the statement that made it.
pub struct Draw<'f, 'g> {
    frame: &'f mut Frame<'g>,
    mesh: Mesh,
    /// None when the frame had no room, or had failed already.
    place: Option<Place>,
    /// Which of the program's uniforms are set, a bit each.
    set: u64,
    /// Which samplers have a texture, a bit each.
    textures: u32,
    depth_test: bool,
}

impl Draw<'_, '_> {
    fn fail(&mut self, error: Error) {
        self.frame.error.get_or_insert(error);
        self.place = None;
    }

    /// Sets the uniform `name`, in whichever shader has it, or both: an `f32`, `[f32; N]` for
    /// a vector, a [`Mat4`].
    pub fn uniform<T: Floats + ?Sized>(mut self, name: &'static str, value: &T) -> Self {
        let Some(place) = self.place else { return self };
        let values = value.floats();
        let mut found = false;
        for (index, uniform) in self.mesh.program.uniforms.iter().enumerate() {
            if uniform.name != name {
                continue;
            }
            found = true;
            if values.len() != uniform.floats {
                self.fail(Error::UniformSize { name, want: uniform.floats, got: values.len() });
                return self;
            }
            match uniform.stage {
                Stage::Vertex => {
                    // SAFETY: the draw's own uniforms, `place.floats` of them, which hold every
                    // vertex uniform.
                    let floats = unsafe { memory::floats(place.gp_uniforms, place.floats) };
                    floats[uniform.offset..uniform.offset + values.len()].copy_from_slice(values);
                }
                Stage::Fragment => {
                    let halves = self.mesh.program.fragment_uniform_halves;
                    // SAFETY: the draw's own half floats, `halves` of them.
                    let memory = unsafe { core::slice::from_raw_parts_mut(place.pp_uniforms as *mut u16, halves) };
                    for (slot, value) in memory[uniform.offset..].iter_mut().zip(values) {
                        *slot = half(*value);
                    }
                }
            }
            self.set |= 1 << index;
        }
        if !found {
            self.fail(Error::UnknownUniform(name));
        }
        self
    }

    /// Samples `texture` for the sampler `name`.
    pub fn texture(mut self, name: &'static str, texture: &Texture) -> Self {
        let Some(place) = self.place else { return self };
        let Some(index) = self.mesh.program.samplers.iter().position(|sampler| *sampler == name) else {
            self.fail(Error::UnknownSampler(name));
            return self;
        };
        // SAFETY: the draw's own texture list, a word a sampler.
        let list = unsafe { memory::words(place.texture_list, MAX_SAMPLERS) };
        list[index] = texture.descriptor as u32;
        self.textures |= 1 << index;
        self
    }

    /// Whether the PPs keep only what is nearer than what they drew there before in the frame:
    /// a depth test, `Less`. Off unless asked.
    pub fn depth_test(mut self, on: bool) -> Self {
        self.depth_test = on;
        self
    }

    /// The render state, the varyings and the commands: the draw into the frame.
    fn finish(&mut self) -> Result<(), Error> {
        let Some(place) = self.place else { return Ok(()) };
        let program = self.mesh.program;
        if let Some(missing) = program.uniforms.iter().enumerate().find(|(index, _)| self.set & (1 << index) == 0) {
            return Err(Error::MissingUniform(missing.1.name));
        }
        if let Some(index) = (0..program.samplers.len()).find(|index| self.textures & (1 << index) == 0) {
            return Err(Error::MissingTexture(program.samplers[index]));
        }
        debug_assert!(program.samplers.len() <= MAX_SAMPLERS);
        let varyings = place.gp_output + place.positions;

        // SAFETY: the draw's own render state, 16 words.
        let state = unsafe { memory::words(place.render_state, 16) };
        state.copy_from_slice(&program.render_state);
        if self.depth_test {
            state[3] = (state[3] & !DEPTH_FUNC_MASK) | DEPTH_FUNC_LESS | DEPTH_TEST;
        }
        state[9] |= self.mesh.fragment_shader as u32;
        if place.pp_array != 0 {
            state[11] |= place.pp_array as u32;
        }
        if !program.samplers.is_empty() {
            state[12] |= place.texture_list as u32;
        }
        if !program.varyings.is_empty() {
            state[15] |= varyings as u32;
        }

        // SAFETY: the draw's own varying info, two words an output.
        let info = unsafe { memory::words(place.varying_info, (1 + program.varyings.len()) * 2) };
        info[0] = place.gp_output as u32;
        info[1] = POSITION_VARYING;
        for (index, varying) in program.varyings.iter().enumerate() {
            info[2 + index * 2] = varyings as u32 + varying.offset;
            info[3 + index * 2] = (program.varying_stride << 11) | varying.format;
        }

        let frame = &mut *self.frame;
        let fixed = &mut *frame.gpu.fixed;
        let outputs = 1 + program.varyings.len() as u32;
        let attributes = program.attributes.len() as u32;
        // lima_draw.c:287-334.
        let mut vs = Commands::new(&mut fixed.vs_list.0[frame.vs_words..frame.vs_words + VS_DRAW_WORDS]);
        vs.vs_arrays_semaphore_begin();
        vs.vs_uniforms(place.gp_uniforms as u32, (place.floats * 4) as u32);
        vs.vs_shader(self.mesh.vertex_shader as u32, size_of_val(program.vertex) as u32, program.vertex_prefetch);
        vs.vs_counts(outputs, attributes);
        vs.vs_attributes(self.mesh.attribute_info as u32, attributes);
        vs.vs_varyings(place.varying_info as u32, outputs);
        vs.vs_draw(self.mesh.vertices);
        frame.vs_words += vs.bytes() as usize / 4;

        // lima_draw.c:336-439.
        let (width, height) = (frame.area.width, frame.area.height);
        let mut plbu = Commands::new(&mut fixed.plbu_list.0[frame.plbu_words..frame.plbu_words + PLBU_DRAW_WORDS]);
        plbu.plbu_viewport(0.0, width as f32, 0.0, height as f32);
        plbu.plbu_arrays_semaphore_begin();
        plbu.plbu_primitive_setup(0);
        plbu.plbu_render_state(place.render_state as u32, place.gp_output as u32);
        plbu.plbu_scissor(0, width as u32, 0, height as u32);
        plbu.plbu_depth_range(0.0, 1.0);
        plbu.plbu_draw_arrays(mali::MODE_TRIANGLES, 0, self.mesh.vertices);
        frame.plbu_words += plbu.bytes() as usize / 4;
        frame.draws += 1;
        Ok(())
    }
}

impl Drop for Draw<'_, '_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            self.frame.error.get_or_insert(error);
        }
    }
}

/// `value` as a half float, rounded to nearest; beyond its range, infinity.
fn half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xFF) as i32;
    let mantissa = bits & 0x7F_FFFF;
    if exponent == 0xFF {
        return sign | 0x7C00 | if mantissa != 0 { 0x200 } else { 0 };
    }
    let exponent = exponent - 127 + 15;
    if exponent >= 0x1F {
        return sign | 0x7C00;
    }
    if exponent <= 0 {
        if exponent < -10 {
            return sign;
        }
        let mantissa = mantissa | 0x80_0000;
        let shift = (14 - exponent) as u32;
        let rounded = (mantissa + (1 << (shift - 1))) >> shift;
        return sign | rounded as u16;
    }
    let rounded = ((exponent as u32) << 10 | (mantissa >> 13)) + ((mantissa >> 12) & 1);
    sign | rounded as u16
}
