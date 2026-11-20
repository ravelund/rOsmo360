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
use crate::project::{Cut, Fades, Music, MusicClip, Sound};

/// The shortest a split or trim may leave a cut, in seconds.
pub const MIN_CUT: f64 = 0.2;

/// A cut: its track (0 the main one, 1 the video track over it) and its
/// place in that track's list.
pub type CutRef = (usize, usize);

/// What undo brings back: everything an edit changes.
type Snapshot = (Vec<Cut>, Vec<Cut>, Music, Sound, Fades, Vec<Lane>, Vec<Lane>);

#[derive(Default)]
pub struct Edit {
    /// The main track's cuts, in order.
    pub cuts: Vec<Cut>,
    /// The video track over it: cuts placed on the edit (`Cut::at`), in the
    /// order they were added.
    pub upper: Vec<Cut>,
    /// The two as they play (`project::flatten`), made again after every
    /// change (`reflow`): what playback, the edit's times and `locate` go
    /// by. Without upper cuts it's the main track's cuts.
    pub play: Vec<Cut>,
    /// Each played piece's cut.
    pub source: Vec<CutRef>,
    /// The flattening's sound: the upper cuts as they play and the main
    /// track's sound running on under them.
    pub flat_upper: Vec<crate::project::Upper>,
    pub under: Vec<crate::project::Under>,
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

    /// Plays the tracks as they now are: call after changing them.
    pub fn reflow(&mut self) {
        let flat = crate::project::flatten(&self.cuts, &self.upper);
        (self.play, self.source, self.flat_upper, self.under) = (flat.cuts, flat.source, flat.upper, flat.under);
    }

    pub fn cut(&self, (track, i): CutRef) -> Option<&Cut> {
        if track == 0 { self.cuts.get(i) } else { self.upper.get(i) }
    }

    /// Changes go into the cut's own track; `reflow` brings them to `play`.
    pub fn cut_mut(&mut self, (track, i): CutRef) -> Option<&mut Cut> {
        if track == 0 { self.cuts.get_mut(i) } else { self.upper.get_mut(i) }
    }

    /// Where cut `r` starts on the edit: a main cut's place after the ones
    /// before it, an upper cut's own.
    pub fn start_of(&self, (track, i): CutRef) -> f64 {
        if track == 0 {
            if i < self.cuts.len() { crate::project::offsets(&self.cuts[..=i])[i] } else { self.duration() }
        } else {
            self.upper.get(i).and_then(|c| c.at).unwrap_or(0.0)
        }
    }

    /// The piece playing cut `r` at its clip time `t`, and that time; where
    /// it's hidden there (a main cut under an upper one), what plays at that
    /// moment of the edit.
    pub fn piece_of(&self, r: CutRef, t: f64) -> Option<(usize, f64)> {
        let c = self.cut(r)?;
        let own = (0..self.play.len()).find(|&j| self.source[j] == r && t >= self.play[j].start - 1e-9 && t <= self.play[j].end + 1e-9);
        match own {
            Some(j) => Some((j, t.clamp(self.play[j].start, self.play[j].end))),
            None => self.locate((self.start_of(r) + t - c.start).clamp(0.0, self.duration())),
        }
    }

    /// The upper cuts as music clips, so the music's rules place and trim
    /// them: never over each other but partly, through a crossfade.
    fn upper_spans(&self) -> Music {
        let clips = self.upper.iter().map(|c| MusicClip { input: c.input.clone(), at: c.at.unwrap_or(0.0), start: c.start, end: c.end, fade_in: 0.0, fade_out: 0.0 }).collect();
        Music { clips, ..Default::default() }
    }

    /// Adds `cut` to the upper track where it fits nearest `at`; its index.
    pub fn add_upper(&mut self, mut cut: Cut, at: f64) -> usize {
        cut.at = Some(at);
        cut.track = 1;
        self.upper.push(cut);
        let k = self.upper.len() - 1;
        let at = self.upper_spans().place(k, at);
        self.upper[k].at = Some(at);
        self.reflow();
        k
    }

    /// Moves upper cut `k` towards edit time `want`: stopping against its
    /// neighbours, or past one; with `overlap`, partly over one, which
    /// crossfades the two.
    pub fn move_upper(&mut self, k: usize, want: f64, overlap: bool) {
        let m = self.upper_spans();
        let Some(c) = m.clips.get(k) else { return };
        let len = c.end - c.start;
        let at = if overlap || m.crossfades(k) != (0.0, 0.0) { Music::reach(c.at, want.max(0.0), |at| m.fits(k, at, len, overlap)) } else { m.place(k, want) };
        self.upper[k].at = Some(at);
        self.reflow();
    }

