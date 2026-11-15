//! How one cut gives way to the next through their overlap (`Cut::crossfade`
//! seconds of both): a dissolve, a dip through black or white, a wipe, a
//! push, a slide or a spin.
//!
//! Each is two layers, the outgoing cut under the incoming one, and a
//! `Part` per layer saying where it sits, how much of it shows and how far
//! it's dipped. The player's shader draws its two layers by them
//! (`shader.wgsl`'s `trans`) and the export mixes its two frames by them
//! (`mix`), so the two agree; change them together. A spin also turns both
//! cuts' cameras (`spin`), which the stitchers draw.

use crate::stitch::Frame;
use anyhow::{Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Dissolve,
    DipBlack,
    DipWhite,
    Wipe,
    Push,
    Slide,
    Spin,
}

impl Style {
    pub const ALL: [Style; 7] = [Style::Dissolve, Style::DipBlack, Style::DipWhite, Style::Wipe, Style::Push, Style::Slide, Style::Spin];

    fn name(self) -> &'static str {
        match self {
            Style::Dissolve => "dissolve",
            Style::DipBlack => "dip-black",
            Style::DipWhite => "dip-white",
            Style::Wipe => "wipe",
            Style::Push => "push",
            Style::Slide => "slide",
            Style::Spin => "spin",
        }
    }

    /// The ways it can go: none for those that have no direction, and a
    /// spin turns only left or right.
    pub fn directions(self) -> &'static [Direction] {
        match self {
            Style::Dissolve | Style::DipBlack | Style::DipWhite => &[],
            Style::Spin => &[Direction::Left, Direction::Right],
            _ => &Direction::ALL,
        }
    }
}

impl std::fmt::Display for Style {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Style::Dissolve => "Dissolve",
            Style::DipBlack => "Dip to black",
            Style::DipWhite => "Dip to white",
            Style::Wipe => "Wipe",
            Style::Push => "Push",
            Style::Slide => "Slide",
            Style::Spin => "Spin",
        })
    }
}

/// The way the picture (or for a spin the camera) moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub const ALL: [Direction; 4] = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];

    fn name(self) -> &'static str {
        match self {
            Direction::Left => "left",
            Direction::Right => "right",
            Direction::Up => "up",
            Direction::Down => "down",
        }
    }

    /// As a step across the frame, y down.
    fn step(self) -> [f64; 2] {
        match self {
            Direction::Left => [-1.0, 0.0],
            Direction::Right => [1.0, 0.0],
            Direction::Up => [0.0, -1.0],
            Direction::Down => [0.0, 1.0],
        }
    }
}

/// A cut's transition from the cut before, written "dissolve", "dip-black",
/// "wipe-left", "spin-right" and so on in a project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Transition {
    pub style: Style,
    pub direction: Direction,
}

impl Transition {
    pub fn is_dissolve(&self) -> bool {
        self.style == Style::Dissolve
    }

    /// With `style`, going the way it went if the style can, else the
    /// style's first way.
    pub fn with_style(self, style: Style) -> Self {
        let ways = style.directions();
        let direction = if ways.contains(&self.direction) { self.direction } else { ways.first().copied().unwrap_or_default() };
        Transition { style, direction }
    }
}

impl From<Transition> for String {
    fn from(t: Transition) -> String {
        if t.style.directions().is_empty() { t.style.name().to_owned() } else { format!("{}-{}", t.style.name(), t.direction.name()) }
    }
}

impl TryFrom<String> for Transition {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        for style in Style::ALL {
            let ways = style.directions();
            if ways.is_empty() && s == style.name() {
                return Ok(Transition { style, direction: Direction::default() });
            }
            if let Some(&direction) = ways.iter().find(|d| s.strip_prefix(style.name()).and_then(|r| r.strip_prefix('-')) == Some(d.name())) {
                return Ok(Transition { style, direction });
            }
        }
        bail!("\"{s}\" isn't a transition (dissolve, dip-black, dip-white, wipe-left, push-up, slide-right, spin-left, …)")
    }
}

/// Half the width of a wipe's soft edge, in frame widths (or heights).
const SOFT: f64 = 0.04;
/// How far a spin turns each cut's camera through the whole transition
/// (the cut hidden half-way, a quarter of this in), radians.
const SPIN: f64 = std::f64::consts::TAU;
/// The shutter a spin's blur is drawn for, seconds: longer than a camera's,
/// as the smear is what hides the cut.
const SHUTTER: f64 = 0.04;
/// Views averaged across a spin's blur.
pub const TAPS: usize = 16;

/// One cut's layer in a transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Part {
    /// Where it's moved to, in frame widths and heights.
    pub offset: [f64; 2],
    /// How much of it shows over the layer under it.
    pub alpha: f64,
    /// How far it's dipped into `colour` (0..1 each).
    pub dip: f64,
    pub colour: [f64; 3],
    /// A wipe: it shows where `nx·u + ny·v + c0` (u, v across and down the
    /// frame, 0..1) is under `edge`, softly.
    pub wipe: Option<[f64; 4]>,
}

