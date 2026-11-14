//! What loading a clip reads from it, kept so the next load doesn't: the
//! calibration and attitude (telemetry-parser walks the whole `.OSV` for
//! them: 0047, 7.4 min, took 53 s from a network share and 5.5 s from
//! memory), the timeline thumbnails and songs' waveforms for the music
//! lane (a second's decode each).
//!
//! Entries are named by a fingerprint of the clip's contents, not its path,
//! so moved or renamed footage still finds its entry: the size and a hash of
//! the first and last megabyte (the MP4 header and sample index, which any
//! re-encode or trim rewrites) and of four blocks between. That reads ~2 MB
//! where hashing the whole file would read the gigabytes the cache avoids.
//! Each entry holds a format number (`FORMAT`) and is rebuilt when it differs,
//! so updates keep the cache unless a release changes what goes in it (an
//! update used to re-read every clip). Entries are written
//! to a temporary file and renamed into place, so runs side by side never
//! read half an entry; a hit refreshes the file's time, which
//! `rosmo360 cache clear --older-than` goes by.

use crate::imu;
use anyhow::{Context, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use telemetry_parser::rig::CameraRig;

/// Bump this when a release changes what an entry holds or how it is made: its
/// layout, a telemetry-parser update that reads the rig or attitude differently,
/// thumbnails worth redoing. Entries of another format are rebuilt; otherwise
/// they outlive updates, so a new version doesn't re-read every clip.
///
/// 2 (0.7.0): telemetry entries hold the colour mode (D-Log M).
const FORMAT: u32 = 2;
const MAGIC: &[u8; 8] = b"rOsmo360";
/// What entries are named, after the fingerprint.
const KINDS: [&str; 3] = ["telemetry", "thumbs", "wave"];

static OFF: AtomicBool = AtomicBool::new(false);

/// `--no-cache`: read clips afresh and write nothing.
pub fn disable() {
    OFF.store(true, Ordering::Relaxed);
}

/// The cache folder: `$ROSMO360_CACHE`, else the one `cache move` chose,
/// else the system's (`default_dir`).
pub fn dir() -> Option<PathBuf> {
    std::env::var_os(ENV).filter(|v| !v.is_empty()).map(PathBuf::from).or_else(chosen).or_else(default_dir)
}

/// Names a cache folder for one run or a script, over `cache move`'s.
const ENV: &str = "ROSMO360_CACHE";

/// The folder `cache move` chose, kept in a file of its own beside the
/// settings, so a player saving its settings can't undo it.
fn chosen() -> Option<PathBuf> {
    let path = fs::read_to_string(choice_file()?).ok()?;
    Some(PathBuf::from(path.trim_end_matches(['\n', '\r']))).filter(|p| !p.as_os_str().is_empty())
}

fn choice_file() -> Option<PathBuf> {
    Some(crate::player::config_dir()?.join("cache-dir"))
}

/// `$XDG_CACHE_HOME` or `~/.cache` on Linux, `~/Library/Caches` on macOS,
/// `%LOCALAPPDATA%` on Windows.
fn default_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Caches")
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache")))?
    };
    Some(base.join("rosmo360"))
}

/// What the stitcher and stabiliser need from a clip's telemetry.
pub struct Telemetry {
    pub rig: Option<CameraRig>,
    /// Fused attitude, (ms, [w, x, y, z]), as `imu::Imu::quats`.
    pub quats: Vec<(f64, [f64; 4])>,
    /// The colour profile it was filmed in.
    pub color_mode: crate::dji::ColorMode,
}

/// `input`'s telemetry, from the cache when it holds it, else parsed (telling
/// `progress` how far that has got, 0..1) and cached.
pub fn telemetry(input: &Path, progress: &dyn Fn(f64)) -> Result<Telemetry> {
    let entry = entry(input, "telemetry");
    if let Some(t) = entry.as_deref().and_then(|e| hit(e, read_telemetry)) {
        return Ok(t);
    }
    let parsed = crate::load_input(input, progress)?;
    let t = Telemetry { rig: telemetry_parser::util::camera_rig(&parsed), quats: imu::read(&parsed).quats, color_mode: crate::dji::color_mode(input)? };
    if let Some(e) = entry {
        store(&e, |w| write_telemetry(w, &t));
    }
    Ok(t)
}

/// A thumbnail: clip time, and RGBA pixels of `width` × `height`.
pub type Thumb = (f64, Vec<u8>);

