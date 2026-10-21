//! The view as numbers: a strip under the transport row with the yaw,
//! pitch, roll, field of view and correction on screen, so a view can be
//! typed in (the same framing on every clip from a camera mounted the same
//! way). Each follows the view live; typing one, or dragging its name
//! sideways, sets it as a drag on the view would. Shown or hidden in
//! Settings.

use super::{App, Message, tip};
use crate::view::{FLAT_DEGREES, Projection, View};
use iced::advanced::widget::operation::{Focusable, Operation, Outcome};
use iced::advanced::widget::{Id, operate};
use iced::widget::canvas::{self, Frame, Geometry};
use iced::widget::{canvas as canvas_widget, mouse_area, row, text, text_input, tooltip};
use iced::{Alignment, Element, Point, Rectangle, Renderer, Task, Theme, mouse};

/// One number of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Angle {
    Yaw,
    Pitch,
    Roll,
    Fov,
    Correction,
}

impl Angle {
    const ALL: [Angle; 5] = [Angle::Yaw, Angle::Pitch, Angle::Roll, Angle::Fov, Angle::Correction];

    fn id(self) -> Id {
        Id::from(match self {
            Angle::Yaw => "angle-yaw",
            Angle::Pitch => "angle-pitch",
            Angle::Roll => "angle-roll",
            Angle::Fov => "angle-fov",
            Angle::Correction => "angle-correction",
        })
    }

    fn of(id: &Id) -> Option<Angle> {
        Angle::ALL.into_iter().find(|a| &a.id() == id)
    }

    /// Its value in `view`. On an ordinary video or picture the view is a
    /// crop, so the numbers are too: its centre across and down and its
    /// zoom, in % of the picture's height (`view::FLAT_DEGREES`).
    fn value(self, view: &View, flat: bool) -> f64 {
        match (self, flat) {
            (Angle::Yaw, true) => view.yaw / FLAT_DEGREES * 100.0,
            (Angle::Pitch, true) => -view.pitch / FLAT_DEGREES * 100.0,
            (Angle::Fov, true) => FLAT_DEGREES / view.fov * 100.0,
            (Angle::Yaw, _) => view.yaw,
            (Angle::Pitch, _) => view.pitch,
            (Angle::Roll, _) => view.roll,
            (Angle::Fov, _) => view.fov,
            (Angle::Correction, _) => view.correction,
        }
    }

    /// Sets it to `x` in `view`, kept within what the view can show.
    fn set(self, view: &mut View, x: f64, flat: bool) {
        match (self, flat) {
            (Angle::Yaw, true) => view.yaw = x / 100.0 * FLAT_DEGREES,
            (Angle::Pitch, true) => view.pitch = -x / 100.0 * FLAT_DEGREES,
            (Angle::Fov, true) => {
                let (lo, hi) = view.fov_range();
                view.fov = (FLAT_DEGREES * 100.0 / x.max(1e-3)).clamp(lo, hi);
            }
            (Angle::Yaw, _) => view.yaw = x,
            (Angle::Pitch, _) => view.pitch = x.clamp(-90.0, 90.0),
            (Angle::Roll, _) => view.roll = x,
            (Angle::Fov, _) => {
                let (lo, hi) = view.fov_range();
                view.fov = x.clamp(lo, hi);
            }
            (Angle::Correction, _) => view.correction = x.clamp(0.0, 1.0),
        }
    }

    /// One scroll notch or arrow press; Shift makes it a tenth.
    fn step(self, fine: bool) -> f64 {
        let step = if self == Angle::Correction { 0.05 } else { 1.0 };
        if fine { step / 10.0 } else { step }
    }

    /// How far a pixel of dragging its name moves it; Shift a tenth.
    fn per_pixel(self, fine: bool) -> f64 {
        self.step(fine) / 4.0
    }

    fn show(self, x: f64) -> String {
        match self {
            Angle::Correction => format!("{x:.2}"),
            _ => format!("{x:.1}"),
        }
    }

