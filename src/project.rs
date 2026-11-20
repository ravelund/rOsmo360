//! An edit: cuts from one or more clips played one after another, each with
//! its own keyframed views. The player's timeline edits it and `export`
//! renders it.
//!
//! Keyframes belong to their cut and are pinned to clip time, not to the
//! timeline: moving a cut keeps its camera moves, and trimming one keeps
//! each keyframe on the same moment of footage. Keyframes outside the cut's
//! in and out points are kept, so extending the cut again brings them back.
//!
//! Music is the other way round: its clips sit at times on the edit, so
//! cutting the video to the music leaves the music where it is. It is cut
//! on its own lane, and the edit ends with the last video cut, the music
//! faded out there if it runs on.

use crate::stab::Mode;
use crate::transition::Transition;
use crate::view::{Aim, Aspect, Camera, Keyframes, Projection, View};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The picture's shape, width:height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect: Option<Aspect>,
    /// None yet when a project starts from its music.
    #[serde(default)]
    pub cuts: Vec<Cut>,
    /// The clips' own sound in the mix.
    #[serde(default, skip_serializing_if = "Sound::is_default")]
    pub sound: Sound,
    #[serde(default, skip_serializing_if = "Music::is_empty")]
    pub music: Music,
    /// Fades from black at the start and to black at the end.
    #[serde(default, skip_serializing_if = "Fades::is_none")]
    pub fades: Fades,
    /// Lanes of pictures over the picture, the first lowest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<crate::overlay::Lane>,
    /// Lanes of text, over the overlays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text: Vec<crate::overlay::Lane>,
    /// Video tracks over the main one (`cuts`), the first lowest: their
    /// cuts sit at times on the edit and cover the main track while they
    /// play. One for now (`flatten`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<VideoTrack>,
}

/// A video track over the main one: cuts placed at edit times (`Cut::at`),
/// gaps showing the main track through. Its cuts' `crossfade` and
/// `transition` say how each comes in over what's beneath, `out_crossfade`
/// and `out_transition` how it goes; two of its cuts overlapping crossfade
/// into each other instead, by the later one's `transition`. Their sound is
/// mixed over the main track's, which runs on beneath.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoTrack {
    #[serde(default)]
    pub cuts: Vec<Cut>,
}

/// The edit's fades from black at its start and to black at its end, in
/// seconds, picture and sound together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fades {
    #[serde(default, rename = "in", skip_serializing_if = "is_zero")]
    pub fade_in: f64,
    #[serde(default, rename = "out", skip_serializing_if = "is_zero")]
    pub fade_out: f64,
}

impl Fades {
    pub fn is_none(&self) -> bool {
        *self == Fades::default()
    }

    /// How far faded to black at edit time `t` of an edit `length` long,
    /// 0..1.
    pub fn black(&self, t: f64, length: f64) -> f64 {
        let from = |d: f64, x: f64| if d > 0.0 { 1.0 - (x / d).clamp(0.0, 1.0) } else { 0.0 };
        from(self.fade_in, t).max(from(self.fade_out, length - t))
    }
}

/// The clips' own sound, as one track of the mix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sound {
    /// Level in dB; 0 is as recorded.
    #[serde(default)]
    pub volume: f64,
}

impl Sound {
    fn is_default(&self) -> bool {
        *self == Sound::default()
    }
}

/// The music track: clips of songs placed on the edit's timeline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Music {
    /// The track's level in dB; 0 is as the files are.
    #[serde(default)]
    pub volume: f64,
    #[serde(default)]
    pub clips: Vec<MusicClip>,
    /// Volume points at edit times, in time order: dB on top of `volume`,
    /// so the track's level still trims the whole line. They stay where
    /// they are when songs move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyframes: Vec<VolumeKey>,
}

/// A volume point: `volume` dB at time `t`, straight lines (in dB) between
/// points, held before the first and after the last.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeKey {
    pub t: f64,
    pub volume: f64,
}

/// The level `keys` give at `t`, dB; 0 without any.
pub fn volume_at(keys: &[VolumeKey], t: f64) -> f64 {
    let i = keys.partition_point(|k| k.t <= t);
    match (i.checked_sub(1).map(|j| keys[j]), keys.get(i).copied()) {
        (Some(a), Some(b)) => a.volume + (b.volume - a.volume) * (t - a.t) / (b.t - a.t).max(1e-9),
        (Some(k), None) | (None, Some(k)) => k.volume,
        (None, None) => 0.0,
    }
}

