//! Framing the sphere: a `View` (where to look, how wide, which projection),
//! and a `Reframe`, keyframed views over time. The player looks through a
//! `View`; `export` renders a `Reframe`; both turn into a `Camera` for drawing.
//!
//! Angles are in degrees. Yaw turns right, pitch looks up, roll tilts
//! clockwise. With stabilisation on, yaw 0 is straight ahead (`horizon`
//! follows the camera's heading, `level` and `lock` hold the clip's first
//! heading, so there yaw is a direction in the world), and
//! the equirect's longitude and latitude are the yaw and pitch to look there.

use crate::stitch::{M3, mat_mul};
use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Projection {
    Perspective,
    LittlePlanet,
    Equirect,
}

impl Projection {
    pub const ALL: [Projection; 3] = [Projection::Perspective, Projection::LittlePlanet, Projection::Equirect];

    /// How far between the perspective (0) and little-planet (1) mappings
    /// it starts at; a view's `correction` can then set any in between.
    fn planet(self) -> Option<f64> {
        match self {
            Projection::Perspective => Some(0.0),
            Projection::LittlePlanet => Some(1.0),
            Projection::Equirect => None,
        }
    }
}

impl fmt::Display for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Projection::Perspective => "Perspective",
            Projection::LittlePlanet => "Little planet",
            Projection::Equirect => "Equirect",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    /// Vertical field of view; for the little planet, how much of the
    /// sphere the picture's height spans.
    pub fov: f64,
    pub projection: Projection,
    /// Where between perspective (0) and little planet (1) the mapping is,
    /// as DJI Studio's "Correction angle": 0 keeps straight lines straight,
    /// 1 shows twice the angle at the same FOV, bending them. The
    /// projection picker sets it to its own; equirect ignores it.
    pub correction: f64,
}

/// Opens on the little planet, as DJI Studio does.
impl Default for View {
    fn default() -> Self {
        View::home(Projection::LittlePlanet)
    }
}

impl View {
    /// The starting view for a projection: the planet looks straight down.
    pub fn home(projection: Projection) -> View {
        match projection {
            Projection::LittlePlanet => View { yaw: 0.0, pitch: -90.0, roll: 0.0, fov: 120.0, projection, correction: 1.0 },
            _ => View { yaw: 0.0, pitch: 0.0, roll: 0.0, fov: 90.0, projection, correction: 0.0 },
        }
    }

    pub fn reset(&mut self) {
        *self = View::home(self.projection);
    }

    /// The planet zooms out until the whole sphere is a small disc, the way
    /// DJI Studio's tiny planet does; so may a perspective bent most of the
    /// way to it.
    pub fn fov_range(&self) -> (f64, f64) {
        if self.projection == Projection::LittlePlanet || self.correction > 0.5 { (10.0, 179.5) } else { (20.0, 150.0) }
    }

    /// The azimuthal mapping's d (`Camera::planet`), or `None` for equirect.
    fn planet(&self) -> Option<f64> {
        self.projection.planet().map(|_| self.correction)
    }

    /// How wide a picture `aspect` wide for its height shows, side to side,
    /// in degrees: DJI Studio's "Zoom". `None` for equirect.
    pub fn across(&self, aspect: f64) -> Option<f64> {
        let d = self.planet()?;
        let s = (self.fov.to_radians() / 2.0).tan() * aspect;
        // As `Camera::ray`: s = sin θ / (d + cos θ) solved for θ.
        Some(2.0 * (s.atan() + (s * d / (1.0 + s * s).sqrt()).asin()).to_degrees())
    }

    /// Zooms by `steps` scroll notches, each scaling the picture by 10%:
    /// near the planet's limit a fixed step in degrees would barely move.
    pub fn zoom_by(&mut self, steps: f64) {
        let (lo, hi) = self.fov_range();
        let half = ((self.fov.to_radians() / 2.0).tan() * 1.1f64.powf(steps)).atan();
        self.fov = (2.0 * half.to_degrees()).clamp(lo, hi);
    }

    pub fn camera(&self) -> Camera {
        let crop = Crop {
            centre: [(self.yaw / FLAT_DEGREES) as f32, (-self.pitch / FLAT_DEGREES) as f32],
            half: (self.fov / FLAT_DEGREES / 2.0) as f32,
            roll: self.roll.to_radians() as f32,
        };
        Camera { user: self.matrix(), zoom: (self.fov.to_radians() / 2.0).tan(), planet: self.planet(), grade: Default::default(), lut: None, crop }
    }

