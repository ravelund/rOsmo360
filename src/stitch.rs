//! CPU reference stitcher: a telemetry-parser `CameraRig` -> equirectangular.
//! This is the ground truth the GPU shader will later be checked against.
//!
//! Conventions are video-types' geometry module: rig axes x right, y down,
//! z forward; `Lens::rotation` is rig-from-camera; blend ramps are per lens.

use crate::align::{Aligner, Field};
use crate::exposure::Gains;
use anyhow::{Result, bail};
use rayon::prelude::*;
use telemetry_parser::rig::geometry::{self, LensModel, MaskPolarity, Quat};

pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// RGB, 8 bits a sample; or, `deep`, 16 (little-endian): a 10-bit
    /// export's frames from the GPU, through crossfades and overlays to the
    /// encoder.
    pub rgb: Vec<u8>,
    pub deep: bool,
}

#[derive(Clone)]
pub struct Lens {
    /// Index into the rig's pictures, i.e. which decoded video track.
    pub input: usize,
    pub(crate) cam_from_rig: [[f64; 3]; 3],
    pub(crate) k: [f64; 5],
    pub(crate) p: [f64; 2],
    pub(crate) focal: [f64; 2],
    pub(crate) centre: [f64; 2],
    pub(crate) skew: f64,
    pub(crate) mirror: bool,
    pub(crate) calib_to_picture: geometry::Affine2,
    pub(crate) fov_limit: f64,
    pub(crate) blend: geometry::Blend,
    pub(crate) calib_dim: [usize; 2],
    /// How far each calibration pixel lies inside the valid picture (image
    /// circle and masks): 0 outside, rising to 255 at `FEATHER` in.
    pub(crate) edge: Vec<u8>,
}

/// A lens's blend weight fades out over this angle inside its masks. DJI's
/// masks around the selfie stick cut into the ±6.8° blend band (the overlap
/// there narrows to ~86-94°), so without it a lens stopped at a mask edge
/// mid-blend, and where the lenses see the sky differently (sun glare in one)
/// that step showed as a line: on clip 0043 at 5.9 s, one running up the sky
/// from the hand on the stick.
const FEATHER: f64 = 2.0 * std::f64::consts::PI / 180.0;

impl Lens {
    pub fn new(lens: &geometry::Lens) -> Result<Self> {
        let LensModel::ThetaPoly { k, p } = lens.intrinsics.model else {
            bail!("unsupported lens model {:?}", lens.intrinsics.model);
        };
        let calib_dim = [lens.intrinsics.calib_dim[0] as usize, lens.intrinsics.calib_dim[1] as usize];
        Ok(Lens {
            input: lens.picture.input as usize,
            cam_from_rig: transpose(quat_to_mat(lens.rotation)),
            k,
            p,
            focal: lens.intrinsics.focal,
            centre: lens.intrinsics.centre,
            skew: lens.intrinsics.skew,
            mirror: lens.mirror,
            calib_to_picture: lens.calib_to_picture,
            fov_limit: lens.fov_limit.unwrap_or(std::f64::consts::FRAC_PI_2),
            blend: lens.blend,
            calib_dim,
            edge: feather(&rasterise_validity(lens, calib_dim), calib_dim, lens.intrinsics.focal[0] * FEATHER),
        })
    }

    /// Projects a rig-frame direction to picture pixels, plus its angle from
    /// the optical axis and how far inside the masks it is (0..1, for the
    /// blend weight). `None` where the lens has no valid picture.
    pub(crate) fn project(&self, d: [f64; 3]) -> Option<(f64, f64, f64, f64)> {
        let c = apply(self.cam_from_rig, d);
        let rho = (c[0] * c[0] + c[1] * c[1]).sqrt();
        let theta = rho.atan2(c[2]);
        if theta > self.fov_limit {
            return None;
        }
        let t2 = theta * theta;
        let k = &self.k;
        let theta_d = theta * (1.0 + t2 * (k[0] + t2 * (k[1] + t2 * (k[2] + t2 * (k[3] + t2 * k[4])))));
        let (x, y) = if rho > 1e-12 { (theta_d * c[0] / rho, theta_d * c[1] / rho) } else { (0.0, 0.0) };
        let td2 = theta_d * theta_d;
        let [p1, p2] = self.p;
        let xd = x + 2.0 * p1 * x * y + p2 * (td2 + 2.0 * x * x);
        let yd = y + p1 * (td2 + 2.0 * y * y) + 2.0 * p2 * x * y;
        let mut u = self.focal[0] * xd + self.skew * yd + self.centre[0];
        let v = self.focal[1] * yd + self.centre[1];
        if self.mirror {
            u = 2.0 * self.centre[0] - u;
        }
        let (iu, iv) = (u.floor(), v.floor());
        if iu < 0.0 || iv < 0.0 || iu >= self.calib_dim[0] as f64 || iv >= self.calib_dim[1] as f64 {
            return None;
        }
        let edge = self.edge[iv as usize * self.calib_dim[0] + iu as usize];
        if edge == 0 {
            return None;
        }
        let a = &self.calib_to_picture;
        let pu = a.a[0][0] * u + a.a[0][1] * v + a.t[0];
        let pv = a.a[1][0] * u + a.a[1][1] * v + a.t[1];
        Some((pu, pv, theta, edge as f64 / 255.0))
    }

