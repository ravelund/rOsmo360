//! Frame sources for the player: NV12 pictures streamed from ffmpeg on
//! background threads, either the LRF proxy (both lenses side by side, cheap
//! to decode) or the OSV lens tracks, on whichever hardware decoder works.

use crate::ffmpeg::{self, Hwaccel};
use crate::stitch::Lens;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// Whether 10-bit tracks (the OSV's, a phone's HDR video) are decoded at
/// 10 bits (P010) rather than cut to 8 (NV12). One switch for the run: the
/// player's setting, or `ROSMO360_10BIT=1` for the command line, to compare
/// the two while the 10-bit pipeline is new.
static DEEP: AtomicBool = AtomicBool::new(false);

pub fn set_deep(on: bool) {
    DEEP.store(on, Ordering::Relaxed);
}

pub fn deep() -> bool {
    DEEP.load(Ordering::Relaxed) || std::env::var_os("ROSMO360_10BIT").is_some_and(|v| v == "1")
}

/// One NV12 picture: Y plane then interleaved UV at half resolution; or,
/// `deep`, P010: the same with 16-bit little-endian samples, the 10 bits
/// in the high ones.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub deep: bool,
    pub data: Vec<u8>,
}

/// Picture buffers given back by pictures done with, for the decode threads
/// to read the next frames into, rather than a fresh one each frame (two
/// 3840² lenses are 44 MB at 8 bits, 88 at 10), whose pages the kernel
/// zeroes first. A full-res GPU export of 10 s of 0037 took 12.9 s instead
/// of 13.25 at 8 bits, 18.6 instead of 18.9 at 10.
static SPARE: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// At most this many bytes kept spare: they stay held once playback or an
/// export stops (five 3840² lenses at 10 bits).
const SPARE_BYTES: usize = 256 << 20;

/// A buffer of `len` bytes, a spare if one fits. Its contents are whatever
/// was there: it's read into whole.
fn buffer(len: usize) -> Vec<u8> {
    let mut spare = SPARE.lock().unwrap();
    match spare.iter().position(|b| b.len() == len) {
        Some(i) => spare.swap_remove(i),
        None => vec![0; len],
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        let mut spare = SPARE.lock().unwrap();
        if spare.iter().map(Vec::len).sum::<usize>() + self.data.len() <= SPARE_BYTES {
            spare.push(std::mem::take(&mut self.data));
        }
    }
}

impl Picture {
    /// Bytes for a picture of `w` × `h`.
    pub fn bytes(w: u32, h: u32, deep: bool) -> usize {
        (w * h * 3 / 2) as usize * if deep { 2 } else { 1 }
    }
    fn split(&self) -> usize {
        (self.width * self.height) as usize * if self.deep { 2 } else { 1 }
    }
    pub fn y(&self) -> &[u8] {
        &self.data[..self.split()]
    }
    pub fn uv(&self) -> &[u8] {
        &self.data[self.split()..]
    }
    /// Sample `i` of `plane` on an 8-bit scale (0..255 as 0..1), whatever
    /// the depth: 10-bit 64 and 940 land where 8-bit 16 and 235 do.
    fn sample(&self, plane: &[u8], i: usize) -> f64 {
        if self.deep { u16::from_le_bytes([plane[2 * i], plane[2 * i + 1]]) as f64 / 65280.0 } else { plane[i] as f64 / 255.0 }
    }
}

/// How the decoded pictures relate to the lens tracks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Layout {
    /// One picture holding every track side by side in squares of its height.
    SideBySide,
    /// One picture per lens track.
    PerTrack,
}

pub struct Frame {
    pub pts: f64,
    pub layout: Layout,
    pub pictures: Vec<Picture>,
}

