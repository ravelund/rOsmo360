//! The timeline under the view. The top row holds the edit's cuts as
//! blocks showing thumbnails of their footage: click one to select it, drag
//! it to move it, drag its edges to trim it. Below is the seek bar with each
//! cut's keyframes as diamonds (shaped by their ease): click the bar to seek,
//! click a diamond to jump to it, drag one to retime it.
//!
//! Under the seek bar is the music lane: the music track's clips with their
//! waveforms, moved and trimmed like cuts, and a line where the video ends
//! (music beyond it fades out in the export).
//!
//! Above the cuts is the overlay lane, a canvas of its own (`OverlayLane`):
//! its pictures as blocks, selected, moved and trimmed like music. Under
//! each track (the overlays, once there are any, and the cuts) are two rows
//! of its keyframes: effects, then motion (an overlay's) or view (a cut's),
//! each marked by its ease; click one to jump there.
//!
//! It shows a window of the edit: the wheel zooms around the pointer, Shift
//! or a sideways wheel pans, and when zoomed in a scrollbar at the bottom
//! shows and moves the window.

use super::{Message, music, thumbs};
use crate::overlay::Track;
use crate::view::Ease;
use iced::alignment::Vertical;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke, Text};
use iced::widget::image::Handle;
use iced::{Point, Rectangle, Renderer, Size, Theme};
use std::sync::Arc;

/// Diamond half-size, and how close a click must land to pick a diamond or
/// a cut's edge.
const DIAMOND: f32 = 6.0;
const PICK: f32 = 7.0;
const BAR: f32 = 8.0;
/// The cut row's height at scale 1, and the gap between cut blocks.
const CUTS: f32 = 40.0;
const GAP: f32 = 2.0;
/// Under a track, a row of its effect keyframes and one of its view (a
/// cut's) or motion (an overlay's) keyframes.
const EFFECTS: f32 = 16.0;
const KEYS: f32 = 20.0;
const SEEK: f32 = 18.0;
/// The music lane at scale 1.
const MUSIC: f32 = 40.0;
/// The scrollbar's row at the bottom, and the bar's thickness.
const SCROLL: f32 = 8.0;
const SCROLL_BAR: f32 = 5.0;
/// How far the lanes may shrink or grow, as a share of their own height.
pub const SCALES: std::ops::RangeInclusive<f32> = 0.75..=3.0;

/// Where the rows sit, top to bottom: the cuts, their keyframe rows, the
/// seek bar, the music lane and the scrollbar. Dragging the divider above
/// the transport row scales the lanes that hold pictures and waveforms
/// (cuts, music, overlays); the rows of marks and the seek bar keep their
/// height, as their marks do.
#[derive(Clone, Copy, Debug)]
pub struct Rows {
    cuts: f32,
    effects_top: f32,
    keys_top: f32,
    seek_top: f32,
    music: f32,
    music_top: f32,
    height: f32,
    overlay: f32,
}

impl Rows {
    pub fn new(scale: f32) -> Self {
        let scale = scale.clamp(*SCALES.start(), *SCALES.end());
        let (cuts, music) = ((CUTS * scale).round(), (MUSIC * scale).round());
        let seek_top = cuts + EFFECTS + KEYS;
        let music_top = seek_top + SEEK;
        Rows { cuts, effects_top: cuts, keys_top: cuts + EFFECTS, seek_top, music, music_top, height: music_top + music + SCROLL, overlay: (OVERLAY * scale).round() }
    }

    fn music_lane(self) -> Lane {
        (self.music_top + 4.0, self.music - 8.0)
    }

    fn cut_lane(self) -> Lane {
        (4.0, self.cuts - 8.0)
    }
}

/// The lanes' heights at scale 1, which a drag of the divider shares out:
/// the cuts, the music and the two overlay lanes (text and pictures).
pub const SCALABLE: f32 = CUTS + MUSIC + 2.0 * OVERLAY;
/// A press on a cut that moves less than this is a click, not a drag.
const DRAG_FROM: f32 = 4.0;
/// A music edge dragged this close to a cut boundary, the playhead or
/// another song's edge snaps to it.
const SNAP: f32 = 8.0;
/// The volume line's range, dB, top to bottom of its lane (the bottom is
/// silence), and a volume point's radius.
const VOLUME_TOP: f64 = 6.0;
const VOLUME_BOTTOM: f64 = crate::project::OFF_DB;
const POINT: f32 = 4.0;
const EFFECT_MARK: iced::Color = iced::Color::from_rgb(0.78, 0.55, 1.0);
/// Where a volume line runs: its top and height. The music's over the music
/// lane, a cut's sound over its block.
type Lane = (f32, f32);
const MUSIC_LINE: iced::Color = iced::Color::from_rgb(1.0, 0.78, 0.25);
const SOUND_LINE: iced::Color = iced::Color::from_rgb(0.35, 0.85, 1.0);

/// An overlay lane's height at scale 1, above the cuts.
const OVERLAY: f32 = 30.0;
const OVERLAY_COLOUR: iced::Color = iced::Color::from_rgb(0.95, 0.55, 0.3);
const TEXT_COLOUR: iced::Color = iced::Color::from_rgb(0.3, 0.8, 0.75);

/// An overlay item as its lane shows it.
pub struct OverlayBlock {
    pub label: String,
    /// Where it starts on the edit, its in point in its own time, and its
    /// length.
    pub from: f64,
    pub start: f64,
    pub len: f64,
    /// Its effect keyframes' and motion keyframes' own times.
    pub effects: Vec<f64>,
    pub motion: Vec<(f64, Ease)>,
    pub thumb: Option<Handle>,
}

/// One cut as the timeline shows it.
pub struct Block {
    /// Its names, longest first; the first that fits is shown.
    pub labels: Vec<String>,
    /// Where it starts on the timeline, and how long it lasts.
    pub from: f64,
    pub len: f64,
    /// Its in point in clip time.
    pub start: f64,
    /// Its keyframes within the cut: index into the cut's keyframes, clip
    /// time, and how it moves on to the next (drawn as its shape).
    /// Keyframes: index, clip time, ease, and whether it follows the stick.
    pub keys: Vec<(usize, f64, Ease, bool)>,
    /// Thumbnails of its clip by clip time, in order (see `thumbs`).
    pub thumbs: Vec<(f64, Handle)>,
    /// Its sound's volume points, clip time (some may lie outside the cut).
    pub sound: Vec<crate::project::VolumeKey>,
    /// How long it overlaps the cut before, dissolving in.
    pub crossfade: f64,
    /// Its effect keyframes' clip times.
    pub effects: Vec<f64>,
}

impl Block {
    /// The thumbnail nearest clip time `t`.
    fn thumb_at(&self, t: f64) -> Option<&Handle> {
        let i = self.thumbs.partition_point(|(at, _)| *at < t);
        let before = i.checked_sub(1).map(|j| &self.thumbs[j]);
        match (before, self.thumbs.get(i)) {
            (Some(a), Some(b)) => Some(if t - a.0 < b.0 - t { &a.1 } else { &b.1 }),
            (a, b) => a.or(b).map(|x| &x.1),
        }
    }
}

/// A music clip as the music lane shows it.
pub struct MusicBlock {
    pub label: String,
    /// Where it starts on the edit, its in point in the song, and its length.
    pub from: f64,
    pub start: f64,
    pub len: f64,
    /// The song's waveform, once read.
    pub wave: Option<Arc<music::Waveform>>,
}