impl Music {
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty() && self.volume == 0.0 && self.keyframes.is_empty()
    }

    /// The track's level at edit time `t`, dB: its own and its points'.
    pub fn level_at(&self, t: f64) -> f64 {
        self.volume + volume_at(&self.keyframes, t)
    }

    /// Where the last clip ends on the edit, in seconds.
    pub fn end(&self) -> f64 {
        self.clips.iter().map(MusicClip::ends).fold(0.0, f64::max)
    }

    /// The start nearest `want` where clip `i` fits without overlapping the
    /// others: clips never play over each other (crossfades will be an effect
    /// of their own), so one pushed into another stops edge to edge against
    /// it, and hops past it once `want` is nearer the far side.
    pub fn place(&self, i: usize, want: f64) -> f64 {
        let len = self.clips[i].end - self.clips[i].start;
        let others = || self.clips.iter().enumerate().filter(move |&(j, _)| j != i).map(|(_, c)| c);
        let free = |at: f64| at >= 0.0 && others().all(|c| at + len <= c.at + 1e-6 || at >= c.ends() - 1e-6);
        std::iter::once(want.max(0.0))
            .chain(others().flat_map(|c| [c.ends(), c.at - len]))
            .filter(|&at| free(at))
            .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
            .unwrap_or(self.end())
    }

    /// Whether clip `i` fits from `at` for `len` seconds. It may overlap
    /// another clip only partly, at one of that clip's ends (the overlap is
    /// their crossfade), and never where a third clip plays; with `overlap`
    /// false, by no more than it overlaps that clip already (a plain drag
    /// never makes a crossfade, nor grows one).
    pub fn fits(&self, i: usize, at: f64, len: f64, overlap: bool) -> bool {
        const E: f64 = 1e-6;
        let (s, e) = (at, at + len);
        let me = &self.clips[i];
        let others = || self.clips.iter().enumerate().filter(move |&(j, _)| j != i).map(|(_, c)| c);
        at >= -E
            && others().all(|c| {
                let o = overlap_of(s, e, c.at, c.ends());
                if o <= E {
                    return true;
                }
                let partly = (s > c.at + E && e > c.ends() + E) || (s < c.at - E && e < c.ends() - E);
                let (o0, o1) = (s.max(c.at), e.min(c.ends()));
                let alone = others().all(|d| std::ptr::eq(d, c) || overlap_of(o0, o1, d.at, d.ends()) <= E);
                partly && alone && (overlap || o <= overlap_of(me.at, me.ends(), c.at, c.ends()) + E)
            })
    }

    /// How far from `from` towards `to` a value can go while `ok` holds
    /// (it holds at `from`): an edge dragged into a neighbour stops at it.
    pub fn reach(from: f64, to: f64, ok: impl Fn(f64) -> bool) -> f64 {
        if ok(to) {
            return to;
        }
        let (mut good, mut bad) = (from, to);
        for _ in 0..40 {
            let mid = (good + bad) / 2.0;
            if ok(mid) {
                good = mid;
            } else {
                bad = mid;
            }
        }
        good
    }

    /// Clip `i`'s crossfades: how long it overlaps the clip before it, and
    /// the clip after it.
    pub fn crossfades(&self, i: usize) -> (f64, f64) {
        let me = &self.clips[i];
        let (mut fade_in, mut fade_out) = (0.0, 0.0);
        for (j, c) in self.clips.iter().enumerate() {
            let o = overlap_of(me.at, me.ends(), c.at, c.ends());
            if j == i || o <= 1e-6 {
                continue;
            }
            if c.at < me.at {
                fade_in = o;
            } else {
                fade_out = o;
            }
        }
        (fade_in, fade_out)
    }

    /// Splits clip `i` at edit time `t`, each half staying where it was on the
    /// edit; false if `t` leaves either half shorter than `min`.
    pub fn split(&mut self, i: usize, t: f64, min: f64) -> bool {
        let Some(c) = self.clips.get(i) else { return false };
        let song = c.start + t - c.at;
        if song - c.start < min || c.end - song < min {
            return false;
        }
        let second = MusicClip { at: t, start: song, fade_in: 0.0, ..c.clone() };
        let first = &mut self.clips[i];
        first.end = song;
        first.fade_out = 0.0;
        self.clips.insert(i + 1, second);
        true
    }
}

/// A stretch of a song at a place on the edit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MusicClip {
    /// The file; a relative path is relative to the project file.
    pub input: PathBuf,
    /// Where it starts on the edit, in seconds.
    pub at: f64,
    /// In and out points in the song, in seconds.
    #[serde(rename = "in")]
    pub start: f64,
    #[serde(rename = "out")]
    pub end: f64,
    /// Fades at its own start and end, in seconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_in: f64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fade_out: f64,
}

