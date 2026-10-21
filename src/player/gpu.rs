//! The 360 view as an iced shader primitive: one pass of `shader.wgsl` over
//! the widget's rectangle, sharing iced's wgpu device. `Pipeline` doesn't
//! need iced: `render.rs` drives it on a device of its own for export.
//!
//! Through a crossfade between cuts the pipeline draws a second layer, the
//! incoming cut, over the first with a blend constant: each layer keeps its
//! own uniforms and pictures, so neither re-uploads while both play.
//!
//! The overlays then go on top, each a turned quad of its picture
//! (`overlay.wgsl`), blended premultiplied.

use super::source::{Frame, Layout};
use crate::hdr::Hdr;
use crate::align::{Aligner, CELLS_ALPHA, CELLS_PHI, Field, Strips};
use crate::exposure::{self, BINS, FADE_FROM, FADE_TO, Gains, Seam};
use crate::grade::Grade;
use crate::overlay::{Picture, Place, Quad};
use crate::stitch::{Lens, M3};
use crate::view::Camera;
use bytemuck::{Pod, Zeroable};
use iced::wgpu;
use iced::widget::shader::{self, Viewport};
use iced::Rectangle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const RS_STEPS_MAX: usize = 33;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LensU {
    r: [[f32; 4]; 3],
    k: [f32; 4],
    k5p: [f32; 4],
    fc: [f32; 4],
    a0: [f32; 4],
    a1: [f32; 4],
    lim: [f32; 4],
    calib: [f32; 4],
}

/// `overlay.wgsl`'s uniforms.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct OverlayU {
    frame: [f32; 4],
    target: [f32; 4],
    quad: [f32; 4],
    turn: [f32; 4],
    grade: [[f32; 4]; 3],
}

/// An overlay picture to draw over the view: where, graded how, and how far
/// faded to black with the edit.
#[derive(Clone)]
pub struct OverlayDraw {
    /// Tells pictures apart, so each is uploaded once.
    pub id: u64,
    pub picture: Arc<Picture>,
    pub place: Place,
    pub grade: Grade,
    pub black: f64,
}

/// One overlay drawn: its uniforms and picture.
struct OverlaySlot {
    uniforms: wgpu::Buffer,
    texture: Plane,
    bind_group: wgpu::BindGroup,
    picture: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    user: [[f32; 4]; 3],
    screen: [f32; 4],
    misc: [f32; 4],
    lenses: [LensU; 2],
    views: [[f32; 4]; RS_STEPS_MAX * 3],
    /// Exposure seam basis rows; .w of the first two are the fade angles.
    seam: [[f32; 4]; 3],
    gains: [[f32; 4]; BINS * 2],
    /// Seam alignment shifts, four cells per vector.
    field: [[f32; 4]; CELLS_PHI * CELLS_ALPHA / 4],
    /// Disagreement cut strength per azimuth bin, four per vector.
    cut: [[f32; 4]; CELLS_PHI / 4],
    /// Colour effects (`Grade::uniforms`).
    grade: [[f32; 4]; 3],
    /// An ordinary video: 1 if it is one, its picture's width over height, 1 if
    /// it came as 10-bit, its HDR (`Hdr::code`).
    flat: [f32; 4],
    /// Its crop (`view::Crop`): centre x, y, half height, roll.
    crop: [f32; 4],
    /// 1 to dither the picture's last rounding to 8 bits, then the frame's
    /// number for the noise (`Pipeline::dither`).
    dither: [f32; 4],
    /// The LUT before the effects (`lut.rs`): (1 if on, strength, size, 0),
    /// its domain's low and high ends.
    lut: [[f32; 4]; 3],
}

/// Numbers rigs and frames so each is uploaded once however often it is drawn.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A clip's lens geometry; the pipeline rebuilds its masks only when another
/// clip is opened.
pub struct Rig {
    id: u64,
    pub lenses: Vec<Lens>,
    pub tracks: Vec<u32>,
    pub seam: Option<Seam>,
    pub aligner: Option<Aligner>,
    /// Places the seam for the shader's blend whatever corrections are on.
    ring: Option<crate::ring::Ring>,
    /// An ordinary video: no lenses, its one picture cropped as the camera
    /// says.
    pub flat: bool,
    /// An ordinary video in HDR, brought down to SDR in the shader.
    pub hdr: Hdr,
}

