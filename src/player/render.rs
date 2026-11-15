//! Export on the GPU: the player's stitch shader (`gpu.rs`) rendering into
//! an off-screen texture on a wgpu device of its own, read back for the
//! encoder. The CPU stitcher in `stitch.rs` stays the reference (`export
//! --cpu`), and the two must match.
//!
//! The work overlaps in stages, each on its own thread: ffmpeg decodes, one
//! thread samples each frame's seam, the next matches it against the frame
//! before and works out the attitude, and the GPU renders a frame while the
//! last one is read back and encoded. Done one after another on 0037 at
//! 1080p the steps took 22 ms (seam), 14 ms (upload), 10 ms (GPU) and 6 ms
//! (conversion) a frame, and the export ran at 11 fps; overlapped, with
//! CUDA's NV12 decode, 21 fps.

use super::gpu::{self, Rig, SeamFix, SeamSample, Shown};
use super::source::{self, Frame, Kind};
use crate::ffmpeg::{self, Hwaccel};
use crate::stitch::{self, M3};
use crate::view::Camera;
use anyhow::{Context, Result, bail};
use iced::Rectangle;
use iced::wgpu;
use rayon::prelude::*;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel, sync_channel};

/// Display-encoded RGBA, as the shader writes it for a non-sRGB target; for
/// a 10-bit export 16-bit floats (a core format, blending included; its 11
/// bits of precision near white are half a 10-bit step).
fn format(deep: bool) -> wgpu::TextureFormat {
    if deep { wgpu::TextureFormat::Rgba16Float } else { wgpu::TextureFormat::Rgba8Unorm }
}
/// Frames in flight on the GPU: one renders while the other is read back.
const IN_FLIGHT: usize = 2;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    stitcher: gpu::Pipeline,
    size: (usize, usize),
    targets: Vec<Target>,
    /// Submitted frames, oldest first: the target each renders into, and
    /// word of its readback being mapped.
    pending: VecDeque<(usize, Receiver<Result<(), wgpu::BufferAsyncError>>)>,
    next: usize,
    /// Frames submitted, numbering the dither noise so it changes each frame.
    frames: u32,
    /// Renders and hands back 16-bit frames (`stitch::Frame::deep`).
    deep: bool,
    /// The adapter's name, for the log.
    pub name: String,
    /// Zero-copy decoding on this device (the prototype; `zerocopy::on`
    /// asks for it), and the lens planes made ready for its copies.
    #[cfg(feature = "zero-copy")]
    shared: Option<crate::zerocopy::Shared>,
    #[cfg(feature = "zero-copy")]
    primed: Vec<wgpu::Texture>,
    /// The seam strips read off those planes.
    #[cfg(feature = "zero-copy")]
    gather: Option<super::strips::Gather>,
}

/// What a frame renders into, and the buffer it is read back through.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Bytes per row in `readback`, padded as copies require.
    row: usize,
}

