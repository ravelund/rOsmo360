//! Zero-copy decode, a prototype (CLAUDE.md item 1; the `zero-copy` cargo
//! feature): the lens tracks decoded in-process by ffmpeg's libraries, the
//! frames kept on the GPU. So far only the decode and a measure of it
//! (`probe`); handing the frames to the stitcher comes next.

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, unnecessary_transmutes, clippy::all)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/libav.rs"));
}

use anyhow::{Context, Result, bail};
use ffi::*;
use std::sync::atomic::{AtomicBool, Ordering};

/// Asked for: by `--zero-copy`, the player's test-feature setting, or
/// `ROSMO360_ZEROCOPY` in the environment. One switch a process, as the
/// fine seam alignment's.
static ON: AtomicBool = AtomicBool::new(false);

pub fn set_on(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed) || std::env::var_os("ROSMO360_ZEROCOPY").is_some()
}
use std::ffi::{CStr, CString, c_int};
use std::path::Path;
use std::ptr::{null, null_mut};

/// ffmpeg's `AVERROR(EAGAIN)` and `AVERROR_EOF`.
const AGAIN: c_int = -11;
const EOF: c_int = -0x20464f45;

fn check(code: c_int, what: &str) -> Result<c_int> {
    if code >= 0 {
        return Ok(code);
    }
    let mut text = [0 as std::ffi::c_char; 128];
    // SAFETY: `text` is a buffer of the length given.
    let why = unsafe {
        av_strerror(code, text.as_mut_ptr(), text.len());
        CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned()
    };
    bail!("{what}: {why}")
}

/// One track's decoder, on a hardware device, its frames left there.
struct Track {
    stream: c_int,
    ctx: *mut AVCodecContext,
    frames: usize,
}

/// Takes the hardware format the context was opened for (kept in its
/// `opaque`), or none: a software fallback would defeat the point.
unsafe extern "C" fn pick(ctx: *mut AVCodecContext, mut offered: *const AVPixelFormat::Type) -> AVPixelFormat::Type {
    // SAFETY: ffmpeg hands a list ended by NONE, and the context we opened.
    unsafe {
        let want = (*ctx).opaque as usize as AVPixelFormat::Type;
        while *offered != AVPixelFormat::AV_PIX_FMT_NONE {
            if *offered == want {
                return want;
            }
            offered = offered.add(1);
        }
    }
    AVPixelFormat::AV_PIX_FMT_NONE
}

/// What `probe` found: frames decoded per track, how long it took, and the
/// mean of the first track's first frame's luma (10-bit), to check against
/// another decode of the same frame.
pub struct Probed {
    pub frames: Vec<usize>,
    pub seconds: f64,
    pub first_luma: f64,
}