impl Rig {
    /// `exposure`: match the lenses' exposure at the seam. Off by default
    /// (2026-10-12): where the sun veils one lens it brightened the clean one
    /// to meet it, dulling its side, and without it both test clips looked
    /// better and close to DJI Studio's.
    pub fn new(lenses: Vec<Lens>, tracks: Vec<u32>, exposure: bool) -> Self {
        let seam = if exposure { Seam::new(&lenses) } else { None };
        let (aligner, ring) = (Aligner::new(&lenses), crate::ring::Ring::new(&lenses));
        Rig { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), lenses, tracks, seam, aligner, ring, flat: false, hdr: Hdr::None }
    }

    /// An ordinary video's: its picture track and nothing to stitch.
    pub fn flat(track: u32, hdr: Hdr) -> Self {
        Rig { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), lenses: Vec::new(), tracks: vec![track], seam: None, aligner: None, ring: None, flat: true, hdr }
    }
}

/// Per-frame seam corrections: exposure matching and close-subject alignment.
#[derive(Clone, Default)]
pub struct SeamFix {
    pub gains: Gains,
    pub field: Field,
}

/// What `SeamFix::follow` reads off a frame before matching it against the
/// last: split out so export can sample one frame while matching the one
/// before (14 and 17 ms a frame at 3840²).
pub struct SeamSample {
    gains: Option<Gains>,
    strips: Option<Strips>,
}

impl SeamSample {
    pub fn new(rig: &Rig, frame: &Frame) -> Self {
        let rgb = |i: usize, u: f64, v: f64| frame.rgb_at(&rig.lenses[i], rig.tracks[rig.lenses[i].input], u, v);
        SeamSample {
            gains: rig.seam.as_ref().map(|seam| seam.measure(rgb)),
            strips: rig.aligner.as_ref().map(|aligner| aligner.strips(|i, u, v| rgb(i, u, v).map(|c| c.iter().sum::<f64>() / 3.0))),
        }
    }
}

impl SeamFix {
    /// Measures `frame`'s seam. When it follows on from the last frame
    /// (`continuous`) the corrections ease towards it, else they jump.
    pub fn follow(&mut self, rig: &Rig, frame: &Frame, continuous: bool) {
        self.apply(rig, SeamSample::new(rig, frame), continuous);
    }

    pub fn apply(&mut self, rig: &Rig, sample: SeamSample, continuous: bool) {
        if let Some(now) = sample.gains {
            if continuous {
                self.gains.ease(&now, exposure::EASE);
            } else {
                self.gains = now;
            }
        }
        if let (Some(aligner), Some(strips)) = (&rig.aligner, sample.strips) {
            let now = aligner.measure(&strips, continuous.then_some(&self.field));
            if continuous {
                self.field.ease(&now, crate::align::EASE);
            } else {
                self.field = now;
            }
        }
    }
}

/// A decoded frame as handed to the view, with its seam corrections.
pub struct Shown {
    id: u64,
    frame: Frame,
    fix: SeamFix,
}

impl Shown {
    pub fn new(frame: Frame, fix: SeamFix) -> Self {
        Shown { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), frame, fix }
    }

    pub fn frame(&self) -> &Frame {
        &self.frame
    }
}

/// Everything one draw needs; rebuilt by the view on every redraw.
pub struct Primitive {
    pub rig: Arc<Rig>,
    pub shown: Arc<Shown>,
    /// The user's free look.
    pub camera: Camera,
    /// rig_from_view at instants spread over the readout.
    pub rig_from_view: Vec<M3>,
    /// Through a crossfade: the next cut, drawn over this one.
    pub incoming: Option<Incoming>,
    /// The overlays showing, lowest first.
    pub overlays: Vec<OverlayDraw>,
}

