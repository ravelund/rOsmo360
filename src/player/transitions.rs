//! The Transitions panel: how the selected (or playing) cut comes in from
//! the cut before, docked at the left of the view like the Settings panel,
//! opened by the transport row's transitions button or T. Its choices are icons,
//! named by their hints, like the rest of the app's buttons.

use super::{App, Message, icon_button, tip};
use crate::transition::{Direction, Style};
use iced::widget::{button, column, container, row, rule, scrollable, slider, svg, text, tooltip};
use iced::{Alignment, Element, Length, Theme};

const WIDTH: f32 = 220.0;
/// What a transition is given when a style is first picked, seconds.
const LENGTH: f64 = 1.0;

#[derive(Clone, Copy, Debug)]
pub enum TransitionMsg {
    /// Open or close the panel.
    Toggle,
    /// The style, or a plain cut.
    Style(Option<Style>),
    Direction(Direction),
    /// Its length dragged to this many seconds.
    Length(f64),
    /// Give every cut this one's transition.
    ToAll,
}

impl App {
    /// The cut whose transition the panel shows: the selected one, else
    /// the playing one; none for the first, which has no cut before it.
    fn transition_cut(&self) -> Option<usize> {
        Some(self.selected.unwrap_or(self.current)).filter(|&i| i > 0 && i < self.edit.cuts.len())
    }

    pub(super) fn transition_message(&mut self, msg: TransitionMsg) {
        if let TransitionMsg::Toggle = msg {
            self.transitions_open = !self.transitions_open;
            if self.transitions_open {
                (self.settings_open, self.info) = (false, None);
            }
            return;
        }
        let Some(i) = self.transition_cut() else { return };
        match msg {
            TransitionMsg::Toggle => {}
            TransitionMsg::Style(None) => {
                self.edit.checkpoint();
                self.edit.set_crossfade(i, 0.0);
            }
            TransitionMsg::Style(Some(style)) => {
                self.edit.checkpoint();
                let cut = &mut self.edit.cuts[i];
                cut.transition = cut.transition.with_style(style);
                if cut.crossfade <= 0.0 {
                    self.edit.set_crossfade(i, LENGTH);
                }
            }
            TransitionMsg::Direction(direction) => {
                self.edit.checkpoint();
                self.edit.cuts[i].transition.direction = direction;
            }
            TransitionMsg::Length(seconds) => {
                self.begin_drag();
                self.edit.set_crossfade(i, seconds);
            }
            TransitionMsg::ToAll => {
                self.edit.checkpoint();
                let (transition, seconds) = (self.edit.cuts[i].transition, self.edit.cuts[i].crossfade);
                // In order: each leaves room for the one after it.
                for j in 1..self.edit.cuts.len() {
                    self.edit.cuts[j].transition = transition;
                    self.edit.set_crossfade(j, seconds);
                }
            }
        }
    }

    /// The Transitions panel, while it's open.
    pub(super) fn transitions_panel(&self) -> Option<Element<'_, Message>> {
        if !self.transitions_open {
            return None;
        }
        let send = |m: TransitionMsg| Message::Transition(m);
        let cut = self.transition_cut();
        // A transition that's there: the cut overlaps the one before.
        let on = cut.map(|i| &self.edit.cuts[i]).filter(|c| c.crossfade > 0.0);
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(send(TransitionMsg::Toggle)), text("Close (T)"), tooltip::Position::Bottom);
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
        let none = tip(choice(super::TR_NONE, cut.is_some() && on.is_none(), cut.map(|_| send(TransitionMsg::Style(None)))), text("None: a plain cut"), tooltip::Position::Bottom);
        let styles = Style::ALL.map(|style| tip(choice(style_icon(style), on.is_some_and(|c| c.transition.style == style), cut.map(|_| send(TransitionMsg::Style(Some(style))))), text(about(style)), tooltip::Position::Bottom));
        let [a, b, c, d, e, f, g] = styles;
        let mut body = column![
            row![text("Transition").size(18).width(Length::Fill), close].align_y(Alignment::Center),
            // What's chosen in words, for the cut it's on.
            text(match (cut, on) {
                (Some(i), Some(c)) => format!("Cut {} · {}", i + 1, c.transition.style),
                (Some(i), None) => format!("Cut {} · None", i + 1),
                _ => String::new(),
            })
            .size(13)
            .style(text::secondary),
            column![row![none, a, b, c].spacing(6), row![d, e, f, g].spacing(6)].spacing(6),
        ]
        .spacing(12)
        .padding(12);
        if let (Some(i), Some(c)) = (cut, on) {
            let ways = c.transition.style.directions();
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
                    picks = picks.push(tip(choice(icon, c.transition.direction == d, Some(send(TransitionMsg::Direction(d)))), text(name), tooltip::Position::Bottom));
                }
                body = body.push(rule::horizontal(1)).push(column![text("Direction").size(15), picks].spacing(6));
            }
            let most = self.edit.most_crossfade(i).max(0.1);
            body = body.push(rule::horizontal(1)).push(
                column![
                    row![text("Length").size(15).width(Length::Fill), text(format!("{:.2} s", c.crossfade)).size(13)].align_y(Alignment::Center),
                    slider(0.1..=most, c.crossfade, move |v| send(TransitionMsg::Length(v))).step(0.05).on_release(Message::LevelDone),
                ]
                .spacing(6),
            );
            body = body.push(rule::horizontal(1)).push(tip(icon_button(super::TO_ALL_CUTS, Some(send(TransitionMsg::ToAll))), text("To all cuts: give every cut this transition, at this length where the cuts have room"), tooltip::Position::Bottom));
        }
        Some(container(scrollable(body)).width(WIDTH).height(Length::Fill).into())
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