impl Frame {
    /// Display RGB (0..1) at `lens`'s full-resolution picture pixel, wherever
    /// this frame's layout and scale put it. Must match the shader's mapping.
    pub fn rgb_at(&self, lens: &Lens, track: u32, u: f64, v: f64) -> Option<[f64; 3]> {
        let (pic, off_x) = match self.layout {
            Layout::SideBySide => (self.pictures.first()?, track as f64 * self.pictures.first()?.height as f64),
            Layout::PerTrack => (self.pictures.get(track as usize)?, 0.0),
        };
        let cell = if self.layout == Layout::SideBySide { pic.height } else { pic.width } as f64;
        let s = cell / (lens.calib_dim[0] as f64 * lens.calib_to_picture.a[0][0]); // picture width, see stitch.rs
        let (x, y) = ((s * u + off_x) as i64, (s * v) as i64);
        if x < 0 || y < 0 || x >= pic.width as i64 || y >= pic.height as i64 {
            return None;
        }
        let [yy, cu, cv] = ycc(pic, x as usize, y as usize);
        // BT.709, limited range, as in the shader.
        Some([yy + 1.5748 * cv, yy - 0.1873 * cu - 0.4681 * cv, yy + 1.8556 * cu].map(|v| v.clamp(0.0, 1.0)))
    }

    /// An ordinary video's colour at `(u, v)` across its picture (0..1), as
    /// the shader shows it: an HDR one brought down to SDR.
    pub fn flat_rgb_at(&self, u: f64, v: f64, hdr: crate::hdr::Hdr) -> Option<[f64; 3]> {
        let pic = self.pictures.first()?;
        let x = ((u * pic.width as f64) as usize).min(pic.width as usize - 1);
        let y = ((v * pic.height as f64) as usize).min(pic.height as usize - 1);
        let [yy, cu, cv] = ycc(pic, x, y);
        if hdr == crate::hdr::Hdr::None {
            return Some([yy + 1.5748 * cv, yy - 0.1873 * cu - 0.4681 * cv, yy + 1.8556 * cu].map(|v| v.clamp(0.0, 1.0)));
        }
        // BT.2020's matrix, as the shader's `hdr_to_sdr`.
        let signal = [yy + 1.4746 * cv, yy - 0.16455 * cu - 0.57135 * cv, yy + 1.8814 * cu].map(|v| v as f32);
        Some(crate::hdr::to_sdr(signal, hdr).map(|v| v as f64))
    }
}

/// Y′, Cb and Cr at a pixel, Y′ 0..1 and Cb/Cr about 0, from video range.
fn ycc(pic: &Picture, x: usize, y: usize) -> [f64; 3] {
    let w = pic.width as usize;
    let c = (y / 2) * w + (x / 2) * 2;
    [
        (pic.sample(pic.y(), y * w + x) - 16.0 / 255.0) * (255.0 / 219.0),
        (pic.sample(pic.uv(), c) - 128.0 / 255.0) * (255.0 / 224.0),
        (pic.sample(pic.uv(), c + 1) - 128.0 / 255.0) * (255.0 / 224.0),
    ]
}

#[derive(Clone)]
pub enum Kind {
    Lrf(PathBuf),
    /// OSV tracks scaled to this many pixels square, or at their own size.
    Osv { path: PathBuf, tracks: Vec<u32>, size: Option<u32> },
    /// An ordinary video, upright, scaled to `size` or at its own, and at
    /// `rate` frames a second if given.
    Flat { path: PathBuf, flat: ffmpeg::Flat, size: Option<(u32, u32)>, rate: Option<f64> },
}

/// A running decode from `start`, delivering frames in order.
pub struct Stream {
    pub frames: Receiver<Frame>,
    /// Its decoders, killed as soon as it's dropped: left to the pump, one
    /// only stopped after its next frame, a second or more for an OSV
    /// starting up, and scrubbing piled them up until memory ran out.
    procs: Arc<Mutex<Vec<Child>>>,
    /// Why a decoder failed, set before `frames` closes: the frames simply
    /// stopping looked like the clip's end, and an export held the last one.
    pub error: Arc<Mutex<Option<String>>>,
    /// Dropped: its decoders were killed, which is no failure.
    dropped: Arc<AtomicBool>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
        kill(&self.procs);
    }
}