/// How long the spans `a0..a1` and `b0..b1` share.
fn overlap_of(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

impl MusicClip {
    /// Where it ends on the edit.
    pub fn ends(&self) -> f64 {
        self.at + self.end - self.start
    }
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// Music running on past the last cut fades out over this long before it.
pub const END_FADE: f64 = 3.0;

/// A level at or below this is off: the sliders' bottom is silence.
pub const OFF_DB: f64 = -40.0;

/// A mix level in dB as a gain factor.
pub fn gain(db: f64) -> f64 {
    if db <= OFF_DB { 0.0 } else { 10f64.powf(db / 20.0) }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    /// The clip; a relative path is relative to the project file.
    pub input: PathBuf,
    /// In and out points, clip time in seconds.
    #[serde(rename = "in")]
    pub start: f64,
    #[serde(rename = "out")]
    pub end: f64,
    #[serde(default)]
    pub keyframes: Keyframes,
    /// Its stabilisation, or the export's `--stabilize` (horizon by
    /// default) if it doesn't set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stabilize: Option<Mode>,
    /// What the cut is for, shown on the timeline: written by whoever
    /// assembled the edit, such as an automatic highlight pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Its sound's volume points, clip time, in dB on top of the project's
    /// `sound` level. Pinned to the footage like the keyframes, so a shout
    /// keeps its level wherever the cut goes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume: Vec<VolumeKey>,
    /// Seconds it overlaps the cut before it, dissolving in from it
    /// (picture and sound); the edit is that much shorter.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub crossfade: f64,
    /// How it comes in through that overlap: a dissolve unless it says.
    #[serde(default, skip_serializing_if = "Transition::is_dissolve")]
    pub transition: Transition,
    /// Its colour effects' keyframes, clip time, apart from the camera's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<crate::grade::EffectKey>,
    /// A LUT applied before the effects: a name in the LUT library, or a
    /// `.cube` file (relative to the project). D-Log M needs DJI's to look
    /// like Rec.709; a missing one leaves the cut as filmed, with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lut: Option<String>,
    /// The LUT's strength as projects saved it for a day (2026-10-15)
    /// before it became an effect: read, moved into the effect keyframes
    /// (`Project::load`), never written.
    #[serde(default, rename = "lut_strength", skip_serializing)]
    pub old_lut_strength: Option<f64>,
    /// On a video track over the main one: where it starts on the edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<f64>,
    /// On a track over the main one: seconds it goes out over what's
    /// beneath at its end, and how (`crossfade` and `transition` are how it
    /// comes in).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub out_crossfade: f64,
    #[serde(default, skip_serializing_if = "Transition::is_dissolve")]
    pub out_transition: Transition,
    /// Which track it plays on in a flattened edit (`flatten`): 0 the main
    /// one. Not saved.
    #[serde(skip)]
    pub track: usize,
    /// In a flattened edit, a piece's whole cut's out point, where a later
    /// cut without keyframes reads the view it holds. Not saved.
    #[serde(skip)]
    pub source_end: Option<f64>,
    /// Its clip is an ordinary video (framed by cropping, not a 360 view):
    /// found when the clip is opened, not saved.
    #[serde(skip)]
    pub flat: bool,
    /// Its LUT, loaded (`resolve_lut`): found when the clip is opened.
    #[serde(skip)]
    pub lut_applied: Option<usize>,
}

impl Cut {
    /// Loads the LUT it names (a library name, or a file) for `camera_at`.
    /// Says why when it can't: the cut then shows as filmed.
    pub fn resolve_lut(&mut self) -> Result<()> {
        self.lut_applied = None;
        let Some(name) = &self.lut else { return Ok(()) };
        let path = crate::lut::find(name).with_context(|| format!("the LUT \"{name}\" isn't in the LUT library (or isn't there as a file)"))?;
        self.lut_applied = Some(crate::lut::shared(&path)?);
        Ok(())
    }

    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

/// The shortest stretch of a cut its crossfades leave to it alone.
pub const SOLO: f64 = 0.1;

/// Where each of `cuts` starts on the edit: after the one before, less the
/// crossfade into it.
pub fn offsets(cuts: &[Cut]) -> Vec<f64> {
    let mut from = 0.0;
    cuts.iter()
        .map(|c| {
            from -= c.crossfade;
            let at = from;
            from += c.len();
            at
        })
        .collect()
}

/// The edit's length: the cuts' less their crossfades.
pub fn edit_length(cuts: &[Cut]) -> f64 {
    cuts.iter().map(|c| c.len() - c.crossfade).sum()
}

/// The longest crossfade into cut `i`: none into the first, and leaving
/// both cuts a moment of their own with the crossfade before.
pub fn max_crossfade(cuts: &[Cut], i: usize) -> f64 {
    if i == 0 || i >= cuts.len() {
        return 0.0;
    }
    let before = &cuts[i - 1];
    (before.len() - before.crossfade).min(cuts[i].len()) - SOLO
}

/// Keeps every crossfade within `max_crossfade` after cuts were trimmed,
/// moved or deleted.
pub fn settle_crossfades(cuts: &mut [Cut]) {
    for i in 0..cuts.len() {
        cuts[i].crossfade = cuts[i].crossfade.clamp(0.0, max_crossfade(cuts, i).max(0.0));
    }
}

/// The main track and the video track over it as one sequence, the way the
/// export and the player play them: the main track's cuts in pieces around
/// where the track above covers them, and that track's cuts between, each
/// overlapping only the piece before it (its `crossfade`, through its
/// `transition`), so the machinery for a single track's crossfades plays it.
#[derive(Debug, Default)]
pub struct Flat {
    pub cuts: Vec<Cut>,
    /// Each piece's cut: (track, index), track 0 the main one.
    pub source: Vec<(usize, usize)>,
    /// The upper track's cuts as they play, for the sound.
    pub upper: Vec<Upper>,
    /// Where the main track's sound runs on under the upper track: the
    /// stretches of its cuts no piece plays.
    pub under: Vec<Under>,
}

/// An upper-track cut as it plays: from `at` for `len` seconds on the edit,
/// its sound faded in and out (equal power) over its transitions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Upper {
    pub cut: usize,
    pub at: f64,
    pub len: f64,
    pub fade_in: f64,
    pub fade_out: f64,
}

/// A stretch of main-track cut `cut`'s sound: from `at` on the edit, `len`
/// seconds of its clip from clip time `from`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Under {
    pub cut: usize,
    pub at: f64,
    pub from: f64,
    pub len: f64,
}

/// What's beneath an upper cut's transition must be one main-track cut, so
/// that no more than two pictures are ever mixed: the transitions are
/// shortened to stop short of the main track's cut changes and crossfades,
/// and none where an upper cut touches another.
pub fn flatten(main: &[Cut], upper: &[Cut]) -> Flat {
    const E: f64 = 1e-9;
    let offs = offsets(main);
    let length = edit_length(main);
    // The main track's cut changes: a crossfade's stretch, or a point.
    let changes: Vec<(f64, f64)> = (1..main.len()).map(|i| (offs[i], offs[i] + main[i].crossfade)).collect();
    let mut order: Vec<usize> = (0..upper.len()).filter(|&k| upper[k].at.unwrap_or(0.0) < length - E).collect();
    order.sort_by(|&x, &y| upper[x].at.unwrap_or(0.0).total_cmp(&upper[y].at.unwrap_or(0.0)));
    let span = |k: usize| {
        let a = upper[k].at.unwrap_or(0.0).max(0.0);
        (a, (a + upper[k].len()).min(length))
    };
    // Each upper cut: its span and transitions, as they can be.
    let mut ups: Vec<(usize, f64, f64, f64, f64)> = Vec::new();
    for (n, &k) in order.iter().enumerate() {
        let (a, b) = span(k);
        let near_before = n > 0 && span(order[n - 1]).1 > a - SOLO;
        let near_after = order.get(n + 1).is_some_and(|&j| span(j).0 < b + SOLO);
        let mut lin = if near_before { 0.0 } else { upper[k].crossfade.max(0.0) };
        let mut lout = if near_after { 0.0 } else { upper[k].out_crossfade.max(0.0) };
        for &(r0, r1) in &changes {
            if r1 > a + E && r0 < a + lin - E {
                lin = if r0 > a + E { lin.min(r0 - a) } else { 0.0 };
            }
            if r0 < b - E && r1 > b - lout + E {
                lout = if r1 < b - E { lout.min(b - r1) } else { 0.0 };
            }
        }
        let room = b - a - SOLO;
        if lin + lout > room {
            let k = (room / (lin + lout)).max(0.0);
            (lin, lout) = (lin * k, lout * k);
        }
        ups.push((k, a, b, lin, lout));
    }
    // Where the upper track hides the main one.
    let opaque: Vec<(f64, f64)> = ups.iter().map(|&(_, a, b, lin, lout)| (a + lin, b - lout)).collect();
    // (start, end, piece, source)
    let mut items: Vec<(f64, f64, Cut, (usize, usize))> = Vec::new();
    for (i, cut) in main.iter().enumerate() {
        let (s, e) = (offs[i], offs[i] + cut.len());
        let mut from = s;
        let mut visible = Vec::new();
        for &(o0, o1) in &opaque {
            if o1 <= from || o0 >= e {
                continue;
            }
            if o0 > from {
                visible.push((from, o0));
            }
            from = from.max(o1);
        }
        if from < e {
            visible.push((from, e));
        }
        for (p, q) in visible.into_iter().filter(|(p, q)| q - p > 1e-6) {
            let piece = Cut { start: cut.start + (p - s), end: cut.start + (q - s), track: 0, source_end: Some(cut.end), ..cut.clone() };
            items.push((p, q, piece, (0, i)));
        }
    }
    for &(k, a, b, ..) in &ups {
        let piece = Cut { end: upper[k].start + (b - a), track: 1, source_end: Some(upper[k].end), ..upper[k].clone() };
        items.push((a, b, piece, (1, k)));
    }
    items.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
    // A piece left no moment of its own, or under another, goes: the one
    // after it takes over (a cut on the upper track ending in the middle of
    // a crossfade on the main one shows its later cut).
    let overlap = |items: &[(f64, f64, Cut, (usize, usize))], j: usize| if j == 0 { 0.0 } else { (items[j - 1].1 - items[j].0).max(0.0) };
    'settle: loop {
        for j in 0..items.len() {
            let swallowed = j > 0 && items[j].1 <= items[j - 1].1 + E;
            let crowded = items.get(j + 1).is_some_and(|next| next.0 <= items[j].0 + overlap(&items, j) + E);
            if swallowed || crowded {
                items.remove(j);
                continue 'settle;
            }
        }
        break;
    }
    let mut flat = Flat::default();
    for j in 0..items.len() {
        let crossfade = overlap(&items, j);
        let (_, _, mut piece, source) = items[j].clone();
        piece.crossfade = crossfade;
        piece.transition = match (source.0, j.checked_sub(1).map(|p| &items[p])) {
            (0, Some((.., before, (1, _)))) => before.out_transition,
            _ => piece.transition,
        };
        if crossfade == 0.0 {
            piece.transition = Transition::default();
        }
        piece.out_crossfade = 0.0;
        flat.cuts.push(piece);
        flat.source.push(source);
    }
    for &(k, a, b, lin, lout) in &ups {
        let Some(j) = flat.source.iter().position(|&s| s == (1, k)) else { continue };
        // Over another upper cut, the sound crossfades through their overlap.
        let after = flat.cuts.get(j + 1).filter(|_| flat.source[j + 1].0 == 1).map(|c| c.crossfade);
        let fade_in = if j > 0 && flat.source[j - 1].0 == 1 { flat.cuts[j].crossfade } else { lin };
        flat.upper.push(Upper { cut: k, at: a, len: b - a, fade_in, fade_out: after.unwrap_or(lout) });
    }
    // The main track's sound wherever its own pieces don't play it.
    let starts = offsets(&flat.cuts);
    for (i, cut) in main.iter().enumerate() {
        let (s, e) = (offs[i], offs[i] + cut.len());
        let mut from = s;
        let mut gaps = Vec::new();
        for (j, piece) in flat.cuts.iter().enumerate().filter(|(j, _)| flat.source[*j] == (0, i)) {
            let (p, q) = (starts[j], starts[j] + piece.len());
            if p > from {
                gaps.push((from, p));
            }
            from = from.max(q);
        }
        if from < e {
            gaps.push((from, e));
        }
        for (p, q) in gaps.into_iter().filter(|(p, q)| q - p > 1e-6) {
            flat.under.push(Under { cut: i, at: p, from: cut.start + (p - s), len: q - p });
        }
    }
    flat
}