impl Gpu {
    /// A device of our own on the fastest GPU. A software rasteriser
    /// doesn't count: the CPU stitcher is quicker than that.
    /// `deep`: 16-bit frames, for a 10-bit export.
    pub fn new(deep: bool) -> Result<Gpu> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let options = wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, ..Default::default() };
        let adapter = block_on(instance.request_adapter(&options)).context("no GPU adapter")?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            bail!("only a software renderer ({})", info.name);
        }
        #[cfg(feature = "zero-copy")]
        if crate::zerocopy::on() {
            match crate::zerocopy::Shared::open(&adapter) {
                Ok((device, queue, shared)) => {
                    let stitcher = gpu::Pipeline::create(&device, format(deep));
                    return Ok(Gpu { device, queue, stitcher, size: (0, 0), targets: Vec::new(), pending: VecDeque::new(), next: 0, frames: 0, deep, name: info.name, shared: Some(shared), primed: Vec::new(), gather: None });
                }
                // The pipe path is always there.
                Err(e) => eprintln!("zero-copy decoding isn't available ({e:#}); decoding as usual"),
            }
        }
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor { label: Some("export"), ..Default::default() }))?;
        let stitcher = gpu::Pipeline::create(&device, format(deep));
        Ok(Gpu {
            device,
            queue,
            stitcher,
            size: (0, 0),
            targets: Vec::new(),
            pending: VecDeque::new(),
            next: 0,
            frames: 0,
            deep,
            name: info.name,
            #[cfg(feature = "zero-copy")]
            shared: None,
            #[cfg(feature = "zero-copy")]
            primed: Vec::new(),
            #[cfg(feature = "zero-copy")]
            gather: None,
        })
    }

    /// Puts a zero-copy pair's lens pictures into the stitcher's planes, on
    /// the GPU, for the frames drawn from here on (`submit` with pictures
    /// that hold no bytes).
    #[cfg(feature = "zero-copy")]
    pub fn load_pair(&mut self, rig: &Rig, shown: &Shown, camera: &Camera, (w, h): (usize, usize), pair: &crate::zerocopy::Pair) -> Result<()> {
        // The copy isn't in wgpu's queue: the frames before must have been
        // drawn from the planes before they change.
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        self.stitcher.prepare_frame(&self.device, &self.queue, rig, shown, camera, &[stitch::IDENTITY], [w as f32, h as f32], [0.0, 0.0]);
        let planes: Vec<wgpu::Texture> = self.stitcher.lens_planes().iter().flatten().map(|&t| t.clone()).collect();
        if planes != self.primed {
            // New planes: written once through wgpu and drawn from once,
            // which leaves them in the layout it samples them in and
            // believes them in from then on.
            self.stitcher.clear_lens_planes(&self.queue);
            let scratch = self.make_target(w, h);
            let mut encoder = self.device.create_command_encoder(&Default::default());
            self.stitcher.draw(&mut encoder, &scratch.view, &Rectangle { x: 0, y: 0, width: w as u32, height: h as u32 });
            self.queue.submit([encoder.finish()]);
            self.device.poll(wgpu::PollType::wait_indefinitely())?;
            self.primed = planes;
        }
        self.shared.as_ref().context("zero-copy isn't on")?.copy(pair, self.stitcher.lens_planes())
    }

    /// The seam strips of the pictures `load_pair` put in the planes.
    #[cfg(feature = "zero-copy")]
    pub fn seam_strips(&mut self, rig: &Rig, picture: (u32, u32)) -> Result<Option<crate::align::Strips>> {
        let gather = self.gather.get_or_insert_with(|| super::strips::Gather::new(&self.device));
        gather.strips(&self.device, &self.queue, rig, self.stitcher.lens_planes(), picture)
    }

    /// Starts rendering `shown` through `camera` at `size`; `finish` hands
    /// the frames back in order. At most `IN_FLIGHT` may be waiting, so
    /// `finish` one first when that many are.
    pub fn submit(&mut self, rig: &Rig, shown: &Shown, camera: &Camera, views: &[M3], (w, h): (usize, usize)) {
        assert!(self.pending.len() < IN_FLIGHT, "finish a frame before submitting another");
        if self.size != (w, h) {
            assert!(self.pending.is_empty(), "the size changed with frames in flight");
            self.targets = (0..IN_FLIGHT).map(|_| self.make_target(w, h)).collect();
            self.size = (w, h);
        }
        let slot = self.next;
        self.next = (self.next + 1) % IN_FLIGHT;
        // A 16-bit target keeps the precision itself.
        self.stitcher.dither = (!self.deep).then_some(self.frames);
        self.frames = self.frames.wrapping_add(1);
        self.stitcher.prepare_frame(&self.device, &self.queue, rig, shown, camera, views, [w as f32, h as f32], [0.0, 0.0]);
        let target = &self.targets[slot];
        let mut encoder = self.device.create_command_encoder(&Default::default());
        self.stitcher.draw(&mut encoder, &target.view, &Rectangle { x: 0, y: 0, width: w as u32, height: h as u32 });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(target.row as u32), rows_per_image: Some(h as u32) },
            },
            wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        let (tx, rx) = channel();
        target.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.pending.push_back((slot, rx));
    }

    /// The oldest submitted frame as RGB, waiting for the GPU if need be;
    /// `None` if none is in flight.
    pub fn finish(&mut self) -> Result<Option<stitch::Frame>> {
        let Some((slot, mapped)) = self.pending.pop_front() else { return Ok(None) };
        let (w, h) = self.size;
        let target = &self.targets[slot];
        // Callbacks fire only from a poll; wait until this one has.
        let result = loop {
            match mapped.try_recv() {
                Ok(r) => break r,
                Err(_) => {
                    self.device.poll(wgpu::PollType::wait_indefinitely())?;
                }
            }
        };
        result.context("reading the frame back from the GPU")?;
        let data = target.readback.slice(..).get_mapped_range();
        let rgb = if self.deep {
            let mut rgb = vec![0; w * h * 6];
            rgb.par_chunks_mut(w * 6).zip(data.par_chunks(target.row)).for_each(|(out, row)| {
                for (o, px) in out.chunks_exact_mut(6).zip(row.chunks_exact(8)) {
                    for c in 0..3 {
                        let v = half(u16::from_le_bytes([px[2 * c], px[2 * c + 1]]));
                        o[2 * c..2 * c + 2].copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0).round() as u16).to_le_bytes());
                    }
                }
            });
            rgb
        } else {
            let mut rgb = vec![0; w * h * 3];
            rgb.par_chunks_mut(w * 3).zip(data.par_chunks(target.row)).for_each(|(out, row)| {
                for (o, px) in out.chunks_exact_mut(3).zip(row.chunks_exact(4)) {
                    o.copy_from_slice(&px[..3]);
                }
            });
            rgb
        };
        drop(data);
        target.readback.unmap();
        Ok(Some(stitch::Frame { width: w, height: h, rgb, deep: self.deep }))
    }

    fn make_target(&self, w: usize, h: usize) -> Target {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("export"),
            size: wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: format(self.deep),
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let row = (w * if self.deep { 8 } else { 4 }).div_ceil(align) * align;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Target { texture, view, readback, row }
    }
}