pub struct Timeline {
    pub blocks: Vec<Block>,
    pub music: Vec<MusicBlock>,
    pub selected_music: Option<usize>,
    /// The music track's volume points (edit time, dB on top of the
    /// track's level), and the one selected.
    pub volume: Vec<crate::project::VolumeKey>,
    pub selected_volume: Option<usize>,
    /// The cut sound point selected: (cut, point).
    pub selected_sound: Option<(usize, usize)>,
    /// Where the video ends; `duration` runs on to the music's end if later.
    pub video: f64,
    pub duration: f64,
    /// The stretch of edit time shown: from `start`, `span` seconds.
    pub start: f64,
    pub span: f64,
    /// Playhead, in edit time.
    pub position: f64,
    pub selected: Option<usize>,
    /// The keyframe at the playhead (cut, keyframe), drawn highlighted.
    pub current: Option<(usize, usize)>,
    /// The overlay track's lane and the text track's.
    pub overlays: Vec<OverlayBlock>,
    pub texts: Vec<OverlayBlock>,
    pub selected_overlay: Option<crate::overlay::ItemRef>,
    /// The rows' heights, from the divider's scale.
    pub rows: Rows,
}

#[derive(Default)]
pub enum Drag {
    #[default]
    None,
    Playhead,
    Key { cut: usize, key: usize },
    /// Trimming a cut's in (`start`) or out point. The scale is kept from
    /// the press: the timeline rescales as the edit's length changes.
    Trim { cut: usize, start: bool, x0: f32, from: f64, per_px: f64 },
    /// Moving a cut; `to` is where it would go.
    Move { cut: usize, x0: f32, moved: bool, to: usize },
    /// Dragging the scrollbar, which was at `start` when pressed at `x0`.
    Scroll { x0: f32, start: f64 },
    /// Moving music clip `clip`, which started at `at` when pressed at `x0`.
    MusicMove { clip: usize, x0: f32, at: f64, per_px: f64 },
    /// Trimming music clip `clip`'s in (`start`) or out edge, at song time
    /// `from` when pressed.
    MusicTrim { clip: usize, start: bool, x0: f32, from: f64, per_px: f64 },
    /// Shift-dragging cut `cut`'s start edge: its crossfade, `from` seconds
    /// when pressed at `x0`; dragging left lengthens it.
    Crossfade { cut: usize, x0: f32, from: f64, per_px: f64 },
    /// Dragging music volume point `key`.
    Volume { key: usize },
    /// Dragging cut `cut`'s sound point `key`.
    Sound { cut: usize, key: usize },
    /// Dragging cut `cut`'s effect keyframe `key` to another time.
    Effect { cut: usize, key: usize },
}

#[derive(Default)]
pub struct State {
    drag: Drag,
    /// Shift held: the wheel pans instead of zooming, and music may
    /// overlap. Ctrl held: a click on the music lane adds a volume point.
    shift: bool,
    ctrl: bool,
}

impl Timeline {
    fn width(b: Rectangle) -> f32 {
        (b.width - 2.0 * PICK).max(1.0)
    }

    /// Edit time to x, with room either side for diamonds at the ends.
    /// Times outside the window land outside the widget.
    fn x(&self, t: f64, b: Rectangle) -> f32 {
        PICK + ((t - self.start) / self.span.max(1e-9)) as f32 * Self::width(b)
    }

    fn t(&self, x: f32, b: Rectangle) -> f64 {
        (self.start + ((x - PICK) / Self::width(b)) as f64 * self.span).clamp(0.0, self.duration)
    }

    fn zoomed(&self) -> bool {
        self.span < self.duration - 1e-6
    }

    /// The scrollbar's thumb, left and right x.
    fn scroll_thumb(&self, b: Rectangle) -> (f32, f32) {
        let w = Self::width(b) as f64 / self.duration.max(1e-9);
        (PICK + (self.start * w) as f32, PICK + ((self.start + self.span) * w) as f32)
    }

    fn block_at(&self, x: f32, b: Rectangle) -> Option<usize> {
        // Later first: in a crossfade the incoming cut lies over the one before.
        self.blocks.iter().rposition(|c| x >= self.x(c.from, b) && x <= self.x(c.from + c.len, b))
    }

    /// The nearest diamond within reach of `x`: (cut, keyframe).
    fn key_at(&self, x: f32, b: Rectangle) -> Option<(usize, usize)> {
        let mut best = None;
        for (ci, c) in self.blocks.iter().enumerate() {
            for &(ki, t, _, _) in &c.keys {
                let d = (self.x(c.from + t - c.start, b) - x).abs();
                if d <= PICK && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, (ci, ki)));
                }
            }
        }
        best.map(|(_, k)| k)
    }

    /// Whether `x` is on cut `i`'s in edge (`Some(true)`) or out edge.
    fn edge_at(&self, i: usize, x: f32, b: Rectangle) -> Option<bool> {
        let c = &self.blocks[i];
        let (l, r) = (self.x(c.from, b), self.x(c.from + c.len, b));
        if r - l < 3.0 * PICK {
            // Too narrow for a body: the nearer edge.
            return Some(x - l < r - x);
        }
        if x - l <= PICK {
            Some(true)
        } else if r - x <= PICK {
            Some(false)
        } else {
            None
        }
    }

    /// The effect keyframe mark under `p` (they sit in a row under the
    /// cuts): (cut, keyframe, clip time).
    fn effect_at(&self, p: Point, b: Rectangle) -> Option<(usize, usize, f64)> {
        if !(self.rows.effects_top..self.rows.keys_top).contains(&p.y) {
            return None;
        }
        self.blocks.iter().enumerate().rev().find_map(|(i, c)| {
            c.effects.iter().enumerate().find(|&(_, &t)| t >= c.start && t <= c.start + c.len && (self.x(c.from + t - c.start, b) - p.x).abs() <= PICK).map(|(k, &t)| (i, k, t))
        })
    }

    /// The cut edge under `x`: (cut, whether it's the in edge). Later cuts
    /// first: a crossfading cut's in edge lies over the cut before it.
    fn edge_hit(&self, x: f32, b: Rectangle) -> Option<(usize, bool)> {
        (0..self.blocks.len()).rev().find_map(|i| {
            let (l, r) = self.block_x(&self.blocks[i], b);
            (x >= l - PICK && x <= r + PICK).then(|| self.edge_at(i, x, b)).flatten().map(|e| (i, e))
        })
    }

    fn music_at(&self, x: f32, b: Rectangle) -> Option<usize> {
        self.music.iter().position(|m| x >= self.x(m.from, b) && x <= self.x(m.from + m.len, b))
    }

    /// Whether `x` is on music clip `i`'s in edge (`Some(true)`) or out edge.
    fn music_edge_at(&self, i: usize, x: f32, b: Rectangle) -> Option<bool> {
        let m = &self.music[i];
        let (l, r) = (self.x(m.from, b), self.x(m.from + m.len, b));
        if r - l < 3.0 * PICK {
            Some(x - l < r - x)
        } else if x - l <= PICK {
            Some(true)
        } else if r - x <= PICK {
            Some(false)
        } else {
            None
        }
    }

    /// A volume line's dB to y on `lane` (its top and height) and back.
    fn volume_y(lane: Lane, db: f64) -> f32 {
        lane.0 + ((VOLUME_TOP - db.clamp(VOLUME_BOTTOM, VOLUME_TOP)) / (VOLUME_TOP - VOLUME_BOTTOM)) as f32 * lane.1
    }

    /// y on `lane` to dB, snapping to 0 within a few pixels.
    fn volume_db(lane: Lane, y: f32) -> f64 {
        if (y - Self::volume_y(lane, 0.0)).abs() < 3.0 {
            return 0.0;
        }
        (VOLUME_TOP - ((y - lane.0) / lane.1) as f64 * (VOLUME_TOP - VOLUME_BOTTOM)).clamp(VOLUME_BOTTOM, VOLUME_TOP)
    }

    /// The music volume point under `p`, if any.
    fn volume_at(&self, p: Point, b: Rectangle) -> Option<usize> {
        self.volume.iter().position(|k| (self.x(k.t, b) - p.x).abs() <= PICK && (Self::volume_y(self.rows.music_lane(), k.volume) - p.y).abs() <= PICK)
    }

    /// A cut's sound point under `p`, if any: (cut, point).
    fn sound_at(&self, p: Point, b: Rectangle) -> Option<(usize, usize)> {
        self.blocks.iter().enumerate().find_map(|(ci, c)| {
            let k = c.sound.iter().position(|k| {
                k.t >= c.start && k.t <= c.start + c.len && (self.x(c.from + k.t - c.start, b) - p.x).abs() <= PICK && (Self::volume_y(self.rows.cut_lane(), k.volume) - p.y).abs() <= PICK
            })?;
            Some((ci, k))
        })
    }

    /// Edit times a music clip's edges snap to, but for music clip `clip`'s
    /// own: the start, the cut boundaries, the playhead, and the other songs'
    /// edges.
    fn snap_targets(&self, clip: usize) -> Vec<f64> {
        let mut at = vec![0.0, self.position];
        at.extend(self.blocks.iter().flat_map(|c| [c.from, c.from + c.len]));
        at.extend(self.music.iter().enumerate().filter(|&(i, _)| i != clip).flat_map(|(_, m)| [m.from, m.from + m.len]));
        at
    }

    /// How far to shift any of `edges` (edit times) onto the nearest snap
    /// target within `SNAP` pixels, or 0.
    fn snap(&self, clip: usize, edges: &[f64], per_px: f64) -> f64 {
        let reach = SNAP as f64 * per_px;
        self.snap_targets(clip)
            .iter()
            .flat_map(|t| edges.iter().map(move |e| t - e))
            .filter(|d| d.abs() < reach)
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or(0.0)
    }

    /// Where a moved cut would land: before the first cut whose middle is
    /// right of `x`.
    fn drop_at(&self, x: f32, b: Rectangle) -> usize {
        self.blocks.iter().filter(|c| self.x(c.from + c.len / 2.0, b) < x).count()
    }
}