impl Project {
    /// How far faded to black cut `n` is at clip time `t`, 0..1.
    pub fn black_at(&self, n: usize, t: f64) -> f64 {
        let at = offsets(&self.cuts)[n] + t - self.cuts[n].start;
        self.fades.black(at, self.duration())
    }

    pub fn load(path: &Path) -> Result<Project> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut project: Project = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        // Music alone is a start: its length is the frame to fill.
        if project.cuts.is_empty() && project.music.clips.is_empty() {
            bail!("{} has no cuts", path.display());
        }
        let dir = path.parent().unwrap_or(Path::new(""));
        if project.video.len() > 1 {
            bail!("{} has {} video tracks over the main one; this version plays one", path.display(), project.video.len());
        }
        for track in &mut project.video {
            for (n, cut) in track.cuts.iter_mut().enumerate() {
                match cut.at {
                    Some(at) if at.is_finite() && at >= 0.0 => {}
                    _ => bail!("cut {} on the upper video track needs a place on the edit (\"at\", 0 or later)", n + 1),
                }
                if !cut.out_crossfade.is_finite() {
                    bail!("an out crossfade isn't a number");
                }
            }
            track.cuts.sort_by(|a, b| a.at.unwrap_or(0.0).total_cmp(&b.at.unwrap_or(0.0)));
        }
        let video = project.video.iter_mut().flat_map(|t| &mut t.cuts);
        for (n, cut) in project.cuts.iter_mut().chain(video).enumerate() {
            if cut.end <= cut.start {
                bail!("cut {} ends ({} s) before it starts ({} s)", n + 1, cut.end, cut.start);
            }
            cut.input = dir.join(&cut.input);
            for track in cut.keyframes.tracks_mut() {
                *track = dir.join(&*track);
            }
            if let Some(lut) = cut.lut.as_mut().filter(|l| crate::lut::is_file(l)) {
                *lut = dir.join(&*lut).to_string_lossy().into_owned();
            }
            if let Some(strength) = cut.old_lut_strength.take().filter(|s| *s != 1.0) {
                if cut.effects.is_empty() {
                    cut.effects.push(crate::grade::EffectKey { t: cut.start, grade: Default::default(), ease: Default::default() });
                }
                for key in &mut cut.effects {
                    key.grade.lut = strength.clamp(0.0, 1.0);
                }
            }
        }
        for (n, clip) in project.music.clips.iter_mut().enumerate() {
            if clip.end <= clip.start || clip.at < 0.0 {
                bail!("music clip {} is empty or starts before the edit", n + 1);
            }
            clip.input = dir.join(&clip.input);
        }
        for (n, item) in project.overlays.iter_mut().chain(&mut project.text).flat_map(|l| &mut l.items).enumerate() {
            if item.end <= item.start || item.at < 0.0 {
                bail!("overlay {} is empty or starts before the edit", n + 1);
            }
            if item.text.is_none() == item.input.as_os_str().is_empty() {
                bail!("overlay {} needs a picture (\"input\") or \"text\", one of them", n + 1);
            }
            if item.motion.iter().any(|k| !k.t.is_finite() || k.place.values().iter().any(|v| !v.is_finite())) {
                bail!("an overlay's motion keyframe isn't a number");
            }
            if item.effects.iter().any(|k| !k.t.is_finite() || k.grade.values().iter().any(|v| !v.is_finite())) {
                bail!("an overlay's effect keyframe isn't a number");
            }
            if item.text.is_none() {
                item.input = dir.join(&item.input);
            }
            item.motion.sort_by(|a, b| a.t.total_cmp(&b.t));
            item.effects.sort_by(|a, b| a.t.total_cmp(&b.t));
        }
        if project.all_cuts().any(|c| !c.crossfade.is_finite()) {
            bail!("a crossfade isn't a number");
        }
        settle_crossfades(&mut project.cuts);
        let video = project.video.iter_mut().flat_map(|t| &mut t.cuts);
        let keys = project.cuts.iter_mut().chain(video).map(|c| &mut c.volume).chain([&mut project.music.keyframes]);
        for keys in keys {
            if keys.iter().any(|k| !k.t.is_finite() || !k.volume.is_finite()) {
                bail!("a volume point isn't a number");
            }
            keys.sort_by(|a, b| a.t.total_cmp(&b.t));
        }
        let video = project.video.iter_mut().flat_map(|t| &mut t.cuts);
        for cut in project.cuts.iter_mut().chain(video) {
            if cut.effects.iter().any(|k| !k.t.is_finite() || k.grade.values().iter().any(|v| !v.is_finite())) {
                bail!("an effect keyframe isn't a number");
            }
            cut.effects.sort_by(|a, b| a.t.total_cmp(&b.t));
        }
        Ok(project)
    }

    /// Writes the project as JSON. Clips in the project's folder or below
    /// are stored relative to it, so the folder can move as a whole.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().unwrap_or(Path::new(""));
        let mut project = self.clone();
        let video = project.video.iter_mut().flat_map(|t| &mut t.cuts);
        for cut in project.cuts.iter_mut().chain(video) {
            if let Ok(rel) = cut.input.strip_prefix(dir) {
                cut.input = rel.to_owned();
            }
            for track in cut.keyframes.tracks_mut() {
                if let Ok(rel) = track.strip_prefix(dir) {
                    *track = rel.to_owned();
                }
            }
            if let Some(lut) = cut.lut.as_mut().filter(|l| crate::lut::is_file(l))
                && let Ok(rel) = Path::new(lut.as_str()).strip_prefix(dir)
            {
                *lut = rel.to_string_lossy().into_owned();
            }
        }
        for clip in &mut project.music.clips {
            if let Ok(rel) = clip.input.strip_prefix(dir) {
                clip.input = rel.to_owned();
            }
        }
        for item in project.overlays.iter_mut().chain(&mut project.text).flat_map(|l| &mut l.items) {
            if let Ok(rel) = item.input.strip_prefix(dir) {
                item.input = rel.to_owned();
            }
        }
        crate::replace_file(path, (serde_json::to_string_pretty(&project)? + "\n").as_bytes())
    }

    /// Length of the edit in seconds: the main track's.
    pub fn duration(&self) -> f64 {
        edit_length(&self.cuts)
    }

    /// The upper video track's cuts, if it has any.
    pub fn upper(&self) -> &[Cut] {
        self.video.first().map_or(&[], |t| &t.cuts)
    }

    /// Every cut, the main track's first.
    pub fn all_cuts(&self) -> impl Iterator<Item = &Cut> {
        self.cuts.iter().chain(self.video.iter().flat_map(|t| &t.cuts))
    }

    /// The edit as one sequence (`flatten`).
    pub fn flatten(&self) -> Flat {
        flatten(&self.cuts, self.upper())
    }
}