/// What the GPU needs of a clip besides `crate::Pipeline`.
pub struct Clip {
    /// The GPU's copy of the rig, without the seam corrections the
    /// pipeline was set up without.
    rig: Arc<Rig>,
    /// What decodes the lens tracks: CUDA if it can, which converts to NV12
    /// on the GPU (see `ffmpeg::nv12`), else the pipeline's decoder.
    hw: Hwaccel,
}

impl Clip {
    pub fn new(p: &crate::Pipeline) -> Result<Clip> {
        if p.flat.is_some() {
            return Ok(Clip { rig: Arc::new(Rig::flat(p.tracks[0], p.flat.map_or(crate::hdr::Hdr::None, |f| f.hdr))), hw: p.hw });
        }
        let mut rig = Rig::new(p.lenses.clone(), p.tracks.clone(), p.seam.is_some());
        if p.aligner.is_none() {
            rig.aligner = None;
        }
        let cuda = p.proxy.is_none() && p.hw != Hwaccel::Software && p.hw != Hwaccel::Cuda && Hwaccel::Cuda.works(&p.input, p.tracks[0])?;
        Ok(Clip { rig: Arc::new(rig), hw: if cuda { Hwaccel::Cuda } else { p.hw } })
    }
}

/// A decoded frame with what the shader needs besides the picture.
struct Measured {
    frame: Frame,
    fix: SeamFix,
    views: Vec<M3>,
    t: f64,
}