/// The cut crossfading in: its own picture and framing, and how far in it
/// is (0..1).
pub struct Incoming {
    pub rig: Arc<Rig>,
    pub shown: Arc<Shown>,
    pub camera: Camera,
    pub rig_from_view: Vec<M3>,
    pub mix: f32,
}

impl std::fmt::Debug for Primitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Primitive").field("rig", &self.rig.id).finish_non_exhaustive()
    }
}

struct Plane {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct Pipeline {
    pipeline: wgpu::RenderPipeline,
    /// The same, blending over what's drawn by the pass's blend constant.
    blend: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb: bool,
    /// The cut shown, and the one crossfading in over it.
    layers: [Layer; 2],
    /// How far the second layer is in, if it's drawn.
    mix: Option<f32>,
    overlay: wgpu::RenderPipeline,
    overlay_layout: wgpu::BindGroupLayout,
    /// Kept between frames; the first `overlays` are drawn.
    overlay_slots: Vec<OverlaySlot>,
    overlays: usize,
    /// Dither the picture into its 8-bit target, with noise for this frame
    /// number: an export's. A smooth sky came out in steps of one level
    /// once rounded to 8 bits, however finely it was decoded and graded.
    /// The player's target is the screen's, sRGB-encoded by the GPU after
    /// the shader, so it isn't dithered there.
    pub dither: Option<u32>,
}

/// One picture's resources: uniforms, textures and what they hold.
struct Layer {
    uniforms: wgpu::Buffer,
    /// Per texture slot: Y plane, UV plane.
    slots: [(Plane, Plane); 2],
    /// The same for a 10-bit (P010) picture: 16-bit integer planes, read by
    /// hand in the shader (`deep_y`), as the filterable 16-bit formats need a
    /// device feature iced doesn't ask for.
    deep: [(Plane, Plane); 2],
    masks: [Plane; 2],
    /// The LUT as a 3D texture, and which loaded one (`lut::shared`) it is.
    lut: Plane,
    lut_id: Option<usize>,
    bind_group: wgpu::BindGroup,
    lens_u: [LensU; 2],
    rig_id: u64,
    shown_id: u64,
}

impl shader::Pipeline for Pipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        Pipeline::create(device, format)
    }
}