impl Default for Part {
    fn default() -> Self {
        Part { offset: [0.0; 2], alpha: 1.0, dip: 0.0, colour: [0.0; 3], wipe: None }
    }
}

fn smoothstep(lo: f64, hi: f64, x: f64) -> f64 {
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Part {
    /// For the shader's `trans`: offset, alpha, dip; the wipe; the dip's
    /// colour and whether there's a wipe; then the layer's place (`over`
    /// for the one drawn over the other) and its spin blur, radians.
    pub fn uniforms(&self, over: bool, blur: f64) -> [[f32; 4]; 4] {
        let f = |v: f64| v as f32;
        let w = self.wipe.unwrap_or_default();
        [
            [f(self.offset[0]), f(self.offset[1]), f(self.alpha), f(self.dip)],
            w.map(f),
            [f(self.colour[0]), f(self.colour[1]), f(self.colour[2]), self.wipe.is_some() as u8 as f32],
            [over as u8 as f32, f(blur), 0.0, 0.0],
        ]
    }

    /// How much of it shows at (`u`, `v`) of the frame.
    fn shows(&self, u: f64, v: f64) -> f64 {
        self.alpha * self.wipe.map_or(1.0, |[nx, ny, c0, edge]| 1.0 - smoothstep(edge - SOFT, edge + SOFT, nx * u + ny * v + c0))
    }
}

impl Transition {
    /// The outgoing cut's layer and, over it, the incoming one's, `p` of
    /// the way through (0..1).
    pub fn parts(self, p: f64) -> [Part; 2] {
        let p = p.clamp(0.0, 1.0);
        let (mut out, mut inc) = (Part::default(), Part::default());
        let d = self.direction.step();
        let e = smoothstep(0.0, 1.0, p);
        match self.style {
            Style::Dissolve => inc.alpha = p,
            Style::DipBlack | Style::DipWhite => {
                let colour = if self.style == Style::DipWhite { [1.0; 3] } else { [0.0; 3] };
                (out.colour, inc.colour) = (colour, colour);
                out.dip = (2.0 * p).min(1.0);
                inc.dip = (2.0 * (1.0 - p)).min(1.0);
                inc.alpha = (p >= 0.5) as u8 as f64;
            }
            Style::Wipe => {
                // The line runs the way of `d`, the new cut behind it.
                let c0 = if d[0] + d[1] < 0.0 { 1.0 } else { 0.0 };
                inc.wipe = Some([d[0], d[1], c0, -SOFT + p * (1.0 + 2.0 * SOFT)]);
            }
            Style::Slide => inc.offset = [(e - 1.0) * d[0], (e - 1.0) * d[1]],
            Style::Push => {
                out.offset = [e * d[0], e * d[1]];
                inc.offset = [(e - 1.0) * d[0], (e - 1.0) * d[1]];
            }
            Style::Spin => inc.alpha = smoothstep(0.35, 0.65, p),
        }
        [out, inc]
    }

    /// A spin's turn of the outgoing and the incoming cut's camera `p` of
    /// the way through a transition `seconds` long: each a yaw (radians)
    /// and the yaw its shutter smears over. The outgoing camera swings
    /// away ever faster and the incoming one arrives from as far the other
    /// side, slowing into its framing; at the middle, where one takes over
    /// from the other, they turn alike.
    pub fn spin(self, p: f64, seconds: f64) -> Option<[(f64, f64); 2]> {
        if self.style != Style::Spin || seconds <= 0.0 {
            return None;
        }
        let p = p.clamp(0.0, 1.0);
        let way = if self.direction == Direction::Left { -1.0 } else { 1.0 };
        let blur = |q: f64| SPIN * 2.0 * q / seconds * SHUTTER;
        Some([(way * SPIN * p * p, blur(p)), (-way * SPIN * (1.0 - p) * (1.0 - p), blur(1.0 - p))])
    }

    /// The export's frame `p` of the way through: the outgoing cut's `a`
    /// and the incoming cut's `b` (alike in size and depth), as the
    /// player's shader layers them.
    pub fn mix(self, p: f64, a: &Frame, b: &Frame) -> Vec<u8> {
        let [out, inc] = self.parts(p);
        let (w, h) = (a.width, a.height);
        let bytes = if a.deep { 2 } else { 1 };
        let max = if a.deep { 65535.0 } else { 255.0 };
        // Moved by whole pixels, as the shader rounds its offsets.
        let shift = |part: &Part| [(part.offset[0] * w as f64).round() as isize, (part.offset[1] * h as f64).round() as isize];
        let (so, si) = (shift(&out), shift(&inc));
        let at = |f: &Frame, part: &Part, s: [isize; 2], x: usize, y: usize| -> Option<[f64; 3]> {
            let (sx, sy) = (x as isize - s[0], y as isize - s[1]);
            if sx < 0 || sy < 0 || sx >= w as isize || sy >= h as isize {
                return None;
            }
            let i = (sy as usize * w + sx as usize) * 3 * bytes;
            let px = |c: usize| if f.deep { u16::from_le_bytes([f.rgb[i + 2 * c], f.rgb[i + 2 * c + 1]]) as f64 } else { f.rgb[i + c] as f64 };
            Some([0, 1, 2].map(|c| px(c) + (part.colour[c] * max - px(c)) * part.dip))
        };
        let mut rgb = vec![0u8; a.rgb.len()];
        rgb.par_chunks_mut(w * 3 * bytes).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let under = at(a, &out, so, x, y).unwrap_or([0.0; 3]);
                let px = match at(b, &inc, si, x, y) {
                    Some(over) => {
                        let k = inc.shows((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64);
                        [0, 1, 2].map(|c| under[c] + (over[c] - under[c]) * k)
                    }
                    None => under,
                };
                for c in 0..3 {
                    if a.deep {
                        row[(x * 3 + c) * 2..(x * 3 + c) * 2 + 2].copy_from_slice(&(px[c].round() as u16).to_le_bytes());
                    } else {
                        row[x * 3 + c] = px[c].round() as u8;
                    }
                }
            }
        });
        rgb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_back_as_written() {
        for style in Style::ALL {
            let ways = style.directions();
            for &direction in ways.iter().chain(ways.is_empty().then_some(&Direction::Left)) {
                let t = Transition { style, direction };
                assert_eq!(Transition::try_from(String::from(t)).unwrap(), t);
            }
        }
        assert_eq!(String::from(Transition { style: Style::Wipe, direction: Direction::Up }), "wipe-up");
        assert!(Transition::try_from("spin-up".to_owned()).is_err());
        assert!(Transition::try_from("wipe".to_owned()).is_err());
    }

    fn flat(v: u8) -> Frame {
        Frame { width: 8, height: 4, rgb: vec![v; 8 * 4 * 3], deep: false }
    }

    #[test]
    fn every_style_starts_on_the_old_cut_and_ends_on_the_new() {
        let (a, b) = (flat(40), flat(200));
        for style in Style::ALL {
            for &direction in &Direction::ALL {
                let t = Transition { style, direction };
                assert!(t.mix(0.0, &a, &b).iter().all(|&v| v == 40), "{t:?} at its start");
                assert!(t.mix(1.0, &a, &b).iter().all(|&v| v == 200), "{t:?} at its end");
            }
        }
    }

    #[test]
    fn a_dissolve_is_the_linear_blend_it_was() {
        let t = Transition::default();
        assert!(t.mix(0.25, &flat(40), &flat(200)).iter().all(|&v| v == 80));
        let deep = |v: u16| Frame { width: 2, height: 2, rgb: [v; 12].iter().flat_map(|s| s.to_le_bytes()).collect(), deep: true };
        assert!(t.mix(0.5, &deep(1000), &deep(3000)).chunks_exact(2).all(|s| u16::from_le_bytes([s[0], s[1]]) == 2000));
    }

    #[test]
    fn pictures_move_and_dip_the_way_asked() {
        let (a, b) = (flat(40), flat(200));
        // Half-way through a push to the left the old cut holds the left
        // half and the new one has come in over the right.
        let push = Transition { style: Style::Push, direction: Direction::Left }.mix(0.5, &a, &b);
        assert_eq!((push[0], push[7 * 3]), (40, 200));
        // A slide to the right brings the new cut in from the left over a
        // still old one.
        let slide = Transition { style: Style::Slide, direction: Direction::Right }.mix(0.5, &a, &b);
        assert_eq!((slide[0], slide[7 * 3]), (200, 40));
        // A wipe to the right uncovers the new cut from the left.
        let wipe = Transition { style: Style::Wipe, direction: Direction::Right }.mix(0.5, &a, &b);
        assert_eq!((wipe[0], wipe[7 * 3]), (200, 40));
        // A dip is all its colour half-way.
        assert!(Transition { style: Style::DipWhite, direction: Direction::Left }.mix(0.5, &a, &b).iter().all(|&v| v == 255));
        assert!(Transition { style: Style::DipBlack, direction: Direction::Left }.mix(0.5, &a, &b).iter().all(|&v| v == 0));
    }

    #[test]
    fn a_spin_hands_over_at_one_speed() {
        let t = Transition { style: Style::Spin, direction: Direction::Right };
        let [(out, blur_out), (inc, blur_in)] = t.spin(0.5, 1.0).unwrap();
        assert!((out + inc).abs() < 1e-12 && out > 0.0);
        assert_eq!(blur_out, blur_in);
        // Still at each end: the cuts as framed.
        assert_eq!(t.spin(0.0, 1.0).unwrap()[0], (0.0, 0.0));
        assert_eq!(t.spin(1.0, 1.0).unwrap()[1], (0.0, 0.0));
        assert!(Transition::default().spin(0.5, 1.0).is_none());
    }
}