/// Decodes the first `seconds` of `input`'s first two video tracks on the
/// hardware device `hw` ("vulkan", "cuda", "vaapi"), the frames staying on
/// the GPU, and waits for the last to be finished.
pub fn probe(input: &Path, hw: &str, seconds: f64) -> Result<Probed> {
    let path = CString::new(input.to_string_lossy().as_bytes())?;
    let name = CString::new(hw)?;
    // SAFETY: the libav calls below follow its decode example: every
    // pointer is one it returned, checked before use, and freed at the end.
    unsafe {
        let kind = av_hwdevice_find_type_by_name(name.as_ptr());
        if kind == AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            bail!("ffmpeg knows no \"{hw}\" device");
        }
        let format = match hw {
            "vulkan" => AVPixelFormat::AV_PIX_FMT_VULKAN,
            "cuda" => AVPixelFormat::AV_PIX_FMT_CUDA,
            "vaapi" => AVPixelFormat::AV_PIX_FMT_VAAPI,
            _ => bail!("no frame format known for \"{hw}\""),
        };
        let mut device = null_mut();
        check(av_hwdevice_ctx_create(&mut device, kind, null(), null_mut(), 0), "opening the device")?;
        let mut fmt = null_mut();
        check(avformat_open_input(&mut fmt, path.as_ptr(), null(), null_mut()), "opening the clip")?;
        check(avformat_find_stream_info(fmt, null_mut()), "reading the clip's streams")?;
        let streams = std::slice::from_raw_parts((*fmt).streams, (*fmt).nb_streams as usize);
        let mut tracks = Vec::new();
        for (i, &s) in streams.iter().enumerate() {
            let par = (*s).codecpar;
            if (*par).codec_type != AVMediaType::AVMEDIA_TYPE_VIDEO || (*par).width < 1500 || tracks.len() == 2 {
                continue;
            }
            let codec = avcodec_find_decoder((*par).codec_id);
            let ctx = avcodec_alloc_context3(codec);
            check(avcodec_parameters_to_context(ctx, par), "setting up a decoder")?;
            (*ctx).hw_device_ctx = av_buffer_ref(device);
            (*ctx).opaque = format as usize as *mut _;
            (*ctx).get_format = Some(pick);
            check(avcodec_open2(ctx, codec, null_mut()), "opening a decoder")?;
            tracks.push(Track { stream: i as c_int, ctx, frames: 0 });
        }
        if tracks.is_empty() {
            bail!("no lens tracks in {}", input.display());
        }
        let (mut packet, mut frame, mut first) = (av_packet_alloc(), av_frame_alloc(), av_frame_alloc());
        let mut last: Vec<*mut AVFrame> = tracks.iter().map(|_| av_frame_alloc()).collect();
        let began = std::time::Instant::now();
        let mut ended = false;
        while !ended {
            ended = av_read_frame(fmt, packet) < 0;
            for (k, t) in tracks.iter_mut().enumerate() {
                if !ended && (*packet).stream_index != t.stream {
                    continue;
                }
                let base = (*streams[t.stream as usize]).time_base;
                // Past the span asked for: flush as at the file's end.
                if !ended && (*packet).pts as f64 * base.num as f64 / base.den as f64 >= seconds {
                    ended = true;
                }
                check(avcodec_send_packet(t.ctx, if ended { null() } else { packet }), "decoding")?;
                loop {
                    let got = avcodec_receive_frame(t.ctx, frame);
                    if got == AGAIN || got == EOF {
                        break;
                    }
                    check(got, "decoding")?;
                    if k == 0 && t.frames == 0 {
                        av_frame_ref(first, frame);
                    }
                    t.frames += 1;
                    av_frame_unref(last[k]);
                    av_frame_move_ref(last[k], frame);
                }
            }
            av_packet_unref(packet);
        }
        // The decode runs on behind the frames handed out: downloading each
        // track's last one waits for it.
        let mut cpu = av_frame_alloc();
        for &l in &last {
            if !(*l).buf[0].is_null() {
                av_frame_unref(cpu);
                check(av_hwframe_transfer_data(cpu, l, 0), "downloading a frame")?;
            }
        }
        let took = began.elapsed().as_secs_f64();
        av_frame_unref(cpu);
        check(av_hwframe_transfer_data(cpu, first, 0), "downloading the first frame")?;
        let (w, h, stride) = ((*cpu).width as usize, (*cpu).height as usize, (*cpu).linesize[0] as usize);
        let deep = matches!((*cpu).format, f if f == AVPixelFormat::AV_PIX_FMT_P010LE);
        let mut sum = 0u64;
        for y in 0..h {
            let row = std::slice::from_raw_parts((*cpu).data[0].add(y * stride), w * if deep { 2 } else { 1 });
            sum += if deep { row.chunks_exact(2).map(|s| (u16::from_le_bytes([s[0], s[1]]) >> 6) as u64).sum::<u64>() } else { row.iter().map(|&v| (v as u64) << 2).sum() };
        }
        let probed = Probed { frames: tracks.iter().map(|t| t.frames).collect(), seconds: took, first_luma: sum as f64 / (w * h) as f64 };
        for l in &mut last {
            av_frame_free(l);
        }
        av_frame_free(&mut cpu);
        av_frame_free(&mut first);
        av_frame_free(&mut frame);
        av_packet_free(&mut packet);
        for t in &mut tracks {
            avcodec_free_context(&mut t.ctx);
        }
        avformat_close_input(&mut fmt);
        av_buffer_unref(&mut device);
        anyhow::Ok(probed)
    }
    .with_context(|| format!("zero-copy decode of {}", input.display()))
}