pub fn spawn(kind: Kind, hw: Hwaccel, start: f64, fps: f64) -> Result<Stream> {
    let (tx, rx) = sync_channel(3);
    let procs: Vec<(Child, u32, u32, bool)> = match &kind {
        Kind::Lrf(path) => {
            // The proxy is small H.264: software decodes it at 4-8x real time
            // on one to four threads, and starts in 0.15 s where setting up
            // Vulkan or CUDA takes 0.5-1 s, which made every scrub lag.
            let (w, h) = dims(path, 0)?;
            // The LRF is 25 fps while `pump` stamps frames at the clip's rate (100 fps on some
            // clips), so repeat its frames up to that rate.
            let (child, deep) = decode(path, 0, start, None, Hwaccel::Software, Some(fps))?;
            vec![(child, w, h, deep)]
        }
        Kind::Osv { path, tracks, size } => {
            tracks
                .iter()
                .map(|&t| {
                    let (w, h) = size.map_or_else(|| dims(path, t), |s| Ok((s, s)))?;
                    let (child, deep) = decode(path, t, start, *size, hw, Some(fps))?;
                    Ok((child, w, h, deep))
                })
                .collect::<Result<_>>()?
        }
        Kind::Flat { path, flat, size, rate } => {
            let (w, h) = size.unwrap_or_else(|| flat.size());
            let (mut cmd, deep) = ffmpeg::yuv(path, flat.track, start, flat.out(*size, *rate), hw, deep())?;
            cmd.args(["-f", "rawvideo", "-"]).stdout(Stdio::piped()).stderr(Stdio::piped());
            vec![(cmd.spawn().context("running ffmpeg")?, w, h, deep)]
        }
    };
    let layout = match &kind {
        Kind::Lrf(_) => Layout::SideBySide,
        Kind::Osv { .. } | Kind::Flat { .. } => Layout::PerTrack,
    };
    let (mut children, sizes): (Vec<Child>, Vec<(u32, u32, bool)>) = procs.into_iter().map(|(c, w, h, deep)| (c, (w, h, deep))).unzip();
    let outs: Vec<_> = children.iter_mut().map(|c| c.stdout.take().unwrap()).collect();
    let mut tails: Vec<_> = children.iter_mut().map(ffmpeg::Tail::of).collect();
    let procs = Arc::new(Mutex::new(children));
    let (error, dropped) = (Arc::new(Mutex::new(None)), Arc::new(AtomicBool::new(false)));
    let (decoders, failed, gone) = (procs.clone(), error.clone(), dropped.clone());
    thread::spawn(move || {
        if let Some(i) = pump(outs, sizes, layout, start, fps, &tx)
            && !gone.load(Ordering::Relaxed)
        {
            // The one that ended is exiting; the others are killed below.
            let status = decoders.lock().unwrap()[i].wait();
            if let Ok(status) = status
                && !status.success()
                && !gone.load(Ordering::Relaxed)
            {
                let why = tails[i].failure(status);
                eprintln!("{why}");
                *failed.lock().unwrap() = Some(why);
            }
        }
        kill(&decoders);
        drop(tx);
    });
    Ok(Stream { frames: rx, procs, error, dropped })
}

/// Reads whole frames from the decoders until one ends, giving its index,
/// or nobody wants more.
fn pump(mut outs: Vec<std::process::ChildStdout>, sizes: Vec<(u32, u32, bool)>, layout: Layout, start: f64, fps: f64, tx: &SyncSender<Frame>) -> Option<usize> {
    for n in 0.. {
        let mut pictures = Vec::with_capacity(sizes.len());
        for (i, (&(w, h, deep), out)) in sizes.iter().zip(outs.iter_mut()).enumerate() {
            let mut data = buffer(Picture::bytes(w, h, deep));
            if out.read_exact(&mut data).is_err() {
                return Some(i);
            }
            pictures.push(Picture { width: w, height: h, deep, data });
        }
        if tx.send(Frame { pts: start + n as f64 / fps, layout, pictures }).is_err() {
            return None;
        }
    }
    None
}

