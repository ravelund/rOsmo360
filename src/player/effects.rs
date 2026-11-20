//! The Effects panel: the playing cut's colour effects at the playhead as
//! sliders (or, with an overlay selected, that overlay's place and effects),
//! and the edit's fades.
//!
//! Moving a slider sets the effect keyframe at the playhead, making one
//! there if there isn't (holding the look there so far, so nothing else
//! moves): a cut's single keyframe is its look throughout, and more of them
//! animate it.

use super::{App, Message, icon_button, tip};
use crate::grade::{EffectKey, Grade, grade_at};
use crate::overlay::{MotionKey, Place, place_at};
use crate::text::{Colour, Outline, TextAlign};
use crate::view::Ease;
use iced::widget::{button, column, container, pick_list, row, rule, scrollable, slider, text, toggler, tooltip};
use iced::{Alignment, Element, Length};

#[derive(Debug, Clone)]
pub enum EffectMsg {
    /// Open or close the panel.
    Toggle,
    /// Effect `i` (as in `PARAMS`) moved to this value.
    Set(usize, f64),
    /// A slider let go: the next move is a change of its own for undo.
    Done,
    /// A keyframe at the playhead holding the effects there.
    Key,
    DeleteKey,
    Ease(Ease),
    /// Effect `i` back to none here, or the cut's effects all gone.
    Reset(Option<usize>),
    /// The selected overlay's place value `i` (as in `PLACE`) moved to this.
    Place(usize, f64),
    /// Its place value `i` back to the default here.
    ResetPlace(usize),
    /// Its place dragged on the view (`handles`).
    Drag(Place),
    /// The selected text overlay's words edited, and its look.
    TextEdit(iced::widget::text_editor::Action),
    Text(TextChange),
    /// The look at the playhead on every cut.
    ToAll,
    /// Draw a box over something grey or white to set the white balance.
    PickWhite,
    FadeIn(f64),
    FadeOut(f64),
    /// Jump to cut `cut`'s effect keyframe at clip time `t`.
    Jump(super::edit::CutRef, f64),
    /// Cut `cut`'s effect keyframe `key` dragged to clip time `t`.
    KeyMove(super::edit::CutRef, usize, f64),
    /// The cut's LUT (how much of it is effect `LUT`, keyframed).
    Lut(LutChoice),
}

/// A LUT in a list: none, or a library name (or a file a project names).
#[derive(Clone, Debug, PartialEq)]
pub enum LutChoice {
    None,
    Named(String),
}

impl LutChoice {
    pub fn of(name: Option<&String>) -> Self {
        name.map_or(LutChoice::None, |n| LutChoice::Named(n.clone()))
    }

    pub fn name(&self) -> Option<String> {
        match self {
            LutChoice::None => None,
            LutChoice::Named(n) => Some(n.clone()),
        }
    }
}

impl std::fmt::Display for LutChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LutChoice::None => f.write_str("None"),
            // A file a project names: its file name.
            LutChoice::Named(n) => f.write_str(std::path::Path::new(n).file_name().map_or(n.as_str(), |s| s.to_str().unwrap_or(n))),
        }
    }
}

/// Each effect: its name, range, and what it does. Untouched, each is
/// `grade::NEUTRAL`'s.
const PARAMS: [(&str, f64, f64, &str); 10] = [
    ("Exposure", -3.0, 3.0, "Brighter or darker, in stops, like the camera's EV"),
    ("Contrast", -1.0, 1.0, "More or less contrast, around mid-grey"),
    ("Highlights", -1.0, 1.0, "Pull back (−) or lift the bright parts, such as the sky"),
    ("Shadows", -1.0, 1.0, "Lift (+) or deepen the dark parts"),
    ("Temperature", -1.0, 1.0, "Cooler (−) or warmer"),
    ("Tint", -1.0, 1.0, "Greener (−) or more magenta"),
    ("Saturation", -1.0, 1.0, "Less or more colour; −1 is black and white"),
    ("Vibrance", -1.0, 1.0, "More colour mostly where there is little, sparing skin and sky"),
    ("Vignette", 0.0, 1.0, "Darker corners"),
    ("Opacity", 0.0, 1.0, "How much of the picture shows; on the main track it fades towards black"),
];