/// `input`'s cached thumbnails of `width` × `height`, if any.
pub fn thumbs(input: &Path, (width, height): (usize, usize)) -> Option<Vec<Thumb>> {
    hit(&entry(input, "thumbs")?, |r| read_thumbs(r, width, height))
}

/// Caches `input`'s thumbnails, each `width` × `height` RGBA.
pub fn store_thumbs(input: &Path, size: (usize, usize), thumbs: &[Thumb]) {
    if let Some(e) = entry(input, "thumbs") {
        store(&e, |w| write_thumbs(w, size, thumbs));
    }
}

/// `song`'s cached waveform, if it was made with peaks `step` seconds apart:
/// the peaks and the song's length.
pub fn waveform(song: &Path, step: f64) -> Option<(Vec<f32>, f64)> {
    hit(&entry(song, "wave")?, |r| {
        if f64::from_le_bytes(take(r)?) != step {
            return None;
        }
        let duration = f64::from_le_bytes(take(r)?);
        let n = u64_from(r)? as usize;
        let peaks = (0..n).map(|_| take(r).map(f32::from_le_bytes)).collect::<Option<Vec<_>>>()?;
        Some((peaks, duration))
    })
}

pub fn store_waveform(song: &Path, step: f64, peaks: &[f32], duration: f64) {
    if let Some(e) = entry(song, "wave") {
        store(&e, |w| {
            w.extend(step.to_le_bytes());
            w.extend(duration.to_le_bytes());
            w.extend((peaks.len() as u64).to_le_bytes());
            for p in peaks {
                w.extend(p.to_le_bytes());
            }
            Ok(())
        });
    }
}

/// The entry of kind `kind` for `input`, unless the cache is off or there is
/// nowhere to keep it.
fn entry(input: &Path, kind: &str) -> Option<PathBuf> {
    if OFF.load(Ordering::Relaxed) {
        return None;
    }
    let key = fingerprint(input).inspect_err(|e| eprintln!("not caching {}: {e:#}", input.display())).ok()?;
    Some(dir()?.join(format!("{key:032x}.{kind}")))
}

/// Size and a hash of the start, the end and four blocks between: see the
/// module's comment. FNV-1a, 128 bits: there is no adversary to resist,
/// only chance to beat.
fn fingerprint(input: &Path) -> Result<u128> {
    const ENDS: u64 = 1 << 20;
    const BLOCK: u64 = 64 << 10;
    let mut f = File::open(input)?;
    let size = f.metadata()?.len();
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            hash = (hash ^ b as u128).wrapping_mul(0x0000000001000000000000000000013b);
        }
    };
    eat(&size.to_le_bytes());
    let mut spans = vec![(0, ENDS.min(size))];
    spans.extend((1..=4).map(|i| (size * i / 5, BLOCK)));
    spans.push((size.saturating_sub(ENDS), ENDS));
    let mut buf = Vec::new();
    for (at, len) in spans {
        buf.clear();
        f.seek(SeekFrom::Start(at))?;
        (&mut f).take(len).read_to_end(&mut buf)?;
        eat(&buf);
    }
    Ok(hash)
}

/// Reads entry `path` with `read`, refreshing its time; `None` if it is
/// missing, of another format or unreadable (it is then rebuilt).
fn hit<T>(path: &Path, read: impl FnOnce(&mut &[u8]) -> Option<T>) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    let mut r = bytes.as_slice();
    let mut magic = [0; 8];
    r.read_exact(&mut magic).ok()?;
    if &magic != MAGIC || u32_from(&mut r)? != FORMAT {
        return None;
    }
    // Which version wrote it: kept for whoever looks, not checked.
    string_from(&mut r)?;
    let value = read(&mut r)?;
    if let Ok(f) = File::options().write(true).open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Some(value)
}

/// Writes entry `path` through a temporary file; a failure only costs the
/// next load its speed, so it is reported and otherwise ignored.
fn store(path: &Path, write: impl FnOnce(&mut Vec<u8>) -> Result<()>) {
    let result = (|| -> Result<()> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(FORMAT.to_le_bytes());
        put_string(&mut bytes, env!("CARGO_PKG_VERSION"));
        write(&mut bytes)?;
        fs::create_dir_all(path.parent().context("no cache folder")?)?;
        crate::replace_file(path, &bytes)
    })();
    if let Err(e) = result {
        eprintln!("could not cache {}: {e:#}", path.display());
    }
}