    /// Moves upper cut `k`'s in or out point to clip time `t`, its picture
    /// staying where it is on the edit, short of its neighbours.
    pub fn trim_upper(&mut self, k: usize, start: bool, t: f64, clip_len: f64) {
        let m = self.upper_spans();
        let Some(c) = m.clips.get(k) else { return };
        let to_edit = c.at - c.start;
        let (s, e) = (c.start, c.end);
        if start {
            let want = t.max(s - c.at).max(0.0).min(e - MIN_CUT);
            let s = Music::reach(s, want, |s| m.fits(k, s + to_edit, e - s, false));
            self.upper[k].at = Some(s + to_edit);
            self.upper[k].start = s;
        } else {
            let want = t.min(clip_len).max(s + MIN_CUT);
            self.upper[k].end = Music::reach(e, want, |e| m.fits(k, c.at, e - s, false));
        }
        self.reflow();
    }

    /// Splits upper cut `k` at its clip time `t`, each half staying where it
    /// was on the edit: the first keeps its way in, the second its way out.
    pub fn split_upper(&mut self, k: usize, t: f64) -> bool {
        let Some(c) = self.upper.get(k) else { return false };
        if t < c.start + MIN_CUT || t > c.end - MIN_CUT {
            return false;
        }
        let mut second = c.clone();
        second.at = Some(c.at.unwrap_or(0.0) + t - c.start);
        second.start = t;
        second.crossfade = 0.0;
        let first = &mut self.upper[k];
        first.end = t;
        first.out_crossfade = 0.0;
        self.upper.insert(k + 1, second);
        self.reflow();
        true
    }

    /// Sets upper cut `k`'s transition in (`out` false) or out, in seconds.
    pub fn set_upper_crossfade(&mut self, k: usize, out: bool, seconds: f64) {
        let Some(c) = self.upper.get_mut(k) else { return };
        let other = if out { c.crossfade } else { c.out_crossfade };
        let most = (c.len() - other - crate::project::SOLO).max(0.0);
        *(if out { &mut c.out_crossfade } else { &mut c.crossfade }) = seconds.clamp(0.0, most);
        self.reflow();
    }

    /// The timeline's length: the video's, or the music's while it runs on
    /// beyond it (the frame still to fill). The export ends with the video.
    pub fn frame(&self) -> f64 {
        let overlays = self.lanes().flat_map(|l| &l.items).map(Item::ends).fold(0.0, f64::max);
        self.duration().max(self.music.end()).max(overlays)
    }

    /// Where played piece `i` starts on the timeline (the edit's end for
    /// `i` past the last).
    pub fn offset(&self, i: usize) -> f64 {
        match self.play.get(i) {
            Some(_) => crate::project::offsets(&self.play[..=i])[i],
            None => self.duration(),
        }
    }

    /// The played piece at edit time `t` and the clip time there. The end of
    /// the edit is the last piece's out point.
    pub fn locate(&self, t: f64) -> Option<(usize, f64)> {
        let offsets = crate::project::offsets(&self.play);
        for (i, (c, from)) in self.play.iter().zip(offsets).enumerate() {
            if t < from + c.len() || i == self.play.len() - 1 {
                return Some((i, (c.start + (t - from).max(0.0)).min(c.end)));
            }
        }
        None
    }

    /// Where the crossfade into the piece after piece `i` begins, clip time
    /// in piece `i`, if it has one.
    pub fn crossfade_from(&self, i: usize) -> Option<f64> {
        let next = self.play.get(i + 1).filter(|n| n.crossfade > 0.0)?;
        Some(self.play[i].end - next.crossfade)
    }

    pub fn edit_time(&self, i: usize, clip_t: f64) -> f64 {
        self.offset(i) + (clip_t - self.play[i].start)
    }

    fn snapshot(&self) -> Snapshot {
        (self.cuts.clone(), self.upper.clone(), self.music.clone(), self.sound, self.fades, self.overlays.clone(), self.texts.clone())
    }