/// The LUT's strength among `Grade::values` (after `PARAMS`), drawn
/// beside the LUT picker.
const LUT: usize = 10;

/// The panel's width.
const WIDTH: f32 = 300.0;

/// A slider's value as shown: stops for exposure, else a signed amount.
fn label(i: usize, v: f64) -> String {
    match i {
        0 => format!("{v:+.2} EV"),
        8 => format!("{v:.2}"),
        9 => format!("{:.0} %", v * 100.0),
        _ => format!("{v:+.2}"),
    }
}

/// A change to a text overlay's look.
#[derive(Debug, Clone)]
pub enum TextChange {
    Font(String),
    Size(f64),
    Colour(Colour),
    Bold(bool),
    Align(TextAlign),
    Outline(bool),
    OutlineWidth(f64),
    OutlineColour(Colour),
    Shadow(bool),
}

/// The colours a text's swatches offer.
const SWATCHES: [[u8; 3]; 9] = [[255, 255, 255], [0, 0, 0], [255, 214, 10], [255, 140, 0], [230, 40, 40], [255, 90, 170], [40, 120, 255], [30, 200, 110], [130, 130, 130]];

/// What the panel edits: the playing cut, or the overlay selected (in the
/// lane shown).
#[derive(Clone, Copy, PartialEq)]
enum Target {
    Cut(super::edit::CutRef),
    Overlay(crate::overlay::ItemRef),
}

/// Where an overlay is, as sliders: its name, range and what it does.
const PLACE: [(&str, f64, f64, &str); 4] = [
    ("Across", 0.0, 1.0, "Where its centre is, from the left edge to the right"),
    ("Down", 0.0, 1.0, "Where its centre is, from the top to the bottom"),
    ("Size", 0.05, 2.0, "1 fills the frame as far as its shape allows"),
    ("Rotation", -180.0, 180.0, "Degrees, clockwise"),
];

impl App {
    /// What the panel edits and the time there in its own clock: the
    /// overlay selected, if it's showing at the playhead, else the playing
    /// cut.
    fn effect_target(&self) -> Option<(Target, f64)> {
        let t = self.position();
        if let Some(i) = self.selected_overlay
            && let Some(item) = self.edit.item(i)
            && t >= item.at
            && t <= item.ends()
        {
            return Some((Target::Overlay(i), item.local(t)));
        }
        self.playing_cut().map(|(_, t)| (Target::Cut(self.here()), t))
    }

    fn effects_of(&self, target: Target) -> &[EffectKey] {
        match target {
            Target::Cut(i) => &self.edit.cut(i).expect("the playing cut").effects,
            Target::Overlay(i) => &self.edit.item(i).expect("the selected overlay").effects,
        }
    }

    fn effects_mut(&mut self, target: Target) -> &mut Vec<EffectKey> {
        match target {
            Target::Cut(i) => &mut self.edit.cut_mut(i).expect("the playing cut").effects,
            Target::Overlay(i) => &mut self.edit.item_mut(i).expect("the selected overlay").effects,
        }
    }

    /// The effect keyframe of `target` at its time `t`, if there's one.
    fn effect_key_at(&self, target: Target, t: f64) -> Option<usize> {
        let tolerance = self.tolerance();
        self.effects_of(target).iter().position(|k| (k.t - t).abs() <= tolerance)
    }

    /// Overlay `i`'s motion keyframe at its time `t`, if there's one.
    fn motion_key_at(&self, i: crate::overlay::ItemRef, t: f64) -> Option<usize> {
        let tolerance = self.tolerance();
        self.edit.item(i)?.motion.iter().position(|k| (k.t - t).abs() <= tolerance)
    }