fn write_telemetry(w: &mut Vec<u8>, t: &Telemetry) -> Result<()> {
    put_string(w, &t.rig.as_ref().map(serde_json::to_string).transpose()?.unwrap_or_default());
    w.extend((t.quats.len() as u64).to_le_bytes());
    // f32 components: rounding is ~1e-7, far below a pixel; half the size.
    for (ms, q) in &t.quats {
        w.extend(ms.to_le_bytes());
        for c in q {
            w.extend((*c as f32).to_le_bytes());
        }
    }
    w.extend(t.color_mode.number().to_le_bytes());
    Ok(())
}

fn read_telemetry(r: &mut &[u8]) -> Option<Telemetry> {
    let rig = string_from(r)?;
    let rig = if rig.is_empty() { None } else { Some(serde_json::from_str(&rig).ok()?) };
    let n = u64_from(r)? as usize;
    let mut quats = Vec::with_capacity(n.min(r.len() / 24));
    for _ in 0..n {
        let ms = f64::from_le_bytes(take(r)?);
        let mut q = [0.0; 4];
        for c in &mut q {
            *c = f32::from_le_bytes(take(r)?) as f64;
        }
        quats.push((ms, q));
    }
    let color_mode = crate::dji::ColorMode::from_number(u32_from(r)?);
    Some(Telemetry { rig, quats, color_mode })
}

/// PNG each: 128 × 72 RGBA raw would be 37 kB a thumbnail.
fn write_thumbs(w: &mut Vec<u8>, (width, height): (usize, usize), thumbs: &[Thumb]) -> Result<()> {
    use image::ImageEncoder;
    w.extend((width as u32).to_le_bytes());
    w.extend((height as u32).to_le_bytes());
    w.extend((thumbs.len() as u32).to_le_bytes());
    for (t, rgba) in thumbs {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(rgba, width as u32, height as u32, image::ExtendedColorType::Rgba8)?;
        w.extend(t.to_le_bytes());
        w.extend((png.len() as u32).to_le_bytes());
        w.extend(png);
    }
    Ok(())
}

fn read_thumbs(r: &mut &[u8], width: usize, height: usize) -> Option<Vec<Thumb>> {
    if u32_from(r)? as usize != width || u32_from(r)? as usize != height {
        return None;
    }
    let n = u32_from(r)?;
    // An empty set was stored when ffmpeg failed; make it afresh.
    if n == 0 {
        return None;
    }
    (0..n)
        .map(|_| {
            let t = f64::from_le_bytes(take(r)?);
            let len = u32_from(r)? as usize;
            let png = r.get(..len)?;
            *r = &r[len..];
            let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8().into_raw();
            (rgba.len() == width * height * 4).then_some((t, rgba))
        })
        .collect()
}

fn take<const N: usize>(r: &mut &[u8]) -> Option<[u8; N]> {
    let (head, rest) = r.split_first_chunk::<N>()?;
    *r = rest;
    Some(*head)
}

fn u32_from(r: &mut &[u8]) -> Option<u32> {
    take(r).map(u32::from_le_bytes)
}

fn u64_from(r: &mut &[u8]) -> Option<u64> {
    take(r).map(u64::from_le_bytes)
}

fn put_string(w: &mut Vec<u8>, s: &str) {
    w.extend((s.len() as u64).to_le_bytes());
    w.extend(s.as_bytes());
}

fn string_from(r: &mut &[u8]) -> Option<String> {
    let len = u64_from(r)? as usize;
    let s = r.get(..len)?;
    *r = &r[len..];
    String::from_utf8(s.to_vec()).ok()
}

/// `rosmo360 cache`: where the cache is and what it holds.
pub fn report() -> Result<()> {
    let dir = dir().context("no cache folder (HOME is not set)")?;
    let (clips, bytes) = entries(&dir).fold((0, 0), |(n, b), (p, m)| (n + usize::from(p.extension().is_some_and(|e| e == "telemetry")), b + m.len()));
    let why = if std::env::var_os(ENV).is_some_and(|v| !v.is_empty()) {
        format!(" (from ${ENV})")
    } else if chosen().is_some() {
        " (chosen with cache move)".to_owned()
    } else {
        String::new()
    };
    println!("{}{why}: {clips} clips, {:.1} MB", dir.display(), bytes as f64 / 1e6);
    Ok(())
}

