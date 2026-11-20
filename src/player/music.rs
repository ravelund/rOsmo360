//! The music track in the player: each song's waveform for the timeline,
//! and its sound kept in step with the edit while it plays.
//!
//! Music has an audio stream of its own, apart from the clips' sound, so it
//! plays on through cut changes untouched. It follows edit time: started at
//! the song's place when playback reaches a music clip, restarted only after
//! a jump (or if it drifts), and set to its level every frame, which carries
//! the clip's fades and the fade-out where the video ends. The next clip is
//! opened `PRIME` seconds ahead and let through one device latency early, so
//! one song hands over to the next without the gap of starting ffmpeg and the
//! device; a clip that carries on in the same song just plays on.

use super::audio::Audio;
use crate::project::{self, MusicClip};
use anyhow::{Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Seconds per waveform peak.
pub const WAVE_STEP: f64 = 0.05;
/// Restart the music when it is this far from where the edit is.
const DRIFT: f64 = 0.15;
/// Open the next clip's sound this long before it starts.
const PRIME: f64 = 1.0;

/// A song's waveform: the loudest sample in each `WAVE_STEP`, 0..1, and the
/// song's length.
#[derive(Debug)]
pub struct Waveform {
    pub peaks: Vec<f32>,
    pub duration: f64,
}

impl Waveform {
    /// From the clip cache, else decoded from `path` to mono at 8 kHz, which
    /// is plenty for a picture of its loudness: a 6-minute song takes about
    /// a second.
    pub fn read(path: &Path) -> Result<Waveform> {
        if let Some((peaks, duration)) = crate::cache::waveform(path, WAVE_STEP) {
            return Ok(Waveform { peaks, duration });
        }
        const RATE: usize = 8000;
        let mut child = crate::ffmpeg::tool("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-ac", "1", "-ar", &RATE.to_string(), "-f", "f32le", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running ffmpeg")?;
        let mut bytes = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut bytes)?;
        child.wait()?;
        let per = (WAVE_STEP * RATE as f64) as usize;
        let samples: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs()).collect();
        if samples.is_empty() {
            anyhow::bail!("{} has no sound", path.display());
        }
        let peaks: Vec<f32> = samples.chunks(per).map(|c| c.iter().fold(0.0f32, |m, &v| m.max(v)).min(1.0)).collect();
        let duration = samples.len() as f64 / RATE as f64;
        crate::cache::store_waveform(path, WAVE_STEP, &peaks, duration);
        Ok(Waveform { peaks, duration })
    }
}

/// A mix level in dB as the speakers' gain factor.
pub fn gain(db: f64) -> f32 {
    project::gain(db) as f32
}

/// How loud music clip `clip` is at edit time `t` from its own fades, its
/// `crossfades` with the clips before and after it (equal power, so the
/// loudness holds through the middle), and the fade-out before `video_end`
/// when it runs on past it, 0..1.
pub fn envelope(clip: &MusicClip, (cross_in, cross_out): (f64, f64), t: f64, video_end: f64) -> f64 {
    let into = t - clip.at;
    let left = clip.ends() - t;
    let mut g: f64 = 1.0;
    if clip.fade_in > 0.0 {
        g = g.min(into / clip.fade_in);
    }
    if clip.fade_out > 0.0 {
        g = g.min(left / clip.fade_out);
    }
    if clip.ends() > video_end {
        g = g.min((video_end - t) / project::END_FADE);
    }
    let g = g.clamp(0.0, 1.0);
    let quarter_sine = |x: f64| (x.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2).sin();
    let mut x = 1.0;
    if cross_in > 0.0 {
        x *= quarter_sine(into / cross_in);
    }
    if cross_out > 0.0 {
        x *= quarter_sine(left / cross_out);
    }
    g * x
}

/// A clip's sound, open: its song, where the clip sits on the edit, from
/// where in the song the stream started (so it's at `from + elapsed` once
/// let through), and whether it has been.
struct Open {
    input: PathBuf,
    at: f64,
    from: f64,
    audio: Audio,
    started: bool,
}

impl Open {
    fn is(&self, clip: &MusicClip) -> bool {
        self.input == clip.input && self.at == clip.at && self.from == clip.start
    }

    fn play(&mut self) {
        self.audio.play();
        self.started = true;
    }
}

/// The music's open streams: the clips playing now (two through a
/// crossfade), and the next one's, ready.
#[derive(Default)]
pub struct Player {
    open: Vec<Open>,
    /// When to try the sound device again after it failed: not at every
    /// frame, as it was.
    retry: Option<std::time::Instant>,
}

