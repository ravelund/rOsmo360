//! The Transitions panel: how the selected (or playing) cut comes in from
//! the cut before, docked at the left of the view like the Settings panel,
//! opened by the transport row's transitions button or T. Its choices are icons,
//! named by their hints, like the rest of the app's buttons. A cut on the
//! upper video track has two: how it comes in over what's beneath, and how
//! it goes.

use super::edit::CutRef;
use super::{App, Message, icon_button, tip};
use crate::transition::{Direction, Style, Transition};
use iced::widget::{button, column, container, row, rule, scrollable, slider, svg, text, tooltip};
use iced::{Alignment, Element, Length, Theme};

const WIDTH: f32 = 220.0;
/// What a transition is given when a style is first picked, seconds.
const LENGTH: f64 = 1.0;

#[derive(Clone, Copy, Debug)]
pub enum TransitionMsg {
    /// Open or close the panel.
    Toggle,
    /// The style, or a plain cut; `.0` true for an upper cut's way out.
    Style(bool, Option<Style>),
    Direction(bool, Direction),
    /// Its length dragged to this many seconds.
    Length(bool, f64),
    /// Give every cut of its track this one's transitions.
    ToAll,
}

/// Cut `c`'s transition in, or (`out`) an upper cut's way out.
fn pick(c: &mut crate::project::Cut, out: bool) -> &mut Transition {
    if out { &mut c.out_transition } else { &mut c.transition }
}

impl App {
    /// The cut whose transition the panel shows: the selected one, else
    /// the playing one; none for the main track's first, which has no cut
    /// before it.
    fn transition_cut(&self) -> Option<CutRef> {
        Some(self.selected.unwrap_or(self.here())).filter(|&r| self.edit.cut(r).is_some() && r != (0, 0))
    }

    /// Cut `r`'s transition in (or, `out`, an upper cut's way out), and how
    /// long it is.
    fn transition_of(&self, r: CutRef, out: bool) -> Option<(Transition, f64)> {
        let c = self.edit.cut(r)?;
        Some(if out { (c.out_transition, c.out_crossfade) } else { (c.transition, c.crossfade) })
    }

    fn set_length(&mut self, r: CutRef, out: bool, seconds: f64) {
        match r {
            (0, i) => self.edit.set_crossfade(i, seconds),
            (_, k) => self.edit.set_upper_crossfade(k, out, seconds),
        }
    }

    /// The longest cut `r`'s transition can be.
    fn most_length(&self, r: CutRef, out: bool) -> f64 {
        match r {
            (0, i) => self.edit.most_crossfade(i),
            (_, k) => self.edit.upper.get(k).map_or(0.0, |c| c.len() - if out { c.crossfade } else { c.out_crossfade } - crate::project::SOLO),
        }
    }

    pub(super) fn transition_message(&mut self, msg: TransitionMsg) {
        if let TransitionMsg::Toggle = msg {
            self.transitions_open = !self.transitions_open;
            if self.transitions_open {
                (self.settings_open, self.info) = (false, None);
            }
            return;
        }
        let Some(r) = self.transition_cut() else { return };
        let at = self.position();
        match msg {
            TransitionMsg::Toggle => {}
            TransitionMsg::Style(out, None) => {
                self.edit.checkpoint();
                self.set_length(r, out, 0.0);
            }
            TransitionMsg::Style(out, Some(style)) => {
                self.edit.checkpoint();
                let Some(cut) = self.edit.cut_mut(r) else { return };
                let t = pick(cut, out);
                *t = t.with_style(style);
                if self.transition_of(r, out).is_some_and(|(_, s)| s <= 0.0) {
                    self.set_length(r, out, LENGTH);
                }
            }
            TransitionMsg::Direction(out, direction) => {
                self.edit.checkpoint();
                if let Some(cut) = self.edit.cut_mut(r) {
                    pick(cut, out).direction = direction;
                }
            }
            TransitionMsg::Length(out, seconds) => {
                self.begin_drag();
                self.set_length(r, out, seconds);
            }
            TransitionMsg::ToAll => {
                self.edit.checkpoint();
                let c = self.edit.cut(r).unwrap().clone();
                if r.0 == 0 {
                    // In order: each leaves room for the one after it.
                    for j in 1..self.edit.cuts.len() {
                        self.edit.cuts[j].transition = c.transition;
                        self.edit.set_crossfade(j, c.crossfade);
                    }
                } else {
                    for k in 0..self.edit.upper.len() {
                        (self.edit.upper[k].transition, self.edit.upper[k].out_transition) = (c.transition, c.out_transition);
                        self.edit.upper[k].crossfade = 0.0;
                        self.edit.set_upper_crossfade(k, true, c.out_crossfade);
                        self.edit.set_upper_crossfade(k, false, c.crossfade);
                    }
                }
            }
        }
        self.edit.reflow();
        self.relocate(at);
    }