    /// Sets `target`'s effects at the playhead with `change`: in the
    /// keyframe there, made from the look so far if there's none.
    fn change_effects(&mut self, change: impl FnOnce(&mut Grade)) {
        let Some((target, t)) = self.effect_target() else { return };
        let at = self.effect_key_at(target, t);
        let keys = self.effects_mut(target);
        let k = match at {
            Some(k) => k,
            None => {
                let k = keys.partition_point(|k| k.t <= t);
                keys.insert(k, EffectKey { t, grade: grade_at(keys, t), ease: Ease::Smooth });
                k
            }
        };
        change(&mut keys[k].grade);
    }

    /// Sets the selected overlay's place at the playhead with `change`, in
    /// the motion keyframe there, made from where it is if there's none.
    fn change_place(&mut self, change: impl FnOnce(&mut Place)) {
        let Some((Target::Overlay(i), t)) = self.effect_target() else { return };
        let at = self.motion_key_at(i, t);
        let Some(item) = self.edit.item_mut(i) else { return };
        let keys = &mut item.motion;
        let k = match at {
            Some(k) => k,
            None => {
                let k = keys.partition_point(|k| k.t <= t);
                keys.insert(k, MotionKey { t, place: place_at(keys, t), ease: Ease::Smooth });
                k
            }
        };
        change(&mut keys[k].place);
    }

