//! The real per-cell terminal renderer.
//!
//! Replaces the flat-string glyphon hack with a proper fixed-grid GPU renderer:
//!   - a glyph atlas (R8 coverage) filled on demand via cosmic-text + swash;
//!   - one instanced quad pipeline for cell backgrounds (color/reverse/cursor);
//!   - one instanced quad pipeline for foreground glyphs (atlas-sampled, per-cell color).
//!
//! Everything is in physical pixels. Cell metrics are MEASURED from the monospace font
//! (retires the old hardcoded CELL_W). Bold is rendered with a real bold face (and the 8
//! ANSI colors brighten under bold, per xterm tradition). Not yet handled, by design:
//! italic, color emoji (Mask glyphs only), ligatures (correct for a grid), and damage
//! tracking (we rebuild instances each frame — fine at these sizes, the next perf step).

use std::collections::HashMap;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight};
use wgpu::util::DeviceExt;

const ATLAS: u32 = 1024;

/// Selection highlight background (could become a palette entry later).
const SELECTION_BG: Rgb = Rgb { r: 0x33, g: 0x4a, b: 0x6b };

/// Measured monospace cell box, in physical pixels.
#[derive(Clone, Copy)]
pub struct CellMetrics {
    pub w: f32,
    pub h: f32,
    pub ascent: f32,
}

/// The 16 base ANSI colors (xterm defaults), used as the palette baseline.
pub const BASE16: [(u8, u8, u8); 16] = [
    (0, 0, 0), (205, 0, 0), (0, 205, 0), (205, 205, 0),
    (0, 0, 238), (205, 0, 205), (0, 205, 205), (229, 229, 229),
    (127, 127, 127), (255, 0, 0), (0, 255, 0), (255, 255, 0),
    (92, 92, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
];

pub fn default_ansi() -> [Rgb; 16] {
    BASE16.map(|(r, g, b)| Rgb { r, g, b })
}

/// Resolved color scheme the renderer draws with (settable from config).
#[derive(Clone, Copy)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            fg: Rgb { r: 0xcc, g: 0xcc, b: 0xcc },
            bg: Rgb { r: 0x0d, g: 0x0d, b: 0x10 },
            cursor: Rgb { r: 0xcc, g: 0xcc, b: 0xcc },
            ansi: default_ansi(),
        }
    }
}

#[derive(Clone, Copy)]
struct Glyph {
    uv0: [f32; 2],
    uv1: [f32; 2],
    size: [f32; 2],
    offset: [f32; 2], // placement left/top from rasterizer
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BgInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FgInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv0: [f32; 2],
    uv1: [f32; 2],
    color: [f32; 4],
}

/// Simple shelf packer over one atlas texture.
struct Atlas {
    texture: wgpu::Texture,
    x: u32,
    y: u32,
    row_h: u32,
}

impl Atlas {
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > ATLAS || h > ATLAS {
            return None;
        }
        if self.x + w > ATLAS {
            self.x = 0;
            self.y += self.row_h;
            self.row_h = 0;
        }
        if self.y + h > ATLAS {
            return None; // full — acceptable for a spike; a real build would grow/evict
        }
        let pos = (self.x, self.y);
        self.x += w + 1;
        self.row_h = self.row_h.max(h + 1);
        Some(pos)
    }
}

/// Per-pane instance data + GPU buffers. Built by `prepare` and kept between frames so a pane
/// that didn't change is rendered straight from its cache (no rebuild) — the heart of damage
/// tracking. `bg`/`fg` are retained to reuse their allocation across rebuilds.
struct PaneBuffers {
    bg: Vec<BgInstance>,
    fg: Vec<FgInstance>,
    bg_buf: wgpu::Buffer,
    fg_buf: wgpu::Buffer,
    bg_cap: usize,
    fg_cap: usize,
}

impl PaneBuffers {
    fn new(device: &wgpu::Device) -> Self {
        let cap = 1024;
        Self {
            bg: Vec::new(),
            fg: Vec::new(),
            bg_buf: inst_buf(device, "bg-inst", cap * std::mem::size_of::<BgInstance>()),
            fg_buf: inst_buf(device, "fg-inst", cap * std::mem::size_of::<FgInstance>()),
            bg_cap: cap,
            fg_cap: cap,
        }
    }
}