/// Stitches `total` frames of `p`'s clip from `start` into `encoder` on the
/// GPU, as `crate::encode` does on the CPU: the same frame times, attitude
/// and rolling-shutter steps, and the last frame repeated if the clip ends
/// early so the sound stays in step.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    gpu: &mut Gpu,
    p: &crate::Pipeline,
    clip: &Clip,
    start: f64,
    total: usize,
    encoder: &mut dyn ffmpeg::FrameSink,
    camera: impl Fn(f64) -> Camera,
    size: (usize, usize),
    tick: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<()> {
    let kind = match (&p.flat, &p.proxy) {
        (Some(flat), _) => Kind::Flat { path: p.input.clone(), flat: *flat, size: None, rate: p.rate },
        (None, Some(proxy)) => Kind::Lrf(proxy.clone()),
        (None, None) => Kind::Osv { path: p.input.clone(), tracks: p.tracks.clone(), size: None },
    };
    // Zero-copy (the prototype): the lens tracks decoded on this device,
    // their pictures never leaving the GPU; the seam is measured from
    // strips sampled there. Not with exposure matching, which wants
    // samples from all over the pictures.
    #[cfg(feature = "zero-copy")]
    if let (Some(shared), Kind::Osv { .. }, None) = (&gpu.shared, &kind, &clip.rig.seam) {
        let frames = shared.stream(&p.input, &[0, 1], start)?;
        let rig = &clip.rig;
        let mut fix = SeamFix::default();
        let mut done = 0;
        let mut last: Option<stitch::Frame> = None;
        // The picture showing, the output frame it came in at, and the one
        // decoded after it. Pictures are taken on time as the pipe path
        // takes them (`Stamp::frame`): each shows from its frame until the
        // next one's.
        let mut showing: Option<(crate::zerocopy::Pair, i64)> = None;
        let mut next = frames.recv().ok().transpose()?;
        let first = next.as_ref().map(|pair| pair.1);
        let mut step = 1;
        for n in 0..total {
            let mut fresh = false;
            while let (Some(pair), Some(first)) = (&next, &first) {
                let at = pair.1.frame(first, p.fps);
                if showing.is_some() && at > n as i64 {
                    break;
                }
                if let Some((_, before)) = &showing {
                    step = (at - before).max(1);
                }
                showing = next.take().map(|pair| (pair, at));
                fresh = true;
                next = frames.recv().ok().transpose()?;
            }
            // The clip ran out: the last frame is held, as below.
            let Some((pair, at)) = &showing else { break };
            if next.is_none() && !fresh && n as i64 >= at + step {
                break;
            }
            let t = start + n as f64 / p.fps;
            let views = if p.stab.has_data() { p.stab.rig_from_view(t * 1000.0, p.readout_ms, p.rs_steps) } else { vec![stitch::IDENTITY] };
            if gpu.pending.len() == IN_FLIGHT
                && let Some(frame) = gpu.finish()?
            {
                encoder.write(&frame)?;
                done += 1;
                tick(done)?;
                last = Some(frame);
            }
            let (w, h) = pair.size();
            // Pictures without bytes: they're in the planes already.
            let remote = || Frame { pts: t, layout: source::Layout::PerTrack, pictures: (0..2).map(|_| source::Picture { width: w, height: h, deep: true, data: Vec::new() }).collect() };
            let camera = camera(t);
            if fresh {
                gpu.load_pair(rig, &Shown::new(remote(), fix.clone()), &camera, size, pair)?;
            }
            // A repeated picture's seam is measured again and eased on
            // towards, as the pipe path does with the copy it's handed.
            fix.apply(rig, SeamSample::from_strips(gpu.seam_strips(rig, (w, h))?), n > 0);
            if !p.cut {
                fix.field.cut.fill(0.0);
            }
            gpu.submit(rig, &Shown::new(remote(), fix.clone()), &camera, &views, size);
        }
        while let Some(frame) = gpu.finish()? {
            encoder.write(&frame)?;
            done += 1;
            tick(done)?;
            last = Some(frame);
        }
        let last = last.context("no frames decoded")?;
        p.warn_held(total - done);
        for _ in done..total {
            encoder.write(&last)?;
        }
        return Ok(());
    }
    let stream = source::spawn(kind, clip.hw, start, p.fps)?;
    let failed = stream.error.clone();
    let rig = &clip.rig;
    // Seam and attitude for the next frames while this one renders. The
    // threads end with the stream, or when this returns and drops `measured`.
    let (sampled_tx, sampled) = sync_channel::<(Frame, SeamSample)>(2);
    let sample_rig = rig.clone();
    std::thread::spawn(move || {
        for frame in stream.frames.iter().take(total) {
            let sample = SeamSample::new(&sample_rig, &frame);
            if sampled_tx.send((frame, sample)).is_err() {
                break;
            }
        }
    });
    let (tx, measured) = sync_channel::<Measured>(2);
    let (seam_rig, stab, readout_ms, rs_steps, cut, fps) = (rig.clone(), p.stab.clone(), p.readout_ms, p.rs_steps, p.cut, p.fps);
    std::thread::spawn(move || {
        let mut fix = SeamFix::default();
        for (n, (frame, sample)) in sampled.iter().enumerate() {
            let t = start + n as f64 / fps;
            fix.apply(&seam_rig, sample, n > 0);
            if !cut {
                fix.field.cut.fill(0.0);
            }
            let views = if stab.has_data() { stab.rig_from_view(t * 1000.0, readout_ms, rs_steps) } else { vec![stitch::IDENTITY] };
            if tx.send(Measured { frame, fix: fix.clone(), views, t }).is_err() {
                break;
            }
        }
    });
    let mut last: Option<stitch::Frame> = None;
    let mut done = 0;
    let mut write = |frame: &stitch::Frame| -> Result<usize> {
        encoder.write(frame)?;
        done += 1;
        tick(done)?;
        Ok(done)
    };
    let mut written = 0;
    for m in measured.iter() {
        if gpu.pending.len() == IN_FLIGHT
            && let Some(frame) = gpu.finish()?
        {
            written = write(&frame)?;
            last = Some(frame);
        }
        gpu.submit(rig, &Shown::new(m.frame, m.fix), &camera(m.t), &m.views, size);
    }
    while let Some(frame) = gpu.finish()? {
        written = write(&frame)?;
        last = Some(frame);
    }
    // The clip ran out before the cut did: a decoder that failed says so,
    // else the last frame is held.
    if let Some(why) = failed.lock().unwrap().take() {
        bail!("{}: {why}", p.input.display());
    }
    let last = last.context("no frames decoded")?;
    p.warn_held(total - written);
    for _ in written..total {
        write(&last)?;
    }
    Ok(())
}