/// The keyframes that frame cut `n` of `cuts` (in edit order) at clip time
/// `t`, and the time to read them at. A 360 cut without keyframes holds the
/// view the edit had last: the end of the nearest earlier 360 cut with some,
/// as the player shows it when playing on. An ordinary video without them
/// shows its whole picture (a view's angles mean another thing there), so
/// neither kind holds the other's. `None` before any keyframe.
pub fn keyframed(cuts: &[Cut], n: usize, t: f64) -> Option<(&Keyframes, f64)> {
    if !cuts[n].keyframes.is_empty() {
        return Some((&cuts[n].keyframes, t));
    }
    if cuts[n].flat {
        return None;
    }
    // Within its own track: a cut over the main track holds nothing of it,
    // nor passes anything on to it.
    let track = cuts[n].track;
    cuts[..n].iter().rev().find(|c| c.track == track && !c.flat && !c.keyframes.is_empty()).map(|c| (&c.keyframes, c.source_end.unwrap_or(c.end)))
}

/// The view of cut `n` at clip time `t` (see `keyframed`), or straight ahead
/// before any keyframe. `aim` is where following keyframes point in this
/// cut's clip at `t`, so a cut that holds an earlier cut's following view
/// goes on following in its own footage.
pub fn camera_at(cuts: &[Cut], n: usize, t: f64, aim: Aim) -> Camera {
    let camera = keyframed(cuts, n, t).and_then(|(k, at)| k.camera_at(at, &held(aim, at == t, t))).unwrap_or_else(|| View::home(Projection::Perspective).camera());
    Camera { grade: crate::grade::grade_at(&cuts[n].effects, t), lut: cuts[n].lut_applied, ..camera }
}