impl Pipeline {
    pub fn create(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("stitch"),
            source: wgpu::ShaderSource::Wgsl(concat!(include_str!("shader.wgsl"), include_str!("grade.wgsl")).into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: binding < 6 },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let deep_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Uint, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                tex_entry(2),
                tex_entry(3),
                tex_entry(4),
                tex_entry(5),
                tex_entry(6),
                tex_entry(7),
                deep_entry(8),
                deep_entry(9),
                deep_entry(10),
                deep_entry(11),
                wgpu::BindGroupLayoutEntry {
                    binding: 12,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D3, multisampled: false },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let make = |blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("stitch"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview: None,
                cache: None,
            })
        };
        let pipeline = make(None);
        // new × k + old × (1 − k), k the blend constant.
        let over = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Constant, dst_factor: wgpu::BlendFactor::OneMinusConstant, operation: wgpu::BlendOperation::Add };
        let blend = make(Some(wgpu::BlendState { color: over, alpha: over }));
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let layers = [0, 1].map(|_| {
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let slots = [0, 1].map(|_| (plane(device, 2, 2, wgpu::TextureFormat::R8Unorm), plane(device, 1, 1, wgpu::TextureFormat::Rg8Unorm)));
            let deep = [0, 1].map(|_| deep_planes(device, 2, 2));
            let masks = [0, 1].map(|_| plane(device, 1, 1, wgpu::TextureFormat::R8Unorm));
            let lut = lut_texture(device, 2);
            let bind_group = bind(device, &layout, &uniforms, &sampler, &slots, &deep, &masks, &lut);
            Layer { uniforms, slots, deep, masks, lut, lut_id: None, bind_group, lens_u: [LensU::zeroed(); 2], rig_id: 0, shown_id: 0 }
        });
        let (overlay, overlay_layout) = overlay_pipeline(device, format);
        Pipeline { pipeline, blend, layout, sampler, srgb: format.is_srgb(), layers, mix: None, overlay, overlay_layout, overlay_slots: Vec::new(), overlays: 0, dither: None }
    }

    /// Gets `overlays` ready to draw over the view (`size` pixels at
    /// `origin` in a `target` pixel target), uploading each picture once.
    pub fn prepare_overlays(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, overlays: &[OverlayDraw], size: [f32; 2], origin: [f32; 2], target: [f32; 2]) {
        self.overlays = overlays.len();
        for (k, d) in overlays.iter().enumerate() {
            let pic = &d.picture;
            if self.overlay_slots.len() <= k {
                let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("overlay"),
                    size: std::mem::size_of::<OverlayU>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let texture = plane(device, 1, 1, wgpu::TextureFormat::Rgba8Unorm);
                let bind_group = bind_overlay(device, &self.overlay_layout, &uniforms, &self.sampler, &texture);
                self.overlay_slots.push(OverlaySlot { uniforms, texture, bind_group, picture: 0 });
            }
            let slot = &mut self.overlay_slots[k];
            if slot.picture != d.id {
                slot.texture = plane(device, pic.width as u32, pic.height as u32, wgpu::TextureFormat::Rgba8Unorm);
                write(queue, &slot.texture.texture, &pic.rgba, pic.width as u32, pic.height as u32, 4);
                slot.bind_group = bind_overlay(device, &self.overlay_layout, &slot.uniforms, &self.sampler, &slot.texture);
                slot.picture = d.id;
            }
            let q = Quad::new(&d.place, pic, (size[0].round() as usize, size[1].round() as usize));
            let f = |v: f64| v as f32;
            let (s, c) = q.turn.sin_cos();
            // Its opacity and fade are applied on their own, after the grade.
            let shade = Grade { opacity: 1.0, black: 0.0, ..d.grade };
            let u = OverlayU {
                frame: [size[0], size[1], origin[0], origin[1]],
                target: [target[0], target[1], self.srgb as u8 as f32, f(d.black)],
                quad: [f(q.centre[0]), f(q.centre[1]), f(q.half[0]), f(q.half[1])],
                turn: [f(c), f(s), f(d.grade.opacity), if shade.is_neutral() { 0.0 } else { 1.0 }],
                grade: shade.uniforms(),
            };
            queue.write_buffer(&slot.uniforms, 0, bytemuck::bytes_of(&u));
        }
    }

    /// Gets ready to draw `shown` through `camera` into a `size` pixel
    /// rectangle at `origin` in its target. The rig and pictures are
    /// uploaded only when they change.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_frame(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig, shown: &Shown, camera: &Camera, rig_from_view: &[M3], size: [f32; 2], origin: [f32; 2]) {
        self.mix = None;
        self.overlays = 0;
        self.prepare_layer(0, device, queue, rig, shown, camera, rig_from_view, size, origin);
    }

    /// Gets layer `layer` ready (see `prepare_frame`).
    #[allow(clippy::too_many_arguments)]
    fn prepare_layer(&mut self, layer: usize, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig, shown: &Shown, camera: &Camera, rig_from_view: &[M3], size: [f32; 2], origin: [f32; 2]) {
        let (srgb, dither) = (self.srgb, self.dither);
        let (layout, sampler) = (&self.layout, &self.sampler);
        let l = &mut self.layers[layer];
        if l.rig_id != rig.id {
            l.set_rig(device, queue, rig, layout, sampler);
        }
        if l.shown_id != shown.id {
            l.upload(device, queue, &shown.frame, rig, layout, sampler);
            l.shown_id = shown.id;
        }
        let mut u = Uniforms::zeroed();
        u.dither = dither.map_or([0.0; 4], |n| [1.0, (n % 65536) as f32, 0.0, 0.0]);
        u.user = rows(camera.user);
        // A negative mapping means equirect; see the shader's `screen_ray`.
        u.screen = [size[0], size[1], camera.zoom as f32, camera.planet.map_or(-1.0, |d| d as f32)];
        let steps = rig_from_view.len().min(RS_STEPS_MAX);
        u.misc = [steps as f32, srgb as u8 as f32, origin[0], origin[1]];
        u.lenses = l.lens_u;
        u.grade = camera.grade.uniforms();
        if let Some(id) = camera.lut {
            let lut = crate::lut::get(id);
            if l.lut_id != Some(id) {
                l.lut = upload_lut(device, queue, &lut);
                l.lut_id = Some(id);
                l.bind_group = bind(device, layout, &l.uniforms, sampler, &l.slots, &l.deep, &l.masks, &l.lut);
            }
            let (lo, hi) = lut.domain;
            u.lut = [[1.0, camera.grade.lut.clamp(0.0, 1.0) as f32, lut.size as f32, 0.0], [lo[0], lo[1], lo[2], 0.0], [hi[0], hi[1], hi[2], 0.0]];
        }
        if rig.flat
            && let Some(p) = shown.frame.pictures.first()
        {
            u.flat = [1.0, p.width as f32 / p.height as f32, p.deep as u8 as f32, rig.hdr.code()];
            let c = &camera.crop;
            u.crop = [c.centre[0], c.centre[1], c.half, c.roll];
        }
        for (k, m) in rig_from_view.iter().take(steps).enumerate() {
            u.views[k * 3..k * 3 + 3].copy_from_slice(&rows(*m));
        }
        // Set whatever corrections are on: the blend needs it too (without
        // it the picture was black), and without exposure matching the
        // gains stay 1.
        if let Some(ring) = &rig.ring {
            u.seam = rows(ring.basis);
            u.seam[0][3] = FADE_FROM as f32;
            u.seam[1][3] = FADE_TO as f32;
        }
        for (lens, bins) in shown.fix.gains.0.iter().enumerate() {
            for (b, g) in bins.iter().enumerate() {
                u.gains[lens * BINS + b] = [g[0], g[1], g[2], 0.0];
            }
        }
        for (v, cells) in u.field.iter_mut().zip(shown.fix.field.shift.chunks_exact(4)) {
            v.copy_from_slice(cells);
        }
        for (v, bins) in u.cut.iter_mut().zip(shown.fix.field.cut.chunks_exact(4)) {
            v.copy_from_slice(bins);
        }
        queue.write_buffer(&l.uniforms, 0, bytemuck::bytes_of(&u));
    }

    /// Draws the prepared frame into `target`, within `clip`.
    pub fn draw(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, clip: &Rectangle<u32>) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("stitch"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_scissor_rect(clip.x, clip.y, clip.width, clip.height);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.layers[0].bind_group, &[]);
        pass.draw(0..3, 0..1);
        if let Some(k) = self.mix {
            let k = k as f64;
            pass.set_pipeline(&self.blend);
            pass.set_blend_constant(wgpu::Color { r: k, g: k, b: k, a: k });
            pass.set_bind_group(0, &self.layers[1].bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        if self.overlays > 0 {
            pass.set_pipeline(&self.overlay);
            for slot in &self.overlay_slots[..self.overlays] {
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
        }
    }
}

impl Layer {
    fn set_rig(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler) {
        if rig.flat {
            self.masks = [0, 1].map(|_| plane(device, 1, 1, wgpu::TextureFormat::R8Unorm));
            self.lens_u = [LensU::zeroed(); 2];
            self.bind_group = bind(device, layout, &self.uniforms, sampler, &self.slots, &self.deep, &self.masks, &self.lut);
            self.rig_id = rig.id;
            self.shown_id = 0;
            return;
        }
        self.masks = [0, 1].map(|i| {
            let l = &rig.lenses[i];
            let [w, h] = l.calib_dim;
            let p = plane(device, w as u32, h as u32, wgpu::TextureFormat::R8Unorm);
            write(queue, &p.texture, &l.edge, w as u32, h as u32, 1);
            p
        });
        self.lens_u = [0, 1].map(|i| lens_uniform(&rig.lenses[i]));
        self.bind_group = bind(device, layout, &self.uniforms, sampler, &self.slots, &self.deep, &self.masks, &self.lut);
        self.rig_id = rig.id;
        self.shown_id = 0;
    }

    /// Uploads a frame's pictures and points each lens at its part of them.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &Frame, rig: &Rig, layout: &wgpu::BindGroupLayout, sampler: &wgpu::Sampler) {
        let mut rebind = false;
        for (slot, pic) in frame.pictures.iter().enumerate().take(2) {
            let bytes = if pic.deep { 2 } else { 1 };
            let (y, uv) = if pic.deep { &mut self.deep[slot] } else { &mut self.slots[slot] };
            if y.texture.width() != pic.width || y.texture.height() != pic.height {
                (*y, *uv) = if pic.deep {
                    deep_planes(device, pic.width, pic.height)
                } else {
                    (plane(device, pic.width, pic.height, wgpu::TextureFormat::R8Unorm), plane(device, pic.width / 2, pic.height / 2, wgpu::TextureFormat::Rg8Unorm))
                };
                rebind = true;
            }
            write(queue, &y.texture, pic.y(), pic.width, pic.height, bytes);
            write(queue, &uv.texture, pic.uv(), pic.width / 2, pic.height / 2, 2 * bytes);
        }
        if rebind {
            self.bind_group = bind(device, layout, &self.uniforms, sampler, &self.slots, &self.deep, &self.masks, &self.lut);
        }
        for (i, lens) in rig.lenses.iter().enumerate().take(2) {
            let track = rig.tracks[lens.input];
            let (slot, pic) = match frame.layout {
                Layout::SideBySide => (0, &frame.pictures[0]),
                Layout::PerTrack => (track as usize, &frame.pictures[track as usize]),
            };
            let cell = if frame.layout == Layout::SideBySide { pic.height } else { pic.width } as f64;
            let s = cell / (lens.calib_dim[0] as f64 * lens.calib_to_picture.a[0][0]); // picture width, see stitch.rs
            let off_x = if frame.layout == Layout::SideBySide { track as f64 * cell } else { 0.0 };
            let (w, h) = (pic.width as f64, pic.height as f64);
            let a = &lens.calib_to_picture;
            let u = &mut self.lens_u[i];
            u.a0 = [(s * a.a[0][0] / w) as f32, (s * a.a[0][1] / w) as f32, ((s * a.t[0] + off_x) / w) as f32, lens.mirror as u8 as f32];
            // The slot, plus 2 for its 10-bit planes.
            u.a1 = [(s * a.a[1][0] / h) as f32, (s * a.a[1][1] / h) as f32, (s * a.t[1] / h) as f32, (slot + 2 * pic.deep as usize) as f32];
        }
    }
}