    /// RGB (0..255) at picture pixel (u, v) of this lens's frame, which may
    /// be a scaled-down copy (the LRF proxy) of the full-size picture.
    pub(crate) fn sample(&self, frames: &[Frame], u: f64, v: f64) -> [f64; 3] {
        let f = &frames[self.input];
        // The picture may be a scaled copy of calib_dim (1920 px lenses: calib_to_picture 0.5), so scale
        // against the picture's width, not the calibration's.
        let s = f.width as f64 / (self.calib_dim[0] as f64 * self.calib_to_picture.a[0][0]);
        sample(f, u * s, v * s)
    }
}

/// Per-frame seam corrections: exposure matching and close-subject alignment.
#[derive(Default)]
pub struct Seam<'a> {
    pub exposure: Option<(&'a crate::exposure::Seam, &'a Gains)>,
    pub align: Option<(&'a Aligner, &'a Field)>,
    /// Cut instead of blending where the lenses disagree.
    pub cut: bool,
}

/// Feathered composite: each lens weighted by its own blend ramp, normalised.
/// `only` forces a single lens (for checking alignment in the overlap zone).
///
/// `views` holds `rig_from_view` at instants spread evenly over the frame's
/// readout. With more than one, each ray is re-aimed with the rotation at the
/// sensor row it lands on (rolling-shutter correction, top-down readout).
pub fn equirect(lenses: &[Lens], frames: &[Frame], out_w: usize, only: Option<usize>, views: &[M3], seam: &Seam<'_>) -> Frame {
    use std::f64::consts::PI;
    let out_h = out_w / 2;
    let ray = |x: f64, y: f64| {
        let lat = PI / 2.0 - y / out_h as f64 * PI;
        let lon = x / out_w as f64 * 2.0 * PI - PI;
        [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()]
    };
    render(lenses, frames, (out_w, out_h), ray, only, views, seam)
}

/// Stitches any picture: `ray(x, y)` gives the view-space direction through
/// pixel centre (x, y), which `views` then turn into the rig.
/// Where the lenses still disagree after alignment (a hand close to the
/// stick, which one lens sees as an arm running into its mask and the other
/// as a fist), blending shows both at half strength; cut to whichever lens
/// owns that side of the seam instead. The cut fades in between these
/// disagreements (largest RGB difference, 0..1) and sharpens the blend to a
/// step this wide around the seam's middle. On clip 0043 it turned the
/// see-through fist on the stick solid and left the sky seam unchanged; it
/// touched ~1.4% of the sphere. The shader's `cut_share` must match.
const CUT_FROM: f64 = 0.06;
const CUT_FULL: f64 = 0.18;
const CUT_EDGE: f64 = 0.08;

/// Lens 0's share of a two-lens blend once disagreement cuts it, `strength`
/// (0..1) being how far the cut may act there (`Field::cut_at`).
pub(crate) fn cut_share(share: f64, a: [f64; 3], b: [f64; 3], strength: f64) -> f64 {
    let diff = (0..3).map(|c| (a[c] - b[c]).abs()).fold(0.0, f64::max);
    let cut = smoothstep(0.5 - CUT_EDGE, 0.5 + CUT_EDGE, share);
    share + (cut - share) * smoothstep(CUT_FROM, CUT_FULL, diff) * strength
}