/// Whether `input`'s telemetry is cached in this format.
pub fn has(input: &Path) -> bool {
    entry(input, "telemetry").is_some_and(|e| hit(&e, |_| Some(())).is_some())
}

/// Drops `input`'s entries, so it is read afresh (`cache add --force`).
pub fn forget(input: &Path) -> Result<()> {
    for kind in KINDS {
        if let Some(e) = entry(input, kind) {
            match fs::remove_file(&e) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err).with_context(|| format!("removing {}", e.display())),
                _ => {}
            }
        }
    }
    Ok(())
}

/// `rosmo360 cache move`: moves the entries to `to` (the system's folder if
/// `None`) and keeps the cache there from now on.
pub fn move_to(to: Option<PathBuf>) -> Result<()> {
    let from = dir().context("no cache folder (HOME is not set)")?;
    let to = match to {
        Some(to) => std::path::absolute(to)?,
        None => default_dir().context("no default cache folder (HOME is not set)")?,
    };
    fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
    let mut n = 0;
    if fs::canonicalize(&from).ok() != fs::canonicalize(&to).ok() {
        for (path, _) in entries(&from) {
            let dest = to.join(path.file_name().unwrap());
            // Another filesystem (a media disk) can't take a rename: copy.
            if fs::rename(&path, &dest).is_err() {
                fs::copy(&path, &dest).with_context(|| format!("copying {} to {}", path.display(), to.display()))?;
                fs::remove_file(&path)?;
            }
            n += 1;
        }
    }
    let choice = choice_file().context("no configuration folder (HOME is not set)")?;
    if Some(&to) == default_dir().as_ref() {
        let _ = fs::remove_file(&choice);
    } else {
        fs::create_dir_all(choice.parent().unwrap())?;
        crate::replace_file(&choice, format!("{}\n", to.display()).as_bytes())?;
    }
    println!("moved {n} files from {} to {}; the cache is kept there from now on", from.display(), to.display());
    if std::env::var_os(ENV).is_some_and(|v| !v.is_empty()) {
        println!("note: ${ENV} is set, and wins over this while it is");
    }
    Ok(())
}

/// `rosmo360 cache clear`: every entry, or those unused for `older_than`
/// days.
pub fn clear(older_than: Option<f64>) -> Result<()> {
    let dir = dir().context("no cache folder (HOME is not set)")?;
    let now = std::time::SystemTime::now();
    let (mut n, mut bytes) = (0, 0);
    for (path, meta) in entries(&dir) {
        let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).map_or(f64::INFINITY, |d| d.as_secs_f64() / 86400.0);
        if older_than.is_none_or(|days| age > days) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            n += 1;
            bytes += meta.len();
        }
    }
    println!("removed {n} files, {:.1} MB, from {}", bytes as f64 / 1e6, dir.display());
    Ok(())
}

/// The cache's entry files (not others someone put there).
fn entries(dir: &Path) -> impl Iterator<Item = (PathBuf, fs::Metadata)> {
    fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| {
        let path = e.path();
        let ours = path.extension().is_some_and(|x| KINDS.iter().any(|k| x == *k));
        (ours && e.file_type().ok()?.is_file()).then(|| Some((path, e.metadata().ok()?))).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_round_trips() {
        let color_mode = crate::dji::ColorMode::DLogM;
        let t = Telemetry { rig: None, quats: vec![(0.0, [1.0, 0.0, 0.0, 0.0]), (1.25, [0.5, -0.5, 0.5, -0.5])], color_mode };
        let mut w = Vec::new();
        write_telemetry(&mut w, &t).unwrap();
        let back = read_telemetry(&mut w.as_slice()).unwrap();
        assert_eq!((back.rig, back.quats, back.color_mode), (None, t.quats, color_mode));
        // Cut short, it is a miss rather than a wrong answer.
        assert!(read_telemetry(&mut &w[..w.len() - 1]).is_none());
    }

    #[test]
    fn thumbs_round_trip() {
        let thumbs = vec![(1.5, (0..4 * 6).map(|i| i as u8 * 10).collect::<Vec<u8>>())];
        let mut w = Vec::new();
        write_thumbs(&mut w, (3, 2), &thumbs).unwrap();
        assert_eq!(read_thumbs(&mut w.as_slice(), 3, 2), Some(thumbs));
        assert_eq!(read_thumbs(&mut w.as_slice(), 2, 3), None);
    }
}