impl Timeline {
    /// The two layers: block backgrounds with their thumbnails, and over them
    /// everything else, which also takes the mouse. Within one layer iced
    /// draws images over shapes, so the labels and handles need their own.
    pub fn view<'a>(self) -> iced::Element<'a, Message> {
        let t = Arc::new(self);
        use iced::widget::canvas;
        let layers = iced::widget::stack![
            canvas(Pictures(t.clone())).width(iced::Length::Fill).height(t.rows.height),
            canvas(Marks(t.clone())).width(iced::Length::Fill).height(t.rows.height),
        ];
        // Zoomed in, what is outside the window is drawn outside the widget;
        // canvases don't clip, so the container does.
        let lane = |track| {
            let lane = OverlayLane(t.clone(), track);
            let height = lane.height();
            iced::widget::container(canvas(lane).width(iced::Length::Fill).height(height)).clip(true)
        };
        iced::widget::column![lane(Track::Text), lane(Track::Overlay), iced::widget::container(layers).clip(true)].into()
    }

    /// The music lane: its clips with their waveforms and handles, and where
    /// the video ends, the music past it dimmed (it fades out there).
    fn draw_music(&self, frame: &mut Frame, theme: &Theme, b: Rectangle) {
        let palette = theme.extended_palette();
        let (top, h) = (self.rows.music_top + 2.0, self.rows.music - 4.0);
        let lane = Path::rounded_rectangle(Point::new(self.x(0.0, b), top), Size::new((self.x(self.duration, b) - self.x(0.0, b)).max(0.0), h), 4.0.into());
        frame.fill(&lane, palette.background.weak.color.scale_alpha(0.5));
        for (i, m) in self.music.iter().enumerate() {
            let (l, r) = (self.x(m.from, b) + GAP / 2.0, self.x(m.from + m.len, b) - GAP / 2.0);
            if r < 0.0 || l > b.width {
                continue;
            }
            let block = Path::rounded_rectangle(Point::new(l, top), Size::new((r - l).max(1.0), h), 4.0.into());
            let selected = Some(i) == self.selected_music;
            frame.fill(&block, if selected { palette.success.weak.color } else { palette.success.base.color.scale_alpha(0.55) });
            // The waveform: per two pixels, the loudest peak under them.
            if let Some(w) = &m.wave {
                let per_px = m.len / (r - l).max(1.0) as f64;
                let bars = Path::new(|p| {
                    let mut x = l.max(0.0);
                    while x < r.min(b.width) {
                        let (t0, t1) = (m.start + (x - l) as f64 * per_px, m.start + (x + 2.0 - l) as f64 * per_px);
                        let (i0, i1) = ((t0 / music::WAVE_STEP) as usize, ((t1 / music::WAVE_STEP) as usize).max((t0 / music::WAVE_STEP) as usize + 1));
                        let peak = w.peaks.get(i0..i1.min(w.peaks.len())).map_or(0.0, |s| s.iter().fold(0.0f32, |a, &v| a.max(v)));
                        let bh = (peak * (h - 4.0)).max(1.0);
                        p.rectangle(Point::new(x, top + h / 2.0 - bh / 2.0), Size::new(1.5, bh));
                        x += 2.0;
                    }
                });
                frame.fill(&bars, palette.background.base.text.scale_alpha(0.7));
            }
            if selected {
                frame.stroke(&block, Stroke::default().with_width(2.0).with_color(palette.primary.strong.color));
            }
            for x in [l + 4.0, r - 4.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, top + 4.0), Size::new(2.0, h - 8.0), 1.0.into()), iced::Color::from_rgba(1.0, 1.0, 1.0, 0.8));
            }
            // On a dark tag, as on the cuts, so it reads over the waveform.
            if r - l > m.label.chars().count() as f32 * 6.0 + 28.0 {
                // At the bottom: the volume line runs along the top.
                let tag = Path::rounded_rectangle(Point::new(l + 9.0, top + h - 18.0), Size::new(m.label.chars().count() as f32 * 5.5 + 8.0, 16.0), 3.0.into());
                frame.fill(&tag, iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6));
                frame.fill_text(Text {
                    content: m.label.clone(),
                    position: Point::new(l + 13.0, top + h - 10.0),
                    color: iced::Color::WHITE,
                    size: 11.0.into(),
                    align_y: Vertical::Center,
                    ..Default::default()
                });
            }
        }
        // Overlaps are crossfades: an X across each, as editors draw them.
        for (i, a) in self.music.iter().enumerate() {
            for c in &self.music[i + 1..] {
                let (t0, t1) = (a.from.max(c.from), (a.from + a.len).min(c.from + c.len));
                if t1 - t0 <= 1e-6 {
                    continue;
                }
                let (l, r) = (self.x(t0, b), self.x(t1, b));
                frame.fill(&Path::rectangle(Point::new(l, top), Size::new(r - l, h)), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.35));
                let x = Path::new(|p| {
                    p.move_to(Point::new(l, top + h));
                    p.line_to(Point::new(r, top));
                    p.move_to(Point::new(l, top));
                    p.line_to(Point::new(r, top + h));
                });
                frame.stroke(&x, Stroke::default().with_width(1.5).with_color(iced::Color::from_rgba(1.0, 1.0, 1.0, 0.85)));
            }
        }
        // Past the video's end the music fades out in the export: dim it,
        // and mark the end.
        if self.duration > self.video + 1e-6 {
            let x = self.x(self.video, b);
            let to = self.x(self.duration, b);
            frame.fill(&Path::rectangle(Point::new(x, top), Size::new((to - x).max(0.0), h)), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.55));
            frame.fill(&Path::rectangle(Point::new(x - 1.0, self.rows.music_top), Size::new(2.0, self.rows.music)), palette.background.base.text.scale_alpha(0.8));
        }
        self.draw_volume(frame, b);
    }

    /// The music's volume line over its lane, once there are points: a flat
    /// line along every lane would only be in the way.
    fn draw_volume(&self, frame: &mut Frame, b: Rectangle) {
        let (Some(first), Some(last)) = (self.volume.first(), self.volume.last()) else { return };
        let points: Vec<_> = self.volume.iter().map(|k| (self.x(k.t, b), k.volume)).collect();
        let ends = ((self.x(0.0, b), first.volume), (self.x(self.duration, b), last.volume));
        draw_line(frame, b, self.rows.music_lane(), MUSIC_LINE, ends, &points, self.selected_volume);
    }

    /// Each cut's sound line over its block, for cuts with points: the
    /// points within the cut, and the line from its in to its out point.
    fn draw_sound(&self, frame: &mut Frame, b: Rectangle) {
        for (ci, c) in self.blocks.iter().enumerate() {
            if c.sound.is_empty() {
                continue;
            }
            let (l, r) = self.block_x(c, b);
            let end = c.start + c.len;
            let at = |t: f64| crate::project::volume_at(&c.sound, t);
            // Points outside the cut aren't drawn; their indices still count.
            let points: Vec<_> = c.sound.iter().map(|k| if k.t >= c.start && k.t <= end { (self.x(c.from + k.t - c.start, b), k.volume) } else { (f32::NAN, k.volume) }).collect();
            let selected = self.selected_sound.filter(|s| s.0 == ci).map(|s| s.1);
            draw_line(frame, b, self.rows.cut_lane(), SOUND_LINE, ((l, at(c.start)), (r, at(end))), &points, selected);
        }
    }

    /// A cut block's left and right edges.
    fn block_x(&self, c: &Block, b: Rectangle) -> (f32, f32) {
        (self.x(c.from, b) + GAP / 2.0, self.x(c.from + c.len, b) - GAP / 2.0)
    }
}