pub struct GridRenderer {
    font_system: FontSystem,
    swash: SwashCache,
    font_px: f32,
    metrics: CellMetrics,
    family: Option<String>,
    palette: Palette,
    families: Vec<String>,

    atlas: Atlas,
    glyphs: HashMap<(char, bool), Option<Glyph>>,

    screen_buf: wgpu::Buffer,
    common_bg: wgpu::BindGroup,
    atlas_bg: wgpu::BindGroup,
    bg_pipeline: wgpu::RenderPipeline,
    fg_pipeline: wgpu::RenderPipeline,
    quad: wgpu::Buffer,

    /// Cached instance buffers, one set per pane (keyed by PaneId as a u64).
    panes: HashMap<u64, PaneBuffers>,
}

/// sRGB u8 → linear f32 (surface is *_Srgb, so it re-encodes linear → sRGB on write).
fn lin(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn rgba(c: Rgb, a: f32) -> [f32; 4] {
    [lin(c.r), lin(c.g), lin(c.b), a]
}

impl GridRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, font_px: f32, line_px: f32) -> Self {
        let mut font_system = FontSystem::new();
        let swash = SwashCache::new();
        let metrics = measure(&mut font_system, &None, font_px, line_px);

        // Sorted, de-duplicated list of monospace font families for the visual picker.
        let families = {
            let mut set = std::collections::BTreeSet::new();
            for f in font_system.db_mut().faces() {
                if f.monospaced {
                    if let Some((name, _)) = f.families.first() {
                        set.insert(name.clone());
                    }
                }
            }
            set.into_iter().collect::<Vec<_>>()
        };