fn smoothstep(lo: f64, hi: f64, x: f64) -> f64 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub fn render(
    lenses: &[Lens],
    frames: &[Frame],
    (out_w, out_h): (usize, usize),
    ray: impl Fn(f64, f64) -> [f64; 3] + Sync,
    only: Option<usize>,
    views: &[M3],
    seam: &Seam<'_>,
) -> Frame {
    let mut rgb = vec![0u8; out_w * out_h * 3];
    rgb.par_chunks_mut(out_w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..out_w {
            let d = ray(x as f64 + 0.5, y as f64 + 0.5);
            let mid = apply(views[views.len() / 2], d);
            let mut hits: Vec<([f64; 3], f64)> = Vec::with_capacity(2);
            for (i, lens) in lenses.iter().enumerate() {
                let warp = |d: [f64; 3]| seam.align.map_or(d, |(a, f)| a.warp(f, i, d));
                let Some(mut hit) = lens.project(warp(mid)) else { continue };
                if views.len() > 1 {
                    let row = (hit.1 / lens.calib_dim[1] as f64).clamp(0.0, 1.0);
                    let at = views[(row * (views.len() - 1) as f64).round() as usize];
                    let Some(h) = lens.project(warp(apply(at, d))) else { continue };
                    hit = h;
                }
                let (u, v, theta, edge) = hit;
                let w = match only {
                    Some(o) if o == i => 1.0,
                    Some(_) => 0.0,
                    None => lens.blend.weight(theta) * edge,
                };
                if w <= 0.0 {
                    continue;
                }
                let px = lens.sample(frames, u, v);
                let gain = seam.exposure.map_or([1.0; 3], |(s, g)| s.gain(g, i, mid, theta));
                hits.push(([0, 1, 2].map(|c| px[c] * gain[c]), w));
            }
            if let ([(a, wa), (b, wb)], true) = (&hits[..], seam.cut) {
                let strength = seam.align.map_or(1.0, |(al, f)| f.cut_at(al.ring.coords(mid).0));
                let s = cut_share(wa / (wa + wb), a.map(|v| v / 255.0), b.map(|v| v / 255.0), strength);
                hits = vec![(*a, s), (*b, 1.0 - s)];
            }
            let wsum: f64 = hits.iter().map(|h| h.1).sum();
            if wsum > 0.0 {
                for c in 0..3 {
                    let v = hits.iter().map(|(px, w)| px[c] * w).sum::<f64>() / wsum;
                    row[x * 3 + c] = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    });
    Frame { width: out_w, height: out_h, rgb, deep: false }
}

/// Image circle and include polygons are unioned, exclude polygons subtracted;
/// with neither a circle nor an include, the whole frame is valid.
fn rasterise_validity(lens: &geometry::Lens, [w, h]: [usize; 2]) -> Vec<bool> {
    let includes: Vec<_> = lens.masks.iter().filter(|m| m.polarity == MaskPolarity::Include).collect();
    let excludes: Vec<_> = lens.masks.iter().filter(|m| m.polarity != MaskPolarity::Include).collect();
    let open = includes.is_empty() && lens.image_circle.is_none();
    let mut valid = vec![open; w * h];
    valid.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let py = y as f64 + 0.5;
        if let Some(c) = &lens.image_circle {
            for (x, v) in row.iter_mut().enumerate() {
                let (dx, dy) = (x as f64 + 0.5 - c.centre[0], py - c.centre[1]);
                *v |= dx * dx + dy * dy <= c.radius * c.radius;
            }
        }
        for m in &includes {
            fill_row(row, &m.points, py, true);
        }
        for m in &excludes {
            fill_row(row, &m.points, py, false);
        }
    });
    valid
}

/// The validity map as distances in from its edge: 0 outside, 255 at
/// `radius` pixels or more, smoothstepped so the weight eases in. An exact
/// Euclidean distance (Felzenszwalb and Huttenlocher's separable transform),
/// rows then columns, each in parallel: ~0.05 s for two 3840² lenses, where a
/// one-thread chamfer pass took 0.6 s of every clip's load.
fn feather(valid: &[bool], [w, h]: [usize; 2], radius: f64) -> Vec<u8> {
    const FAR: f32 = 1e12;
    let mut rows: Vec<f32> = valid.iter().map(|&v| if v { FAR } else { 0.0 }).collect();
    rows.par_chunks_mut(w).for_each(squared_distance);
    let mut cols = vec![0f32; w * h];
    cols.par_chunks_mut(h).enumerate().for_each(|(x, col)| {
        for (y, c) in col.iter_mut().enumerate() {
            *c = rows[y * w + x];
        }
        squared_distance(col);
    });
    let mut edge = vec![0u8; w * h];
    edge.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, e) in row.iter_mut().enumerate() {
            let d = cols[x * h + y];
            *e = if d == 0.0 { 0 } else { (smoothstep(0.0, radius, (d as f64).sqrt()) * 254.0).round() as u8 + 1 };
        }
    });
    edge
}