    /// view_from_screen-ray: yaw about +y, then pitch about +x, then roll.
    fn matrix(&self) -> M3 {
        let (sy, cy) = self.yaw.to_radians().sin_cos();
        let (sp, cp) = self.pitch.to_radians().sin_cos();
        let (sr, cr) = self.roll.to_radians().sin_cos();
        let ry = [[cy, 0.0, sy], [0.0, 1.0, 0.0], [-sy, 0.0, cy]];
        let rx = [[1.0, 0.0, 0.0], [0.0, cp, -sp], [0.0, sp, cp]];
        let rz = [[cr, -sr, 0.0], [sr, cr, 0.0], [0.0, 0.0, 1.0]];
        mat_mul(ry, mat_mul(rx, rz))
    }
}

/// On an ordinary video a view's angles become a crop of the picture, so
/// keyframes, eases, dragging and scrolling work as on a 360 clip: this many
/// degrees of yaw or pitch move by one picture height, and a field of view
/// this wide shows the whole height (as the home view's 90° does).
pub const FLAT_DEGREES: f64 = 90.0;

/// The part of an ordinary video a view shows, in picture heights.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Crop {
    /// Where the view's centre is from the picture's centre, x right, y down.
    pub centre: [f32; 2],
    /// Half the view's height.
    pub half: f32,
    /// The view's roll, radians.
    pub roll: f32,
}

/// A view ready to draw.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    /// view_from_screen-ray rotation.
    pub user: M3,
    /// Tangent of half the vertical field of view.
    pub zoom: f64,
    /// `Some(d)`: an azimuthal mapping from perspective (0) to little planet
    /// (1); in between is how one morphs into the other. `None`: equirect.
    pub planet: Option<f64>,
    /// The colour effects on what it shows.
    pub grade: crate::grade::Grade,
    /// The LUT applied before them (`lut::shared`'s id), as much of it as
    /// the grade's `lut` says.
    pub lut: Option<usize>,
    /// What it shows of an ordinary video instead.
    pub crop: Crop,
}

impl Camera {
    /// The view-space ray through screen point `ndc` (−1..1 each way, y down).
    /// The shader's `screen_ray` must match.
    ///
    /// The azimuthal mappings put the ray at angle θ from the view axis where
    /// s = sin θ / (d + cos θ), s being the distance from the centre in units
    /// of `zoom`: d = 0 is perspective (s = tan θ), d = 1 stereographic
    /// (s = tan θ/2), so blending d morphs one into the other smoothly.
    pub fn ray(&self, aspect: f64, [nx, ny]: [f64; 2]) -> [f64; 3] {
        let Some(d) = self.planet else {
            let (lon, lat) = (nx * std::f64::consts::PI, -ny * std::f64::consts::FRAC_PI_2);
            return [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()];
        };
        let q = [nx * aspect * self.zoom, ny * self.zoom];
        let s = (q[0] * q[0] + q[1] * q[1]).sqrt().max(1e-12);
        let th = s.atan() + (s * d / (1.0 + s * s).sqrt()).asin();
        [th.sin() * q[0] / s, th.sin() * q[1] / s, th.cos()]
    }

    /// Where screen point `ndc` falls in an ordinary video `picture` wide for
    /// its height (0..1 each way), or `None` off its edge. The shader's
    /// `flat_uv` must match.
    pub fn flat_uv(&self, aspect: f64, picture: f64, [nx, ny]: [f64; 2]) -> Option<[f64; 2]> {
        let c = &self.crop;
        let (half, centre) = (c.half as f64, c.centre.map(|v| v as f64));
        let q = [nx * aspect * half, ny * half];
        let (s, k) = (c.roll as f64).sin_cos();
        let p = [k * q[0] - s * q[1], s * q[0] + k * q[1]];
        let uv = [0.5 + (centre[0] + p[0]) / picture, 0.5 + centre[1] + p[1]];
        uv.iter().all(|v| (0.0..=1.0).contains(v)).then_some(uv)
    }
}

/// A picture shape, written "16:9" in JSON and on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Aspect(pub u32, pub u32);

impl Aspect {
    /// The size with `short` pixels on the short side, both sides even so
    /// encoders that halve chroma accept it.
    pub fn size(self, short: u32) -> [u32; 2] {
        let long = |a: u32, b: u32| ((short as f64 * a as f64 / b as f64 / 2.0).round() as u32).max(8) * 2;
        if self.0 >= self.1 { [long(self.0, self.1), short] } else { [short, long(self.1, self.0)] }
    }
}

