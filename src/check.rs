//! `rosmo360 check`: what in a project is likely a mistake, found from what
//! the clips already tell us, so an edit needn't be read off a contact
//! sheet for it. One line each: edit time, cut, what and why.

use crate::project::{self, Project};
use crate::{StitchOpts, cut_pipelines, ffmpeg, view};
use anyhow::Result;
use std::path::Path;

/// The pan is measured over this long, seconds, so hand shake and the bob
/// of walking don't count.
const PAN_WINDOW: f64 = 1.0;
const PAN_STEP: f64 = 0.1;
/// Cuts of one clip closer than this, seconds, are likely one scene.
const SAME_SCENE: f64 = 10.0;
/// Shorter cuts barely register.
const SHORT_CUT: f64 = 1.0;
/// The music ending this much before the edit leaves its end unscored.
const MUSIC_SHORT: f64 = 2.0;

struct Finding {
    at: f64,
    cut: Option<usize>,
    what: &'static str,
    why: String,
}

/// Seconds since 1970 of an ISO 8601 UTC time as ffprobe gives it
/// (`2026-07-13T13:30:35.000000Z`).
fn utc_seconds(s: &str) -> Option<f64> {
    let (date, time) = s.trim().trim_end_matches('Z').split_once(['T', ' '])?;
    let mut d = date.split('-').map(|v| v.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let (h, min, sec) = (t.next()?.parse::<f64>().ok()?, t.next()?.parse::<f64>().ok()?, t.next()?.parse::<f64>().ok()?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + h * 3600.0 + min * 60.0 + sec)
}

/// `m:ss.s` of edit time.
pub fn clock(t: f64) -> String {
    // In tenths first, so 59.96 reads 1:00.0, not 0:60.0.
    let tenths = (t.max(0.0) * 10.0).round() as u64;
    format!("{}:{:02}.{}", tenths / 600, tenths / 10 % 60, tenths % 10)
}

/// The world direction a cut's view looks along at clip time `t`, and the
/// direction its keyframes alone look along (in the stabilised view).
fn looking(p: &crate::Pipeline, cuts: &[project::Cut], n: usize, t: f64) -> ([f64; 3], [f64; 3]) {
    let v = project::keyframed(cuts, n, t)
        .and_then(|(k, at)| k.view_at(at, &project::held(&|f, t| p.aim(f, t), at == t, t)))
        .unwrap_or_else(|| view::View::home(view::Projection::Perspective));
    let (lon, lat) = (v.yaw.to_radians(), v.pitch.to_radians());
    let d = [lat.cos() * lon.sin(), -lat.sin(), lat.cos() * lon.cos()];
    if p.flat.is_some() || !p.stab.has_data() {
        return (d, d);
    }
    let m = p.stab.world_from_view(t * 1000.0);
    ([0, 1, 2].map(|i| m[i][0] * d[0] + m[i][1] * d[1] + m[i][2] * d[2]), d)
}

fn angle(a: [f64; 3], b: [f64; 3]) -> f64 {
    let dot: f64 = (0..3).map(|i| a[i] * b[i]).sum();
    dot.clamp(-1.0, 1.0).acos().to_degrees()
}

/// See `Cmd::Check`; pans over `fast` degrees a second count.
pub fn check(path: &Path, opts: &StitchOpts, fast_pan: f64) -> Result<()> {
    let mut project = Project::load(path)?;
    let (pipelines, of_cut) = cut_pipelines(&mut project, opts, true)?;
    let cuts = &project.cuts;
    let offsets = project::offsets(cuts);
    let mut found = Vec::new();
    let mut say = |at: f64, cut: Option<usize>, what: &'static str, why: String| found.push(Finding { at, cut, what, why });

    // When each cut's footage was filmed, where the clip says.
    let mut recorded = std::collections::HashMap::new();
    for c in cuts {
        if !recorded.contains_key(&c.input) {
            let t = ffmpeg::creation_time(&c.input).ok().flatten().and_then(|s| utc_seconds(&s));
            recorded.insert(c.input.clone(), t);
        }
    }
    let filmed = |n: usize| recorded[&cuts[n].input].map(|t| t + cuts[n].start);

    for (n, c) in cuts.iter().enumerate() {
        let at = offsets[n];
        let p = &pipelines[of_cut[n]];
        if c.len() < SHORT_CUT {
            say(at, Some(n), "short", format!("only {:.1} s long", c.len()));
        }
        if n > 0 {
            let before = &cuts[n - 1];
            if let (Some(a), Some(b)) = (filmed(n - 1), filmed(n))
                && b < a
            {
                say(at, Some(n), "order", format!("filmed {} before #{n}", span(a - b), ));
            }
            if before.input == c.input {
                let gap = c.start - before.end;
                if c.start < before.end && c.end > before.start {
                    say(at, Some(n), "repeat", format!("shows {:.1} s of footage #{n} already shows", before.end.min(c.end) - c.start.max(before.start)));
                } else if c.crossfade > 0.0 && c.transition.is_dissolve() && (0.0..SAME_SCENE).contains(&gap) {
                    say(at, Some(n), "crossfade", format!("dissolves from #{n} of the same clip only {gap:.1} s later: one scene, the picture seems to stutter (a plain cut reads as a jump on purpose)"));
                }
            }
        }
        // Pans: how fast the view turns in the world, smoothed.
        let samples: Vec<_> = (0..)
            .map(|k| c.start + k as f64 * PAN_STEP)
            .take_while(|&t| t <= c.end)
            .map(|t| (t, looking(p, cuts, n, t)))
            .collect();
        let per = (PAN_WINDOW / PAN_STEP).round() as usize;
        // (from, to, top speed, top keyframes' share of it)
        let mut fast: Option<(f64, f64, f64, f64)> = None;
        let mode = p.stab.mode();
        let mut flush = |fast: &mut Option<(f64, f64, f64, f64)>| {
            if let Some((from, to, top, keyed)) = fast.take() {
                let why = if keyed >= 0.5 {
                    "the keyframes turn it".to_owned()
                } else if p.flat.is_none() && mode == crate::stab::Mode::Horizon {
                    "the camera turns and horizon follows its heading: level holds it".to_owned()
                } else {
                    "the camera turns".to_owned()
                };
                say(offsets[n] + from - c.start, Some(n), "fast pan", format!("{top:.0}°/s at {from:.1}–{to:.1} s in the clip, {why}"));
            }
        };
        for w in samples.windows(per + 1) {
            let (t0, (a, ka)) = w[0];
            let (t1, (b, kb)) = w[per];
            let speed = angle(a, b) / (t1 - t0);
            let keyed = (angle(ka, kb) / (t1 - t0) / speed.max(1e-9)).min(1.0);
            if speed > fast_pan {
                fast = Some(match fast {
                    Some((from, _, top, k)) if speed <= top => (from, t1, top, k),
                    Some((from, ..)) => (from, t1, speed, keyed),
                    None => (t0, t1, speed, keyed),
                });
            } else if fast.is_some_and(|(_, to, ..)| t1 - to > PAN_WINDOW) {
                // A moment's slowing doesn't split one pan in two.
                flush(&mut fast);
            }
        }
        flush(&mut fast);
    }

    let length = project::edit_length(cuts);
    if let Some(end) = project.music.clips.iter().map(|m| m.ends()).reduce(f64::max) {
        if end < length - MUSIC_SHORT {
            say(end, None, "music", format!("ends at {} but the edit runs to {}: the last {:.0} s have no music", clock(end), clock(length), length - end));
        } else if end > length + project::END_FADE + MUSIC_SHORT {
            say(length, None, "music", format!("runs to {} past the edit's end at {}: export fades it out over the last {:.0} s", clock(end), clock(length), project::END_FADE));
        }
    }

    found.sort_by(|a, b| a.at.total_cmp(&b.at));
    for f in &found {
        let cut = f.cut.map_or_else(|| "   ".to_owned(), |n| format!("#{}", n + 1));
        println!("{} {cut:>4} {}: {}", clock(f.at), f.what, f.why);
    }
    eprintln!("{}: {} cuts, {} long, {} finding{}", path.display(), cuts.len(), clock(length), found.len(), if found.len() == 1 { "" } else { "s" });
    Ok(())
}

fn span(s: f64) -> String {
    if s >= 120.0 { format!("{:.0} min", s / 60.0) } else { format!("{s:.0} s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_and_print() {
        // 2026-07-13 13:30:35 UTC, by `date -u -d ... +%s`.
        assert_eq!(utc_seconds("2026-07-13T13:30:35.000000Z"), Some(1_783_949_435.0));
        assert_eq!(utc_seconds("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(clock(59.96), "1:00.0");
        assert_eq!(clock(136.4), "2:16.4");
    }
}