/// `camera`, cut `n`'s at clip time `t`, turned and smeared as a spin
/// into it or out of it has it there; as it is anywhere else.
pub fn spun(cuts: &[Cut], n: usize, t: f64, camera: Camera) -> Camera {
    let cut = &cuts[n];
    let turn = if cut.crossfade > 0.0 && t < cut.start + cut.crossfade {
        cut.transition.spin((t - cut.start) / cut.crossfade, cut.crossfade).map(|s| s[1])
    } else {
        cuts.get(n + 1).filter(|next| next.crossfade > 0.0 && t > cut.end - next.crossfade).and_then(|next| next.transition.spin((t - (cut.end - next.crossfade)) / next.crossfade, next.crossfade).map(|s| s[0]))
    };
    turn.map_or(camera, |(angle, blur)| camera.spun(angle, blur))
}

/// `aim` for reading keyframes from `keyframed`: as asked in the cut's own
/// keyframes, else always at the cut's time `t`, as the keyframes held
/// from an earlier cut are read at that cut's end.
pub fn held(aim: Aim<'_>, own: bool, t: f64) -> impl Fn(&crate::view::Follow, f64) -> Option<(f64, f64)> + '_ {
    move |f, at| aim(f, if own { at } else { t })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_cuts_relative_to_the_file() {
        let dir = std::env::temp_dir().join(format!("rosmo360-project-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.json");
        std::fs::write(
            &path,
            r#"{"aspect": "9:16", "cuts": [
                {"input": "a.OSV", "in": 28, "out": 37, "keyframes": [{"t": 28, "yaw": -20}, {"t": 37, "projection": "little-planet"}, {"t": 37.5, "follow": "friend.csv"}]},
                {"input": "/clips/b.OSV", "in": 5, "out": 12, "stabilize": "lock", "note": "the pan"}
            ]}"#,
        )
        .unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!(p.cuts[0].input, dir.join("a.OSV"));
        assert_eq!(p.cuts[1].input, PathBuf::from("/clips/b.OSV"));
        assert_eq!(p.duration(), 16.0);
        assert_eq!(p.cuts[1].keyframes.len(), 0);
        assert_eq!((p.cuts[0].stabilize, p.cuts[1].stabilize), (None, Some(Mode::Lock)));
        assert_eq!((p.cuts[0].note.as_deref(), p.cuts[1].note.as_deref()), (None, Some("the pan")));
        assert_eq!(p.cuts[0].keyframes[1].view.projection, Projection::LittlePlanet);
        assert_eq!(p.cuts[0].keyframes[2].follow, Some(crate::view::Follow::Track(dir.join("friend.csv"))));
        let saved = dir.join("again.json");
        p.save(&saved).unwrap();
        let text = std::fs::read_to_string(&saved).unwrap();
        assert!(text.contains(r#""input": "a.OSV""#) && text.contains(r#""follow": "friend.csv""#));
        assert_eq!(Project::load(&saved).unwrap(), p);
        std::fs::write(&path, r#"{"cuts": [{"input": "a.OSV", "in": 5, "out": 2}]}"#).unwrap();
        assert!(Project::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A project saved while the LUT's strength was the cut's own opens,
    /// the strength moved into its effects; it isn't written back.
    #[test]
    fn old_lut_strength_becomes_an_effect() {
        let dir = std::env::temp_dir().join(format!("rosmo360-lut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.json");
        std::fs::write(
            &path,
            r#"{"cuts": [{"input": "a.OSV", "in": 2, "out": 5, "lut": "x", "lut_strength": 0.84},
                         {"input": "a.OSV", "in": 6, "out": 9, "lut": "x", "lut_strength": 0.5, "effects": [{"t": 7, "exposure": 1}, {"t": 8}]}]}"#,
        )
        .unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!(p.cuts[0].effects.iter().map(|k| (k.t, k.grade.lut)).collect::<Vec<_>>(), [(2.0, 0.84)]);
        assert_eq!(p.cuts[1].effects.iter().map(|k| (k.grade.exposure, k.grade.lut)).collect::<Vec<_>>(), [(1.0, 0.5), (0.0, 0.5)]);
        p.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("lut_strength") && text.contains(r#""lut": 0.84"#));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn music_sits_on_the_edit_relative_to_the_file() {
        let dir = std::env::temp_dir().join(format!("rosmo360-music-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edit.json");
        // Music alone is a project: the song is the frame to fill.
        std::fs::write(&path, r#"{"music": {"volume": -3, "clips": [{"input": "song.mp3", "at": 2, "in": 60, "out": 200, "fade_in": 1.5}]}}"#).unwrap();
        let p = Project::load(&path).unwrap();
        assert_eq!((p.cuts.len(), p.sound.volume, p.music.volume), (0, 0.0, -3.0));
        assert_eq!(p.music.clips[0].input, dir.join("song.mp3"));
        assert_eq!(p.music.end(), 142.0);
        let keys = [VolumeKey { t: 10.0, volume: -6.0 }, VolumeKey { t: 20.0, volume: 0.0 }];
        assert_eq!([5.0, 10.0, 15.0, 25.0].map(|t| volume_at(&keys, t)), [-6.0, -6.0, -3.0, 0.0]);
        assert_eq!(volume_at(&[], 3.0), 0.0);
        p.save(&dir.join("again.json")).unwrap();
        let text = std::fs::read_to_string(dir.join("again.json")).unwrap();
        assert!(text.contains(r#""input": "song.mp3""#) && !text.contains("fade_out") && !text.contains("sound"));
        assert_eq!(Project::load(&dir.join("again.json")).unwrap(), p);
        // Without music or sound settings nothing is written for them.
        assert!(!serde_json::to_string(&Project::default()).unwrap().contains("music"));
        std::fs::write(&path, r#"{"music": {"clips": [{"input": "song.mp3", "at": 0, "in": 9, "out": 9}]}}"#).unwrap();
        assert!(Project::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn music_clips_split_in_place_and_never_overlap() {
        let clip = |at: f64, start: f64, end: f64| MusicClip { input: "song.mp3".into(), at, start, end, fade_in: 1.0, fade_out: 2.0 };
        let mut m = Music { clips: vec![clip(0.0, 0.0, 30.0)], ..Default::default() };
        assert!(m.split(0, 10.0, 0.1));
        assert_eq!((m.clips[0].end, m.clips[0].fade_out), (10.0, 0.0));
        assert_eq!((m.clips[1].at, m.clips[1].start, m.clips[1].end, m.clips[1].fade_in, m.clips[1].fade_out), (10.0, 10.0, 30.0, 0.0, 2.0));
        assert!(!m.split(0, 10.05, 0.1));
        // The second half pushed onto the first stops against it (there's
        // no room before it); the first dragged over the second stays before
        // it until it's nearer the far side.
        assert_eq!(m.place(1, 5.0), 10.0);
        assert_eq!(m.place(0, 12.0), 0.0);
        assert_eq!(m.place(0, 18.0), 30.0);
        m.clips[1].at = 40.0;
        assert_eq!(m.place(0, 35.0), 30.0);
        // A crossfade is a partial overlap of two, made only on purpose.
        assert!(!m.fits(0, 35.0, 10.0, false));
        assert!(m.fits(0, 35.0, 10.0, true));
        assert_eq!(m.crossfades(1), (0.0, 0.0));
        m.clips[0].at = 35.0;
        assert_eq!((m.crossfades(0), m.crossfades(1)), ((0.0, 5.0), (5.0, 0.0)));
        // Not inside the other, nor over a third.
        assert!(!m.fits(0, 45.0, 10.0, true));
        m.clips.push(clip(58.0, 0.0, 10.0));
        assert!(!m.fits(2, 50.0, 10.0, true));
        assert!(m.fits(2, 46.0, 30.0, true));
        assert!(!m.fits(2, 43.0, 30.0, true));
        // An edge dragged into a neighbour without Shift stops against it.
        let len = m.clips[2].end - m.clips[2].start;
        let at = Music::reach(58.0, 40.0, |at| m.fits(2, at, len, false));
        assert!((at - 58.0).abs() < 1e-6, "{at}");
        m.clips[2].at = 70.0;
        let at = Music::reach(70.0, 40.0, |at| m.fits(2, at, len, false));
        assert!((at - 60.0).abs() < 1e-6, "{at}");
    }

    fn cut(name: &str, start: f64, end: f64) -> Cut {
        Cut { input: name.into(), start, end, ..Default::default() }
    }

    fn over(name: &str, at: f64, start: f64, end: f64, fade_in: f64, fade_out: f64) -> Cut {
        Cut { at: Some(at), crossfade: fade_in, out_crossfade: fade_out, ..cut(name, start, end) }
    }

    /// (source, edit start, clip in, clip out, crossfade) of each piece.
    fn pieces(f: &Flat) -> Vec<((usize, usize), f64, f64, f64, f64)> {
        let at = offsets(&f.cuts);
        f.cuts.iter().zip(&f.source).zip(at).map(|((c, &s), at)| (s, at, c.start, c.end, c.crossfade)).collect()
    }

    #[test]
    fn an_upper_cut_covers_the_main_track() {
        // Without one the main track plays as it is.
        let main = [cut("a", 0.0, 4.0), Cut { crossfade: 1.0, ..cut("b", 2.0, 6.0) }];
        let f = flatten(&main, &[]);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 4.0, 0.0), ((0, 1), 3.0, 2.0, 6.0, 1.0)]);
        assert!(f.under.is_empty() && f.upper.is_empty());
        // Over the middle of a cut: in over 1 s, out over 0.5 s, the main
        // cut's sound running on beneath where it's hidden.
        let main = [cut("a", 0.0, 10.0)];
        let up = [Cut { out_transition: Transition { style: crate::transition::Style::DipBlack, ..Default::default() }, ..over("u", 3.0, 20.0, 24.0, 1.0, 0.5) }];
        let f = flatten(&main, &up);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 4.0, 0.0), ((1, 0), 3.0, 20.0, 24.0, 1.0), ((0, 0), 6.5, 6.5, 10.0, 0.5)]);
        assert_eq!(f.cuts[2].transition, up[0].out_transition);
        assert_eq!((f.cuts[1].track, f.cuts[2].track), (1, 0));
        assert_eq!(f.under, [Under { cut: 0, at: 4.0, from: 4.0, len: 2.5 }]);
        assert_eq!(f.upper, [Upper { cut: 0, at: 3.0, len: 4.0, fade_in: 1.0, fade_out: 0.5 }]);
        assert!((edit_length(&f.cuts) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn upper_transitions_keep_to_one_main_cut() {
        // A main cut change 0.5 s into the transition in: shortened to it.
        let main = [cut("a", 0.0, 5.0), cut("b", 0.0, 5.0)];
        let f = flatten(&main, &[over("u", 4.5, 0.0, 2.0, 1.0, 1.0)]);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 5.0, 0.0), ((1, 0), 4.5, 0.0, 2.0, 0.5), ((0, 1), 5.5, 0.5, 5.0, 1.0)]);
        assert_eq!(f.under, [Under { cut: 1, at: 5.0, from: 0.0, len: 0.5 }]);
        // Two upper cuts overlapping crossfade into each other, the main
        // track hidden from the first's start to the second's end.
        let f = flatten(&[cut("a", 0.0, 10.0)], &[over("u", 2.0, 0.0, 4.0, 0.0, 0.5), over("v", 5.0, 0.0, 3.0, 0.5, 0.0)]);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 2.0, 0.0), ((1, 0), 2.0, 0.0, 4.0, 0.0), ((1, 1), 5.0, 0.0, 3.0, 1.0), ((0, 0), 8.0, 8.0, 10.0, 0.0)]);
        assert_eq!(f.upper.iter().map(|u| (u.fade_in, u.fade_out)).collect::<Vec<_>>(), [(0.0, 1.0), (1.0, 0.0)]);
        assert_eq!(f.under, [Under { cut: 0, at: 2.0, from: 2.0, len: 6.0 }]);
        // Past the edit's end it's cut off there.
        let f = flatten(&[cut("a", 0.0, 10.0)], &[over("u", 8.0, 0.0, 5.0, 0.0, 0.0), over("w", 11.0, 0.0, 1.0, 0.0, 0.0)]);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 8.0, 0.0), ((1, 0), 8.0, 0.0, 2.0, 0.0)]);
    }

    #[test]
    fn an_upper_cut_ending_in_a_main_crossfade_shows_its_later_cut() {
        let main = [cut("a", 0.0, 6.0), Cut { crossfade: 2.0, ..cut("b", 0.0, 6.0) }];
        let f = flatten(&main, &[over("u", 1.0, 0.0, 4.0, 0.0, 0.0)]);
        assert_eq!(pieces(&f), [((0, 0), 0.0, 0.0, 1.0, 0.0), ((1, 0), 1.0, 0.0, 4.0, 0.0), ((0, 1), 5.0, 1.0, 6.0, 0.0)]);
        assert!((edit_length(&f.cuts) - 10.0).abs() < 1e-9);
        // Its held view comes from its own track only.
        let mut pair = [Cut { keyframes: serde_json::from_str(r#"[{"t": 0, "yaw": 50}]"#).unwrap(), ..cut("a", 0.0, 6.0) }, Cut { track: 1, ..cut("u", 0.0, 4.0) }];
        assert_eq!(keyframed(&pair, 1, 2.0), None);
        pair[1].track = 0;
        assert!(keyframed(&pair, 1, 2.0).is_some());
    }

    #[test]
    fn cuts_without_keyframes_hold_the_last_view() {
        let p: Project = serde_json::from_str(
            r#"{"cuts": [
                {"input": "a.OSV", "in": 0, "out": 4},
                {"input": "a.OSV", "in": 10, "out": 20, "keyframes": [{"t": 10, "yaw": 0}, {"t": 14, "yaw": 40}, {"t": 30, "yaw": 200}]},
                {"input": "b.OSV", "in": 50, "out": 60},
                {"input": "b.OSV", "in": 70, "out": 80}
            ]}"#,
        )
        .unwrap();
        let yaw = |n, t| camera_at(&p.cuts, n, t, &|_, _| None).user;
        assert_eq!(yaw(0, 2.0), View::home(Projection::Perspective).camera().user);
        assert_eq!(keyframed(&p.cuts, 1, 12.0).map(|k| k.1), Some(12.0));
        // An ordinary video in between holds nothing and passes nothing on.
        let mut mixed = p.cuts.clone();
        mixed[2].flat = true;
        assert_eq!(keyframed(&mixed, 2, 55.0), None);
        assert_eq!(keyframed(&mixed, 3, 75.0).map(|k| k.1), Some(20.0));
        // The view at the earlier cut's out point, not at its last keyframe.
        let held = p.cuts[1].keyframes.camera_at(20.0, &|_, _| None).unwrap().user;
        assert_eq!((yaw(2, 55.0), yaw(3, 75.0)), (held, held));
    }
}