// ---------------------------------------------------------------------
// Decoding on the stitcher's own device, the pictures handed over on the
// GPU.

use ash::vk::{self, Handle};
use iced::wgpu::{self, hal};
use std::sync::mpsc::{Receiver, sync_channel};

/// The export's wgpu device made so ffmpeg can decode on it, and what the
/// raw Vulkan side needs to copy a decoded picture into wgpu's textures.
///
/// ffmpeg gets queue families of its own (compute and transfer, and video
/// decode) so it never shares a queue with wgpu, and this side copies on a
/// second queue of wgpu's family: wgpu's own queue is locked inside wgpu.
pub struct Shared {
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// ffmpeg's handle on the device (an `AVHWDeviceContext`).
    hw: *mut AVBufferRef,
}

// SAFETY: used from one thread at a time (the export's); the decode
// threads take their own references to `hw`, which ffmpeg counts.
unsafe impl Send for Shared {}

impl Shared {
    /// A wgpu device on `adapter` that ffmpeg's Vulkan decoder can use too.
    pub fn open(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue, Shared)> {
        use ash::khr;
        // SAFETY: the handles are wgpu's own, alive as long as the device
        // returned; the calls follow wgpu-hal's and ffmpeg's rules for a
        // device made by the caller (see the comments along the way).
        unsafe {
            let hal_adapter = adapter.as_hal::<hal::api::Vulkan>().context("zero-copy needs Vulkan")?;
            let instance = hal_adapter.shared_instance();
            let raw_instance = instance.raw_instance();
            let phys = hal_adapter.raw_physical_device();
            let families = raw_instance.get_physical_device_queue_family_properties(phys);
            // wgpu takes family 0, queue 0.
            let family = |want: vk::QueueFlags| (1..families.len()).find(|&i| families[i].queue_flags.contains(want));
            let work = family(vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER).context("no second queue family for the decoder's transfers")? as u32;
            let decode = family(vk::QueueFlags::VIDEO_DECODE_KHR).context("this GPU has no Vulkan video decoding")? as u32;
            if families[0].queue_count < 2 {
                bail!("the graphics queue family has one queue only");
            }
            let has: Vec<_> = raw_instance.enumerate_device_extension_properties(phys)?;
            let video = [khr::video_queue::NAME, khr::video_decode_queue::NAME, khr::video_decode_h265::NAME, khr::video_decode_h264::NAME];
            let wanted: Vec<&'static CStr> = video.into_iter().filter(|n| has.iter().any(|e| e.extension_name_as_c_str() == Ok(*n))).collect();
            if !wanted.contains(&khr::video_decode_h265::NAME) {
                bail!("this GPU's driver has no Vulkan H.265 decoding");
            }
            static PRIORITIES: [f32; 2] = [1.0, 1.0];
            // ffmpeg's commands are synchronization2's; wgpu doesn't ask for it.
            let sync2: &'static mut _ = Box::leak(Box::new(vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true)));
            let features = wgpu::Features::empty();
            let open = hal_adapter
                .open_with_callback(
                    features,
                    &wgpu::MemoryHints::default(),
                    Some(Box::new(move |args| {
                        for name in &wanted {
                            if !args.extensions.contains(name) {
                                args.extensions.push(name);
                            }
                        }
                        args.queue_create_infos[0] = args.queue_create_infos[0].queue_priorities(&PRIORITIES);
                        for index in [work, decode] {
                            args.queue_create_infos.push(vk::DeviceQueueCreateInfo::default().queue_family_index(index).queue_priorities(&PRIORITIES[..1]));
                        }
                        *args.create_info = std::mem::take(args.create_info).push_next(sync2);
                    })),
                )
                .map_err(|e| anyhow::anyhow!("opening the device for zero-copy: {e}"))?;
            drop(hal_adapter);
            let (device, queue) = adapter.create_device_from_hal(open, &wgpu::DeviceDescriptor { label: Some("export, zero-copy"), required_features: features, ..Default::default() })?;
            let hal_device = device.as_hal::<hal::api::Vulkan>().context("zero-copy needs Vulkan")?;
            let raw = hal_device.raw_device().clone();
            let hal_adapter = adapter.as_hal::<hal::api::Vulkan>().context("zero-copy needs Vulkan")?;
            let instance = hal_adapter.shared_instance();

            // ffmpeg's side: the device as it is, and which queues are its.
            // Only errors on the terminal: opening an .OSV has ffmpeg remark
            // on its data tracks at every cut.
            av_log_set_level(16);
            let hw = av_hwdevice_ctx_alloc(AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN);
            if hw.is_null() {
                bail!("ffmpeg has no Vulkan support");
            }
            let ctx = (*hw).data as *mut AVHWDeviceContext;
            let v = (*ctx).hwctx as *mut AVVulkanDeviceContext;
            (*v).get_proc_addr = std::mem::transmute::<vk::PFN_vkGetInstanceProcAddr, PFN_vkGetInstanceProcAddr>(instance.entry().static_fn().get_instance_proc_addr);
            (*v).inst = instance.raw_instance().handle().as_raw() as VkInstance;
            (*v).phys_dev = phys.as_raw() as VkPhysicalDevice;
            (*v).act_dev = raw.handle().as_raw() as VkDevice;
            (*v).device_features.sType = VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
            // The names are static; the lists live as long as the process
            // (one device a run).
            let names = |list: &[&'static CStr]| -> (*const *const std::ffi::c_char, c_int) {
                let ptrs: &'static [*const std::ffi::c_char] = Box::leak(list.iter().map(|n| n.as_ptr()).collect::<Vec<_>>().into_boxed_slice());
                (ptrs.as_ptr(), ptrs.len() as c_int)
            };
            ((*v).enabled_inst_extensions, (*v).nb_enabled_inst_extensions) = names(instance.extensions());
            ((*v).enabled_dev_extensions, (*v).nb_enabled_dev_extensions) = names(hal_device.enabled_device_extensions());
            // The decoder's images are shared between the families listed,
            // so wgpu's is among them; by its flags here ffmpeg takes it for
            // nothing but graphics, which decoding has none of.
            let qf = |idx: u32, flags: vk::QueueFlags| AVVulkanDeviceQueueFamily { idx: idx as c_int, num: 1, flags: flags.as_raw() as _, video_caps: 0 };
            (*v).qf[0] = qf(work, vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER);
            (*v).qf[1] = qf(decode, vk::QueueFlags::VIDEO_DECODE_KHR);
            (*v).qf[2] = qf(0, vk::QueueFlags::GRAPHICS);
            (*v).nb_qf = 3;
            check(av_hwdevice_ctx_init(hw), "giving ffmpeg the device")?;

            let copy_queue = raw.get_device_queue(0, 1);
            let pool = raw.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(0).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)?;
            let cmd = raw.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1))?[0];
            let fence = raw.create_fence(&vk::FenceCreateInfo::default(), None)?;
            drop(hal_device);
            drop(hal_adapter);
            Ok((device, queue, Shared { device: raw, queue: copy_queue, pool, cmd, fence, hw }))
        }
    }

    /// Copies a decoded pair's pictures into `planes` (per lens track its
    /// 16-bit Y and UV textures, which wgpu believes are in the layout it
    /// samples them in), and waits for the copy.
    pub fn copy(&self, pair: &Pair, planes: [[&wgpu::Texture; 2]; 2]) -> Result<()> {
        // SAFETY: the frames are ffmpeg's Vulkan frames on this device,
        // changed under their locks as ffmpeg's own filters do: wait for
        // the frame's semaphore at its value, signal the next, and record
        // the layout left.
        unsafe {
            let d = &self.device;
            d.reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(self.cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let colour = |aspect| vk::ImageSubresourceRange::default().aspect_mask(aspect).level_count(1).layer_count(1);
            let layers = |aspect| vk::ImageSubresourceLayers::default().aspect_mask(aspect).layer_count(1);
            let mut waits = Vec::new();
            let mut signals = Vec::new();
            let mut locked = Vec::new();
            for (k, &frame) in pair.0.iter().enumerate() {
                let vkf = (*frame).data[0] as *mut AVVkFrame;
                let fc = (*(*frame).hw_frames_ctx).data as *mut AVHWFramesContext;
                let vfc = (*fc).hwctx as *mut AVVulkanFramesContext;
                if !(*vkf).img[1].is_null() {
                    bail!("the decoder gave a picture as separate planes");
                }
                (*vfc).lock_frame.context("ffmpeg's frames have no lock")?(fc, vkf);
                locked.push((fc, vfc, vkf));
                let src = vk::Image::from_raw((*vkf).img[0] as u64);
                let sem = vk::Semaphore::from_raw((*vkf).sem[0] as u64);
                let value = (*vkf).sem_value[0];
                waits.push(vk::SemaphoreSubmitInfo::default().semaphore(sem).value(value).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS));
                signals.push(vk::SemaphoreSubmitInfo::default().semaphore(sem).value(value + 1).stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS));
                let dst: [vk::Image; 2] = [0, 1].map(|i| planes[k][i].as_hal::<hal::api::Vulkan>().map(|t| t.raw_handle()).unwrap_or_default());
                if dst.iter().any(|i| i.is_null()) {
                    bail!("the stitcher's textures aren't Vulkan's");
                }
                let whole = |image, from, to, src_access, dst_access| {
                    vk::ImageMemoryBarrier2::default()
                        .image(image)
                        .old_layout(from)
                        .new_layout(to)
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .src_access_mask(src_access)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                        .dst_access_mask(dst_access)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .subresource_range(colour(vk::ImageAspectFlags::COLOR))
                };
                // The bindings give the access flags as 32 or 64 bits, by the
                // compiler that read the header.
                #[allow(clippy::unnecessary_cast)]
                let before = [
                    whole(src, vk::ImageLayout::from_raw((*vkf).layout[0] as i32), vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::AccessFlags2::from_raw((*vkf).access[0] as u64), vk::AccessFlags2::TRANSFER_READ),
                    whole(dst[0], vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags2::SHADER_READ, vk::AccessFlags2::TRANSFER_WRITE),
                    whole(dst[1], vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags2::SHADER_READ, vk::AccessFlags2::TRANSFER_WRITE),
                ];
                d.cmd_pipeline_barrier2(self.cmd, &vk::DependencyInfo::default().image_memory_barriers(&before));
                let (w, h) = ((*frame).width as u32, (*frame).height as u32);
                for (plane, aspect, extent) in [(0, vk::ImageAspectFlags::PLANE_0, (w, h)), (1, vk::ImageAspectFlags::PLANE_1, (w.div_ceil(2), h.div_ceil(2)))] {
                    let region = vk::ImageCopy::default().src_subresource(layers(aspect)).dst_subresource(layers(vk::ImageAspectFlags::COLOR)).extent(vk::Extent3D { width: extent.0, height: extent.1, depth: 1 });
                    d.cmd_copy_image(self.cmd, src, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst[plane], vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
                }
                let after = [0, 1].map(|i| whole(dst[i], vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::AccessFlags2::TRANSFER_WRITE, vk::AccessFlags2::SHADER_READ));
                d.cmd_pipeline_barrier2(self.cmd, &vk::DependencyInfo::default().image_memory_barriers(&after));
            }
            d.end_command_buffer(self.cmd)?;
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd)];
            let submit = vk::SubmitInfo2::default().wait_semaphore_infos(&waits).signal_semaphore_infos(&signals).command_buffer_infos(&cmds);
            let sent = d.queue_submit2(self.queue, &[submit], self.fence);
            for (fc, vfc, vkf) in locked {
                if sent.is_ok() {
                    (*vkf).layout[0] = vk::ImageLayout::TRANSFER_SRC_OPTIMAL.as_raw() as _;
                    (*vkf).access[0] = vk::AccessFlags2::TRANSFER_READ.as_raw() as _;
                    (*vkf).sem_value[0] += 1;
                }
                if let Some(unlock) = (*vfc).unlock_frame {
                    unlock(fc, vkf);
                }
            }
            sent?;
            d.wait_for_fences(&[self.fence], true, 5_000_000_000)?;
            d.reset_fences(&[self.fence])?;
        }
        Ok(())
    }

    /// Starts decoding `input`'s lens tracks (video stream numbers) from
    /// `start` seconds on this device: pairs of pictures, one per track, in
    /// order, until the file ends.
    pub fn stream(&self, input: &Path, tracks: &[u32], start: f64) -> Result<Receiver<Result<Pair>>> {
        let path = CString::new(input.to_string_lossy().as_bytes())?;
        // SAFETY: a reference of its own for the thread, freed there.
        let hw = Hw(unsafe { av_buffer_ref(self.hw) });
        let tracks = tracks.to_vec();
        let (tx, rx) = sync_channel(2);
        std::thread::spawn(move || {
            let hw = hw;
            // SAFETY: see `decode`.
            if let Err(e) = unsafe { decode(&path, &tracks, start, hw.0, &tx) } {
                let _ = tx.send(Err(e));
            }
            let mut hw = hw.0;
            // SAFETY: the thread's own reference.
            unsafe { av_buffer_unref(&mut hw) };
        });
        Ok(rx)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: made in `open`, used by nothing else now.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            av_buffer_unref(&mut self.hw);
        }
    }
}