impl shader::Primitive for Primitive {
    type Pipeline = Pipeline;

    fn prepare(&self, p: &mut Pipeline, device: &wgpu::Device, queue: &wgpu::Queue, bounds: &Rectangle, viewport: &Viewport) {
        let px = *bounds * viewport.scale_factor();
        let (size, origin) = ([px.width, px.height], [px.x, px.y]);
        p.prepare_frame(device, queue, &self.rig, &self.shown, &self.camera, &self.rig_from_view, size, origin);
        if let Some(i) = &self.incoming {
            p.prepare_layer(1, device, queue, &i.rig, &i.shown, &i.camera, &i.rig_from_view, size, origin);
            p.mix = Some(i.mix);
        }
        let target = viewport.physical_size();
        p.prepare_overlays(device, queue, &self.overlays, size, origin, [target.width as f32, target.height as f32]);
    }

    fn render(&self, p: &Pipeline, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, clip: &Rectangle<u32>) {
        p.draw(encoder, target, clip);
    }
}

fn lens_uniform(l: &Lens) -> LensU {
    let f = |v: f64| v as f32;
    LensU {
        r: rows(l.cam_from_rig),
        k: [f(l.k[0]), f(l.k[1]), f(l.k[2]), f(l.k[3])],
        k5p: [f(l.k[4]), f(l.p[0]), f(l.p[1]), f(l.skew)],
        fc: [f(l.focal[0]), f(l.focal[1]), f(l.centre[0]), f(l.centre[1])],
        a0: [0.0; 4],
        a1: [0.0; 4],
        lim: [f(l.fov_limit), f(l.blend.limit), f(l.blend.width), 0.0],
        calib: [l.calib_dim[0] as f32, l.calib_dim[1] as f32, 0.0, 0.0],
    }
}