impl std::str::FromStr for Aspect {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (w, h) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("aspect {s:?} should look like 16:9"))?;
        let (w, h): (u32, u32) = (w.trim().parse()?, h.trim().parse()?);
        if w == 0 || h == 0 || w.max(h) > 8 * w.min(h) {
            bail!("aspect {s:?} is out of range");
        }
        Ok(Aspect(w, h))
    }
}

impl TryFrom<String> for Aspect {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<Aspect> for String {
    fn from(a: Aspect) -> String {
        format!("{}:{}", a.0, a.1)
    }
}

/// How a keyframe moves on to the next one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ease {
    /// Eases out of this keyframe and into the next.
    #[default]
    Smooth,
    /// Constant speed.
    Linear,
    /// Stays put, then cuts to the next keyframe.
    Hold,
}

impl Ease {
    pub const ALL: [Ease; 3] = [Ease::Smooth, Ease::Linear, Ease::Hold];

    /// How far along the move from this keyframe to the next it is at
    /// `x` (0..1) of the time between them; `None` while it holds.
    pub fn shape(self, x: f64) -> Option<f64> {
        match self {
            Ease::Smooth => Some(x * x * (3.0 - 2.0 * x)),
            Ease::Linear => Some(x),
            Ease::Hold => None,
        }
    }
}

impl fmt::Display for Ease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Ease::Smooth => "Smooth",
            Ease::Linear => "Linear",
            Ease::Hold => "Hold",
        })
    }
}

/// What a keyframe can follow instead of looking at fixed angles. In JSON
/// it is `"stick"` or a track file's path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Follow {
    /// The selfie stick, and so whoever holds it: it runs down the rig's
    /// +y, so the camera's attitude says where it points, with no picture
    /// analysis, even straight below the camera.
    Stick,
    /// A subject tracked by colour (`rosmo360 track`): the CSV it wrote.
    Track(PathBuf),
}

impl Serialize for Follow {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Follow::Stick => s.serialize_str("stick"),
            Follow::Track(path) => path.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Follow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(if s == "stick" { Follow::Stick } else { Follow::Track(s.into()) })
    }
}

impl fmt::Display for Follow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Follow::Stick => f.write_str("Stick"),
            Follow::Track(path) => write!(f, "Colour track {}", path.file_stem().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned())),
        }
    }
}

/// Where each `Follow` points at a clip time, as view yaw and pitch in
/// degrees; `None` where that can't be told (no attitude data, a track that
/// is missing or lost there). Editing asks only at the keyframe's own time.
pub type Aim<'a> = &'a dyn Fn(&Follow, f64) -> Option<(f64, f64)>;

#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    /// Clip time in seconds.
    pub t: f64,
    /// With `follow`, `view.yaw` and `view.pitch` are offsets from where
    /// it points.
    pub view: View,
    pub ease: Ease,
    pub follow: Option<Follow>,
}

impl Keyframe {
    /// The view it shows at clip time `t`.
    fn view_with(&self, aim: Aim, t: f64) -> View {
        match &self.follow {
            None => self.view,
            Some(f) => {
                let (yaw, pitch) = aim(f, t).unwrap_or((0.0, 0.0));
                View { yaw: yaw + self.view.yaw, pitch: (pitch + self.view.pitch).clamp(-90.0, 90.0), ..self.view }
            }
        }
    }

    /// `view` (as shown at its time) stored the way this keyframe keeps it.
    fn store(&self, view: View, aim: Aim) -> View {
        match self.follow.as_ref().and_then(|f| aim(f, self.t)) {
            Some((yaw, pitch)) => View { yaw: view.yaw - yaw, pitch: view.pitch - pitch, ..view },
            None => view,
        }
    }
}

/// The JSON form: a following keyframe has `follow` and offsets in place
/// of `yaw` and `pitch`.
impl Serialize for Keyframe {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Json {
            t: f64,
            #[serde(skip_serializing_if = "Option::is_none")]
            yaw: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pitch: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            follow: Option<Follow>,
            #[serde(skip_serializing_if = "Option::is_none")]
            offset_yaw: Option<f64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            offset_pitch: Option<f64>,
            roll: f64,
            fov: f64,
            projection: Projection,
            /// Only where it isn't the projection's own.
            #[serde(skip_serializing_if = "Option::is_none")]
            correction: Option<f64>,
            ease: Ease,
        }
        let (fixed, follows) = (self.follow.is_none(), self.follow.is_some());
        let v = &self.view;
        Json {
            t: self.t,
            yaw: fixed.then_some(v.yaw),
            pitch: fixed.then_some(v.pitch),
            follow: self.follow.clone(),
            offset_yaw: follows.then_some(v.yaw),
            offset_pitch: follows.then_some(v.pitch),
            roll: v.roll,
            fov: v.fov,
            projection: v.projection,
            correction: v.projection.planet().filter(|&d| d != v.correction).map(|_| v.correction),
            ease: self.ease,
        }
        .serialize(s)
    }
}