    fn restore(&mut self, (cuts, upper, music, sound, fades, overlays, texts): Snapshot) -> Snapshot {
        let now = self.snapshot();
        (self.cuts, self.upper, self.music, self.sound, self.fades, self.overlays, self.texts) = (cuts, upper, music, sound, fades, overlays, texts);
        self.reflow();
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

    /// Starts over with `cuts`, the `upper` track's and their music, mix
    /// and overlays (an opened or new project), forgetting undo.
    #[allow(clippy::too_many_arguments)]
    pub fn replace(&mut self, cuts: Vec<Cut>, upper: Vec<Cut>, music: Music, sound: Sound, fades: Fades, overlays: Vec<Lane>, texts: Vec<Lane>) {
        self.overlays = overlays;
        self.texts = texts;
        self.cuts = cuts;
        self.upper = upper.into_iter().map(|c| Cut { track: 1, ..c }).collect();
        self.reflow();
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
        self.reflow();
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
        self.reflow();
    }

    /// Sets the crossfade into cut `i`, within what the cuts either side
    /// allow.
    /// The longest cut `i` can overlap the cut before it.
    pub fn most_crossfade(&self, i: usize) -> f64 {
        let mut max = crate::project::max_crossfade(&self.cuts, i);
        // Leaving the cut what the next one's crossfade takes of its end,
        // or three cuts overlapped and the next one's was dropped (and the
        // edit a second longer) when the project was opened again.
        if let (Some(c), Some(next)) = (self.cuts.get(i), self.cuts.get(i + 1)) {
            max = max.min(c.len() - next.crossfade - crate::project::SOLO);
        }
        max.max(0.0)
    }

    pub fn set_crossfade(&mut self, i: usize, seconds: f64) {
        let max = self.most_crossfade(i);
        if let Some(c) = self.cuts.get_mut(i) {
            c.crossfade = seconds.clamp(0.0, max);
        }
        self.reflow();
    }

    pub fn remove(&mut self, i: usize) {
        self.cuts.remove(i);
        crate::project::settle_crossfades(&mut self.cuts);
        self.reflow();
    }

    /// Moves cut `from` so it plays at position `to` (counted before the move).
    pub fn move_cut(&mut self, from: usize, to: usize) {
        let cut = self.cuts.remove(from);
        let to = if to > from { to - 1 } else { to };
        self.cuts.insert(to.min(self.cuts.len()), cut);
        crate::project::settle_crossfades(&mut self.cuts);
        self.reflow();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cut(name: &str, start: f64, end: f64) -> Cut {
        Cut { input: PathBuf::from(name), start, end, ..Default::default() }
    }

    fn edit() -> Edit {
        let mut e = Edit::default();
        e.replace(vec![cut("a", 10.0, 14.0), cut("b", 2.0, 5.0)], Vec::new(), Default::default(), Default::default(), Default::default(), Vec::new(), Vec::new());
        e
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

    #[test]
    fn upper_cuts_play_over_the_main_track() {
        let mut e = edit();
        let k = e.add_upper(cut("u", 0.0, 2.0), 1.0);
        // The main cut a plays to 1 s, u to 3 s, then a's last second.
        assert_eq!(e.source, [(0, 0), (1, 0), (0, 0), (0, 1)]);
        assert_eq!(e.locate(2.0), Some((1, 1.0)));
        assert_eq!(e.locate(3.5), Some((2, 13.5)));
        assert_eq!(e.duration(), 7.0);
        // A main cut's hidden moment shows what plays there.
        assert_eq!(e.piece_of((0, 0), 12.0), Some((1, 1.0)));
        assert_eq!(e.piece_of((0, 0), 13.5), Some((2, 13.5)));
        // A second one dragged onto it stops against it, or with Shift goes
        // partly over it; trims stop at it.
        let j = e.add_upper(cut("v", 0.0, 2.0), 4.0);
        e.move_upper(j, 2.0, false);
        assert_eq!(e.upper[j].at, Some(3.0));
        e.move_upper(j, 2.5, true);
        assert_eq!(e.upper[j].at, Some(2.5));
        assert_eq!(e.play[e.source.iter().position(|&s| s == (1, j)).unwrap()].crossfade, 0.5);
        e.move_upper(j, 4.0, false);
        e.trim_upper(k, false, 9.0, 10.0);
        assert!((e.upper[k].end - 3.0).abs() < 1e-6, "{}", e.upper[k].end);
        // Split, each half where it was; undo brings the track back.
        e.checkpoint();
        assert!(e.split_upper(j, 1.0));
        assert_eq!((e.upper[j + 1].at, e.upper[j + 1].start), (Some(5.0), 1.0));
        assert!(e.undo());
        assert_eq!(e.upper.len(), 2);
        assert_eq!(e.source.len(), e.play.len());
    }
}