    /// The Transitions panel, while it's open.
    pub(super) fn transitions_panel(&self) -> Option<Element<'_, Message>> {
        if !self.transitions_open {
            return None;
        }
        let send = |m: TransitionMsg| Message::Transition(m);
        let cut = self.transition_cut();
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(send(TransitionMsg::Toggle)), text("Close (T)"), tooltip::Position::Bottom);
        let name = |r: CutRef| if r.0 == 0 { format!("Cut {}", r.1 + 1) } else { format!("Upper cut {}", r.1 + 1) };
        let mut body = column![
            row![text("Transition").size(18).width(Length::Fill), close].align_y(Alignment::Center),
            // Which cut it's for.
            text(cut.map_or(String::new(), name)).size(13).style(text::secondary),
        ]
        .spacing(12)
        .padding(12);
        // A main cut comes in from the one before; an upper cut comes in
        // over what's beneath and goes out again.
        let sides: &[bool] = match cut {
            Some((0, _)) => &[false],
            Some(_) => &[false, true],
            None => &[false],
        };
        for &out in sides {
            body = self.transition_section(body, cut, out);
        }
        if cut.is_some_and(|r| self.transition_of(r, false).is_some_and(|(_, s)| s > 0.0) || self.transition_of(r, true).is_some_and(|(_, s)| s > 0.0)) {
            let hint = if cut.is_some_and(|r| r.0 == 0) { "To all cuts: give every cut this transition, at this length where the cuts have room" } else { "To all upper cuts: give every cut of the upper track these transitions" };
            body = body.push(rule::horizontal(1)).push(tip(icon_button(super::TO_ALL_CUTS, Some(send(TransitionMsg::ToAll))), text(hint), tooltip::Position::Bottom));
        }
        Some(container(scrollable(body)).width(WIDTH).height(Length::Fill).into())
    }
}