/// Keyframed views in clip time. Between keyframes yaw, pitch and roll move
/// evenly, the zoom changes at an even rate (in tan of half the FOV), and
/// perspective and little planet morph into each other. Yaw isn't wrapped:
/// 0 → 360 is a full turn.
///
/// The JSON form lets a keyframe leave out anything that stays the same:
/// missing fields carry over from the keyframe before, and the first
/// keyframe's from its projection's home view.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<RawKeyframe>", into = "Vec<Keyframe>")]
pub struct Keyframes(Vec<Keyframe>);

impl std::ops::Deref for Keyframes {
    type Target = [Keyframe];

    fn deref(&self) -> &[Keyframe] {
        &self.0
    }
}

impl From<Keyframes> for Vec<Keyframe> {
    fn from(k: Keyframes) -> Self {
        k.0
    }
}

impl Keyframes {
    /// The span the keyframes cover, if more than one instant.
    pub fn span(&self) -> Option<(f64, f64)> {
        let (a, b) = (self.first()?.t, self.last()?.t);
        (b > a).then_some((a, b))
    }

    /// The view at clip time `t`; `None` without keyframes. `aim` says
    /// where following keyframes point in the frame being drawn.
    pub fn camera_at(&self, t: f64, aim: Aim) -> Option<Camera> {
        let (view, planet) = self.blend(t, aim)?;
        Some(Camera { planet, ..view.camera() })
    }

    /// The view at clip time `t` as angles, to carry on from by hand. Mid-
    /// morph it takes the nearer keyframe's projection.
    pub fn view_at(&self, t: f64, aim: Aim) -> Option<View> {
        self.blend(t, aim).map(|(view, _)| view)
    }

    /// The interpolated view at `t`, plus its perspective-to-planet morph.
    /// A following keyframe takes part as the view it shows now, so a move
    /// from it to a fixed one keeps tracking until it hands over.
    fn blend(&self, t: f64, aim: Aim) -> Option<(View, Option<f64>)> {
        let k = &self.0;
        let i = k.partition_point(|f| f.t <= t);
        let held = |f: &Keyframe| {
            let v = f.view_with(aim, t);
            (v, v.planet())
        };
        if i == 0 || i == k.len() {
            return k.get(i.saturating_sub(1)).map(held);
        }
        let (a, b) = (&k[i - 1], &k[i]);
        let x = ((t - a.t) / (b.t - a.t)).clamp(0.0, 1.0);
        let Some(u) = a.ease.shape(x) else { return Some(held(a)) };
        let lerp = |p: f64, q: f64| p + (q - p) * u;
        let (va, mut vb) = (a.view_with(aim, t), b.view_with(aim, t));
        // Where a followed subject is is known only modulo a turn, so a move
        // to or from a following keyframe takes the shorter way round from
        // what one keyframe shows at its time to what the other shows at
        // its own, and keeps to that side throughout. Choosing per frame
        // flipped sides mid-move whenever the subject wobbled across half a
        // turn away (a fixed view ahead, the holder behind), so the view
        // swung back and forth. Two fixed keyframes keep their yaws as
        // written: 0 → 360 is a full turn on purpose.
        if a.follow.is_some() || b.follow.is_some() {
            let half = |y: f64| (y + 180.0).rem_euclid(360.0) - 180.0;
            let turn = half(b.view_with(aim, b.t).yaw - a.view_with(aim, a.t).yaw);
            vb.yaw = va.yaw + turn + half(vb.yaw - va.yaw - turn);
        }
        let zoom = |v: View| (v.fov.to_radians() / 2.0).tan().ln();
        let near = if u < 0.5 { va } else { vb };
        let planet = match (va.planet(), vb.planet()) {
            (Some(p), Some(q)) => Some(lerp(p, q)),
            // Equirect has no in-between: cut half way.
            _ => near.planet(),
        };
        let view = View {
            yaw: lerp(va.yaw, vb.yaw),
            pitch: lerp(va.pitch, vb.pitch),
            roll: lerp(va.roll, vb.roll),
            fov: 2.0 * lerp(zoom(va), zoom(vb)).exp().atan().to_degrees(),
            projection: near.projection,
            correction: planet.unwrap_or(near.correction),
        };
        Some((view, planet))
    }