/// An overlay track's lane (the text's on top, then the pictures'), over
/// the cuts: a canvas of its own, so the rows under it keep their places.
struct OverlayLane(Arc<Timeline>, Track);

#[derive(Default)]
enum OverlayDrag {
    #[default]
    None,
    Move { item: usize, x0: f32, at: f64, per_px: f64 },
    /// An edge, at own time `from` when pressed.
    Trim { item: usize, start: bool, x0: f32, from: f64, per_px: f64 },
    /// A keyframe mark: item `item`'s effect keyframe `key` (`effects`) or
    /// motion keyframe, to another time.
    Key { item: usize, effects: bool, key: usize },
}

impl OverlayLane {
    fn blocks(&self) -> &[OverlayBlock] {
        match self.1 {
            Track::Overlay => &self.0.overlays,
            Track::Text => &self.0.texts,
        }
    }

    fn item_at(&self, x: f32, b: Rectangle) -> Option<usize> {
        self.blocks().iter().position(|o| x >= self.0.x(o.from, b) && x <= self.0.x(o.from + o.len, b))
    }

    fn edge_at(&self, i: usize, x: f32, b: Rectangle) -> Option<bool> {
        let o = &self.blocks()[i];
        let (l, r) = (self.0.x(o.from, b), self.0.x(o.from + o.len, b));
        if r - l < 3.0 * PICK {
            Some(x - l < r - x)
        } else if x - l <= PICK {
            Some(true)
        } else if r - x <= PICK {
            Some(false)
        } else {
            None
        }
    }

    /// The lane, and its keyframe rows once it has items.
    fn height(&self) -> f32 {
        if self.blocks().is_empty() { self.0.rows.overlay } else { self.0.rows.overlay + EFFECTS + KEYS }
    }

    /// The keyframe mark under `p` in its row: the item, whether it's in
    /// the effects row, the keyframe and its edit time.
    fn key_at(&self, p: Point, b: Rectangle) -> Option<(usize, bool, usize, f64)> {
        let effects = (self.0.rows.overlay..self.0.rows.overlay + EFFECTS).contains(&p.y);
        if !effects && !(self.0.rows.overlay + EFFECTS..self.0.rows.overlay + EFFECTS + KEYS).contains(&p.y) {
            return None;
        }
        self.blocks().iter().enumerate().find_map(|(item, o)| {
            let times: Vec<f64> = if effects { o.effects.clone() } else { o.motion.iter().map(|k| k.0).collect() };
            times
                .into_iter()
                .enumerate()
                .filter(|&(_, t)| t >= o.start && t <= o.start + o.len)
                .map(|(key, t)| (key, o.from + t - o.start))
                .find(|&(_, t)| (self.0.x(t, b) - p.x).abs() <= PICK)
                .map(|(key, t)| (item, effects, key, t))
        })
    }
}

impl canvas::Program<Message> for OverlayLane {
    type State = OverlayDrag;