    pub(super) fn effect_message(&mut self, m: EffectMsg) {
        let target = self.effect_target();
        match m {
            EffectMsg::Toggle => self.toggle_side(super::Side::Effects),
            EffectMsg::Set(p, v) => {
                self.begin_drag();
                self.change_effects(|g| {
                    let mut values = g.values();
                    values[p] = v;
                    *g = Grade::from_values(values);
                });
            }
            EffectMsg::Place(p, v) => {
                self.begin_drag();
                self.change_place(|place| {
                    let mut values = place.values();
                    values[p] = v;
                    *place = Place::from_values(values);
                });
            }
            EffectMsg::Drag(to) => {
                self.begin_drag();
                self.change_place(|place| *place = to);
            }
            EffectMsg::Done => self.dragging = false,
            EffectMsg::Lut(choice) => {
                let Some((Target::Cut(i), _)) = target else { return };
                self.edit.checkpoint();
                let Some(cut) = self.edit.cut_mut(i) else { return };
                cut.lut = choice.name();
                if let Err(e) = cut.resolve_lut() {
                    self.error = Some(format!("{e:#}"));
                }
            }
            EffectMsg::TextEdit(action) => {
                let edits = action.is_edit();
                self.text_content.perform(action);
                let words = self.text_content.text().trim_end_matches('\n').to_owned();
                if edits && self.selected_text().is_some_and(|t| t.content != words) {
                    // A run of typing is one undo step, until something else is done.
                    self.begin_drag();
                    self.typing = true;
                    if let Some(text) = self.selected_text() {
                        text.content = words;
                    }
                }
            }
            EffectMsg::Text(change) => {
                // Sliders are one undo step a drag, the rest one a click.
                match change {
                    TextChange::Size(_) | TextChange::OutlineWidth(_) => self.begin_drag(),
                    _ => self.edit.checkpoint(),
                }
                let Some(text) = self.selected_text() else { return };
                match change {
                    TextChange::Font(f) => text.font = f,
                    TextChange::Size(v) => text.size = v,
                    TextChange::Colour(c) => text.colour = c,
                    TextChange::Bold(b) => text.bold = b,
                    TextChange::Align(a) => text.align = a,
                    TextChange::Outline(on) => text.outline = on.then_some(Outline { colour: Colour([0, 0, 0]), width: 0.08 }),
                    TextChange::OutlineWidth(w) => {
                        if let Some(o) = &mut text.outline {
                            o.width = w;
                        }
                    }
                    TextChange::OutlineColour(c) => {
                        if let Some(o) = &mut text.outline {
                            o.colour = c;
                        }
                    }
                    TextChange::Shadow(on) => text.shadow = on,
                }
            }
            // On an overlay a keyframe holds both where it is and its look.
            EffectMsg::Key => {
                self.edit.checkpoint();
                self.change_effects(|_| {});
                self.change_place(|_| {});
            }
            EffectMsg::DeleteKey => {
                let Some((target, t)) = target else { return };
                let (effect, motion) = (self.effect_key_at(target, t), if let Target::Overlay(i) = target { self.motion_key_at(i, t) } else { None });
                if effect.is_some() || motion.is_some() {
                    self.edit.checkpoint();
                }
                if let Some(k) = effect {
                    self.effects_mut(target).remove(k);
                }
                if let (Target::Overlay(i), Some(k)) = (target, motion)
                    && let Some(item) = self.edit.item_mut(i)
                {
                    item.motion.remove(k);
                }
            }
            EffectMsg::Ease(ease) => {
                let Some((target, t)) = target else { return };
                let (effect, motion) = (self.effect_key_at(target, t), if let Target::Overlay(i) = target { self.motion_key_at(i, t) } else { None });
                if effect.is_some() || motion.is_some() {
                    self.edit.checkpoint();
                }
                if let Some(k) = effect {
                    self.effects_mut(target)[k].ease = ease;
                }
                if let (Target::Overlay(i), Some(k)) = (target, motion)
                    && let Some(item) = self.edit.item_mut(i)
                {
                    item.motion[k].ease = ease;
                }
            }
            EffectMsg::Reset(Some(p)) => {
                self.edit.checkpoint();
                self.change_effects(|g| {
                    let mut values = g.values();
                    values[p] = crate::grade::NEUTRAL[p];
                    *g = Grade::from_values(values);
                });
            }
            EffectMsg::ResetPlace(p) => {
                self.edit.checkpoint();
                self.change_place(|place| {
                    let mut values = place.values();
                    values[p] = Place::default().values()[p];
                    *place = Place::from_values(values);
                });
            }
            EffectMsg::Reset(None) => {
                let Some((target, _)) = target else { return };
                let moves = matches!(target, Target::Overlay(i) if self.edit.item(i).is_some_and(|it| !it.motion.is_empty()));
                if !self.effects_of(target).is_empty() || moves {
                    self.edit.checkpoint();
                    self.effects_mut(target).clear();
                    if let Target::Overlay(i) = target
                        && let Some(item) = self.edit.item_mut(i)
                    {
                        item.motion.clear();
                    }
                }
            }
            EffectMsg::ToAll => {
                let Some((_, t)) = self.playing_cut() else { return };
                let grade = grade_at(&self.edit.play[self.current].effects, t);
                self.edit.checkpoint();
                // Every cut of both tracks but this one.
                let here = self.here();
                let tracks = [&mut self.edit.cuts, &mut self.edit.upper];
                for (track, cuts) in tracks.into_iter().enumerate() {
                    for (i, cut) in cuts.iter_mut().enumerate() {
                        if (track, i) != here {
                            cut.effects = vec![EffectKey { t: cut.start, grade, ease: Ease::Smooth }];
                        }
                    }
                }
            }
            EffectMsg::PickWhite => {
                if self.picking.take().is_none() && self.clip.is_some() {
                    self.with_playback(super::Playback::pause);
                    self.picking = Some(None);
                    self.picking_white = true;
                    self.save_status = Some("Draw a box over something that should be grey or white (Esc cancels)".to_owned());
                } else {
                    self.picking_white = false;
                    self.save_status = None;
                }
            }
            EffectMsg::FadeIn(v) => {
                self.begin_drag();
                self.edit.fades.fade_in = v;
            }
            EffectMsg::FadeOut(v) => {
                self.begin_drag();
                self.edit.fades.fade_out = v;
            }
            EffectMsg::Jump(cut, t) => {
                self.follow = true;
                self.selected = Some(cut);
                self.selected_overlay = None;
                if let Some((j, t)) = self.edit.piece_of(cut, t) {
                    self.goto(j, t, false);
                }
            }
            EffectMsg::KeyMove(cut, key, t) => {
                self.follow = true;
                self.begin_drag();
                let gap = 1.0 / self.fps();
                let Some(c) = self.edit.cut_mut(cut) else { return };
                crate::view::retime(&mut c.effects, key, t, gap);
                if let Some(t) = c.effects.get(key).map(|k| k.t) {
                    self.edit.reflow();
                    if let Some((j, t)) = self.edit.piece_of(cut, t) {
                        self.scrub_to(j, t);
                    }
                }
            }
        }
    }