    /// The keyframe within `tolerance` seconds of `t`, if any.
    pub fn near(&self, t: f64, tolerance: f64) -> Option<usize> {
        self.0.iter().position(|f| (f.t - t).abs() <= tolerance)
    }

    /// Sets the view shown at `t`: updates a keyframe within `tolerance`,
    /// else adds one, easing and following like the keyframe before it.
    /// A following keyframe keeps `view` as offsets from `aim`. Returns its
    /// index.
    pub fn set(&mut self, t: f64, view: View, tolerance: f64, aim: Aim) -> usize {
        if let Some(i) = self.near(t, tolerance) {
            self.0[i].view = self.0[i].store(view, aim);
            return i;
        }
        let i = self.0.partition_point(|f| f.t < t);
        let (ease, follow) = i.checked_sub(1).map_or((Ease::default(), None), |p| (self.0[p].ease, self.0[p].follow.clone()));
        let mut key = Keyframe { t, view, ease, follow };
        key.view = key.store(view, aim);
        self.0.insert(i, key);
        i
    }

    /// The track files followed, to resolve or relativise their paths.
    pub fn tracks_mut(&mut self) -> impl Iterator<Item = &mut PathBuf> {
        self.0.iter_mut().filter_map(|k| match &mut k.follow {
            Some(Follow::Track(path)) => Some(path),
            _ => None,
        })
    }

    /// Makes keyframe `i` follow `follow` (or stop following), keeping the
    /// view it shows now, per `aim`.
    pub fn set_follow(&mut self, i: usize, follow: Option<Follow>, aim: Aim) {
        let shown = self.0[i].view_with(aim, self.0[i].t);
        self.0[i].follow = follow;
        self.0[i].view = self.0[i].store(shown, aim);
    }

    pub fn remove(&mut self, i: usize) {
        self.0.remove(i);
    }

    /// Sets how keyframe `i` moves on to the next.
    pub fn set_ease(&mut self, i: usize, ease: Ease) {
        self.0[i].ease = ease;
    }

    /// Moves keyframe `i` to time `t` (see `retime`).
    pub fn retime(&mut self, i: usize, t: f64, gap: f64) {
        retime(&mut self.0, i, t, gap);
    }
}

/// A keyframe of any kind: the view's, effects', an overlay's motion.
pub trait Timed {
    fn time(&self) -> f64;
    fn set_time(&mut self, t: f64);
}

impl Timed for Keyframe {
    fn time(&self) -> f64 {
        self.t
    }
    fn set_time(&mut self, t: f64) {
        self.t = t;
    }
}

/// Moves `keys[i]` (in time order) to time `t`, kept between its
/// neighbours (`gap` seconds clear of them) so the order and indices stay
/// as they are: dragging one past another would swap what each holds.
pub fn retime<T: Timed>(keys: &mut [T], i: usize, t: f64, gap: f64) {
    let lo = i.checked_sub(1).map_or(f64::NEG_INFINITY, |p| keys[p].time() + gap);
    let hi = keys.get(i + 1).map_or(f64::INFINITY, |n| n.time() - gap);
    if lo <= hi
        && let Some(k) = keys.get_mut(i)
    {
        k.set_time(t.clamp(lo, hi));
    }
}

/// Keyframed views over a stretch of one clip, as `export --reframe` reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawReframe")]
pub struct Reframe {
    pub keyframes: Keyframes,
    /// The picture's shape, width:height, if the reframe was made for one
    /// (a 9:16 framing stays 9:16 whatever the resolution).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
}

impl Reframe {
    pub fn fixed(view: View) -> Reframe {
        Reframe { keyframes: Keyframes(vec![Keyframe { t: 0.0, view, ease: Ease::Hold, follow: None }]), aspect: None }
    }

    pub fn span(&self) -> Option<(f64, f64)> {
        self.keyframes.span()
    }

