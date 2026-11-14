//! The timeline's edit: cuts played one after another, the music under them
//! and the mix, with undo. Plain data, so the edits can be checked without a
//! window.
//!
//! Edit time runs along the timeline from 0 to the edit's length; each cut
//! covers its stretch of it, and clip time is where in its own clip that is.
//! A cut with a crossfade starts that much before the one before it ends;
//! through the overlap the earlier cut is the one located, the later one
//! dissolving in over it.

use crate::overlay::{Item, ItemRef, Lane, Track};
use crate::project::{Cut, Fades, Music, Sound};

/// The shortest a split or trim may leave a cut, in seconds.
pub const MIN_CUT: f64 = 0.2;

/// What undo brings back: everything an edit changes.
type Snapshot = (Vec<Cut>, Music, Sound, Fades, Vec<Lane>, Vec<Lane>);

#[derive(Default)]
pub struct Edit {
    pub cuts: Vec<Cut>,
    pub music: Music,
    pub sound: Sound,
    pub fades: Fades,
    /// The overlay lanes and the text's; the player shows and edits the
    /// first of each.
    pub overlays: Vec<Lane>,
    pub texts: Vec<Lane>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Counts changes, so the app can tell whether the edit was saved.
    pub version: u64,
}

impl Edit {
    pub fn duration(&self) -> f64 {
        crate::project::edit_length(&self.cuts)
    }

    /// The timeline's length: the video's, or the music's while it runs on
    /// beyond it (the frame still to fill). The export ends with the video.
    pub fn frame(&self) -> f64 {
        let overlays = self.lanes().flat_map(|l| &l.items).map(Item::ends).fold(0.0, f64::max);
        self.duration().max(self.music.end()).max(overlays)
    }

    /// Where cut `i` starts on the timeline (the edit's end for `i` past
    /// the last).
    pub fn offset(&self, i: usize) -> f64 {
        match self.cuts.get(i) {
            Some(_) => crate::project::offsets(&self.cuts[..=i])[i],
            None => self.duration(),
        }
    }

    /// The cut at edit time `t` and the clip time there. The end of the edit
    /// is the last cut's out point.
    pub fn locate(&self, t: f64) -> Option<(usize, f64)> {
        let offsets = crate::project::offsets(&self.cuts);
        for (i, (c, from)) in self.cuts.iter().zip(offsets).enumerate() {
            if t < from + c.len() || i == self.cuts.len() - 1 {
                return Some((i, (c.start + (t - from).max(0.0)).min(c.end)));
            }
        }
        None
    }

    /// Where the crossfade into the cut after cut `i` begins, clip time in
    /// cut `i`, if it has one.
    pub fn crossfade_from(&self, i: usize) -> Option<f64> {
        let next = self.cuts.get(i + 1).filter(|n| n.crossfade > 0.0)?;
        Some(self.cuts[i].end - next.crossfade)
    }

    pub fn edit_time(&self, i: usize, clip_t: f64) -> f64 {
        self.offset(i) + (clip_t - self.cuts[i].start)
    }

    fn snapshot(&self) -> Snapshot {
        (self.cuts.clone(), self.music.clone(), self.sound, self.fades, self.overlays.clone(), self.texts.clone())
    }

    fn restore(&mut self, (cuts, music, sound, fades, overlays, texts): Snapshot) -> Snapshot {
        let now = self.snapshot();
        (self.cuts, self.music, self.sound, self.fades, self.overlays, self.texts) = (cuts, music, sound, fades, overlays, texts);
        now
    }

