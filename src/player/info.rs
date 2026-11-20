//! The Info panel: what a file on the timeline is (right-click a cut, a
//! song or an overlay picture), where it is in the edit, and buttons to
//! show it in its folder or copy its path. Docked at the left of the view,
//! in the Text panel's place.

use super::{App, COPY, FOLDER, Message, clock, icon_button, tip};
use crate::overlay::ItemRef;
use anyhow::Result;
use iced::widget::{button, column, container, row, rule, scrollable, text, tooltip};
use iced::{Alignment, Element, Length};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const WIDTH: f32 = 300.0;

/// What on the timeline the panel is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Of {
    Cut(super::edit::CutRef),
    Music(usize),
    Overlay(ItemRef),
}

/// The panel's file, and what's been read about it (`None` while reading).
pub struct Info {
    pub of: Of,
    pub path: PathBuf,
    pub file: Option<Result<Arc<File>, String>>,
}

/// What a file is, as lines to show.
#[derive(Debug)]
pub struct File {
    pub kind: &'static str,
    pub rows: Vec<(&'static str, String)>,
}

/// Reads what `path` is: ffprobe's account of it, the file's size, and for
/// an Osmo 360 clip its proxy. Blocking (ffprobe; a clip on a share).
pub fn read(path: &Path, song: bool) -> Result<File> {
    let probe = crate::ffmpeg::probe_all(path)?;
    let flat = if song { None } else { crate::ffmpeg::flat(path)? };
    let kind = match &flat {
        _ if song => "Sound",
        None => "Osmo 360 clip",
        Some(f) if f.picture => "Picture",
        Some(_) => "Video",
    };
    let format = &probe["format"];
    let streams = probe["streams"].as_array().cloned().unwrap_or_default();
    let of_type = |t: &str| streams.iter().filter(|s| s["codec_type"] == t && s["disposition"]["attached_pic"] != 1).cloned().collect::<Vec<_>>();
    let (video, audio) = (of_type("video"), of_type("audio"));
    let number = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).or(v.as_f64());
    let mut rows = Vec::new();
    if kind != "Picture"
        && let Some(d) = number(&format["duration"])
    {
        rows.push(("Length", clock(d)));
    }
    if let Some(v) = video.first() {
        let (w, h) = flat.map_or((v["width"].as_u64().unwrap_or(0) as u32, v["height"].as_u64().unwrap_or(0) as u32), |f| f.size());
        rows.push(("Picture", if kind == "Osmo 360 clip" { format!("two lenses, {w} × {h} each") } else { format!("{w} × {h}") }));
        if kind != "Picture" {
            if let Some(fps) = v["avg_frame_rate"].as_str().and_then(rate).or_else(|| v["r_frame_rate"].as_str().and_then(rate)) {
                rows.push(("Frame rate", format!("{} fps", trim(fps))));
            }
            let codec = codec_name(v["codec_name"].as_str().unwrap_or("?"));
            let profile = v["profile"].as_str().map_or(String::new(), |p| format!(" {p}"));
            let bits = if v["pix_fmt"].as_str().is_some_and(|f| f.contains("10")) { 10 } else { 8 };
            rows.push(("Video", format!("{codec}{profile}, {bits}-bit")));
        } else {
            rows.push(("Format", codec_name(v["codec_name"].as_str().unwrap_or("?"))));
        }
    }
    if kind != "Picture" {
        rows.push((
            "Sound",
            match audio.first() {
                Some(a) => {
                    let khz = number(&a["sample_rate"]).map_or(String::new(), |r| format!(", {} kHz", trim(r / 1000.0)));
                    let layout = a["channel_layout"].as_str().map(str::to_owned).or_else(|| a["channels"].as_u64().map(|c| format!("{c} channels"))).unwrap_or_default();
                    format!("{}{khz}{}", codec_name(a["codec_name"].as_str().unwrap_or("?")), if layout.is_empty() { String::new() } else { format!(", {layout}") })
                }
                None => "none".to_owned(),
            },
        ));
    }
    if let Some(t) = format["tags"]["creation_time"].as_str() {
        // "2026-07-13T13:30:35.000000Z": to the second, in UTC as stored.
        rows.push(("Recorded", format!("{} UTC", t.replace('T', " ").split('.').next().unwrap_or(t).trim_end_matches('Z'))));
    }
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let rate = number(&format["bit_rate"]).filter(|_| kind != "Picture").map_or(String::new(), |b| {
        if b >= 1e6 { format!(", {:.0} Mbit/s", b / 1e6) } else { format!(", {:.0} kbit/s", b / 1e3) }
    });
    rows.push(("File", format!("{}{rate}", size(bytes))));
    if kind == "Osmo 360 clip" {
        rows.push((
            "Proxy",
            // Its name is the clip's with .LRF, too long for the panel.
            if crate::ffmpeg::proxy(path).is_some() { "the .LRF beside it" } else { "none: it loads and plays from the full clip" }.to_owned(),
        ));
    }
    Ok(File { kind, rows })
}

/// "30000/1001" as 29.97.
fn rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

/// A number to two decimals at most, without trailing zeros.
fn trim(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..1_000_000 => format!("{:.0} kB", b / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", b / 1e6),
        _ => format!("{:.2} GB", b / 1e9),
    }
}