    pub fn camera_at(&self, t: f64, aim: Aim) -> Camera {
        self.keyframes.camera_at(t, aim).expect("a reframe has keyframes")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReframe {
    keyframes: Keyframes,
    #[serde(default)]
    aspect: Option<Aspect>,
}

impl TryFrom<RawReframe> for Reframe {
    type Error = anyhow::Error;

    fn try_from(raw: RawReframe) -> Result<Self> {
        if raw.keyframes.is_empty() {
            bail!("a reframe needs at least one keyframe");
        }
        Ok(Reframe { keyframes: raw.keyframes, aspect: raw.aspect })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKeyframe {
    t: f64,
    yaw: Option<f64>,
    pitch: Option<f64>,
    roll: Option<f64>,
    fov: Option<f64>,
    projection: Option<Projection>,
    correction: Option<f64>,
    ease: Option<Ease>,
    follow: Option<Follow>,
    offset_yaw: Option<f64>,
    offset_pitch: Option<f64>,
}

impl TryFrom<Vec<RawKeyframe>> for Keyframes {
    type Error = anyhow::Error;

    fn try_from(raw: Vec<RawKeyframe>) -> Result<Self> {
        let mut keyframes: Vec<Keyframe> = Vec::with_capacity(raw.len());
        for r in raw {
            let prev = keyframes.last();
            if prev.is_some_and(|p| r.t < p.t) {
                bail!("keyframe at {} s comes after one at {} s; keep them in time order", r.t, prev.unwrap().t);
            }
            // Following carries over like the rest, until a keyframe gives
            // its own yaw or pitch.
            let fixed = r.yaw.is_some() || r.pitch.is_some();
            if fixed && r.follow.is_some() {
                bail!("keyframe at {} s has both follow and yaw/pitch; aim it with offset_yaw/offset_pitch", r.t);
            }
            let follow = r.follow.or(if fixed { None } else { prev.and_then(|p| p.follow.clone()) });
            if follow.is_none() && (r.offset_yaw.is_some() || r.offset_pitch.is_some()) {
                bail!("keyframe at {} s has offset_yaw/offset_pitch but follows nothing", r.t);
            }
            // A projection named anew starts at its own correction.
            let base = match (prev, r.projection) {
                (None, p) => View::home(p.unwrap_or(Projection::Perspective)),
                (Some(p), Some(proj)) if proj != p.view.projection => View { projection: proj, correction: View::home(proj).correction, ..p.view },
                (Some(p), _) => p.view,
            };
            // Offsets carry over between keyframes following the same thing;
            // a fixed keyframe after a following one has no angles to carry.
            let prev_follow = prev.and_then(|p| p.follow.as_ref());
            let (yaw, pitch) = match &follow {
                Some(f) => {
                    let (y, p) = if prev_follow == Some(f) { (base.yaw, base.pitch) } else { (0.0, 0.0) };
                    (r.offset_yaw.unwrap_or(y), r.offset_pitch.unwrap_or(p))
                }
                None if prev_follow.is_some() => match (r.yaw, r.pitch) {
                    (Some(y), Some(p)) => (y, p),
                    _ => bail!("keyframe at {} s stops following; give it both yaw and pitch", r.t),
                },
                None => (r.yaw.unwrap_or(base.yaw), r.pitch.unwrap_or(base.pitch)),
            };
            let view = View { yaw, pitch, roll: r.roll.unwrap_or(base.roll), fov: r.fov.unwrap_or(base.fov), projection: base.projection, correction: r.correction.unwrap_or(base.correction) };
            if !(1.0..=359.0).contains(&view.fov) {
                bail!("fov {} at {} s is out of range (1–359°)", view.fov, r.t);
            }
            if !(0.0..=1.0).contains(&view.correction) {
                bail!("correction {} at {} s is out of range (0–1)", view.correction, r.t);
            }
            keyframes.push(Keyframe { t: r.t, view, ease: r.ease.or(prev.map(|p| p.ease)).unwrap_or_default(), follow });
        }
        Ok(Keyframes(keyframes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn flat_views_crop_the_picture() {
        let at = |v: View, ndc| v.camera().flat_uv(16.0 / 9.0, 16.0 / 9.0, ndc);
        let home = View::home(Projection::Perspective);
        // Home shows the whole picture when the shapes match.
        assert_eq!(at(home, [-1.0, -1.0]), Some([0.0, 0.0]));
        assert_eq!(at(home, [1.0, 1.0]), Some([1.0, 1.0]));
        // Half the field of view: the middle half.
        let zoomed = View { fov: 45.0, ..home };
        assert_eq!(at(zoomed, [-1.0, -1.0]), Some([0.25, 0.25]));
        // Yaw right and pitch up move the centre right and up; past the edge is off it.
        let moved = View { yaw: 22.5, pitch: 22.5, fov: 45.0, ..home };
        let c = at(moved, [0.0, 0.0]).unwrap();
        assert!((c[0] - (0.5 + 0.25 * 9.0 / 16.0)).abs() < 1e-6 && (c[1] - 0.25).abs() < 1e-6);
        assert_eq!(at(View { yaw: 90.0, ..home }, [1.0, 0.0]), None);
        // A quarter turn of roll stands the screen's x on the picture's y.
        let rolled = at(View { roll: 90.0, fov: 45.0, ..home }, [0.5, 0.0]).unwrap();
        assert!((rolled[0] - 0.5).abs() < 1e-6 && (rolled[1] - (0.5 + 0.5 * 16.0 / 9.0 * 0.25)).abs() < 1e-6);
    }

    #[test]
    fn azimuthal_matches_perspective_and_planet() {
        let cam = |planet| Camera { user: crate::stitch::IDENTITY, zoom: 0.7, planet: Some(planet), grade: Default::default(), lut: None, crop: Default::default() };
        let ndc = [0.3, -0.8];
        let (q, z) = ([0.3 * 1.5 * 0.7, -0.8 * 0.7], 0.7);
        let n = (q[0] * q[0] + q[1] * q[1] + 1.0f64).sqrt();
        assert!(close(cam(0.0).ray(1.5, ndc), [q[0] / n, q[1] / n, 1.0 / n]));
        // The old little planet: stereographic on twice the scaled point.
        let p = [ndc[0] * 1.5 * z * 2.0, ndc[1] * z * 2.0];
        let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
        let th = 2.0 * (r / 2.0).atan();
        assert!(close(cam(1.0).ray(1.5, ndc), [th.sin() * p[0] / r, th.sin() * p[1] / r, th.cos()]));
    }

    #[test]
    fn missing_fields_carry_over() {
        let r: Reframe = serde_json::from_str(
            r#"{"aspect": "9:16", "keyframes": [{"t": 1, "yaw": 30, "fov": 70}, {"t": 3, "pitch": 10, "ease": "linear"}, {"t": 5, "projection": "little-planet"}]}"#,
        )
        .unwrap();
        let k = &r.keyframes;
        assert_eq!(k[0].view, View { yaw: 30.0, pitch: 0.0, roll: 0.0, fov: 70.0, projection: Projection::Perspective, correction: 0.0 });
        assert_eq!((k[1].view.yaw, k[1].view.pitch, k[1].view.fov), (30.0, 10.0, 70.0));
        assert_eq!((k[2].view.projection, k[2].ease), (Projection::LittlePlanet, Ease::Linear));
        assert_eq!(r.span(), Some((1.0, 5.0)));
        assert_eq!(r.aspect.map(|a| a.size(1080)), Some([1080, 1920]));
        let back: Reframe = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn correction_bends_between_the_projections() {
        let k: Keyframes = serde_json::from_str(
            r#"[{"t": 0, "fov": 100.2}, {"t": 1, "correction": 0.49, "fov": 84.1}, {"t": 2}, {"t": 3, "projection": "little-planet"}, {"t": 4, "projection": "perspective", "ease": "linear"}, {"t": 6, "correction": 1}]"#,
        )
        .unwrap();
        let d: Vec<f64> = k.iter().map(|k| k.view.correction).collect();
        // Carried over, reset by a projection named anew, given its own.
        assert_eq!(d, [0.0, 0.49, 0.49, 1.0, 0.0, 1.0]);
        // DJI Studio's readouts at 16:9: Zoom 129.6° and 259.2° for FOV
        // 100.2° at correction 0 and 1, 165.5° for 84.1° at 0.49.
        let across = |i: usize, fov| View { fov, ..k[i].view }.across(16.0 / 9.0).unwrap();
        assert!((across(0, 100.2) - 129.6).abs() < 0.05 && (across(5, 100.2) - 259.2).abs() < 0.05);
        assert!((across(1, 84.1) - 165.5).abs() < 0.5);
        assert_eq!(View::home(Projection::Equirect).across(2.0), None);
        // The drawing follows it, and moves evenly between keyframes.
        assert_eq!(k.camera_at(1.5, &|_, _| None).unwrap().planet, Some(0.49));
        assert!((k.camera_at(5.0, &|_, _| None).unwrap().planet.unwrap() - 0.5).abs() < 1e-9);
        assert!((k.view_at(5.0, &|_, _| None).unwrap().correction - 0.5).abs() < 1e-9);
        // Written only where it isn't the projection's own.
        let json = serde_json::to_string(&k).unwrap();
        assert_eq!(json.matches("correction").count(), 3, "{json}");
        let back: Keyframes = serde_json::from_str(&json).unwrap();
        assert_eq!(back, k);
        assert!(serde_json::from_str::<Keyframes>(r#"[{"t": 0, "correction": 1.5}]"#).is_err());
    }

    #[test]
    fn follows_the_stick() {
        let k: Keyframes = serde_json::from_str(
            r#"[{"t": 0, "follow": "stick", "offset_pitch": 30, "fov": 110},
                {"t": 2},
                {"t": 4, "yaw": 170, "pitch": 0}]"#,
        )
        .unwrap();
        assert_eq!((k[1].follow.clone(), k[1].view.pitch, k[1].view.fov), (Some(Follow::Stick), 30.0, 110.0));
        assert_eq!(k[2].follow, None);
        // The stick behind and below: yaw 170, pitch −60.
        let aim = |_: &Follow, _| Some((170.0, -60.0));
        let v = k.view_at(1.0, &aim).unwrap();
        assert_eq!((v.yaw, v.pitch), (170.0, -30.0));
        // Half way from the stick at −170° to the fixed 170° it turns the
        // short way, through 180°, not back through 0°.
        let half = k.view_at(3.0, &|_, _| Some((-170.0, -60.0))).unwrap();
        assert!(((half.yaw - 180.0).rem_euclid(360.0)).abs() < 1e-9, "{}", half.yaw);
        let back: Keyframes = serde_json::from_str(&serde_json::to_string(&k).unwrap()).unwrap();
        assert_eq!(back, k);
        assert!(serde_json::from_str::<Keyframes>(r#"[{"t": 0, "follow": "stick", "yaw": 3}]"#).is_err());
        assert!(serde_json::from_str::<Keyframes>(r#"[{"t": 0, "follow": "stick"}, {"t": 1, "yaw": 3}]"#).is_err());
        assert!(serde_json::from_str::<Keyframes>(r#"[{"t": 0, "offset_yaw": 3}]"#).is_err());
        let tracked: Keyframes = serde_json::from_str(r#"[{"t": 0, "follow": "work/friend.csv"}]"#).unwrap();
        assert_eq!(tracked[0].follow, Some(Follow::Track("work/friend.csv".into())));
        assert!(serde_json::to_string(&tracked).unwrap().contains(r#""follow":"work/friend.csv""#));
        // Editing keeps the shown view: offsets come from the aim.
        let mut e = k.clone();
        e.set_follow(2, Some(Follow::Stick), &aim);
        assert_eq!((e[2].view.yaw, e[2].view.pitch), (0.0, 60.0));
        e.set(1.0, View { yaw: 175.0, pitch: -20.0, ..k[0].view }, 0.01, &aim);
        assert_eq!((e[1].view.yaw, e[1].view.pitch), (5.0, 40.0));
    }

    #[test]
    fn a_move_to_a_subject_half_a_turn_away_keeps_its_side() {
        // From a view ahead to a subject behind that wobbles across 180°:
        // the move must turn one way all through, not flip sides.
        let k: Keyframes = serde_json::from_str(r#"[{"t": 0, "yaw": 0}, {"t": 6, "follow": "stick"}]"#).unwrap();
        let aim = |_: &Follow, t: f64| Some((if t == 6.0 { 170.0 } else { 175.0 + 10.0 * (t * 9.0).sin() }, 0.0));
        let yaws: Vec<f64> = (0..=60).map(|i| k.view_at(i as f64 / 10.0, &aim).unwrap().yaw).collect();
        assert!(yaws.windows(2).all(|w| (w[1] - w[0]).abs() < 15.0), "{yaws:?}");
    }

    #[test]
    fn edits_keyframes() {
        let mut k = Keyframes::default();
        let v = |yaw| View { yaw, ..View::home(Projection::Perspective) };
        assert_eq!(k.set(2.0, v(10.0), 0.01, &|_, _| None), 0);
        assert_eq!(k.set(1.0, v(0.0), 0.01, &|_, _| None), 0);
        assert_eq!(k.set(2.005, v(20.0), 0.01, &|_, _| None), 1);
        assert_eq!((k.len(), k[1].view.yaw), (2, 20.0));
        assert_eq!(k.view_at(1.5, &|_, _| None).unwrap().yaw, 10.0);
        k.retime(0, 5.0, 0.1);
        assert!((k[0].t - 1.9).abs() < 1e-9);
        k.remove(0);
        assert_eq!(k.near(2.0, 0.01), Some(0));
    }

    #[test]
    fn interpolates_between_keyframes() {
        let r: Reframe = serde_json::from_str(r#"{"keyframes": [{"t": 0, "yaw": 0, "ease": "linear"}, {"t": 2, "yaw": 90}]}"#).unwrap();
        let mid = View { yaw: 45.0, ..View::home(Projection::Perspective) }.camera();
        assert!(close(r.camera_at(1.0, &|_, _| None).user[0], mid.user[0]));
        assert!(close(r.camera_at(-1.0, &|_, _| None).user[0], r.keyframes[0].view.camera().user[0]));
    }
}