impl Player {
    /// Keeps the music in step with edit time `t` while the edit plays (stops
    /// it otherwise): the clips under `t`, each at the track's level there
    /// (with its volume points) times the clip's envelope.
    pub fn follow(&mut self, track: &project::Music, t: f64, video_end: f64, playing: bool) {
        self.follow_with(track, t, video_end, playing, &|_, _| 0.0);
    }

    /// As `follow`, each clip `i` also `points(i, t)` dB at edit time `t`:
    /// the main track's sound running on under the upper track, as music
    /// clips, with its cuts' volume points.
    pub fn follow_with(&mut self, track: &project::Music, t: f64, video_end: f64, playing: bool, points: &dyn Fn(usize, f64) -> f64) {
        if !playing {
            self.open.clear();
            return;
        }
        let level = |i: usize, t: f64| gain(track.level_at(t) + points(i, t)) * envelope(&track.clips[i], track.crossfades(i), t, video_end) as f32;
        let mut old = std::mem::take(&mut self.open);
        let now: Vec<usize> = (0..track.clips.len()).filter(|&i| t >= track.clips[i].at && t < track.clips[i].ends() && t < video_end).collect();
        for &i in &now {
            let clip = &track.clips[i];
            let want = clip.start + (t - clip.at);
            // Playing and in step (a split song's second half plays on in its
            // first half's stream), else made ready for it, else started.
            let in_step = old.iter().position(|o| o.started && o.input == clip.input && (o.from + o.audio.elapsed() - want).abs() < DRIFT);
            let ready = || old.iter().position(|o| !o.started && o.is(clip));
            let mut open = match in_step.or_else(ready) {
                Some(k) => old.swap_remove(k),
                None if self.retry.is_some_and(|at| std::time::Instant::now() < at) => continue,
                None => match Audio::prepare(&clip.input, want, 0.0) {
                    Ok(audio) => Open { input: clip.input.clone(), at: clip.at, from: want, audio, started: false },
                    Err(e) => {
                        eprintln!("no music: {e:#}");
                        self.retry = Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                        continue;
                    }
                },
            };
            if !open.started {
                open.play();
            }
            open.audio.set_volume(level(i, t));
            self.open.push(open);
        }
        // The clips starting within `PRIME`, opened ahead and let through a
        // device latency early so they're heard on time; not one the song
        // playing runs straight on into.
        for (i, next) in track.clips.iter().enumerate() {
            if next.at <= t || next.at >= t + PRIME || next.at >= video_end {
                continue;
            }
            let carries_on = now.iter().any(|&j| {
                let c = &track.clips[j];
                c.input == next.input && (c.ends() - next.at).abs() < 1e-3 && (c.end - next.start).abs() < 1e-3
            });
            if carries_on {
                continue;
            }
            let mut open = match old.iter().position(|o| o.is(next)) {
                Some(k) => old.swap_remove(k),
                None => match Audio::prepare(&next.input, next.start, level(i, next.at)) {
                    Ok(audio) => Open { input: next.input.clone(), at: next.at, from: next.start, audio, started: false },
                    Err(e) => {
                        eprintln!("no music: {e:#}");
                        continue;
                    }
                },
            };
            if !open.started && t >= next.at - open.audio.latency().min(0.25) {
                open.audio.set_volume(level(i, next.at));
                open.play();
            }
            self.open.push(open);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fades_in_out_and_before_the_video_ends() {
        let clip = MusicClip { input: "song.mp3".into(), at: 10.0, start: 0.0, end: 100.0, fade_in: 2.0, fade_out: 0.0 };
        let none = (0.0, 0.0);
        assert_eq!(envelope(&clip, none, 11.0, 1000.0), 0.5);
        assert_eq!(envelope(&clip, none, 50.0, 1000.0), 1.0);
        // The video ends at 60 s, so it fades over the last 3 s before it.
        assert_eq!(envelope(&clip, none, 58.5, 60.0), 0.5);
        assert_eq!(envelope(&clip, none, 56.0, 60.0), 1.0);
        let short = MusicClip { fade_out: 4.0, end: 20.0, ..clip.clone() };
        assert_eq!(envelope(&short, none, 28.0, 1000.0), 0.5);
        // Halfway through a 4 s crossfade into the next clip: equal power,
        // so the two together are as loud as one.
        let out = envelope(&clip, (0.0, 4.0), 108.0, 1000.0);
        assert!((out * out - 0.5).abs() < 1e-9, "{out}");
    }
}