    fn label(self, flat: bool) -> &'static str {
        match (self, flat) {
            (Angle::Yaw, true) => "Across %",
            (Angle::Pitch, true) => "Down %",
            (Angle::Fov, true) => "Zoom %",
            (Angle::Yaw, _) => "Yaw",
            (Angle::Pitch, _) => "Pitch",
            (Angle::Roll, _) => "Roll",
            (Angle::Fov, _) => "FOV",
            (Angle::Correction, _) => "Correction",
        }
    }

    fn hint(self, flat: bool) -> &'static str {
        match (self, flat) {
            (Angle::Yaw, true) => "Where the view's centre is across the picture, in % of its height from the middle",
            (Angle::Pitch, true) => "Where the view's centre is down the picture, in % of its height from the middle",
            (Angle::Fov, true) => "Zoom: 100% shows the picture's whole height",
            (Angle::Yaw, _) => "Yaw, degrees right; past 360° is more than a turn",
            (Angle::Pitch, _) => "Pitch, degrees up",
            (Angle::Roll, _) => "Roll, degrees clockwise",
            (Angle::Fov, _) => "Field of view, degrees from the top of the picture to the bottom",
            (Angle::Correction, _) => "Correction: 0 keeps straight lines straight, 1 is the little planet's mapping, twice as wide at the same FOV",
        }
    }
}

/// A field's name, dragged sideways to scrub its value.
struct Scrub {
    angle: Angle,
    label: &'static str,
}

/// Where the pointer last was while dragging.
#[derive(Default)]
struct ScrubState {
    from: Option<f32>,
}

/// Room for `label` at 14 px, a little more than the letters take: a box
/// narrower than its text clipped it, and a press on the overflow missed.
fn name_width(label: &str) -> f32 {
    6.0 + label.chars().map(|c| if c.is_uppercase() || c == '%' { 11.0 } else if c == ' ' { 4.5 } else { 8.0 }).sum::<f32>()
}

impl canvas::Program<Message> for Scrub {
    type State = ScrubState;

    fn update(&self, state: &mut ScrubState, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<canvas::Action<Message>> {
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if cursor.is_over(bounds) => {
                state.from = cursor.position().map(|p| p.x);
                Some(canvas::Action::publish(Message::AngleScrubStart(self.angle)).and_capture())
            }
            // Followed outside its box too, as a drag runs past it.
            // Only while dragging: `replace` on its own would start every
            // name dragging at the first move, and each then ended the
            // others' drags.
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let from = state.from?;
                state.from = Some(position.x);
                Some(canvas::Action::publish(Message::AngleScrub(self.angle, (position.x - from) as f64)).and_capture())
            }
            // Any release ends it, and so does anything that says the button
            // can't still be down (another press, the pointer leaving the
            // window, where its release is never seen): a drag left hanging
            // turned the value with the bare pointer.
            iced::Event::Mouse(mouse::Event::ButtonReleased(_) | mouse::Event::ButtonPressed(_) | mouse::Event::CursorLeft) => {
                state.from.take()?;
                Some(canvas::Action::publish(Message::AngleScrubDone))
            }
            _ => None,
        }
    }

    fn draw(&self, state: &ScrubState, renderer: &Renderer, theme: &Theme, bounds: Rectangle, cursor: mouse::Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();
        let lit = state.from.is_some() || cursor.is_over(bounds);
        frame.fill_text(canvas::Text {
            content: self.label.to_owned(),
            position: Point::new(0.0, bounds.height / 2.0),
            color: if lit { palette.primary.strong.color } else { palette.background.base.text },
            size: 14.0.into(),
            align_y: iced::alignment::Vertical::Center,
            ..canvas::Text::default()
        });
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &ScrubState, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if state.from.is_some() || cursor.is_over(bounds) {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::default()
        }
    }
}

/// Finds which widget has the keyboard, if any (iced's own `find_focused`
/// says nothing at all when none has).
struct Focused(Option<Id>);

impl Operation<Option<Id>> for Focused {
    fn focusable(&mut self, id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if state.is_focused() {
            self.0 = id.cloned();
        }
    }

    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Option<Id>>)) {
        operate(self);
    }

    fn finish(&self) -> Outcome<Option<Id>> {
        Outcome::Some(self.0.clone())
    }
}