struct Hw(*mut AVBufferRef);
// SAFETY: an `AVBufferRef` of the thread's own; ffmpeg counts them atomically.
unsafe impl Send for Hw {}

/// A decoded picture per lens track, still on the GPU, and when in the
/// clip it is. Freed (back to the decoder's pool) when dropped.
pub struct Pair(pub [*mut AVFrame; 2], pub Stamp);

/// A picture's time as the file has it: in ticks of `num / den` seconds,
/// in a track of `rate` frames a second.
#[derive(Clone, Copy)]
pub struct Stamp {
    pub pts: i64,
    pub num: i64,
    pub den: i64,
    pub rate: f64,
}

impl Stamp {
    /// The output frame, at `fps`, it first shows in, counted from the
    /// picture stamped `first`: as the pipe path's filters place it
    /// (`ffmpeg::Out::retime`: shifted back half a source frame, then the
    /// `fps` filter rounding up), in their arithmetic, so the two paths
    /// show the same pictures in the same frames.
    pub fn frame(&self, first: &Stamp, fps: f64) -> i64 {
        // setpts: a double, rounded to a tick.
        let shifted = ((self.pts - first.pts) as f64 - 0.5 / self.rate * self.den as f64 / self.num as f64).round_ties_even() as i64;
        if (fps - fps.round()).abs() < 1e-9 {
            // A whole rate: ticks to frames exactly, rounded up.
            let (a, b) = (shifted as i128 * self.num as i128 * fps.round() as i128, self.den as i128);
            (a + b - 1).div_euclid(b) as i64
        } else {
            (shifted as f64 * self.num as f64 / self.den as f64 * fps).ceil() as i64
        }
    }
}
// SAFETY: the frames are references of this value's own.
unsafe impl Send for Pair {}