fn rows(m: M3) -> [[f32; 4]; 3] {
    m.map(|r| [r[0] as f32, r[1] as f32, r[2] as f32, 0.0])
}

fn plane(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> Plane {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Plane { texture, view }
}

/// A 10-bit picture's Y and UV planes, `w` × `h`.
fn deep_planes(device: &wgpu::Device, w: u32, h: u32) -> (Plane, Plane) {
    (plane(device, w, h, wgpu::TextureFormat::R16Uint), plane(device, (w / 2).max(1), (h / 2).max(1), wgpu::TextureFormat::Rg16Uint))
}

fn write(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * bpp), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
}

/// The overlays' pipeline: `overlay.wgsl` over a triangle strip, blended
/// premultiplied over what's drawn.
fn overlay_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat) -> (wgpu::RenderPipeline, wgpu::BindGroupLayout) {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("overlay"),
        source: wgpu::ShaderSource::Wgsl(concat!(include_str!("overlay.wgsl"), include_str!("grade.wgsl")).into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("overlay"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
                count: None,
            },
            wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
                count: None,
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[&layout], push_constant_ranges: &[] });
    let over = wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add };
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("overlay"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: Some("ovs"), compilation_options: Default::default(), buffers: &[] },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("ofs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: Some(wgpu::BlendState { color: over, alpha: over }), write_mask: wgpu::ColorWrites::ALL })],
        }),
        multiview: None,
        cache: None,
    });
    (pipeline, layout)
}