    /// The selected overlay's text, if it's a text overlay.
    fn selected_text(&mut self) -> Option<&mut crate::text::Text> {
        let i = self.selected_overlay?;
        self.edit.item_mut(i)?.text.as_mut()
    }

    /// The text panel, at the player's left while a text item is selected:
    /// its words and look, wherever the playhead is (where it is and its
    /// effects stay in the Effects panel).
    pub(super) fn text_panel(&self) -> Option<Element<'_, Message>> {
        let i = self.selected_overlay?;
        let words = self.edit.item(i)?.text.as_ref()?;
        let t = |c: TextChange| Message::Effect(EffectMsg::Text(c));
        let swatches = |chosen: Colour, f: fn(Colour) -> TextChange| {
            let mut r = row![].spacing(4);
            for c in SWATCHES {
                let colour = iced::Color::from_rgb8(c[0], c[1], c[2]);
                let on = chosen.0 == c;
                r = r.push(
                    button(iced::widget::space().width(16).height(16))
                        .padding(0)
                        .style(move |theme: &iced::Theme, _| button::Style {
                            background: Some(colour.into()),
                            border: iced::Border { color: if on { theme.extended_palette().primary.strong.color } else { iced::Color::from_rgba(0.5, 0.5, 0.5, 0.6) }, width: if on { 2.5 } else { 1.0 }, radius: 3.0.into() },
                            ..button::Style::default()
                        })
                        .on_press(Message::Effect(EffectMsg::Text(f(Colour(c))))),
                );
            }
            r
        };
        let families: Vec<FontChoice> = crate::text::families().iter().map(|f| FontChoice(f.clone())).collect();
        let close = tip(
            button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(Message::OverlaySelect(None)),
            text("Close (deselects the text)"),
            tooltip::Position::Bottom,
        );
        let mut controls = column![
            row![text("Text").size(18).width(Length::Fill), close].align_y(Alignment::Center),
            iced::widget::text_editor(&self.text_content).placeholder("Type the text here").height(80).on_action(|a| Message::Effect(EffectMsg::TextEdit(a))),
            row![
                tip(
                    pick_list(families, Some(FontChoice(words.font.clone())), |f| Message::Effect(EffectMsg::Text(TextChange::Font(f.0)))).width(Length::Fill),
                    text("Built-in fonts come with rOsmo360, so the text looks the same on any machine; the others are this system's, and where a project's font is missing the text falls back to Inter."),
                    tooltip::Position::Bottom,
                ),
                iced::widget::checkbox(words.bold).label("Bold").on_toggle(move |b| Message::Effect(EffectMsg::Text(TextChange::Bold(b)))),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            row![text("Size").width(Length::Fill), text(format!("{:.0} %", words.size * 100.0)).size(13)],
            slider(0.02..=0.3, words.size, move |v| Message::Effect(EffectMsg::Text(TextChange::Size(v)))).step(0.005).on_release(Message::Effect(EffectMsg::Done)),
            row![text("Lines").width(Length::Fill), pick_list(&TextAlign::ALL[..], Some(words.align), move |a| t(TextChange::Align(a))).width(110)].align_y(Alignment::Center),
            text("Colour"),
            swatches(words.colour, TextChange::Colour),
            iced::widget::checkbox(words.shadow).label("Shadow").on_toggle(move |b| Message::Effect(EffectMsg::Text(TextChange::Shadow(b)))),
            iced::widget::checkbox(words.outline.is_some()).label("Outline").on_toggle(move |b| Message::Effect(EffectMsg::Text(TextChange::Outline(b)))),
        ]
        .spacing(6);
        if let Some(o) = words.outline {
            controls = controls.push(row![text("Outline width").width(Length::Fill), text(format!("{:.0} %", o.width * 100.0)).size(13)]);
            controls = controls.push(slider(0.01..=0.3, o.width, move |v| Message::Effect(EffectMsg::Text(TextChange::OutlineWidth(v)))).step(0.005).on_release(Message::Effect(EffectMsg::Done)));
            controls = controls.push(swatches(o.colour, TextChange::OutlineColour));
        }
        Some(container(scrollable(controls.spacing(8).padding(12))).width(WIDTH).height(Length::Fill).into())
    }