    fn update(&self, drag: &mut OverlayDrag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        use iced::Event::Mouse;
        let t = &self.0;
        match event {
            Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cursor.position_in(bounds)?;
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (x, y),
                    mouse::ScrollDelta::Pixels { x, y } => (x / 40.0, y / 40.0),
                };
                let message = if dy != 0.0 {
                    Message::TimelineZoom(1.25f64.powf(dy as f64), Some(t.t(p.x, bounds)))
                } else if dx != 0.0 {
                    Message::TimelinePan(t.start - dx as f64 * t.span * 0.1)
                } else {
                    return None;
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            // Right-click an overlay picture: what file it is (text has none).
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) if self.1 == Track::Overlay => {
                let p = cursor.position_in(bounds)?;
                let item = self.item_at(p.x, bounds).filter(|_| p.y < self.0.rows.overlay)?;
                Some(canvas::Action::publish(Message::Info(super::info::Of::Overlay((self.1, item)))).and_capture())
            }
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                if let Some((item, effects, key, at)) = self.key_at(p, bounds) {
                    // A jump there, and a drag retimes it.
                    *drag = OverlayDrag::Key { item, effects, key };
                    return Some(canvas::Action::publish(Message::OverlayJump(at)).and_capture());
                }
                // Elsewhere in the keyframe rows: seek there.
                if p.y >= self.0.rows.overlay {
                    return Some(canvas::Action::publish(Message::OverlayJump(t.t(p.x, bounds))).and_capture());
                }
                let Some(item) = self.item_at(p.x, bounds) else {
                    return Some(canvas::Action::publish(Message::OverlaySelect(None)).and_capture());
                };
                let o = &self.blocks()[item];
                let per_px = t.span / Timeline::width(bounds) as f64;
                *drag = match self.edge_at(item, p.x, bounds) {
                    Some(start) => OverlayDrag::Trim { item, start, x0: p.x, from: if start { o.start } else { o.start + o.len }, per_px },
                    None => OverlayDrag::Move { item, x0: p.x, at: o.from, per_px },
                };
                Some(canvas::Action::publish(Message::OverlaySelect(Some((self.1, item)))).and_capture())
            }
            Mouse(mouse::Event::CursorMoved { position }) => {
                let x = position.x - bounds.x;
                let message = match drag {
                    OverlayDrag::None => return None,
                    OverlayDrag::Move { item, x0, at, per_px } => Message::OverlayMove((self.1, *item), *at + (x - *x0) as f64 * *per_px),
                    OverlayDrag::Trim { item, start, x0, from, per_px } => Message::OverlayTrim((self.1, *item), *start, *from + (x - *x0) as f64 * *per_px),
                    OverlayDrag::Key { item, effects, key } => {
                        let o = &self.blocks()[*item];
                        let at = self.0.t(x, bounds).clamp(o.from, o.from + o.len);
                        Message::OverlayKeyMove((self.1, *item), *effects, *key, o.start + at - o.from)
                    }
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                OverlayDrag::None => None,
                _ => Some(canvas::Action::publish(Message::ScrubDone).and_capture()),
            },
            _ => None,
        }
    }

    fn draw(&self, _: &OverlayDrag, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let t = &self.0;
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        let (top, h) = (2.0, self.0.rows.overlay - 4.0);
        let lane = Path::rounded_rectangle(Point::new(t.x(0.0, bounds), top), Size::new((t.x(t.duration, bounds) - t.x(0.0, bounds)).max(0.0), h), 4.0.into());
        frame.fill(&lane, palette.background.weak.color.scale_alpha(0.5));
        let (name, colour) = match self.1 {
            Track::Overlay => ("Overlay", OVERLAY_COLOUR),
            Track::Text => ("Text", TEXT_COLOUR),
        };
        frame.fill_text(Text {
            content: name.to_owned(),
            position: Point::new(PICK + 12.0, top + h / 2.0),
            color: palette.background.base.text.scale_alpha(0.35),
            size: 10.0.into(),
            align_y: Vertical::Center,
            ..Default::default()
        });
        for (i, o) in self.blocks().iter().enumerate() {
            let (l, r) = (t.x(o.from, bounds) + GAP / 2.0, t.x(o.from + o.len, bounds) - GAP / 2.0);
            if r < 0.0 || l > bounds.width {
                continue;
            }
            let block = Path::rounded_rectangle(Point::new(l, top), Size::new((r - l).max(1.0), h), 4.0.into());
            let selected = Some((self.1, i)) == t.selected_overlay;
            frame.fill(&block, if selected { colour } else { colour.scale_alpha(0.55) });
            if selected {
                frame.stroke(&block, Stroke::default().with_width(2.0).with_color(palette.primary.strong.color));
            }
            for x in [l + 4.0, r - 4.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, top + 4.0), Size::new(2.0, h - 8.0), 1.0.into()), iced::Color::from_rgba(1.0, 1.0, 1.0, 0.8));
            }
            // The picture, then its name, where there's room.
            let mut text_x = l + 9.0;
            if let Some(thumb) = &o.thumb
                && r - l > 3.0 * h
            {
                frame.draw_image(Rectangle::new(Point::new(l + 8.0, top + 2.0), Size::new(h * 1.5, h - 4.0)), canvas::Image::new(thumb.clone()));
                text_x += h * 1.5 + 2.0;
            }
            if r - text_x > o.label.chars().count() as f32 * 6.0 + 12.0 {
                frame.fill_text(Text {
                    content: o.label.clone(),
                    position: Point::new(text_x, top + h / 2.0),
                    color: iced::Color::BLACK,
                    size: 11.0.into(),
                    align_y: Vertical::Center,
                    ..Default::default()
                });
            }
        }
        if !self.blocks().is_empty() {
            draw_rows(&mut frame, theme, t, bounds, self.0.rows.overlay, "Motion");
            let inside = |o: &OverlayBlock, k: f64| k >= o.start && k <= o.start + o.len;
            for o in self.blocks() {
                for &k in o.effects.iter().filter(|&&k| inside(o, k)) {
                    let x = t.x(o.from + k - o.start, bounds);
                    let mark = Path::rectangle(Point::new(x - 3.5, self.0.rows.overlay + EFFECTS / 2.0 - 3.5), Size::new(7.0, 7.0));
                    frame.fill(&mark, EFFECT_MARK);
                    frame.stroke(&mark, Stroke::default().with_width(1.0).with_color(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.7)));
                }
                for &(k, ease) in o.motion.iter().filter(|(k, _)| inside(o, *k)) {
                    let mark = key_mark(t.x(o.from + k - o.start, bounds), self.0.rows.overlay + EFFECTS + KEYS / 2.0, ease);
                    frame.fill(&mark, colour);
                    frame.stroke(&mark, Stroke::default().with_width(1.5).with_color(palette.background.base.color));
                }
            }
        }
        let at = t.x(t.position, bounds);
        frame.fill(&Path::rectangle(Point::new(at - 1.0, 0.0), Size::new(2.0, self.height())), palette.primary.strong.color);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, drag: &OverlayDrag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match drag {
            OverlayDrag::Trim { .. } => mouse::Interaction::ResizingHorizontally,
            OverlayDrag::Move { .. } | OverlayDrag::Key { .. } => mouse::Interaction::Grabbing,
            OverlayDrag::None => {
                let Some(p) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
                if p.y >= self.0.rows.overlay {
                    // A mark can be dragged; elsewhere in the rows a click seeks.
                    return if self.key_at(p, bounds).is_some() { mouse::Interaction::Grab } else { mouse::Interaction::Pointer };
                }
                match self.item_at(p.x, bounds) {
                    Some(i) if self.edge_at(i, p.x, bounds).is_some() => mouse::Interaction::ResizingHorizontally,
                    Some(_) => mouse::Interaction::Grab,
                    None => mouse::Interaction::default(),
                }
            }
        }
    }
}

struct Pictures(Arc<Timeline>);

impl canvas::Program<Message> for Pictures {
    type State = ();

    fn draw(&self, _: &(), renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        for (i, c) in self.0.blocks.iter().enumerate() {
            let (l, r) = self.0.block_x(c, bounds);
            let block = Path::rounded_rectangle(Point::new(l, 0.0), Size::new((r - l).max(1.0), self.0.rows.cuts), 4.0.into());
            frame.fill(&block, if Some(i) == self.0.selected { palette.primary.weak.color } else { palette.background.strong.color });
            // Thumbnails side by side, each showing the moment at its middle.
            let (th, inset) = (self.0.rows.cuts - 4.0, 2.0);
            let tw = th * thumbs::SIZE.0 as f32 / thumbs::SIZE.1 as f32;
            if c.thumbs.is_empty() || r - l <= 2.0 * inset {
                continue;
            }
            // Within the widget too: iced clips a canvas's shapes to it but
            // not its images, and zoomed in the tiles at the ends ran on into
            // the window's margins.
            let (cl, cr) = ((l + inset).max(0.0), (r - inset).min(bounds.width));
            if cr <= cl {
                continue;
            }
            let clip = Rectangle::new(Point::new(cl, inset), Size::new(cr - cl, th));
            let per_px = c.len / (r - l) as f64;
            frame.with_clip(clip, |f| {
                // From the first tile in sight: zoomed in, a block can be
                // far wider than the widget.
                let mut x = l + inset + ((-(l + inset)) / tw).floor().max(0.0) * tw;
                while x < clip.x + clip.width {
                    let t = c.start + (x + tw / 2.0 - l) as f64 * per_px;
                    if let Some(h) = c.thumb_at(t) {
                        f.draw_image(Rectangle::new(Point::new(x, inset), Size::new(tw, th)), canvas::Image::new(h.clone()));
                    }
                    x += tw;
                }
            });
        }
        vec![frame.into_geometry()]
    }
}