fn bind_overlay(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, uniforms: &wgpu::Buffer, sampler: &wgpu::Sampler, texture: &Plane) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("overlay"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&texture.view) },
        ],
    })
}

#[allow(clippy::too_many_arguments)]
fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    slots: &[(Plane, Plane); 2],
    deep: &[(Plane, Plane); 2],
    masks: &[Plane; 2],
    lut: &Plane,
) -> wgpu::BindGroup {
    fn tex(binding: u32, p: &Plane) -> wgpu::BindGroupEntry<'_> {
        wgpu::BindGroupEntry { binding, resource: wgpu::BindingResource::TextureView(&p.view) }
    }
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
            tex(2, &slots[0].0),
            tex(3, &slots[0].1),
            tex(4, &slots[1].0),
            tex(5, &slots[1].1),
            tex(6, &masks[0]),
            tex(7, &masks[1]),
            tex(8, &deep[0].0),
            tex(9, &deep[0].1),
            tex(10, &deep[1].0),
            tex(11, &deep[1].1),
            tex(12, lut),
        ],
    })
}

/// An `n`³ RGBA half-float 3D texture for a LUT: filterable without any
/// device feature, so the hardware interpolates it trilinearly, as
/// `Lut::apply` does; f16's 11 bits are finer than a 10-bit step.
fn lut_texture(device: &wgpu::Device, n: u32) -> Plane {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lut"),
        size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    Plane { texture, view }
}

/// `lut` uploaded: the .cube order (red fastest) is the texture's x, y, z.
fn upload_lut(device: &wgpu::Device, queue: &wgpu::Queue, lut: &crate::lut::Lut) -> Plane {
    let n = lut.size as u32;
    let plane = lut_texture(device, n);
    let data: Vec<u8> = lut.table.iter().flat_map(|c| [c[0], c[1], c[2], 1.0]).flat_map(|v| to_half(v).to_le_bytes()).collect();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo { texture: &plane.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 8), rows_per_image: Some(n) },
        wgpu::Extent3d { width: n, height: n, depth_or_array_layers: n },
    );
    plane
}

/// An IEEE half-precision float's bits, rounded to nearest.
fn to_half(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let man = bits & 0x7f_ffff;
    if v.is_nan() {
        return 0x7e00;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    if exp <= 0 {
        if exp < -10 {
            return sign;
        }
        let m = (man | 0x80_0000) >> (1 - exp);
        return sign | ((m + 0x1000) >> 13) as u16;
    }
    sign | ((((exp as u32) << 10) | (man >> 13)) + ((man >> 12) & 1)) as u16
}