impl App {
    /// Cut `cut`'s transition in (or out) on the panel: its style, direction
    /// and length.
    fn transition_section<'a>(&'a self, mut body: iced::widget::Column<'a, Message>, cut: Option<CutRef>, out: bool) -> iced::widget::Column<'a, Message> {
        let send = |m: TransitionMsg| Message::Transition(m);
        // A transition that's there: the cut overlaps what it comes from.
        let on = cut.and_then(|r| self.transition_of(r, out)).filter(|(_, s)| *s > 0.0);
        let style_icon = |style: Style| match style {
            Style::Dissolve => super::CROSSFADE,
            Style::DipBlack => super::TR_DIP_BLACK,
            Style::DipWhite => super::TR_DIP_WHITE,
            Style::Wipe => super::TR_WIPE,
            Style::Push => super::TR_PUSH,
            Style::Slide => super::TR_SLIDE,
            Style::Spin => super::TR_SPIN,
        };
        let about = |style: Style| match style {
            Style::Dissolve => "Dissolve: the cut before fades into this one",
            Style::DipBlack => "Dip to black: out to black, in from it",
            Style::DipWhite => "Dip to white: out to white, in from it",
            Style::Wipe => "Wipe: a soft line sweeps across, this cut behind it",
            Style::Push => "Push: this cut pushes the one before out of the frame",
            Style::Slide => "Slide: this cut slides in over the one before",
            Style::Spin => "Spin: the camera whips round into this cut",
        };
        let none = tip(choice(super::TR_NONE, cut.is_some() && on.is_none(), cut.map(|_| send(TransitionMsg::Style(out, None)))), text("None: a plain cut"), tooltip::Position::Bottom);
        let styles = Style::ALL.map(|style| tip(choice(style_icon(style), on.is_some_and(|(t, _)| t.style == style), cut.map(|_| send(TransitionMsg::Style(out, Some(style))))), text(about(style)), tooltip::Position::Bottom));
        let [a, b, c, d, e, f, g] = styles;
        if cut.is_some_and(|r| r.0 != 0) {
            let what = if out { "Out" } else { "In" };
            let said = on.map_or("None".to_owned(), |(t, _)| t.style.to_string());
            body = body.push(rule::horizontal(1)).push(row![text(what).size(15).width(Length::Fill), text(said).size(13).style(text::secondary)].align_y(Alignment::Center));
        } else if let Some((t, _)) = on {
            body = body.push(text(t.style.to_string()).size(13).style(text::secondary));
        }
        body = body.push(column![row![none, a, b, c].spacing(6), row![d, e, f, g].spacing(6)].spacing(6));
        if let (Some(r), Some((t, seconds))) = (cut, on) {
            let ways = t.style.directions();
            if !ways.is_empty() {
                let arrow = |d: Direction| match d {
                    Direction::Left => (super::ARROW_LEFT, "Left"),
                    Direction::Right => (super::ARROW_RIGHT, "Right"),
                    Direction::Up => (super::ARROW_UP, "Up"),
                    Direction::Down => (super::ARROW_DOWN, "Down"),
                };
                let mut picks = row![].spacing(6);
                for &d in ways {
                    let (icon, name) = arrow(d);
                    picks = picks.push(tip(choice(icon, t.direction == d, Some(send(TransitionMsg::Direction(out, d)))), text(name), tooltip::Position::Bottom));
                }
                body = body.push(column![text("Direction").size(14), picks].spacing(6));
            }
            let most = self.most_length(r, out).max(0.1);
            body = body.push(
                column![
                    row![text("Length").size(14).width(Length::Fill), text(format!("{seconds:.2} s")).size(13)].align_y(Alignment::Center),
                    slider(0.1..=most, seconds.min(most), move |v| send(TransitionMsg::Length(out, v))).step(0.05).on_release(Message::LevelDone),
                ]
                .spacing(6),
            );
        }
        body
    }
}

/// One of a set of icons to choose between: the chosen one as the app's
/// buttons are, the others on the panel's own shade, in the theme's colours.
fn choice<'a>(icon: &'static [u8], chosen: bool, on_press: Option<Message>) -> Element<'a, Message> {
    let off = on_press.is_none();
    let picture = svg(svg::Handle::from_memory(icon)).width(22).height(22).style(move |theme: &Theme, _| {
        let palette = theme.extended_palette();
        let color = if chosen { palette.primary.base.text } else { theme.palette().text };
        svg::Style { color: Some(iced::Color { a: if off { 0.35 } else { 1.0 }, ..color }) }
    });
    button(picture)
        .padding(8)
        .on_press_maybe(on_press)
        .style(move |theme: &Theme, status| {
            if chosen {
                return button::primary(theme, status);
            }
            let palette = theme.extended_palette();
            let shade = if status == button::Status::Hovered { palette.background.strong.color } else { palette.background.weak.color };
            button::Style { background: Some(shade.into()), ..button::primary(theme, status) }
        })
        .into()
}