impl Pair {
    /// The pictures' size.
    pub fn size(&self) -> (u32, u32) {
        // SAFETY: a frame the decoder returned.
        unsafe { ((*self.0[0]).width as u32, (*self.0[0]).height as u32) }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        for f in &mut self.0 {
            // SAFETY: frames `decode` allocated and handed over.
            unsafe { av_frame_free(f) };
        }
    }
}

/// The decode loop of `Shared::stream`.
///
/// # Safety
/// `hw` is a valid Vulkan `AVHWDeviceContext` reference.
unsafe fn decode(path: &CStr, wanted: &[u32], start: f64, hw: *mut AVBufferRef, tx: &std::sync::mpsc::SyncSender<Result<Pair>>) -> Result<()> {
    // SAFETY: libav's decode loop as in `probe`; every pointer is one it
    // returned and is freed below or handed over in a `Pair`.
    unsafe {
        let mut fmt = null_mut();
        check(avformat_open_input(&mut fmt, path.as_ptr(), null(), null_mut()), "opening the clip")?;
        check(avformat_find_stream_info(fmt, null_mut()), "reading the clip's streams")?;
        let streams = std::slice::from_raw_parts((*fmt).streams, (*fmt).nb_streams as usize);
        // Track n is the nth video stream.
        let video: Vec<usize> = (0..streams.len()).filter(|&i| (*(*streams[i]).codecpar).codec_type == AVMediaType::AVMEDIA_TYPE_VIDEO).collect();
        let mut tracks = Vec::new();
        for &n in wanted {
            let i = *video.get(n as usize).context("no such video track")?;
            let par = (*streams[i]).codecpar;
            let codec = avcodec_find_decoder((*par).codec_id);
            let ctx = avcodec_alloc_context3(codec);
            check(avcodec_parameters_to_context(ctx, par), "setting up a decoder")?;
            (*ctx).hw_device_ctx = av_buffer_ref(hw);
            (*ctx).opaque = AVPixelFormat::AV_PIX_FMT_VULKAN as usize as *mut _;
            (*ctx).get_format = Some(pick);
            // Pictures held while they're copied and queued.
            (*ctx).extra_hw_frames = 8;
            check(avcodec_open2(ctx, codec, null_mut()), "opening a decoder")?;
            tracks.push((i as c_int, ctx, std::collections::VecDeque::<(*mut AVFrame, Stamp)>::new()));
        }
        // The position as the ffmpeg program reads `-ss` (six decimals of
        // what the pipe path prints), from the file's own start.
        let seek_us = {
            let text = start.to_string();
            let (whole, part) = text.split_once('.').unwrap_or((&text, ""));
            let micros: String = part.chars().chain(std::iter::repeat('0')).take(6).collect();
            whole.parse::<i64>().unwrap_or(0) * 1_000_000 + micros.parse::<i64>().unwrap_or(0) + if (*fmt).start_time == i64::MIN { 0 } else { (*fmt).start_time }
        };
        if start > 0.0 {
            check(av_seek_frame(fmt, -1, seek_us, 1), "seeking")?; // AVSEEK_FLAG_BACKWARD
        }
        let mut packet = av_packet_alloc();
        let mut frame = av_frame_alloc();
        let mut ended = false;
        let mut result = Ok(());
        'read: while !ended {
            ended = av_read_frame(fmt, packet) < 0;
            for (stream, ctx, ready) in tracks.iter_mut() {
                if !ended && (*packet).stream_index != *stream {
                    continue;
                }
                let base = (*streams[*stream as usize]).time_base;
                let sent = avcodec_send_packet(*ctx, if ended { null() } else { packet });
                if sent < 0 && sent != EOF {
                    result = check(sent, "decoding").map(|_| ());
                    break 'read;
                }
                loop {
                    let got = avcodec_receive_frame(*ctx, frame);
                    if got == AGAIN || got == EOF {
                        break;
                    }
                    if got < 0 {
                        result = check(got, "decoding").map(|_| ());
                        break 'read;
                    }
                    // From the first frame at or after `start`, as the ffmpeg
                    // program's `-ss` keeps them: the position in whole
                    // microseconds, rounded to the track's ticks.
                    let from = {
                        let (a, b) = (seek_us as i128 * base.den as i128, base.num as i128 * 1_000_000);
                        ((2 * a + b).div_euclid(2 * b)) as i64
                    };
                    if (*frame).best_effort_timestamp < from {
                        av_frame_unref(frame);
                        continue;
                    }
                    let kept = av_frame_alloc();
                    av_frame_move_ref(kept, frame);
                    let rate = (*streams[*stream as usize]).r_frame_rate;
                    ready.push_back((kept, Stamp { pts: (*kept).best_effort_timestamp, num: base.num as i64, den: base.den as i64, rate: rate.num as f64 / rate.den.max(1) as f64 }));
                }
            }
            av_packet_unref(packet);
            while tracks.iter().all(|t| !t.2.is_empty()) {
                let mut pair = [null_mut(); 2];
                // Timed by the first track's picture.
                let at = tracks[0].2[0].1;
                for (k, t) in tracks.iter_mut().enumerate().take(2) {
                    pair[k] = t.2.pop_front().unwrap().0;
                }
                if tracks.len() == 1 {
                    pair[1] = av_frame_clone(pair[0]);
                }
                if tx.send(Ok(Pair(pair, at))).is_err() {
                    break 'read;
                }
            }
        }
        for (_, ctx, ready) in tracks.iter_mut() {
            for (f, _) in ready.iter_mut() {
                av_frame_free(f);
            }
            avcodec_free_context(ctx);
        }
        av_frame_free(&mut frame);
        av_packet_free(&mut packet);
        avformat_close_input(&mut fmt);
        result
    }
}
