//! Saving the paused frame as a PNG: either the view as framed, or the whole
//! sphere as an equirect. Rendered on the CPU from freshly decoded
//! full-resolution frames with the same stitch, stabilisation and seam
//! corrections as playback, so the size isn't tied to the window.

use super::gpu::{RS_STEPS_MAX, Rig};
use crate::ffmpeg;
use crate::stab::Stabilizer;
use crate::stitch::{self, apply};
use crate::view::Camera;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

pub enum Shot {
    /// What is on screen, at this many pixels.
    View { camera: Box<Camera>, size: (usize, usize) },
    /// The whole sphere, `width` × `width / 2`.
    Sphere { width: usize },
}

pub struct Request {
    pub input: PathBuf,
    /// An ordinary video: cropped as framed, or saved whole.
    pub flat: Option<ffmpeg::Flat>,
    pub hw: ffmpeg::Hwaccel,
    pub rig: Arc<Rig>,
    pub stab: Stabilizer,
    pub readout_ms: f64,
    pub t: f64,
    pub rolling_shutter: bool,
    pub shot: Shot,
    pub path: PathBuf,
}

pub fn save(r: Request) -> Result<PathBuf> {
    if let Some(flat) = r.flat {
        let (w, h) = flat.size();
        let frame = ffmpeg::still_out(&r.input, flat.track, r.t, r.hw, (w as usize, h as usize), flat.out(None, None))?;
        let out = match r.shot {
            Shot::Sphere { .. } => frame,
            Shot::View { camera, size } => {
                let mut out = stitch::flat(&frame, size, &camera);
                crate::grade_frame(&mut out, &camera);
                out
            }
        };
        image::save_buffer(&r.path, &out.rgb, out.width as u32, out.height as u32, image::ColorType::Rgb8).with_context(|| format!("writing {}", r.path.display()))?;
        return Ok(r.path);
    }
    let frames = r
        .rig
        .tracks
        .iter()
        .map(|&track| ffmpeg::still(&r.input, track, r.t, r.hw))
        .collect::<Result<Vec<_>>>()?;
    let steps = if r.rolling_shutter && r.readout_ms > 0.0 { RS_STEPS_MAX } else { 1 };
    let views = if r.stab.has_data() { r.stab.rig_from_view(r.t * 1000.0, r.readout_ms, steps) } else { vec![stitch::IDENTITY] };

    let rgb = |i: usize, u: f64, v: f64| r.rig.lenses[i].sample(&frames, u, v).map(|c| c / 255.0);
    let gains = r.rig.seam.as_ref().map(|s| s.measure(|i, u, v| Some(rgb(i, u, v))));
    let field = r.rig.aligner.as_ref().map(|a| a.measure(&a.strips(|i, u, v| Some(rgb(i, u, v).iter().sum::<f64>() / 3.0)), None));
    let seam = stitch::Seam {
        exposure: r.rig.seam.as_ref().zip(gains.as_ref()),
        align: r.rig.aligner.as_ref().zip(field.as_ref()),
        cut: true,
    };

    let out = match r.shot {
        Shot::Sphere { width } => stitch::equirect(&r.rig.lenses, &frames, width, None, &views, &seam),
        Shot::View { camera, size: (w, h) } => {
            let ray = |x: f64, y: f64| apply(camera.user, camera.ray(w as f64 / h as f64, [x / w as f64 * 2.0 - 1.0, y / h as f64 * 2.0 - 1.0]));
            // With its LUT and effects, as the screen shows it: a D-Log M
            // cut was saved flat. The whole sphere stays as filmed.
            let mut out = stitch::render(&r.rig.lenses, &frames, (w, h), ray, None, &views, &seam);
            crate::grade_frame(&mut out, &camera);
            out
        }
    };
    image::save_buffer(&r.path, &out.rgb, out.width as u32, out.height as u32, image::ColorType::Rgb8).with_context(|| format!("writing {}", r.path.display()))?;
    Ok(r.path)
}
