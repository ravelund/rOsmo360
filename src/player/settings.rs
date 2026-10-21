//! The player's settings, remembered between runs: saved whenever one
//! changes, in the OS's usual place for configuration.

use crate::Resolution;
use crate::ffmpeg::Format;
use crate::stab::Mode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub stabilize: Mode,
    /// The OSV size to play, or the proxy if `None`.
    pub osv_size: Option<u32>,
    pub rolling_shutter: bool,
    pub full_on_pause: bool,
    pub volume: f32,
    pub muted: bool,
    pub resolution: Resolution,
    /// The aspect lock, width and height; free if `None`.
    pub aspect: Option<[u16; 2]>,
    /// Export each cut to its own file.
    pub export_clips: bool,
    /// Apply the cuts' colour effects to those files.
    pub export_graded: bool,
    pub export_format: Format,
    /// Looking around while paused keys the view at the playhead.
    pub auto_key: bool,
    /// The colour theme, by iced's name for it (`settings_panel::theme_named`).
    pub theme: String,
    /// Decode 10-bit tracks at 10 bits (`source::set_deep`).
    pub ten_bit: bool,
    /// The library LUT D-Log M clips get when they're added.
    pub dlog_m_lut: Option<String>,
    /// How tall the timeline's lanes are, as the divider above the
    /// transport row left them (`timeline::Rows`).
    pub timeline_scale: f32,
    /// The view-angles strip shows under the transport row.
    pub view_angles: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            stabilize: Mode::Horizon,
            osv_size: None,
            rolling_shutter: true,
            full_on_pause: true,
            volume: 1.0,
            muted: false,
            resolution: Resolution::P1080,
            aspect: None,
            export_clips: false,
            export_graded: false,
            export_format: Format::Mp4,
            auto_key: false,
            theme: iced::Theme::KanagawaDragon.to_string(),
            ten_bit: false,
            dlog_m_lut: None,
            timeline_scale: 1.0,
            view_angles: true,
        }
    }
}

/// rOsmo360's configuration folder: in `$XDG_CONFIG_HOME` or `~/.config` on
/// Linux, `~/Library/Application Support` on macOS, `%APPDATA%` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("APPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Application Support")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?
    };
    Some(base.join("rosmo360"))
}

fn path() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.json"))
}

impl Settings {
    /// The saved settings, or the defaults if there are none (or they can't
    /// be read: a broken file shouldn't stop the player).
    pub fn load() -> Settings {
        let Some(path) = path() else { return Settings::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("ignoring {}: {e}", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let write = || -> anyhow::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
            crate::replace_file(&path, (serde_json::to_string_pretty(self)? + "\n").as_bytes())?;
            Ok(())
        };
        if let Err(e) = write() {
            eprintln!("couldn't save the settings to {}: {e:#}", path.display());
        }
    }
}