/// A volume line on `lane` from `ends.0` to `ends.1` (x, dB) through
/// `points` (x, dB; NaN x for one not shown), the `selected` one labelled
/// with its level.
fn draw_line(frame: &mut Frame, b: Rectangle, lane: Lane, colour: iced::Color, ends: ((f32, f64), (f32, f64)), points: &[(f32, f64)], selected: Option<usize>) {
    let y = |db| Timeline::volume_y(lane, db);
    let line = Path::new(|p| {
        p.move_to(Point::new(ends.0.0, y(ends.0.1)));
        for &(x, db) in points.iter().filter(|p| !p.0.is_nan()) {
            p.line_to(Point::new(x, y(db)));
        }
        p.line_to(Point::new(ends.1.0, y(ends.1.1)));
    });
    frame.stroke(&line, Stroke::default().with_width(1.5).with_color(colour));
    for (i, &(x, db)) in points.iter().enumerate() {
        if x.is_nan() {
            continue;
        }
        let at = Point::new(x, y(db));
        let chosen = Some(i) == selected;
        let dot = Path::circle(at, if chosen { POINT + 1.0 } else { POINT });
        frame.fill(&dot, if chosen { iced::Color::WHITE } else { colour });
        frame.stroke(&dot, Stroke::default().with_width(1.0).with_color(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6)));
        if chosen {
            let label = if db <= VOLUME_BOTTOM { "off".to_owned() } else { format!("{db:+.1} dB") };
            let (w, h) = (label.len() as f32 * 6.0 + 8.0, 14.0);
            let left = if at.x + 8.0 + w > b.width { at.x - 8.0 - w } else { at.x + 8.0 };
            let top = (at.y - h / 2.0).clamp(lane.0 - 4.0, lane.0 + lane.1 + 4.0 - h);
            frame.fill(&Path::rounded_rectangle(Point::new(left, top), Size::new(w, h), 3.0.into()), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.7));
            frame.fill_text(Text {
                content: label,
                position: Point::new(left + 4.0, top + h / 2.0),
                color: iced::Color::WHITE,
                size: 11.0.into(),
                align_y: Vertical::Center,
                ..Default::default()
            });
        }
    }
}

/// The two keyframe rows under a track from `top`: faint bands with their
/// names at the left, effects above, `keys` (view or motion) below.
/// The keyframe rows' bands under a track, over the edit's length (`t`)
/// like the lanes and the seek bar, not the widget's whole width.
fn draw_rows(frame: &mut Frame, theme: &Theme, t: &Timeline, b: Rectangle, top: f32, keys: &str) {
    let palette = theme.extended_palette();
    let (l, r) = (t.x(0.0, b).max(0.0), t.x(t.duration, b).min(b.width));
    for (y, h, name) in [(top, EFFECTS, "Effects"), (top + EFFECTS, KEYS, keys)] {
        frame.fill(&Path::rectangle(Point::new(l, y + 1.0), Size::new((r - l).max(0.0), h - 2.0)), palette.background.weak.color.scale_alpha(0.25));
        frame.fill_text(Text {
            content: name.to_owned(),
            position: Point::new(l.max(PICK) + 12.0, y + h / 2.0),
            color: palette.background.base.text.scale_alpha(0.35),
            size: 10.0.into(),
            align_y: Vertical::Center,
            ..Default::default()
        });
    }
}

/// A keyframe mark at (`x`, `y`) shaped by its ease: smooth a diamond,
/// linear a circle, hold a square.
fn key_mark(x: f32, y: f32, ease: Ease) -> Path {
    match ease {
        Ease::Smooth => Path::new(|p| {
            p.move_to(Point::new(x, y - DIAMOND));
            p.line_to(Point::new(x + DIAMOND, y));
            p.line_to(Point::new(x, y + DIAMOND));
            p.line_to(Point::new(x - DIAMOND, y));
            p.close();
        }),
        Ease::Linear => Path::circle(Point::new(x, y), DIAMOND * 0.8),
        Ease::Hold => Path::rectangle(Point::new(x - DIAMOND * 0.7, y - DIAMOND * 0.7), Size::new(DIAMOND * 1.4, DIAMOND * 1.4)),
    }
}

struct Marks(Arc<Timeline>);

impl std::ops::Deref for Marks {
    type Target = Timeline;

    fn deref(&self) -> &Timeline {
        &self.0
    }
}

impl canvas::Program<Message> for Marks {
    type State = State;

