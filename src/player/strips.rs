//! The seam strips sampled on the GPU, for zero-copy decoding: the lens
//! pictures are in the stitcher's 10-bit planes and never on the CPU, so a
//! small pass reads the strip pixels the aligner wants out of them and only
//! that (1024 × 161 a lens) is read back. Each strip pixel is the picture
//! pixel `source::Frame::rgb_at` takes.

use super::gpu::Rig;
use crate::align::{STRIP_H, STRIP_W, Strips};
use anyhow::{Context, Result};
use iced::wgpu;

const SHADER: &str = r#"
@group(0) @binding(0) var points: texture_2d<f32>;
@group(0) @binding(1) var y0: texture_2d<u32>;
@group(0) @binding(2) var uv0: texture_2d<u32>;
@group(0) @binding(3) var y1: texture_2d<u32>;
@group(0) @binding(4) var uv1: texture_2d<u32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {
    let p = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(p * 2.0 - 1.0, 0.0, 1.0);
}

// A strip pixel: the picture pixel in `points` (x, y, 1 if there is one,
// its picture slot) as it is stored, Y, Cb, Cr and 1; all 0 where the lens
// has no picture. The CPU turns them into the brightness it would have read
// off the picture itself, to the bit: worked out here in 32-bit floats, a
// few near-tied matches in flat areas (a car's dark interior) fell the
// other way.
@fragment
fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4u {
    let p = textureLoad(points, vec2i(pos.xy), 0);
    if p.z < 0.5 {
        return vec4u(0u);
    }
    let at = vec2i(p.xy);
    if p.w < 0.5 {
        return vec4u(textureLoad(y0, at, 0).r, textureLoad(uv0, at / 2, 0).rg, 1u);
    }
    return vec4u(textureLoad(y1, at, 0).r, textureLoad(uv1, at / 2, 0).rg, 1u);
}
"#;

/// Both lenses' strips, one above the other.
const SIZE: (u32, u32) = (STRIP_W as u32, 2 * STRIP_H as u32);

pub struct Gather {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    target: wgpu::Texture,
    readback: wgpu::Buffer,
    /// The strip pixels' picture pixels, for the rig and picture size named.
    points: Option<(u64, (u32, u32), wgpu::Texture)>,
    /// Bound to these planes.
    bound: Option<(Vec<wgpu::Texture>, wgpu::BindGroup)>,
}

impl Gather {
    pub fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("strips"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let entry = |binding, sample_type| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        };
        let uint = wgpu::TextureSampleType::Uint;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("strips"),
            entries: &[entry(0, wgpu::TextureSampleType::Float { filterable: false }), entry(1, uint), entry(2, uint), entry(3, uint), entry(4, uint)],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("strips"), bind_group_layouts: &[&layout], push_constant_ranges: &[] });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("strips"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &module, entry_point: Some("vs"), buffers: &[], compilation_options: Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState { format: wgpu::TextureFormat::Rgba16Uint, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("strips"),
            size: wgpu::Extent3d { width: SIZE.0, height: SIZE.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Uint,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor { label: Some("strips"), size: (SIZE.0 * SIZE.1 * 8) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        Gather { pipeline, layout, target, readback, points: None, bound: None }
    }

    /// `rig`'s seam strips from the lens pictures in `planes` (per picture
    /// slot its 16-bit Y and UV), `picture` pixels square each; `None` for
    /// a rig without seam alignment.
    pub fn strips(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, rig: &Rig, planes: [[&wgpu::Texture; 2]; 2], picture: (u32, u32)) -> Result<Option<Strips>> {
        let Some(aligner) = &rig.aligner else { return Ok(None) };
        if self.points.as_ref().is_none_or(|(id, size, _)| (*id, *size) != (rig.id, picture)) {
            // As `source::Frame::rgb_at` finds a lens's pixel in a picture
            // of its own (`Layout::PerTrack`).
            let mut data = vec![0f32; (SIZE.0 * SIZE.1 * 4) as usize];
            for (i, at) in aligner.strip_points().iter().enumerate() {
                for (l, lens) in rig.lenses.iter().enumerate().take(2) {
                    let Some((u, v)) = at[l] else { continue };
                    let s = picture.0 as f64 / (lens.calib_dim[0] as f64 * lens.calib_to_picture.a[0][0]);
                    let (x, y) = ((s * u) as i64, (s * v) as i64);
                    if x < 0 || y < 0 || x >= picture.0 as i64 || y >= picture.1 as i64 {
                        continue;
                    }
                    let o = (l * STRIP_W * STRIP_H + i) * 4;
                    data[o..o + 4].copy_from_slice(&[x as f32, y as f32, 1.0, rig.tracks[lens.input] as f32]);
                }
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("strip points"),
                size: wgpu::Extent3d { width: SIZE.0, height: SIZE.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                bytemuck::cast_slice(&data),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIZE.0 * 16), rows_per_image: Some(SIZE.1) },
                wgpu::Extent3d { width: SIZE.0, height: SIZE.1, depth_or_array_layers: 1 },
            );
            self.points = Some((rig.id, picture, texture));
            self.bound = None;
        }
        let now: Vec<wgpu::Texture> = planes.iter().flatten().map(|&t| t.clone()).collect();
        if self.bound.as_ref().is_none_or(|(planes, _)| *planes != now) {
            let points = self.points.as_ref().unwrap().2.create_view(&Default::default());
            let views: Vec<_> = now.iter().map(|t| t.create_view(&Default::default())).collect();
            let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&points) }];
            entries.extend(views.iter().enumerate().map(|(i, v)| wgpu::BindGroupEntry { binding: i as u32 + 1, resource: wgpu::BindingResource::TextureView(v) }));
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("strips"), layout: &self.layout, entries: &entries });
            self.bound = Some((now, group));
        }
        let view = self.target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("strips"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bound.as_ref().unwrap().1, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &self.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(SIZE.0 * 8), rows_per_image: Some(SIZE.1) } },
            wgpu::Extent3d { width: SIZE.0, height: SIZE.1, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        self.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely())?;
        rx.recv().context("the strips' readback was dropped")?.context("reading the seam strips back from the GPU")?;
        let lenses = {
            let data = self.readback.slice(..).get_mapped_range();
            let values: &[[u16; 4]] = bytemuck::cast_slice(&data);
            let half = STRIP_W * STRIP_H;
            // No picture there is NaN to the aligner.
            let strip = |l: usize| values[l * half..(l + 1) * half].iter().map(|&[y, cb, cr, there]| if there == 0 { f32::NAN } else { super::source::brightness(y, cb, cr) as f32 }).collect::<Vec<f32>>();
            [strip(0), strip(1)]
        };
        self.readback.unmap();
        Ok(Some(Strips::from_lenses(lenses)))
    }
}