/// In place, each sample's squared distance to the nearest one at 0, given
/// squared distances so far (1-D lower envelope of parabolas).
fn squared_distance(f: &mut [f32]) {
    let n = f.len();
    let src = f.to_vec();
    let (mut v, mut z) = (vec![0usize; n], vec![0f32; n + 1]);
    let mut k = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;
    let cross = |q: usize, p: usize| ((src[q] + (q * q) as f32) - (src[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
    for q in 1..n {
        let mut s = cross(q, v[k]);
        while s <= z[k] {
            k -= 1;
            s = cross(q, v[k]);
        }
        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }
    k = 0;
    for (q, out) in f.iter_mut().enumerate() {
        while z[k + 1] < q as f32 {
            k += 1;
        }
        let d = q as f32 - v[k] as f32;
        *out = d * d + src[v[k]];
    }
}

/// Even-odd scanline fill of one polygon on row centre `py`, half-open edges.
fn fill_row(row: &mut [bool], pts: &[[f64; 2]], py: f64, value: bool) {
    let mut xs: Vec<f64> = (0..pts.len())
        .filter_map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
            ((a[1] <= py) != (b[1] <= py)).then(|| a[0] + (py - a[1]) / (b[1] - a[1]) * (b[0] - a[0]))
        })
        .collect();
    xs.sort_by(f64::total_cmp);
    for pair in xs.chunks_exact(2) {
        let x0 = (pair[0] - 0.5).ceil().max(0.0) as usize;
        let x1 = ((pair[1] - 0.5).floor() + 1.0).clamp(0.0, row.len() as f64) as usize;
        for v in row.iter_mut().take(x1).skip(x0) {
            *v = value;
        }
    }
}

/// An ordinary video's `frame` (upright) as `camera` crops it, `(w, h)`
/// pixels, black off its edge. The shader's flat branch must match.
pub fn flat(frame: &Frame, (w, h): (usize, usize), camera: &crate::view::Camera) -> Frame {
    let (aspect, picture) = (w as f64 / h as f64, frame.width as f64 / frame.height as f64);
    let mut rgb = vec![0u8; w * h * 3];
    rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let ndc = [(x as f64 + 0.5) / w as f64 * 2.0 - 1.0, (y as f64 + 0.5) / h as f64 * 2.0 - 1.0];
            if let Some([u, v]) = camera.flat_uv(aspect, picture, ndc) {
                let px = sample(frame, u * frame.width as f64, v * frame.height as f64);
                for c in 0..3 {
                    row[x * 3 + c] = px[c].round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    });
    Frame { width: w, height: h, rgb, deep: false }
}

fn sample(f: &Frame, u: f64, v: f64) -> [f64; 3] {
    let x0 = (u - 0.5).floor();
    let y0 = (v - 0.5).floor();
    let (fx, fy) = (u - 0.5 - x0, v - 0.5 - y0);
    let px = |x: f64, y: f64| -> [f64; 3] {
        let x = (x as isize).clamp(0, f.width as isize - 1) as usize;
        let y = (y as isize).clamp(0, f.height as isize - 1) as usize;
        let i = (y * f.width + x) * 3;
        [f.rgb[i] as f64, f.rgb[i + 1] as f64, f.rgb[i + 2] as f64]
    };
    let (a, b, c, d) = (px(x0, y0), px(x0 + 1.0, y0), px(x0, y0 + 1.0), px(x0 + 1.0, y0 + 1.0));
    std::array::from_fn(|i| {
        let top = a[i] + (b[i] - a[i]) * fx;
        let bot = c[i] + (d[i] - c[i]) * fx;
        top + (bot - top) * fy
    })
}

pub type M3 = [[f64; 3]; 3];

pub const IDENTITY: M3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn mat_mul(a: M3, b: M3) -> M3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

pub fn quat_to_mat(q: Quat) -> M3 {
    let Quat { w, x, y, z } = q;
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

pub fn transpose(m: M3) -> M3 {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

pub(crate) fn apply(m: M3, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feather_is_the_euclidean_distance_in() {
        let (w, h) = (37, 23);
        // A disc with a notch and scattered holes, like a mask cutting in.
        let valid: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f64, (i / w) as f64);
                let disc = (x - 18.0).powi(2) + (y - 11.0).powi(2) < 120.0;
                disc && !(x > 20.0 && (y - 11.0).abs() < 2.0) && (i * 7919) % 53 != 0
            })
            .collect();
        let radius = 4.0;
        let edge = feather(&valid, [w, h], radius);
        for (i, &e) in edge.iter().enumerate() {
            let (x, y) = ((i % w) as f64, (i / w) as f64);
            let d = (0..w * h).filter(|&j| !valid[j]).map(|j| ((j % w) as f64 - x).hypot((j / w) as f64 - y)).fold(f64::INFINITY, f64::min);
            let want = if valid[i] { (smoothstep(0.0, radius, d) * 254.0).round() as u8 + 1 } else { 0 };
            assert_eq!(e, want, "at {x},{y}");
        }
    }
}