/// ↑ or ↓ (`dir` ±1): a field doesn't use them, so they reach the
/// shortcuts. Asks which widget has the keyboard first, so they nudge the
/// field being typed in rather than pitch the view.
pub fn arrow(dir: f64) -> Task<Message> {
    operate(Focused(None)).map(move |id| Message::AngleArrow(id.as_ref().and_then(Angle::of), dir))
}

impl App {
    /// Sets `angle` of the free look to `x`, as a drag would (Auto keyframe
    /// keys it while paused).
    pub(super) fn set_angle(&mut self, angle: Angle, x: f64) {
        let flat = self.on_flat();
        angle.set(self.free_look(), x, flat);
    }

    /// Typed into `angle`'s field: kept as typed while it's being typed,
    /// and set wherever it reads as a number.
    pub(super) fn type_angle(&mut self, angle: Angle, typed: String) {
        if let Ok(x) = typed.trim().replace(',', ".").parse::<f64>()
            && x.is_finite()
        {
            self.set_angle(angle, x);
        }
        self.angle_draft = Some((angle, typed));
    }

    /// Moves `angle` by `dx` pixels of dragging its name, if that's the
    /// drag going on.
    pub(super) fn scrub_angle(&mut self, angle: Angle, dx: f64) {
        if self.scrubbing != Some(angle) {
            return;
        }
        let x = angle.value(&self.shown_view(), self.on_flat()) + dx * angle.per_pixel(self.modifiers.shift());
        self.set_angle(angle, x);
    }

    /// Moves `angle` by `dir` steps.
    pub(super) fn nudge_angle(&mut self, angle: Angle, dir: f64) {
        let x = angle.value(&self.shown_view(), self.on_flat()) + dir * angle.step(self.modifiers.shift());
        self.angle_draft = None;
        self.set_angle(angle, x);
    }

    /// The strip, while Settings shows it and a clip is open.
    pub(super) fn angles_strip(&self) -> Option<Element<'_, Message>> {
        if !self.angles_shown || self.clip.is_none() {
            return None;
        }
        let flat = self.on_flat();
        let view = self.shown_view();
        let mut strip = row![].spacing(18).padding([2, 10]).align_y(Alignment::Center);
        for angle in Angle::ALL {
            if angle == Angle::Correction && (flat || view.projection == Projection::Equirect) {
                continue;
            }
            let shown = match &self.angle_draft {
                Some((a, typed)) if *a == angle => typed.clone(),
                _ => angle.show(angle.value(&view, flat)),
            };
            let field = text_input("", &shown)
                .id(angle.id())
                .on_input(move |s| Message::AngleInput(angle, s))
                .on_submit(Message::AngleDone)
                .size(14)
                .padding([2, 6])
                .width(72);
            let field = mouse_area(field).on_scroll(move |delta| {
                let y = match delta {
                    mouse::ScrollDelta::Lines { y, .. } | mouse::ScrollDelta::Pixels { y, .. } => y,
                };
                Message::AngleNudge(angle, y.signum() as f64)
            });
            let label = angle.label(flat);
            let name = canvas_widget(Scrub { angle, label }).width(name_width(label)).height(22);
            let mut item = row![name, field].spacing(6).align_y(Alignment::Center);
            // DJI Studio writes a yaw past a turn as turns × degrees.
            if angle == Angle::Yaw && !flat && view.yaw.abs() >= 360.0 {
                let turns = (view.yaw / 360.0).trunc();
                item = item.push(text(format!("{turns:.0} × {:.1}°", view.yaw - turns * 360.0)).size(13).style(text::secondary));
            }
            strip = strip.push(tip(item, text(angle.hint(flat)), tooltip::Position::Top));
        }
        let aspect = (self.view_size.width / self.view_size.height.max(1.0)) as f64;
        if let Some(across) = view.across(aspect).filter(|_| !flat) {
            strip = strip.push(tip(
                text(format!("Zoom {across:.1}°")).size(14).style(text::secondary),
                text("How wide the picture is from side to side"),
                tooltip::Position::Top,
            ));
        }
        Some(strip.into())
    }
}