        // Atlas texture (single-channel coverage).
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph-atlas"),
            size: wgpu::Extent3d { width: ATLAS, height: ATLAS, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let screen_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("screen"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let common_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("common"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let common_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("common"),
            layout: &common_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: screen_buf.as_entire_binding() }],
        });
        let atlas_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let blend = Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        });

        let quad_attrs = wgpu::vertex_attr_array![0 => Float32x2];
        let bg_attrs = wgpu::vertex_attr_array![1 => Float32x2, 2 => Float32x2, 3 => Float32x4];
        let fg_attrs = wgpu::vertex_attr_array![1 => Float32x2, 2 => Float32x2, 3 => Float32x2, 4 => Float32x2, 5 => Float32x4];
        let quad_vb = wgpu::VertexBufferLayout {
            array_stride: 8,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &quad_attrs,
        };

        let bg_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg"),
            source: wgpu::ShaderSource::Wgsl(BG_WGSL.into()),
        });
        let fg_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fg"),
            source: wgpu::ShaderSource::Wgsl(FG_WGSL.into()),
        });

        let bg_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bg"),
            bind_group_layouts: &[Some(&common_layout)],
            immediate_size: 0,
        });
        let fg_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fg"),
            bind_group_layouts: &[Some(&common_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let target = wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL };

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg"),
            layout: Some(&bg_pl),
            vertex: wgpu::VertexState {
                module: &bg_shader,
                entry_point: Some("vs"),
                buffers: &[quad_vb.clone(), wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<BgInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &bg_attrs,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &bg_shader,
                entry_point: Some("fs"),
                targets: &[Some(target.clone())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let fg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fg"),
            layout: Some(&fg_pl),
            vertex: wgpu::VertexState {
                module: &fg_shader,
                entry_point: Some("vs"),
                buffers: &[quad_vb, wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<FgInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &fg_attrs,
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &fg_shader,
                entry_point: Some("fs"),
                targets: &[Some(target)],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Unit quad (two triangles), top-left origin in [0,1].
        let verts: [[f32; 2]; 6] = [[0., 0.], [1., 0.], [0., 1.], [0., 1.], [1., 0.], [1., 1.]];
        let quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        let _ = queue; // atlas is written lazily as glyphs are rasterized

        Self {
            font_system,
            swash,
            font_px,
            metrics,
            family: None,
            palette: Palette::default(),
            families,
            atlas: Atlas { texture, x: 0, y: 0, row_h: 0 },
            glyphs: HashMap::new(),
            screen_buf,
            common_bg,
            atlas_bg,
            bg_pipeline,
            fg_pipeline,
            quad,
            panes: HashMap::new(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Available monospace family names (for the visual picker).
    pub fn families(&self) -> &[String] {
        &self.families
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Switch font (family = None → generic monospace) and/or size. Re-measures the cell,
    /// drops the glyph cache, and rewinds the atlas so glyphs re-rasterize at the new face.
    pub fn set_font(&mut self, family: Option<String>, font_px: f32, line_px: f32) {
        self.family = family;
        self.font_px = font_px;
        self.metrics = measure(&mut self.font_system, &self.family, font_px, line_px);
        self.glyphs.clear();
        self.atlas.x = 0;
        self.atlas.y = 0;
        self.atlas.row_h = 0;
    }

    fn glyph(&mut self, queue: &wgpu::Queue, c: char, bold: bool) -> Option<Glyph> {
        if let Some(g) = self.glyphs.get(&(c, bold)) {
            return *g;
        }
        let g = self.rasterize(queue, c, bold);
        self.glyphs.insert((c, bold), g);
        g
    }

    fn rasterize(&mut self, queue: &wgpu::Queue, c: char, bold: bool) -> Option<Glyph> {
        let mut buf = Buffer::new(&mut self.font_system, Metrics::new(self.font_px, self.metrics.h));
        let weight = if bold { Weight::BOLD } else { Weight::NORMAL };
        buf.set_text(
            &mut self.font_system,
            &c.to_string(),
            &mono_attrs(&self.family, weight),
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut self.font_system, false);
        let run = buf.layout_runs().next()?;
        let lg = run.glyphs.first()?;
        let key = lg.physical((0.0, 0.0), 1.0).cache_key;

        let (placement, data, mask) = match self.swash.get_image(&mut self.font_system, key) {
            Some(img) => (img.placement, img.data.clone(), matches!(img.content, SwashContent::Mask)),
            None => return None,
        };
        if !mask || placement.width == 0 || placement.height == 0 {
            return None;
        }
        let (ax, ay) = self.atlas.alloc(placement.width, placement.height)?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.atlas.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: ax, y: ay, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(placement.width),
                rows_per_image: Some(placement.height),
            },
            wgpu::Extent3d { width: placement.width, height: placement.height, depth_or_array_layers: 1 },
        );
        let f = ATLAS as f32;
        Some(Glyph {
            uv0: [ax as f32 / f, ay as f32 / f],
            uv1: [(ax + placement.width) as f32 / f, (ay + placement.height) as f32 / f],
            size: [placement.width as f32, placement.height as f32],
            offset: [placement.left as f32, placement.top as f32],
        })
    }

    /// Rebuild a pane's instance data, positioned within the pane at `origin` (px). Only called
    /// for panes flagged dirty; clean panes keep their cached buffers from a previous call.
    pub fn prepare<L: EventListener>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pane: u64,
        term: &Term<L>,
        origin: (f32, f32),
        screen: (f32, f32),
    ) {
        queue.write_buffer(&self.screen_buf, 0, bytemuck::cast_slice(&[screen.0, screen.1, 0.0, 0.0]));

        // Detach this pane's vectors (reusing their capacity) so the loop can also borrow
        // `self` mutably for glyph rasterization.
        let (mut bg, mut fg) = match self.panes.get_mut(&pane) {
            Some(pb) => (std::mem::take(&mut pb.bg), std::mem::take(&mut pb.fg)),
            None => (Vec::new(), Vec::new()),
        };
        bg.clear();
        fg.clear();

        let palette = self.palette; // Copy — frees `self` for self.glyph() below
        let content = term.renderable_content();
        let colors = content.colors;
        let cursor_point = content.cursor.point;
        let cursor_on = content.mode.contains(TermMode::SHOW_CURSOR)
            && content.cursor.shape != CursorShape::Hidden;
        let selection = content.selection;
        // When scrolled into history, display_iter yields negative line numbers; shift them
        // back into the 0..screen_lines viewport so scrollback renders in place.
        let off = content.display_offset as i32;

        let (cw, ch, asc) = (self.metrics.w, self.metrics.h, self.metrics.ascent);

        for cell in content.display_iter {
            let point = cell.point;
            let col = point.column.0 as f32;
            let row = (point.line.0 + off) as f32;
            let x = origin.0 + col * cw;
            let y = origin.1 + row * ch;

            let flags = cell.flags;
            let bold = flags.contains(alacritty_terminal::term::cell::Flags::BOLD);
            let mut fg_col = resolve(cell.fg, colors, &palette, bold);
            let mut bg_col = resolve(cell.bg, colors, &palette, false);
            if flags.contains(alacritty_terminal::term::cell::Flags::INVERSE) {
                std::mem::swap(&mut fg_col, &mut bg_col);
            }
            let mut draw_bg = bg_col != palette.bg;
            if selection.as_ref().is_some_and(|r| r.contains(point)) {
                bg_col = SELECTION_BG;
                draw_bg = true;
            }
            if cursor_on && point == cursor_point {
                // Block cursor: fill with the cursor color, draw the glyph in the bg color.
                bg_col = palette.cursor;
                fg_col = palette.bg;
                draw_bg = true;
            }

            if draw_bg {
                bg.push(BgInstance { pos: [x, y], size: [cw, ch], color: rgba(bg_col, 1.0) });
            }

            let c = cell.c;
            if c != ' ' && c != '\0' && !flags.contains(alacritty_terminal::term::cell::Flags::HIDDEN) {
                if let Some(g) = self.glyph(queue, c, bold) {
                    fg.push(FgInstance {
                        pos: [x + g.offset[0], y + asc - g.offset[1]],
                        size: g.size,
                        uv0: g.uv0,
                        uv1: g.uv1,
                        color: rgba(fg_col, 1.0),
                    });
                }
            }
        }

        // Store the rebuilt vectors back and upload them (growing the GPU buffers if needed).
        let pb = self.panes.entry(pane).or_insert_with(|| PaneBuffers::new(device));
        upload(device, queue, &mut pb.bg_buf, &mut pb.bg_cap, &bg, "bg-inst");
        upload(device, queue, &mut pb.fg_buf, &mut pb.fg_cap, &fg, "fg-inst");
        pb.bg = bg;
        pb.fg = fg;
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>, pane: u64) {
        let Some(pb) = self.panes.get(&pane) else { return };
        if !pb.bg.is_empty() {
            pass.set_pipeline(&self.bg_pipeline);
            pass.set_bind_group(0, &self.common_bg, &[]);
            pass.set_vertex_buffer(0, self.quad.slice(..));
            pass.set_vertex_buffer(1, pb.bg_buf.slice(..));
            pass.draw(0..6, 0..pb.bg.len() as u32);
        }
        if !pb.fg.is_empty() {
            pass.set_pipeline(&self.fg_pipeline);
            pass.set_bind_group(0, &self.common_bg, &[]);
            pass.set_bind_group(1, &self.atlas_bg, &[]);
            pass.set_vertex_buffer(0, self.quad.slice(..));
            pass.set_vertex_buffer(1, pb.fg_buf.slice(..));
            pass.draw(0..6, 0..pb.fg.len() as u32);
        }
    }

    /// Drop a closed pane's cached buffers.
    pub fn forget_pane(&mut self, pane: u64) {
        self.panes.remove(&pane);
    }
}

/// Promote one of the 8 standard ANSI colors to its bright variant (for bold text).
fn brighten(i: u8, bright: bool) -> u8 {
    if bright && i < 8 {
        i + 8
    } else {
        i
    }
}

/// Resolve an ANSI cell color against the configured palette. App-set OSC overrides in
/// `colors` win when present; otherwise the config palette (0–15) / computed cube (16–255)
/// applies. `bright` (bold text) promotes the 8 base ANSI colors to their bright variants.
fn resolve(c: AnsiColor, colors: &alacritty_terminal::term::color::Colors, palette: &Palette, bright: bool) -> Rgb {
    let base = |i: u8| if i < 16 { palette.ansi[i as usize] } else { ansi256(i) };
    match c {
        AnsiColor::Spec(rgb) => rgb,
        AnsiColor::Indexed(i) => {
            let i = brighten(i, bright);
            colors[i as usize].unwrap_or_else(|| base(i))
        }
        AnsiColor::Named(n) => {
            let idx = n as usize;
            if idx < 16 {
                let i = brighten(idx as u8, bright);
                colors[i as usize].unwrap_or_else(|| base(i))
            } else if n == NamedColor::Background {
                colors[n].unwrap_or(palette.bg)
            } else {
                colors[n].unwrap_or(palette.fg)
            }
        }
    }
}

/// Build cosmic-text attrs for the active family + weight (None → generic monospace).
fn mono_attrs(family: &Option<String>, weight: Weight) -> Attrs<'_> {
    let fam = match family {
        Some(name) => Family::Name(name.as_str()),
        None => Family::Monospace,
    };
    Attrs::new().family(fam).weight(weight)
}

/// Standard xterm 256-color palette (used for indices 16–255; 0–15 come from the palette).
fn ansi256(i: u8) -> Rgb {
    if i < 16 {
        let (r, g, b) = BASE16[i as usize];
        Rgb { r, g, b }
    } else if i < 232 {
        let i = i - 16;
        let step = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
        Rgb { r: step(i / 36), g: step((i / 6) % 6), b: step(i % 6) }
    } else {
        let v = (i - 232) * 10 + 8;
        Rgb { r: v, g: v, b: v }
    }
}

/// Measure the monospace cell box from the active font itself.
fn measure(fs: &mut FontSystem, family: &Option<String>, font_px: f32, line_px: f32) -> CellMetrics {
    let mut buf = Buffer::new(fs, Metrics::new(font_px, line_px));
    buf.set_size(fs, Some(1000.0), Some(line_px * 2.0));
    buf.set_text(fs, "M", &mono_attrs(family, Weight::NORMAL), Shaping::Advanced, None);
    buf.shape_until_scroll(fs, false);
    let (mut w, mut ascent) = (font_px * 0.6, line_px * 0.8);
    if let Some(run) = buf.layout_runs().next() {
        if let Some(g) = run.glyphs.first() {
            w = g.w;
        }
        ascent = run.line_y;
    }
    CellMetrics { w, h: line_px, ascent }
}

fn inst_buf(device: &wgpu::Device, label: &str, bytes: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.max(64) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn upload<T: bytemuck::Pod>(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    buf: &mut wgpu::Buffer,
    cap: &mut usize,
    data: &[T],
    label: &str,
) {
    if data.is_empty() {
        return;
    }
    if data.len() > *cap {
        *cap = data.len().next_power_of_two();
        *buf = inst_buf(device, label, *cap * std::mem::size_of::<T>());
    }
    queue.write_buffer(buf, 0, bytemuck::cast_slice(data));
}

const BG_WGSL: &str = r#"
struct Screen { size: vec2<f32> };
@group(0) @binding(0) var<uniform> screen: Screen;
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs(
  @location(0) corner: vec2<f32>,
  @location(1) ipos: vec2<f32>,
  @location(2) isize: vec2<f32>,
  @location(3) icolor: vec4<f32>,
) -> VsOut {
  let p = ipos + corner * isize;
  let ndc = vec2<f32>(p.x / screen.size.x * 2.0 - 1.0, 1.0 - p.y / screen.size.y * 2.0);
  var o: VsOut;
  o.pos = vec4<f32>(ndc, 0.0, 1.0);
  o.color = icolor;
  return o;
}
@fragment fn fs(in: VsOut) -> @location(0) vec4<f32> { return in.color; }
"#;

const FG_WGSL: &str = r#"
struct Screen { size: vec2<f32> };
@group(0) @binding(0) var<uniform> screen: Screen;
@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_smp: sampler;
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec4<f32> };
@vertex fn vs(
  @location(0) corner: vec2<f32>,
  @location(1) ipos: vec2<f32>,
  @location(2) isize: vec2<f32>,
  @location(3) iuv0: vec2<f32>,
  @location(4) iuv1: vec2<f32>,
  @location(5) icolor: vec4<f32>,
) -> VsOut {
  let p = ipos + corner * isize;
  let ndc = vec2<f32>(p.x / screen.size.x * 2.0 - 1.0, 1.0 - p.y / screen.size.y * 2.0);
  var o: VsOut;
  o.pos = vec4<f32>(ndc, 0.0, 1.0);
  o.uv = mix(iuv0, iuv1, corner);
  o.color = icolor;
  return o;
}
@fragment fn fs(in: VsOut) -> @location(0) vec4<f32> {
  let a = textureSample(atlas_tex, atlas_smp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * a);
}
"#;
