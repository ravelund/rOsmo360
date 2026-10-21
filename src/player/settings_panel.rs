//! The Settings panel: how the player behaves, kept between runs
//! (`settings.rs`), docked at the left of the view like the Info panel.

use super::effects::LutChoice;
use super::{App, Message, tip};
use iced::widget::{button, column, container, pick_list, row, rule, scrollable, text, toggler, tooltip};
use iced::{Alignment, Element, Length, Theme};

const WIDTH: f32 = 300.0;

/// The theme the settings file names, as iced names it; Kanagawa Dragon
/// (the user's choice, 2026-10-14) for none or one iced no longer has.
/// The desktop's own theme was followed until then; dropped as the user
/// liked a fixed one better, and its reading crashed the Mac at times.
pub fn theme_named(name: &str) -> Theme {
    Theme::ALL.iter().find(|t| t.to_string() == name).cloned().unwrap_or(Theme::KanagawaDragon)
}

impl App {
    /// The LUT library: each LUT with a button to take it out, Add, and
    /// which one D-Log M clips get.
    fn lut_library(&self) -> Element<'_, Message> {
        let note = |s: &'static str| text(s).size(13).style(text::secondary);
        let mut list = column![].spacing(4);
        for name in &self.luts {
            let remove = tip(button(text("✕").size(13)).padding([0, 6]).style(button::text).on_press(Message::RemoveLut(name.clone())), text("Take it out of the library"), tooltip::Position::Left);
            list = list.push(row![text(name.clone()).size(14).width(Length::Fill), remove].align_y(Alignment::Center));
        }
        if self.luts.is_empty() {
            list = list.push(note("None yet."));
        }
        let mut choices = vec![LutChoice::None];
        choices.extend(self.luts.iter().map(|n| LutChoice::Named(n.clone())));
        column![
            list,
            button(text("Add LUT…").size(14)).on_press(Message::AddLut),
            note("Copied into rOsmo360's settings folder, so projects keep working when the download moves."),
            text("For D-Log M clips").size(14),
            pick_list(choices, Some(LutChoice::of(self.dlog_m_lut.as_ref())), Message::DlogMLut).text_size(13).width(Length::Fill),
            note("Clips filmed in D-Log M get it when they're added; the Effects panel changes it per cut. DJI's is \"Osmo 360 D-Log M to Rec.709 LUT\" in the DJI Download Center."),
        ]
        .spacing(6)
        .into()
    }

    /// The Settings panel, while it's open.
    pub(super) fn settings_panel(&self) -> Option<Element<'_, Message>> {
        if !self.settings_open {
            return None;
        }
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(Message::SettingsPanel), text("Close"), tooltip::Position::Bottom);
        let note = |s: &'static str| text(s).size(13).style(text::secondary);
        let body = column![
            row![text("Settings").size(18).width(Length::Fill), close].align_y(Alignment::Center),
            text("Look").size(15),
            column![
                pick_list(Theme::ALL, Some(self.theme.clone()), Message::Theme).width(Length::Fill),
                note("Kanagawa Dragon unless you choose another."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("Decoding").size(15),
            column![
                toggler(self.ten_bit).label("10-bit").on_toggle(Message::TenBit),
                note("Decodes the OSV and 10-bit videos (a phone's HDR) at all their 10 bits instead of 8: smoother skies and gradients, twice the data to move. Applies from the next seek or clip."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("LUTs").size(15),
            self.lut_library(),
            rule::horizontal(1),
            text("Keyframes").size(15),
            column![
                toggler(self.auto_key).label("Auto keyframe").on_toggle(Message::AutoKey),
                note("On: looking around while paused sets the view keyframe at the playhead, or updates the one there, as K does. Each look-around is one undo step. While playing, looking around stays free."),
                note("Off: looking around is free until you press K."),
            ]
            .spacing(6),
            rule::horizontal(1),
            text("View angles").size(15),
            column![
                toggler(self.angles_shown).label("Show the view as numbers").on_toggle(Message::ShowAngles),
                note("A strip under the transport row with the yaw, pitch, roll, FOV and correction: type one, or drag its name sideways, to set it as dragging the view would; scroll or ↑ ↓ on a field nudges it, Shift for finer steps. Handy for giving clips from a camera mounted the same way the same framing."),
            ]
            .spacing(6),
        ]
        .spacing(12)
        .padding(12);
        Some(container(scrollable(body)).width(WIDTH).height(Length::Fill).into())
    }
}

#[cfg(test)]
mod tests {
    use super::theme_named;
    use iced::Theme;

    #[test]
    fn every_theme_is_read_back_as_saved() {
        for theme in Theme::ALL {
            assert_eq!(&theme_named(&theme.to_string()), theme);
        }
        // What the desktop-following choice was saved as.
        assert_eq!(theme_named("system"), Theme::KanagawaDragon);
    }
}