fn kill(procs: &Mutex<Vec<Child>>) {
    for c in procs.lock().unwrap().iter_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Decodes one frame of every OSV track, at full resolution or scaled to
/// `size` pixels square, falling back to software if the hardware decoder
/// fails.
pub fn still(path: &Path, tracks: &[u32], t: f64, size: Option<u32>, hw: Hwaccel) -> Result<Frame> {
    let pictures = tracks
        .iter()
        .map(|&track| {
            let (w, h) = size.map_or_else(|| dims(path, track), |s| Ok((s, s)))?;
            let read = |hw| -> Result<(Vec<u8>, bool)> {
                let (mut child, deep) = decode(path, track, t, size, hw, None)?;
                let mut data = vec![0; Picture::bytes(w, h, deep)];
                let read = child.stdout.take().unwrap().read_exact(&mut data);
                // Reaped even when the read failed, or it lingers a zombie.
                let _ = child.kill();
                let _ = child.wait();
                read?;
                Ok((data, deep))
            };
            let (data, deep) = read(hw).or_else(|_| read(Hwaccel::Software))?;
            Ok(Picture { width: w, height: h, deep, data })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Frame { pts: t, layout: Layout::PerTrack, pictures })
}

/// One frame of an ordinary video at its own size, upright, falling back to
/// software if the hardware decoder fails.
pub fn still_flat(path: &Path, flat: ffmpeg::Flat, t: f64, hw: Hwaccel) -> Result<Frame> {
    let (w, h) = flat.size();
    let read = |hw| -> Result<(Vec<u8>, bool)> {
        let (mut cmd, deep) = ffmpeg::yuv(path, flat.track, t, flat.out(None, None), hw, deep())?;
        let mut child = cmd.args(["-frames:v", "1", "-f", "rawvideo", "-"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("running ffmpeg")?;
        let mut data = vec![0; Picture::bytes(w, h, deep)];
        let read = child.stdout.take().unwrap().read_exact(&mut data);
        // Reaped even when the read failed, or it lingers a zombie.
        let _ = child.kill();
        let _ = child.wait();
        read?;
        Ok((data, deep))
    };
    let (data, deep) = read(hw).or_else(|_| read(Hwaccel::Software))?;
    Ok(Frame { pts: t, layout: Layout::PerTrack, pictures: vec![Picture { width: w, height: h, deep, data }] })
}

/// A lens track's decoder, and whether it delivers P010.
fn decode(path: &Path, track: u32, start: f64, size: Option<u32>, hw: Hwaccel, rate: Option<f64>) -> Result<(Child, bool)> {
    let (mut cmd, deep) = ffmpeg::yuv(path, track, start, ffmpeg::Out { size: size.map(|s| (s, s)), rate, ..Default::default() }, hw, deep())?;
    cmd.args(["-f", "rawvideo", "-"]).stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok((cmd.spawn().context("running ffmpeg")?, deep))
}

/// A track's picture size, asked of ffprobe once a run: each stream start
/// used to, which cost every cut change and scrub ~50 ms.
fn dims(path: &Path, track: u32) -> Result<(u32, u32)> {
    static KNOWN: std::sync::Mutex<Vec<(PathBuf, u32, (u32, u32))>> = std::sync::Mutex::new(Vec::new());
    if let Some(&(_, _, d)) = KNOWN.lock().unwrap().iter().find(|(p, t, _)| p == path && *t == track) {
        return Ok(d);
    }
    let (w, h) = ffmpeg::dimensions(path, track)?;
    KNOWN.lock().unwrap().push((path.to_owned(), track, (w as u32, h as u32)));
    Ok((w as u32, h as u32))
}