    /// Neutralises the playing cut's colour cast from `rgb`, the median of
    /// something meant to be grey or white.
    pub(super) fn white_balance(&mut self, rgb: [f64; 3]) {
        let (temperature, tint) = crate::grade::white_balance(rgb);
        self.edit.checkpoint();
        // The picked colour is the cut's, whatever the panel shows.
        self.selected_overlay = None;
        self.change_effects(|g| {
            g.temperature = temperature;
            g.tint = tint;
        });
    }

    /// Cut `i`'s LUT, and its strength in `grade` (the effects at the
    /// playhead): the LUT acts before the effects, which act on what it gives.
    fn lut_controls(&self, i: super::edit::CutRef, grade: &Grade) -> Element<'_, Message> {
        let Some(cut) = self.edit.cut(i) else { return column![].into() };
        let mut choices = vec![LutChoice::None];
        choices.extend(self.luts.iter().map(|n| LutChoice::Named(n.clone())));
        let current = LutChoice::of(cut.lut.as_ref());
        if !choices.contains(&current) {
            choices.push(current.clone());
        }
        let d_log_m = self.library.iter().find(|m| m.input == cut.input).is_some_and(|m| m.color_mode == crate::dji::ColorMode::DLogM);
        let hint = if self.luts.is_empty() {
            "Add LUTs (.cube) in Settings: DJI's D-Log M to Rec.709 for D-Log M footage, or any look"
        } else {
            "A LUT turns the picture's colours into others before the effects: DJI's D-Log M to Rec.709 makes D-Log M footage look normal"
        };
        let mut col = column![
            row![text("LUT").width(Length::Fill), text(if d_log_m { "filmed in D-Log M" } else { "" }).size(13).style(text::secondary)].align_y(Alignment::Center),
            tip(pick_list(choices, Some(current), |c| Message::Effect(EffectMsg::Lut(c))).text_size(13).width(Length::Fill), text(hint), tooltip::Position::Left),
        ]
        .spacing(4);
        if cut.lut.is_some() {
            // An effect like the others, keyed at the playhead as it moves.
            let v = grade.lut;
            let reset = button(text("↺").size(13)).padding([0, 5]).style(button::text).on_press_maybe((v != 1.0).then(|| Message::Effect(EffectMsg::Reset(Some(LUT)))));
            col = col.push(tip(
                column![
                    row![text("Strength").width(Length::Fill), text(format!("{:.0} %", v * 100.0)).size(13), reset].spacing(4).align_y(Alignment::Center),
                    slider(0.0..=1.0, v, |v| Message::Effect(EffectMsg::Set(LUT, v))).step(0.01).on_release(Message::Effect(EffectMsg::Done)),
                ]
                .spacing(2),
                text("How much of the LUT: 100 % is all of it, less mixes in the picture as filmed. Keyframed with the effects, so it can fade in or out"),
                tooltip::Position::Left,
            ));
        }
        col.push(rule::horizontal(1)).into()
    }

    pub(super) fn effects_panel(&self) -> Element<'_, Message> {
        let m = |e: EffectMsg| Message::Effect(e);
        let target = self.effect_target();
        let grade = target.map(|(tg, t)| grade_at(self.effects_of(tg), t)).unwrap_or_default();
        let key = target.and_then(|(tg, t)| self.effect_key_at(tg, t));
        let overlay = target.and_then(|(tg, t)| match tg {
            Target::Overlay(i) => Some((i, t)),
            Target::Cut(_) => None,
        });
        let motion_key = overlay.and_then(|(i, t)| self.motion_key_at(i, t));
        let on_cut = matches!(target, Some((Target::Cut(_), _)));
        let enabled = target.is_some();
        let mut sliders = column![].spacing(6);
        // An overlay's place first: where it is is what's set most.
        if let Some((i, t)) = overlay {
            let place = self.edit.item(i).map_or_else(Place::default, |it| place_at(&it.motion, t));
            for (p, (name, lo, hi, hint)) in PLACE.iter().enumerate() {
                let v = place.values()[p];
                let shown = if p == 3 { format!("{v:+.0}°") } else { format!("{v:.2}") };
                let reset = button(text("↺").size(13)).padding([0, 5]).style(button::text).on_press_maybe((v != Place::default().values()[p]).then(|| m(EffectMsg::ResetPlace(p))));
                let control = slider(*lo..=*hi, v, move |v| Message::Effect(EffectMsg::Place(p, v))).step(if p == 3 { 1.0 } else { 0.01 }).on_release(m(EffectMsg::Done));
                sliders = sliders.push(tip(
                    column![row![text(*name).width(Length::Fill), text(shown).size(13), reset].spacing(4).align_y(Alignment::Center), control].spacing(2),
                    text(*hint),
                    tooltip::Position::Left,
                ));
            }
            sliders = sliders.push(rule::horizontal(1));
        }
        for (p, (name, lo, hi, hint)) in PARAMS.iter().enumerate() {
            let v = grade.values()[p];
            let reset = button(text("↺").size(13)).padding([0, 5]).style(button::text).on_press_maybe((enabled && v != crate::grade::NEUTRAL[p]).then(|| m(EffectMsg::Reset(Some(p)))));
            let control = slider(*lo..=*hi, v, move |v| Message::Effect(EffectMsg::Set(p, v))).step(0.01).on_release(m(EffectMsg::Done));
            sliders = sliders.push(tip(
                column![row![text(*name).width(Length::Fill), text(label(p, v)).size(13), reset].spacing(4).align_y(Alignment::Center), control].spacing(2),
                text(*hint),
                tooltip::Position::Left,
            ));
        }
        let cut_name = match target {
            Some((Target::Cut(i), _)) => format!("{} {} · {}", if i.0 == 0 { "Cut" } else { "Upper cut" }, i.1 + 1, self.edit.cut(i).map_or(String::new(), |c| super::clip_name(&c.input))),
            Some((Target::Overlay(i), _)) => match self.edit.item(i) {
                Some(it) if it.text.is_some() => format!("Text · {}", it.name()),
                Some(it) => format!("Overlay · {}", super::clip_name(&it.input)),
                None => String::new(),
            },
            None => "No cut".to_owned(),
        };
        let any_key = key.or(motion_key);
        let ease = match (key, motion_key, target) {
            (Some(k), _, Some((tg, _))) => Some(self.effects_of(tg)[k].ease),
            (None, Some(k), _) => overlay.and_then(|(i, _)| self.edit.item(i)).map(|it| it.motion[k].ease),
            _ => None,
        };
        let keys = row![
            tip(icon_button(super::KEY_ADD, enabled.then(|| m(EffectMsg::Key))), text("An effect keyframe here, holding the effects as they are; the sliders also make one as they move"), tooltip::Position::Bottom),
            tip(icon_button(super::KEY_DELETE, any_key.map(|_| m(EffectMsg::DeleteKey))), text("Delete the effect keyframe here"), tooltip::Position::Bottom),
            tip(
                pick_list(&Ease::ALL[..], ease, |e| Message::Effect(EffectMsg::Ease(e))).placeholder("Ease").width(96),
                text("How the effects move on from this keyframe to the next"),
                tooltip::Position::Bottom,
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let tools = row![
            tip(icon_button(super::PICK, on_cut.then(|| m(EffectMsg::PickWhite))), text("White balance: draw a box over something that should be grey or white (snow, a cloud)"), tooltip::Position::Bottom),
            tip(icon_button(super::TO_ALL_CUTS, on_cut.then(|| m(EffectMsg::ToAll))), text("To all cuts: give every other cut this look (as at the playhead), replacing their effects"), tooltip::Position::Bottom),
            tip(
                icon_button(
                    super::RESET_VIEW,
                    target
                        .filter(|&(tg, _)| !self.effects_of(tg).is_empty() || matches!(tg, Target::Overlay(i) if self.edit.item(i).is_some_and(|it| !it.motion.is_empty())))
                        .map(|_| m(EffectMsg::Reset(None))),
                ),
                text(if on_cut { "Reset: remove this cut's effects" } else { "Reset: remove this overlay's effects and moves" }),
                tooltip::Position::Bottom,
            ),
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let fades = &self.edit.fades;
        let fade = |name: &'static str, v: f64, f: fn(f64) -> EffectMsg, hint: &'static str| {
            tip(
                column![
                    row![text(name).width(Length::Fill), text(if v > 0.0 { format!("{v:.1} s") } else { "none".to_owned() }).size(13)],
                    slider(0.0..=5.0, v, move |v| Message::Effect(f(v))).step(0.1).on_release(Message::Effect(EffectMsg::Done)),
                ]
                .spacing(2),
                text(hint),
                tooltip::Position::Left,
            )
        };
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(m(EffectMsg::Toggle)), text("Close (G)"), tooltip::Position::Bottom);
        let mut body = column![row![text("Effects").size(18).width(Length::Fill), close].align_y(Alignment::Center), text(cut_name).size(13)].spacing(10).padding(12);
        if let Some((Target::Cut(i), _)) = target {
            body = body.push(self.lut_controls(i, &grade));
        }
        let body = body.extend([column![
            sliders,
            keys,
            tools,
            rule::horizontal(1),
            text("Edit").size(16),
            fade("Fade in", fades.fade_in, EffectMsg::FadeIn, "From black at the start of the edit, the sound with it"),
            fade("Fade out", fades.fade_out, EffectMsg::FadeOut, "To black at the end of the edit, the sound with it"),
            rule::horizontal(1),
            text("Correction").size(16),
            tip(
                toggler(self.rolling_shutter).label("Rolling shutter").on_toggle(Message::RollingShutter),
                text("Straightens what the sensor's row-by-row readout bends while the camera turns, in playback and the export"),
                tooltip::Position::Left,
            ),
        ]
        .spacing(10)
        .into()]);
        container(scrollable(body)).width(WIDTH).height(Length::Fill).into()
    }
}

/// A font family in the text panel's list, the built-in ones marked.
#[derive(Clone, PartialEq)]
struct FontChoice(String);

impl std::fmt::Display for FontChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if crate::text::BUILT_IN.contains(&self.0.as_str()) { write!(f, "{} · built in", self.0) } else { f.write_str(&self.0) }
    }
}