/// An IEEE half-precision float's value.
fn half(bits: u16) -> f32 {
    let (sign, exp, man) = ((bits >> 15) as u32, ((bits >> 10) & 0x1f) as u32, (bits & 0x3ff) as u32);
    let magnitude = match exp {
        0 => man as f32 / 1024.0 / 16384.0,
        31 => f32::INFINITY,
        _ => f32::from_bits(((exp + 112) << 23) | (man << 13)),
    };
    if sign == 1 { -magnitude } else { magnitude }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    iced::futures::executor::block_on(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{self, Place, Quad};

    /// `w` × `h` 10-bit pictures of one Y′ code a column (`y(x)`) and
    /// these Cb, Cr codes, as P010.
    fn p010(w: usize, h: usize, y: impl Fn(usize) -> u16, cb: u16, cr: u16) -> Frame {
        let mut data = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                data.extend((y(x) << 6).to_le_bytes());
            }
        }
        for _ in 0..w * h / 4 {
            data.extend((cb << 6).to_le_bytes());
            data.extend((cr << 6).to_le_bytes());
        }
        Frame { pts: 0.0, layout: source::Layout::PerTrack, pictures: vec![source::Picture { width: w as u32, height: h as u32, deep: true, data }] }
    }

    /// An ordinary video's `frame` drawn whole into a `w` × `h` target, as
    /// an export draws it.
    fn draw_flat(gpu: &mut Gpu, frame: Frame, hdr: crate::hdr::Hdr, size: (usize, usize)) -> stitch::Frame {
        draw_flat_as(gpu, frame, hdr, size, crate::view::View::home(crate::view::Projection::Perspective).camera())
    }

    /// `draw_flat` through `camera` (its grade and LUT).
    fn draw_flat_as(gpu: &mut Gpu, frame: Frame, hdr: crate::hdr::Hdr, (w, h): (usize, usize), camera: Camera) -> stitch::Frame {
        let shown = Shown::new(frame, SeamFix::default());
        let rig = Rig::flat(0, hdr);
        gpu.stitcher.prepare_frame(&gpu.device, &gpu.queue, &rig, &shown, &camera, &[stitch::IDENTITY], [w as f32, h as f32], [0.0, 0.0]);
        gpu.stitcher.prepare_overlays(&gpu.device, &gpu.queue, &[], [w as f32, h as f32], [0.0, 0.0], [w as f32, h as f32]);
        drawn(gpu, (w, h))
    }

    /// What the stitcher has prepared, drawn into a `w` × `h` target and
    /// read back.
    fn drawn(gpu: &mut Gpu, (w, h): (usize, usize)) -> stitch::Frame {
        gpu.size = (0, 0);
        gpu.targets = vec![gpu.make_target(w, h)];
        let target = &gpu.targets[0];
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        gpu.stitcher.draw(&mut encoder, &target.view, &Rectangle { x: 0, y: 0, width: w as u32, height: h as u32 });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo { buffer: &target.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(target.row as u32), rows_per_image: Some(h as u32) } },
            wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = channel();
        target.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        gpu.pending.push_back((0, rx));
        gpu.size = (w, h);
        gpu.finish().unwrap().unwrap()
    }

    /// The player's two layers through every transition come out as the
    /// export's mix of the two cuts' frames.
    #[test]
    fn gpu_transitions_match_the_cpu() {
        use crate::hdr::Hdr;
        use crate::transition::{Direction, Style, Transition};
        let Ok(mut gpu) = Gpu::new(false) else { return };
        let (w, h) = (64, 32);
        let camera = crate::view::View::home(crate::view::Projection::Perspective).camera();
        let pictures = || [p010(w, h, |x| 200 + 8 * x as u16, 512, 400), p010(w, h, |x| 900 - 10 * x as u16, 600, 512)];
        let [a, b] = pictures().map(|f| draw_flat(&mut gpu, f, Hdr::None, (w, h)));
        let [old, new] = pictures().map(|f| (Arc::new(Rig::flat(0, Hdr::None)), Arc::new(Shown::new(f, SeamFix::default()))));
        let size = [w as f32, h as f32];
        for style in Style::ALL.into_iter().filter(|&s| s != Style::Spin) {
            for direction in Direction::ALL {
                let t = Transition { style, direction };
                for p in [0.3, 0.7] {
                    let [out, inc] = t.parts(p);
                    let want = t.mix(p, &a, &b);
                    // Either cut may be the one the player holds as current.
                    for swapped in [false, true] {
                        let ((first, part), (other, theirs)) = if swapped { ((&new, inc), (&old, out)) } else { ((&old, out), (&new, inc)) };
                        let other = crate::player::gpu::Incoming { rig: other.0.clone(), shown: other.1.clone(), camera, rig_from_view: vec![stitch::IDENTITY], part: theirs, under: swapped };
                        gpu.stitcher.prepare_frame(&gpu.device, &gpu.queue, &first.0, &first.1, &camera, &[stitch::IDENTITY], size, [0.0, 0.0]);
                        gpu.stitcher.prepare_pair(&gpu.device, &gpu.queue, &first.0, &first.1, &camera, &[stitch::IDENTITY], &part, &other, size, [0.0, 0.0]);
                        let got = drawn(&mut gpu, (w, h));
                        let worst = got.rgb.iter().zip(&want).map(|(&g, &c)| (g as i32 - c as i32).abs()).max().unwrap();
                        assert!(worst <= 2, "{t:?} at {p} (swapped: {swapped}): {worst} levels off");
                    }
                }
            }
        }
    }

    /// The shader brings a 10-bit HLG picture to SDR as `hdr::to_sdr` does.
    #[test]
    fn gpu_hdr_matches_the_cpu() {
        use crate::hdr::{self, Hdr};
        let Ok(mut gpu) = Gpu::new(false) else { return };
        let (w, h) = (64, 32);
        // Y′CbCr as 10-bit limited-range codes: grey, sky blue, green, near
        // peak white.
        for [y, cb, cr] in [[502, 512, 512], [600, 640, 440], [420, 420, 470], [900, 512, 512]] {
            let got = draw_flat(&mut gpu, p010(w, h, |_| y, cb, cr), Hdr::Hlg, (w, h));
            let ycc = [(y as f32 - 64.0) / 876.0, (cb as f32 - 512.0) / 896.0, (cr as f32 - 512.0) / 896.0];
            let want = hdr::to_sdr(hdr::bt2020_rgb(ycc), Hdr::Hlg).map(|v| (v * 255.0).round() as i32);
            let mid = ((h / 2) * w + w / 2) * 3;
            let px = [0, 1, 2].map(|c| got.rgb[mid + c] as i32);
            eprintln!("{ycc:?}: GPU {px:?}, CPU {want:?}");
            assert!(px.iter().zip(want).all(|(a, b)| (a - b).abs() <= 2), "GPU {px:?} against CPU {want:?}");
        }
    }

    /// The shader's LUT (a 3D texture, filtered by the hardware) gives what
    /// `Lut::blend` gives, at part strength too.
    #[test]
    fn gpu_lut_matches_the_cpu() {
        let Ok(mut gpu) = Gpu::new(false) else { return };
        // Bent, and mixing the channels, so the interpolation is tested.
        let mut cube = String::from("LUT_3D_SIZE 17\n");
        for b in 0..17 {
            for g in 0..17 {
                for r in 0..17 {
                    let [r, g, b] = [r, g, b].map(|v| v as f64 / 16.0);
                    cube += &format!("{} {} {}\n", r.powf(0.6), 0.2 * r + 0.8 * g * g, (1.0 - b).sqrt());
                }
            }
        }
        let path = std::env::temp_dir().join(format!("rosmo360-test-{}.cube", std::process::id()));
        std::fs::write(&path, cube).unwrap();
        let id = crate::lut::shared(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let lut = crate::lut::get(id);
        let (w, h) = (64, 32);
        let mut camera = crate::view::View::home(crate::view::Projection::Perspective).camera();
        camera.lut = Some(id);
        camera.grade.lut = 0.7;
        for [y, cb, cr] in [[502, 512, 512], [600, 640, 440], [420, 420, 470], [800, 540, 560]] {
            let got = draw_flat_as(&mut gpu, p010(w, h, |_| y, cb, cr), crate::hdr::Hdr::None, (w, h), camera);
            // As the shader decodes it: BT.709, video range.
            let [yy, u, v] = [(y as f64 - 64.0) / 876.0, (cb as f64 - 512.0) / 896.0, (cr as f64 - 512.0) / 896.0];
            let rgb = [yy + 1.5748 * v, yy - 0.1873 * u - 0.4681 * v, yy + 1.8556 * u].map(|c| c.clamp(0.0, 1.0));
            let want = lut.blend(rgb, 0.7).map(|v| (v * 255.0).round() as i32);
            let mid = ((h / 2) * w + w / 2) * 3;
            let px = [0, 1, 2].map(|c| got.rgb[mid + c] as i32);
            eprintln!("{rgb:?}: GPU {px:?}, CPU {want:?}");
            assert!(px.iter().zip(want).all(|(a, b)| (a - b).abs() <= 2), "GPU {px:?} against CPU {want:?}");
        }
    }

    /// Dithered, a 10-bit grey ramp finer than 8 bits holds its shape in the
    /// 8-bit export: each column's average follows it rather than stepping.
    #[test]
    fn dither_keeps_a_smooth_ramp() {
        let Ok(mut gpu) = Gpu::new(false) else { return };
        let (w, h) = (256, 64);
        // 20 codes across: steps of a quarter of an 8-bit level.
        let code = |x: usize| 400 + (x * 20 / w) as u16;
        let mut errors = Vec::new();
        for dither in [None, Some(7)] {
            gpu.stitcher.dither = dither;
            let got = draw_flat(&mut gpu, p010(w, h, code, 512, 512), crate::hdr::Hdr::None, (w, h));
            let mut squares = 0.0;
            for x in 0..w {
                let mean = (0..h).map(|y| got.rgb[(y * w + x) * 3 + 1] as f64).sum::<f64>() / h as f64;
                let want = (code(x) as f64 - 64.0) / 876.0 * 255.0;
                squares += (mean - want).powi(2);
            }
            errors.push((squares / w as f64).sqrt());
        }
        gpu.stitcher.dither = None;
        eprintln!("RMS error of the column averages: {:.3} levels plain, {:.3} dithered", errors[0], errors[1]);
        assert!(errors[0] > 0.2 && errors[1] < 0.12, "{errors:?}");
    }

    /// A 16-bit render (a 10-bit export) keeps a 10-bit ramp's quarter-level
    /// steps, pixel by pixel, where 8 bits round them away.
    #[test]
    fn deep_render_keeps_ten_bits() {
        let Ok(mut gpu) = Gpu::new(true) else { return };
        let (w, h) = (256, 4);
        let code = |x: usize| 400 + (x * 20 / w) as u16;
        let got = draw_flat(&mut gpu, p010(w, h, code, 512, 512), crate::hdr::Hdr::None, (w, h));
        assert!(got.deep);
        let worst = (0..w)
            .map(|x| {
                let i = (2 * w + x) * 6 + 2;
                let g = u16::from_le_bytes([got.rgb[i], got.rgb[i + 1]]) as f64 / 65535.0 * 255.0;
                (g - (code(x) as f64 - 64.0) / 876.0 * 255.0).abs()
            })
            .fold(0.0, f64::max);
        eprintln!("worst error {worst:.3} of an 8-bit level");
        assert!(worst < 0.1, "{worst}");
    }

    /// The player's overlay pass draws what `overlay::composite` does.
    #[test]
    fn gpu_overlays_match_the_cpu() {
        let Ok(mut gpu) = Gpu::new(false) else { return };
        let (w, h) = (320, 180);
        // A mid-grey ordinary video frame (limited-range NV12).
        let mut data = vec![126u8; w * h];
        data.extend(vec![128u8; w * h / 2]);
        let frame = Frame { pts: 0.0, layout: source::Layout::PerTrack, pictures: vec![source::Picture { width: w as u32, height: h as u32, deep: false, data }] };
        let shown = Shown::new(frame, SeamFix::default());
        let rig = Rig::flat(0, crate::hdr::Hdr::None);
        let camera = crate::view::View::home(crate::view::Projection::Perspective).camera();
        // A 4×2 picture: opaque red left, half-transparent green right.
        let mut rgba = Vec::new();
        for _ in 0..2 {
            rgba.extend([255, 0, 0, 255, 255, 0, 0, 255, 0, 128, 0, 128, 0, 128, 0, 128]);
        }
        let picture = Arc::new(overlay::Picture { width: 4, height: 2, rgba, natural: None });
        let place = Place { x: 0.4, y: 0.6, size: 0.5, rotation: 20.0 };
        let grade = crate::grade::Grade { opacity: 0.8, saturation: -0.5, ..Default::default() };
        let draw = gpu::OverlayDraw { id: 1, picture: picture.clone(), place, grade, black: 0.25 };
        let mut render = |overlays: &[gpu::OverlayDraw]| {
            gpu.size = (0, 0);
            gpu.targets = vec![gpu.make_target(w, h)];
            gpu.stitcher.prepare_frame(&gpu.device, &gpu.queue, &rig, &shown, &camera, &[stitch::IDENTITY], [w as f32, h as f32], [0.0, 0.0]);
            gpu.stitcher.prepare_overlays(&gpu.device, &gpu.queue, overlays, [w as f32, h as f32], [0.0, 0.0], [w as f32, h as f32]);
            let target = &gpu.targets[0];
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            gpu.stitcher.draw(&mut encoder, &target.view, &Rectangle { x: 0, y: 0, width: w as u32, height: h as u32 });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &target.texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: &target.readback, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(target.row as u32), rows_per_image: Some(h as u32) } },
                wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 },
            );
            gpu.queue.submit([encoder.finish()]);
            let (tx, rx) = channel();
            target.readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
            gpu.pending.push_back((0, rx));
            gpu.size = (w, h);
            gpu.finish().unwrap().unwrap()
        };
        let mut want = render(&[]);
        let got = render(&[draw]);
        overlay::composite(&mut want, &picture, &Quad::new(&place, &picture, (w, h)), &grade, 0.25);
        let diff: Vec<i32> = want.rgb.iter().zip(&got.rgb).map(|(a, b)| (*a as i32 - *b as i32).abs()).collect();
        let big = diff.iter().filter(|&&d| d > 8).count();
        let mean = diff.iter().sum::<i32>() as f64 / diff.len() as f64;
        eprintln!("mean diff {mean:.3}, {big} of {} values off by more than 8", diff.len());
        // Edge pixels differ by filtering; the inside must agree.
        assert!(mean < 0.3 && big < 50, "mean {mean}, {big} off");
    }
}
