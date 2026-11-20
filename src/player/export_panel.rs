//! The Export panel: what the export makes (resolution, format, one file
//! or a file per cut) and the button that starts it, docked at the right
//! of the view like the Effects panel, so the top bar keeps only the
//! button that opens it.

use super::{App, Aspect, EXPORT, Message, clock, labelled_button, tip};
use crate::ffmpeg::Format;
use iced::widget::{checkbox, column, container, pick_list, radio, row, rule, scrollable, space, text, tooltip};
use iced::{Element, Length};

const WIDTH: f32 = 300.0;

/// How far an option's note and sub-options sit in, to line up with its
/// label after the radio button or checkbox.
const INDENT: f32 = 26.0;

/// Small dimmed text under an option, saying what it does.
fn note<'a>(s: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    text(s).size(13).style(text::secondary).into()
}

/// A choice of the export's frame rate: the clips' own, or a fixed one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate(pub Option<u32>);

impl Rate {
    const ALL: [Rate; 7] = [Rate(None), Rate(Some(24)), Rate(Some(25)), Rate(Some(30)), Rate(Some(50)), Rate(Some(60)), Rate(Some(100))];
}

impl std::fmt::Display for Rate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => f.write_str("As filmed"),
            Some(fps) => write!(f, "{fps} fps"),
        }
    }
}

impl App {
    pub(super) fn export_panel(&self) -> Element<'_, Message> {
        let (shape, about_shape) = match self.aspect {
            Aspect::Ratio(w, h) => (crate::view::Aspect(w.into(), h.into()), "In the aspect's shape (top bar)."),
            Aspect::Free => (crate::view::Aspect(16, 9), "16:9, as the aspect (top bar) is Free."),
        };
        let [w, h] = shape.size(self.resolution.short_side());
        let what = if self.export_clips {
            format!("{} clips, {w} × {h}", self.edit.cuts.len() + self.edit.upper.len())
        } else {
            format!("{w} × {h}, {}", clock(self.edit.duration()))
        };
        let format = |f: Format, name: &'static str, about: &'static str| {
            column![radio(name, f, Some(self.export_format), Message::ExportFormat), row![space().width(INDENT), note(about)]].spacing(2)
        };
        let clips = column![
            tip(
                checkbox(self.export_clips).label("A file per cut").on_toggle(Message::ExportClips),
                text("For another editor: \"<project> - 001\" and on, into a folder you pick; each music clip as a WAV beside them, unfaded"),
                tooltip::Position::Left,
            ),
        ]
        .push(self.export_clips.then(|| {
            row![
                space().width(INDENT),
                tip(
                    checkbox(self.export_graded).label("Graded").on_toggle(Message::ExportGraded),
                    text("Apply the cuts' LUTs and colour effects to the clips too; left off, they're as shot (D-Log M stays log), to grade in the other editor"),
                    tooltip::Position::Left,
                ),
            ]
        }))
        .spacing(6);
        let go = (!self.edit.cuts.is_empty() && self.exporting.is_none()).then_some(Message::Export);
        let body = column![
            text("Export").size(18),
            column![
                text("Resolution"),
                pick_list(&crate::Resolution::ALL[..], Some(self.resolution), Message::Resolution).width(Length::Fill),
                note("Pixels on the picture's short side."),
            ]
            .spacing(4),
            column![
                text("Frame rate"),
                pick_list(&Rate::ALL[..], Some(Rate(self.export_fps)), Message::ExportFps).width(Length::Fill),
                note("As filmed keeps the clips' own rate. A lower one drops frames, not speed: 100 fps clips at 30 export about three times as fast."),
            ]
            .spacing(4),
            column![
                text("Format"),
                format(Format::Mp4, "MP4", "H.264 and AAC: plays anywhere, small."),
                format(Format::Resolve, "DaVinci Resolve", "10-bit DNxHR HQX with PCM sound in .mov, which Resolve's free Linux version reads, with room to grade. Large: about 28 MB a second at 1080p."),
            ]
            .spacing(6),
            column![text("Clips"), clips].spacing(6),
            rule::horizontal(1),
            column![text(what), note(about_shape)].spacing(2),
            labelled_button(EXPORT, if self.export_clips { "Export clips…" } else { "Export…" }, go),
        ]
        .spacing(14)
        .padding(12);
        #[cfg(feature = "zero-copy")]
        let body = body.push(rule::horizontal(1)).push(
            column![
                text("Test features"),
                checkbox(self.zero_copy).label("Zero-copy decoding").on_toggle(Message::ZeroCopy),
                row![space().width(INDENT), note("Decodes the lenses on the graphics card they're stitched on, without copying each frame through memory: half again to twice as fast, at all 10 bits. Needs Vulkan video decoding; where it can't start, the export decodes as usual.")],
            ]
            .spacing(6),
        );
        container(scrollable(body)).width(WIDTH).height(Length::Fill).into()
    }
}