    fn update(&self, state: &mut State, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        use iced::Event::{Keyboard, Mouse};
        let drag = &mut state.drag;
        match event {
            Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                state.shift = m.shift();
                state.ctrl = m.command();
                None
            }
            Mouse(mouse::Event::WheelScrolled { delta }) => {
                let p = cursor.position_in(bounds)?;
                let (dx, dy) = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => (x, y),
                    mouse::ScrollDelta::Pixels { x, y } => (x / 40.0, y / 40.0),
                };
                let (pan, zoom) = if state.shift { (dx + dy, 0.0) } else { (dx, dy) };
                let message = if zoom != 0.0 {
                    Message::TimelineZoom(1.25f64.powf(zoom as f64), Some(self.t(p.x, bounds)))
                } else if pan != 0.0 {
                    Message::TimelinePan(self.start - pan as f64 * self.span * 0.1)
                } else {
                    return None;
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            // Right-click a cut or a song: what file it is.
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let p = cursor.position_in(bounds)?;
                let of = if p.y < self.rows.cuts {
                    super::info::Of::Cut(self.block_at(p.x, bounds)?)
                } else if p.y >= self.rows.music_top && p.y < self.rows.height - SCROLL {
                    super::info::Of::Music(self.music_at(p.x, bounds)?)
                } else {
                    return None;
                };
                Some(canvas::Action::publish(Message::Info(of)).and_capture())
            }
            Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_in(bounds)?;
                let message = if p.y >= self.rows.height - SCROLL {
                    if !self.zoomed() {
                        return None;
                    }
                    // On the thumb, drag it; elsewhere on the bar, centre it there first.
                    let (l, r) = self.scroll_thumb(bounds);
                    let per_px = self.duration / Timeline::width(bounds) as f64;
                    let start = if p.x >= l && p.x <= r { self.start } else { self.start + (p.x - (l + r) / 2.0) as f64 * per_px };
                    *drag = Drag::Scroll { x0: p.x, start };
                    Message::TimelinePan(start)
                } else if let Some((cut, key, t)) = self.effect_at(p, bounds) {
                    // A jump there, and a drag retimes it.
                    *drag = Drag::Effect { cut, key };
                    Message::Effect(super::effects::EffectMsg::Jump(cut, t))
                } else if p.y < self.rows.cuts && let Some((cut, key)) = self.sound_at(p, bounds) {
                    *drag = Drag::Sound { cut, key };
                    Message::SoundKeySelect(cut, key)
                } else if p.y < self.rows.cuts && state.ctrl {
                    // A sound point on the cut where clicked, dragged on.
                    let cut = self.block_at(p.x, bounds)?;
                    let c = &self.blocks[cut];
                    let t = c.start + self.t(p.x, bounds) - c.from;
                    *drag = Drag::Sound { cut, key: c.sound.partition_point(|k| k.t <= t) };
                    Message::SoundKeyAdd(cut, t, Timeline::volume_db(self.rows.cut_lane(), p.y))
                } else if p.y < self.rows.cuts {
                    let per_px = self.span / Timeline::width(bounds) as f64;
                    let cut = match self.edge_hit(p.x, bounds) {
                        // Shift on a start edge: how far it overlaps the
                        // cut before, a crossfade.
                        Some((cut, true)) if state.shift && cut > 0 => {
                            *drag = Drag::Crossfade { cut, x0: p.x, from: self.blocks[cut].crossfade, per_px };
                            cut
                        }
                        Some((cut, start)) => {
                            let c = &self.blocks[cut];
                            *drag = Drag::Trim { cut, start, x0: p.x, from: if start { c.start } else { c.start + c.len }, per_px };
                            cut
                        }
                        None => {
                            let cut = self.block_at(p.x, bounds)?;
                            *drag = Drag::Move { cut, x0: p.x, moved: false, to: cut };
                            cut
                        }
                    };
                    Message::Select(cut)
                } else if p.y >= self.rows.music_top && let Some(key) = self.volume_at(p, bounds) {
                    *drag = Drag::Volume { key };
                    Message::VolumeSelect(key)
                } else if p.y >= self.rows.music_top && state.ctrl {
                    // A new point where clicked, dragged on from there.
                    let t = self.t(p.x, bounds);
                    *drag = Drag::Volume { key: self.volume.partition_point(|k| k.t <= t) };
                    Message::VolumeAdd(t, Timeline::volume_db(self.rows.music_lane(), p.y))
                } else if p.y >= self.rows.music_top {
                    let clip = self.music_at(p.x, bounds)?;
                    let m = &self.music[clip];
                    let per_px = self.span / Timeline::width(bounds) as f64;
                    *drag = match self.music_edge_at(clip, p.x, bounds) {
                        Some(start) => Drag::MusicTrim { clip, start, x0: p.x, from: if start { m.start } else { m.start + m.len }, per_px },
                        None => Drag::MusicMove { clip, x0: p.x, at: m.from, per_px },
                    };
                    Message::SelectMusic(clip)
                } else if (self.rows.keys_top..self.rows.seek_top).contains(&p.y) && let Some((cut, key)) = self.key_at(p.x, bounds) {
                    *drag = Drag::Key { cut, key };
                    Message::KeyJump(cut, key)
                } else {
                    *drag = Drag::Playhead;
                    Message::Scrub(self.t(p.x, bounds))
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::CursorMoved { position }) => {
                let x = position.x - bounds.x;
                let message = match drag {
                    Drag::None => return None,
                    Drag::Playhead => Message::Scrub(self.t(x, bounds)),
                    Drag::Key { cut, key } => {
                        let c = &self.blocks[*cut];
                        let edit_t = self.t(x, bounds).clamp(c.from, c.from + c.len);
                        Message::KeyMove(*cut, *key, c.start + edit_t - c.from)
                    }
                    Drag::Effect { cut, key } => {
                        let c = &self.blocks[*cut];
                        let edit_t = self.t(x, bounds).clamp(c.from, c.from + c.len);
                        Message::Effect(super::effects::EffectMsg::KeyMove(*cut, *key, c.start + edit_t - c.from))
                    }
                    Drag::Trim { cut, start, x0, from, per_px } => Message::Trim(*cut, *start, *from + (x - *x0) as f64 * *per_px),
                    Drag::Move { x0, moved, to, .. } => {
                        *moved |= (x - *x0).abs() > DRAG_FROM;
                        *to = self.drop_at(x, bounds);
                        return Some(canvas::Action::request_redraw().and_capture());
                    }
                    Drag::Scroll { x0, start } => Message::TimelinePan(*start + (x - *x0) as f64 * self.duration / Timeline::width(bounds) as f64),
                    Drag::MusicMove { clip, x0, at, per_px } => {
                        let at = *at + (x - *x0) as f64 * *per_px;
                        let len = self.music[*clip].len;
                        Message::MusicMove(*clip, at + self.snap(*clip, &[at, at + len], *per_px), state.shift)
                    }
                    Drag::Crossfade { cut, x0, from, per_px } => Message::Crossfade(*cut, *from + (*x0 - x) as f64 * *per_px),
                    Drag::Volume { key } => Message::VolumeMove(*key, self.t(x, bounds), Timeline::volume_db(self.rows.music_lane(), position.y - bounds.y)),
                    Drag::Sound { cut, key } => {
                        let c = &self.blocks[*cut];
                        Message::SoundKeyMove(*cut, *key, c.start + self.t(x, bounds) - c.from, Timeline::volume_db(self.rows.cut_lane(), position.y - bounds.y))
                    }
                    Drag::MusicTrim { clip, start, x0, from, per_px } => {
                        // Song time to edit time is fixed while trimming:
                        // either edge moves, the song stays put.
                        let m = &self.music[*clip];
                        let t = *from + (x - *x0) as f64 * *per_px;
                        let to_edit = m.from - m.start;
                        Message::MusicTrim(*clip, *start, t + self.snap(*clip, &[t + to_edit], *per_px), state.shift)
                    }
                };
                Some(canvas::Action::publish(message).and_capture())
            }
            Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => match std::mem::take(drag) {
                Drag::None => None,
                Drag::Move { cut, moved: true, to, .. } => Some(canvas::Action::publish(Message::MoveCut(cut, to)).and_capture()),
                Drag::Move { .. } | Drag::Scroll { .. } => Some(canvas::Action::request_redraw().and_capture()),
                Drag::Playhead | Drag::Key { .. } | Drag::Effect { .. } | Drag::Trim { .. } | Drag::MusicMove { .. } | Drag::MusicTrim { .. } | Drag::Volume { .. } | Drag::Sound { .. } | Drag::Crossfade { .. } => {
                    Some(canvas::Action::publish(Message::ScrubDone).and_capture())
                }
            },
            _ => None,
        }
    }

    fn draw(&self, state: &State, renderer: &Renderer, theme: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<Geometry> {
        let drag = &state.drag;
        let palette = theme.extended_palette();
        let mut frame = Frame::new(renderer, bounds.size());
        draw_rows(&mut frame, theme, self, bounds, self.rows.effects_top, "View");
        for (i, c) in self.blocks.iter().enumerate() {
            let (l, r) = self.block_x(c, bounds);
            let block = Path::rounded_rectangle(Point::new(l, 0.0), Size::new((r - l).max(1.0), self.rows.cuts), 4.0.into());
            let selected = Some(i) == self.selected;
            if selected {
                frame.stroke(&block, Stroke::default().with_width(2.0).with_color(palette.primary.strong.color));
            }
            // Trim handles, light on a dark edge so they show on any picture.
            for x in [l + 4.0, r - 4.0] {
                frame.fill(&Path::rounded_rectangle(Point::new(x - 2.0, 5.0), Size::new(4.0, self.rows.cuts - 10.0), 2.0.into()), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.5));
                frame.fill(&Path::rounded_rectangle(Point::new(x - 1.0, 6.0), Size::new(2.0, self.rows.cuts - 12.0), 1.0.into()), iced::Color::from_rgba(1.0, 1.0, 1.0, 0.8));
            }
            // The longest label that fits (about 7 px a character), on a
            // dark tag, readable over any picture.
            if let Some(label) = c.labels.iter().find(|s| r - l > s.chars().count() as f32 * 7.0 + 24.0) {
                let tag = Path::rounded_rectangle(Point::new(l + 9.0, self.rows.cuts - 19.0), Size::new(label.chars().count() as f32 * 5.9 + 8.0, 15.0), 3.0.into());
                frame.fill(&tag, iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6));
                frame.fill_text(Text {
                    content: label.clone(),
                    position: Point::new(l + 13.0, self.rows.cuts - 11.5),
                    color: iced::Color::WHITE,
                    size: 12.0.into(),
                    align_y: Vertical::Center,
                    ..Default::default()
                });
            }
        }
        // Effect keyframes: small squares along the bottom of their cut.
        for c in &self.blocks {
            for &t in c.effects.iter().filter(|&&t| t >= c.start && t <= c.start + c.len) {
                let x = self.x(c.from + t - c.start, bounds);
                let mark = Path::rectangle(Point::new(x - 3.5, self.rows.effects_top + EFFECTS / 2.0 - 3.5), Size::new(7.0, 7.0));
                frame.fill(&mark, EFFECT_MARK);
                frame.stroke(&mark, Stroke::default().with_width(1.0).with_color(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.7)));
            }
        }
        // Crossfades: an X over each overlap, as on the music lane.
        for c in self.blocks.iter().filter(|c| c.crossfade > 0.0) {
            let (l, r) = (self.x(c.from, bounds), self.x(c.from + c.crossfade, bounds));
            frame.fill(&Path::rectangle(Point::new(l, 0.0), Size::new(r - l, self.rows.cuts)), iced::Color::from_rgba(0.0, 0.0, 0.0, 0.35));
            let x = Path::new(|p| {
                p.move_to(Point::new(l, self.rows.cuts));
                p.line_to(Point::new(r, 0.0));
                p.move_to(Point::new(l, 0.0));
                p.line_to(Point::new(r, self.rows.cuts));
            });
            frame.stroke(&x, Stroke::default().with_width(1.5).with_color(iced::Color::from_rgba(1.0, 1.0, 1.0, 0.85)));
        }
        if let Drag::Move { moved: true, to, .. } = drag {
            let t = self.blocks.get(*to).map_or(self.duration, |c| c.from);
            let x = self.x(t, bounds);
            frame.fill(&Path::rectangle(Point::new(x - 1.5, 0.0), Size::new(3.0, self.rows.cuts)), palette.primary.strong.color);
        }

        self.draw_music(&mut frame, theme, bounds);
        let mid = self.rows.seek_top + SEEK / 2.0;
        let keys_mid = self.rows.keys_top + KEYS / 2.0;
        let (x0, x1) = (self.x(0.0, bounds), self.x(self.duration, bounds));
        let bar = |from: f32, to: f32| Path::rounded_rectangle(Point::new(from, mid - BAR / 2.0), Size::new((to - from).max(0.0), BAR), (BAR / 2.0).into());
        frame.fill(&bar(x0, x1), palette.background.strong.color);
        let at = self.x(self.position, bounds);
        frame.fill(&bar(x0, at), palette.primary.base.color);
        // Where one cut ends and the next begins.
        for c in self.blocks.iter().skip(1) {
            let x = self.x(c.from, bounds);
            frame.fill(&Path::rectangle(Point::new(x - 1.0, mid - BAR), Size::new(2.0, 2.0 * BAR)), palette.background.base.color);
        }
        for (ci, c) in self.blocks.iter().enumerate() {
            for &(ki, t, ease, follows) in &c.keys {
                let (x, mid) = (self.x(c.from + t - c.start, bounds), keys_mid);
                // Smooth is a diamond, linear a circle, hold a square.
                let mark = key_mark(x, mid, ease);
                let fill = if Some((ci, ki)) == self.current { palette.primary.strong.color } else { palette.background.base.text };
                frame.fill(&mark, fill);
                frame.stroke(&mark, Stroke::default().with_width(1.5).with_color(palette.background.base.color));
                // A keyframe that follows the stick wears a ring.
                if follows {
                    frame.stroke(&Path::circle(Point::new(x, mid), DIAMOND * 1.7), Stroke::default().with_width(1.5).with_color(fill));
                }
            }
        }
        self.draw_sound(&mut frame, bounds);
        frame.fill(&Path::rectangle(Point::new(at - 1.0, 0.0), Size::new(2.0, self.rows.seek_top)), palette.primary.strong.color);
        frame.fill(&Path::rectangle(Point::new(at - 1.0, self.rows.music_top), Size::new(2.0, self.rows.music)), palette.primary.strong.color);
        frame.fill(&Path::circle(Point::new(at, mid), BAR), palette.primary.strong.color);
        if self.zoomed() {
            let y = self.rows.height - SCROLL / 2.0 - SCROLL_BAR / 2.0;
            let pill = |l: f32, r: f32| Path::rounded_rectangle(Point::new(l, y), Size::new((r - l).max(SCROLL_BAR), SCROLL_BAR), (SCROLL_BAR / 2.0).into());
            frame.fill(&pill(PICK, PICK + Timeline::width(bounds)), palette.background.strong.color);
            let (l, r) = self.scroll_thumb(bounds);
            let active = matches!(drag, Drag::Scroll { .. });
            frame.fill(&pill(l, r), if active { palette.primary.strong.color } else { palette.background.base.text.scale_alpha(0.5) });
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match state.drag {
            Drag::Trim { .. } | Drag::MusicTrim { .. } | Drag::Crossfade { .. } => mouse::Interaction::ResizingHorizontally,
            Drag::None => {
                let Some(p) = cursor.position_in(bounds) else { return mouse::Interaction::default() };
                if p.y >= self.rows.height - SCROLL {
                    return if self.zoomed() { mouse::Interaction::Grab } else { mouse::Interaction::default() };
                }
                if p.y >= self.rows.music_top {
                    if self.volume_at(p, bounds).is_some() {
                        return mouse::Interaction::Grab;
                    }
                    if state.ctrl {
                        return mouse::Interaction::Crosshair;
                    }
                    return match self.music_at(p.x, bounds) {
                        Some(i) if self.music_edge_at(i, p.x, bounds).is_some() => mouse::Interaction::ResizingHorizontally,
                        Some(_) => mouse::Interaction::Grab,
                        None => mouse::Interaction::default(),
                    };
                }
                if p.y >= self.rows.cuts {
                    // Marks in either row drag to another time.
                    let on = if p.y < self.rows.keys_top { self.effect_at(p, bounds).is_some() } else { p.y < self.rows.seek_top && self.key_at(p.x, bounds).is_some() };
                    return if on { mouse::Interaction::Grab } else { mouse::Interaction::Pointer };
                }
                if self.sound_at(p, bounds).is_some() {
                    return mouse::Interaction::Grab;
                }
                if state.ctrl && self.block_at(p.x, bounds).is_some() {
                    return mouse::Interaction::Crosshair;
                }
                if self.edge_hit(p.x, bounds).is_some() {
                    return mouse::Interaction::ResizingHorizontally;
                }
                match self.block_at(p.x, bounds) {
                    Some(_) => mouse::Interaction::Grab,
                    None => mouse::Interaction::default(),
                }
            }
            _ => mouse::Interaction::Grabbing,
        }
    }
}