fn codec_name(codec: &str) -> String {
    match codec {
        "hevc" => "HEVC (H.265)".to_owned(),
        "h264" => "H.264".to_owned(),
        "aac" => "AAC".to_owned(),
        "mp3" => "MP3".to_owned(),
        "mjpeg" => "JPEG".to_owned(),
        "dnxhd" => "DNxHR".to_owned(),
        c if c.starts_with("pcm_") => "PCM".to_owned(),
        c => c.to_uppercase(),
    }
}

/// Opens the file manager at `path`, the file selected where it can be.
pub fn reveal(path: &Path) -> Result<()> {
    use anyhow::Context;
    use std::process::Command;
    if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn().context("running open")?;
    } else if cfg!(windows) {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path);
        Command::new("explorer").arg(arg).spawn().context("running explorer")?;
    } else {
        // The file managers' D-Bus interface selects the file (Dolphin,
        // Nautilus, Nemo); without it, open the folder.
        #[cfg(target_os = "linux")]
        if show_items(path).is_ok() {
            return Ok(());
        }
        Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn().context("running xdg-open")?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn show_items(path: &Path) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    connection.call_method(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        Some("org.freedesktop.FileManager1"),
        "ShowItems",
        &(vec![file_uri(path)], ""),
    )?;
    Ok(())
}

/// `path` as a file:// URI, its bytes escaped where a URI needs it.
#[cfg(target_os = "linux")]
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = "file://".to_owned();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

impl App {
    /// The Info panel, while one is open.
    pub(super) fn info_panel(&self) -> Option<Element<'_, Message>> {
        let info = self.info.as_ref()?;
        let close = tip(button(text("✕").size(14)).padding([0, 6]).style(button::text).on_press(Message::InfoClose), text("Close"), tooltip::Position::Bottom);
        let name = info.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let line = |label: &'static str, value: String| row![text(label).size(13).style(text::secondary).width(84), text(value).size(13).width(Length::Fill)].spacing(6);
        let mut body = column![row![text("Info").size(18).width(Length::Fill), close].align_y(Alignment::Center), text(name).size(15)].spacing(10).padding(12);
        match &info.file {
            None => body = body.push(text("Reading…").size(13).style(text::secondary)),
            Some(Err(e)) => body = body.push(text(e.clone()).size(13).style(text::danger)),
            Some(Ok(file)) => {
                let mut rows = column![line("Kind", file.kind.to_owned())].spacing(4);
                for (label, value) in &file.rows {
                    rows = rows.push(line(label, value.clone()));
                }
                body = body.push(rows);
            }
        }
        let place = self.info_place(info);
        if !place.is_empty() {
            let mut rows = column![text("In the edit").size(15)].spacing(4);
            for (label, value) in place {
                rows = rows.push(line(label, value));
            }
            body = body.push(rule::horizontal(1)).push(rows);
        }
        let folder = info.path.parent().map_or_else(String::new, |p| p.display().to_string());
        body = body.push(rule::horizontal(1)).push(column![text("Folder").size(13).style(text::secondary), text(folder).size(13)].spacing(2)).push(
            row![
                tip(icon_button(FOLDER, Some(Message::Reveal(info.path.clone()))), text("Show in folder"), tooltip::Position::Bottom),
                tip(icon_button(COPY, Some(Message::CopyPath(info.path.clone()))), text("Copy the file's path"), tooltip::Position::Bottom),
            ]
            .spacing(6),
        );
        Some(container(scrollable(body)).width(WIDTH).height(Length::Fill).into())
    }

    /// Where the panel's item sits in the edit, if it's still there (an
    /// edit since may have moved or removed it).
    fn info_place(&self, info: &Info) -> Vec<(&'static str, String)> {
        let span = |a: f64, b: f64| format!("{} – {}", clock(a), clock(b));
        let mut rows = Vec::new();
        match info.of {
            Of::Cut(r) => {
                let Some(cut) = self.edit.cut(r).filter(|c| c.input == info.path) else { return rows };
                let at = self.edit.start_of(r);
                match r {
                    (0, i) => rows.push(("Cut", format!("{} of {}", i + 1, self.edit.cuts.len()))),
                    (_, k) => rows.push(("Upper cut", format!("{} of {}", k + 1, self.edit.upper.len()))),
                }
                rows.push(("On the edit", span(at, at + cut.end - cut.start)));
                rows.push(("From the file", span(cut.start, cut.end)));
                let uses = self.edit.cuts.iter().chain(&self.edit.upper).filter(|c| c.input == info.path).count();
                if uses > 1 {
                    rows.push(("Used", format!("in {uses} cuts")));
                }
            }
            Of::Music(i) => {
                let Some(m) = self.edit.music.clips.get(i).filter(|m| m.input == info.path) else { return rows };
                rows.push(("On the edit", span(m.at, m.at + m.end - m.start)));
                rows.push(("From the file", span(m.start, m.end)));
            }
            Of::Overlay(r) => {
                let Some(it) = self.edit.item(r).filter(|it| it.input == info.path) else { return rows };
                rows.push(("On the edit", span(it.at, it.at + it.end - it.start)));
            }
        }
        rows
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn file_uris_escape_what_a_uri_needs() {
        assert_eq!(super::file_uri(std::path::Path::new("/Videoklipp/Fjell tur/ø_0037.OSV")), "file:///Videoklipp/Fjell%20tur/%C3%B8_0037.OSV");
    }
}