    /// Remembers the edit as it is, for undo; call before each change.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        self.redo.clear();
        self.version += 1;
    }

    pub fn undo(&mut self) -> bool {
        let Some(then) = self.undo.pop() else { return false };
        let now = self.restore(then);
        self.redo.push(now);
        self.version += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(then) = self.redo.pop() else { return false };
        let now = self.restore(then);
        self.undo.push(now);
        self.version += 1;
        true
    }

    /// Starts over with `cuts` and their music, mix and overlays (an opened
    /// or new project), forgetting undo.
    pub fn replace(&mut self, cuts: Vec<Cut>, music: Music, sound: Sound, fades: Fades, overlays: Vec<Lane>, texts: Vec<Lane>) {
        self.overlays = overlays;
        self.texts = texts;
        self.cuts = cuts;
        self.music = music;
        self.sound = sound;
        self.fades = fades;
        self.undo.clear();
        self.redo.clear();
        self.version += 1;
    }

    /// Every overlay lane, lowest first: the overlays', then the text's.
    pub fn lanes(&self) -> impl Iterator<Item = &Lane> {
        self.overlays.iter().chain(&self.texts)
    }

    /// The lane of `track` the player shows, if there is one yet.
    pub fn lane(&self, track: Track) -> Option<&Lane> {
        match track {
            Track::Overlay => self.overlays.first(),
            Track::Text => self.texts.first(),
        }
    }

    /// The lane of `track` the player shows, made if there's none yet.
    pub fn lane_mut(&mut self, track: Track) -> &mut Lane {
        let lanes = match track {
            Track::Overlay => &mut self.overlays,
            Track::Text => &mut self.texts,
        };
        if lanes.is_empty() {
            lanes.push(Lane::default());
        }
        &mut lanes[0]
    }

    pub fn item(&self, (track, i): ItemRef) -> Option<&Item> {
        self.lane(track)?.items.get(i)
    }

    pub fn item_mut(&mut self, (track, i): ItemRef) -> Option<&mut Item> {
        match track {
            Track::Overlay => self.overlays.first_mut(),
            Track::Text => self.texts.first_mut(),
        }?
        .items
        .get_mut(i)
    }

    /// Splits cut `i` at clip time `t`. Both halves keep all its keyframes,
    /// so the view runs on across the split unchanged, and each half can be
    /// reframed on its own from then on.
    pub fn can_split(&self, i: usize, t: f64) -> bool {
        self.cuts.get(i).is_some_and(|c| t >= c.start + MIN_CUT && t <= c.end - MIN_CUT)
    }

    pub fn split(&mut self, i: usize, t: f64) -> bool {
        if !self.can_split(i, t) {
            return false;
        }
        let c = &self.cuts[i];
        let mut second = c.clone();
        second.start = t;
        second.crossfade = 0.0;
        self.cuts[i].end = t;
        self.cuts.insert(i + 1, second);
        crate::project::settle_crossfades(&mut self.cuts);
        true
    }

    /// Moves cut `i`'s in point (`start`) or out point to clip time `t`,
    /// kept within the clip (`0..clip_len`) and at least `MIN_CUT` long.
    pub fn trim(&mut self, i: usize, start: bool, t: f64, clip_len: f64) {
        let c = &mut self.cuts[i];
        // Not `clamp`: it panics where the limits cross, as they do for a
        // cut shorter than `MIN_CUT` (a very short clip, a hand-written
        // project) or one reaching past its clip's end.
        if start {
            c.start = t.min(c.end - MIN_CUT).max(0.0);
        } else {
            c.end = t.max(c.start + MIN_CUT).min(clip_len.max(c.start));
        }
        crate::project::settle_crossfades(&mut self.cuts);
    }

    /// Sets the crossfade into cut `i`, within what the cuts either side
    /// allow.
    pub fn set_crossfade(&mut self, i: usize, seconds: f64) {
        let mut max = crate::project::max_crossfade(&self.cuts, i);
        // Leaving the cut what the next one's crossfade takes of its end,
        // or three cuts overlapped and the next one's was dropped (and the
        // edit a second longer) when the project was opened again.
        if let (Some(c), Some(next)) = (self.cuts.get(i), self.cuts.get(i + 1)) {
            max = max.min(c.len() - next.crossfade - crate::project::SOLO);
        }
        let max = max.max(0.0);
        if let Some(c) = self.cuts.get_mut(i) {
            c.crossfade = seconds.clamp(0.0, max);
        }
    }

    pub fn remove(&mut self, i: usize) {
        self.cuts.remove(i);
        crate::project::settle_crossfades(&mut self.cuts);
    }

    /// Moves cut `from` so it plays at position `to` (counted before the move).
    pub fn move_cut(&mut self, from: usize, to: usize) {
        let cut = self.cuts.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.cuts.insert(to.min(self.cuts.len()), cut);
        crate::project::settle_crossfades(&mut self.cuts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cut(name: &str, start: f64, end: f64) -> Cut {
        Cut { input: PathBuf::from(name), start, end, keyframes: Default::default(), stabilize: None, note: None, volume: Vec::new(), crossfade: 0.0, effects: Vec::new(), lut: None, old_lut_strength: None, flat: false, lut_applied: None }
    }

    fn edit() -> Edit {
        Edit { cuts: vec![cut("a", 10.0, 14.0), cut("b", 2.0, 5.0)], ..Default::default() }
    }

    #[test]
    fn maps_edit_time_to_cuts() {
        let e = edit();
        assert_eq!(e.duration(), 7.0);
        assert_eq!(e.locate(1.0), Some((0, 11.0)));
        assert_eq!(e.locate(4.5), Some((1, 2.5)));
        assert_eq!(e.locate(9.0), Some((1, 5.0)));
        assert_eq!(e.edit_time(1, 3.0), 5.0);
    }

    #[test]
    fn crossfades_overlap_cuts_and_stay_within_them() {
        let mut e = edit();
        e.set_crossfade(1, 1.0);
        // b starts a second before a ends, and the edit is a second shorter;
        // through the overlap a is the cut located.
        assert_eq!((e.offset(1), e.duration()), (3.0, 6.0));
        assert_eq!(e.locate(3.5), Some((0, 13.5)));
        assert_eq!(e.locate(4.5), Some((1, 3.5)));
        assert_eq!(e.crossfade_from(0), Some(13.0));
        assert_eq!(e.edit_time(1, 2.0), 3.0);
        // Not longer than the cuts allow, none into the first, and a trim
        // that leaves too little shortens it.
        e.set_crossfade(1, 9.0);
        assert!((e.cuts[1].crossfade - 2.9).abs() < 1e-9);
        e.set_crossfade(0, 1.0);
        assert_eq!(e.cuts[0].crossfade, 0.0);
        e.trim(1, false, 3.0, 9.0);
        assert!((e.cuts[1].crossfade - 0.9).abs() < 1e-9);
        // Moved first, it has none.
        e.move_cut(1, 0);
        assert_eq!((e.cuts[0].crossfade, e.duration()), (0.0, 5.0));
    }

    #[test]
    fn splits_trims_moves_and_undoes() {
        let mut e = edit();
        e.checkpoint();
        assert!(e.split(0, 12.0));
        assert!(!e.split(0, 11.9));
        assert_eq!((e.cuts[0].end, e.cuts[1].start, e.cuts.len()), (12.0, 12.0, 3));
        e.trim(2, false, 99.0, 6.0);
        e.trim(1, true, 13.95, 20.0);
        assert_eq!((e.cuts[2].end, e.cuts[1].start), (6.0, 13.8));
        e.move_cut(2, 0);
        assert_eq!(e.cuts[0].input, PathBuf::from("b"));
        e.move_cut(0, 3);
        assert_eq!(e.cuts[2].input, PathBuf::from("b"));
        assert!(e.undo());
        assert_eq!(e.cuts.len(), 2);
        assert!(e.redo());
        assert_eq!(e.cuts.len(), 3);
    }
}
