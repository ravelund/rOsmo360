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
                    let got = avcodec_receive_frame_flags(t.ctx, frame, 0);
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
