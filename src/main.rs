mod align;
mod cache;
mod check;
mod dissolve;
mod dji;
mod exposure;
mod grade;
mod ffmpeg;
mod hdr;
mod imu;
mod lut;
mod overlay;
mod player;
mod project;
mod ring;
mod sheet;
mod sky;
mod stab;
mod stitch;
mod text;
mod track;
mod view;

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use clap::{Args, Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
use view::{Projection, Reframe, View};
use telemetry_parser::rig::{RigPicture, geometry::ProjectionKind};

#[derive(Parser)]
#[command(name = player::NAME, version, about = "rOsmo360: play and stitch DJI Osmo 360 .OSV files")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Read clips afresh instead of from the cache of their telemetry and
    /// thumbnails, and cache nothing.
    #[arg(long, global = true)]
    no_cache: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Dump the camera rig (DJI Studio's default stitch geometry) as JSON.
    Rig { input: PathBuf },
    /// Where the cache of clips' telemetry and thumbnails is and what it
    /// holds (it makes clips open in a fraction of a second after the first
    /// time); `cache clear` empties it.
    Cache {
        #[command(subcommand)]
        action: Option<CacheAction>,
    },
    /// Dump orientation (q), gyro (g) and accelerometer (a) samples as CSV.
    Imu { input: PathBuf },
    /// The clip's length, frame rate, recording time and what it holds, as JSON.
    Info { input: PathBuf },
    /// List what in a project is likely a mistake, one line each (edit
    /// time, cut, what): cuts out of the order they were filmed in, a cut
    /// repeating footage the cut before shows, crossfades between cuts of one
    /// clip seconds apart (one scene: it stutters), pans faster than 30°/s
    /// (and whether keyframes or the camera turn the view),
    /// cuts under a second, and music ending before the edit or running long
    /// past it.
    Check {
        project: PathBuf,
        /// Pans faster than this, degrees a second over a second, are
        /// listed: they smear and are hard to follow.
        #[arg(long, default_value_t = 30.0)]
        fast_pan: f64,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Stitch one frame to an equirectangular PNG.
    Stitch {
        input: PathBuf,
        /// Time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        time: f64,
        #[arg(short, long, default_value = "equirect.png")]
        output: PathBuf,
        /// Output width; height is width / 2.
        #[arg(short, long, default_value_t = 4096)]
        width: usize,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Play clips in a window with free look, stabilisation and rolling-shutter correction.
    ///
    /// Drag: look around · right-drag or Q/E: roll · arrows: pitch, seek (shift: yaw) ·
    /// scroll: zoom · space: play/pause · , .: frame step · S: stabilisation ·
    /// R: rolling shutter · P: projection · M: mute · 0: reset view · F: fullscreen · Ctrl+O: open ·
    /// K: keyframe the view here · [ ]: previous/next keyframe · Del: delete keyframe · J: its ease ·
    /// C: split the cut · Shift+Del: delete the selected cut · Ctrl+Z / Ctrl+Shift+Z: undo / redo ·
    /// Ctrl+= / Ctrl+− / Ctrl+0: zoom the timeline in / out / to the whole edit (or scroll on it)
    Play {
        /// Clips to open at start, one cut each in this order, or a saved
        /// project (.json); otherwise use the Open buttons.
        input: Vec<PathBuf>,
        /// Stream the OSV itself at this size (pixels square) instead of the
        /// LRF proxy [default: the last source used].
        #[arg(long)]
        osv_size: Option<u32>,
        /// Video decoder; the first that works, in this order, if omitted.
        #[arg(long, value_enum)]
        hwaccel: Option<ffmpeg::Hwaccel>,
        /// Stabilisation mode [default: the last one used, at first horizon].
        #[arg(long, value_enum)]
        stabilize: Option<stab::Mode>,
        #[arg(long, value_enum, default_value_t = stab::ImuAxes::X180)]
        imu_axes: stab::ImuAxes,
        #[arg(long)]
        world_z_down: bool,
    },
    /// Report how well the seam alignment works over a span: what ghosting
    /// is left (lens disagreement across the blend band) and how much the
    /// shift wobbles from frame to frame.
    Seam {
        input: PathBuf,
        /// Start time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        start: f64,
        /// Duration in seconds.
        #[arg(short, long, default_value_t = 5.0)]
        duration: f64,
        /// Write the first frame's seam strips, differences and shift field
        /// as images into this directory.
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// Stitch a span of the clip to an equirectangular MP4.
    Render {
        input: PathBuf,
        /// Start time in seconds.
        #[arg(short, long, default_value_t = 0.0)]
        start: f64,
        /// Duration in seconds; the rest of the clip if omitted.
        #[arg(short, long)]
        duration: Option<f64>,
        #[arg(short, long, default_value = "equirect.mp4")]
        output: PathBuf,
        /// Output width; height is width / 2.
        #[arg(short, long, default_value_t = 4096)]
        width: usize,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Render a framed (reframed) video with sound: a clip with a fixed view
    /// or keyframed views from a JSON file, or a project (.json) of cuts from
    /// several clips, each with its own keyframes.
    ///
    /// For a clip without -s/-d, the keyframes' span is rendered.
    /// Stabilisation defaults to horizon.
    Export {
        /// A clip (.OSV), or a project (.json): {"aspect": "9:16", "cuts":
        /// [{"input": "a.OSV", "in": 28, "out": 37, "keyframes": [...]}, ...]}.
        input: PathBuf,
        #[command(flatten)]
        framing: Framing,
        /// Start time in seconds.
        #[arg(short, long)]
        start: Option<f64>,
        /// Duration in seconds.
        #[arg(short, long)]
        duration: Option<f64>,
        /// Picture height for landscape, width for portrait: 1080p is
        /// 1920×1080 at 16:9 and 1080×1920 at 9:16.
        #[arg(short, long, value_enum, default_value_t = Resolution::P1080)]
        resolution: Resolution,
        /// Shape, W:H (16:9, 9:16, 1:1, 4:5, ...); else the reframe's, else 16:9.
        #[arg(short, long)]
        aspect: Option<view::Aspect>,
        /// Exact size, WIDTHxHEIGHT, instead of a resolution and aspect.
        #[arg(long, value_parser = parse_size, conflicts_with_all = ["resolution", "aspect"])]
        size: Option<[u32; 2]>,
        /// The file to write; with --clips, the folder for the clips.
        /// [default: clip.mp4, clip.mov for --format resolve, or the
        /// project's or clip's name for --clips]
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// mp4: H.264 and AAC, plays anywhere. resolve: 10-bit DNxHR HQX and
        /// PCM in MOV, which DaVinci Resolve's free Linux version can read
        /// (about 28 MB a second at 1080p).
        #[arg(long, value_enum, default_value_t = ffmpeg::Format::Mp4)]
        format: ffmpeg::Format,
        /// Write each cut to its own file for another editor, named after
        /// the project: "<name> - 001.mp4" and on, in the folder -o; and each
        /// music clip as "<name> - music 001.wav" (48 kHz 24-bit, unfaded,
        /// at the song's own level).
        #[arg(long)]
        clips: bool,
        /// With --clips, seconds of extra footage either side of each cut
        /// (and song either side of each music clip), where there is some,
        /// for the other editor's transitions.
        #[arg(long, default_value_t = 0.0, requires = "clips")]
        handles: f64,
        /// With --clips, apply the cuts' colour effects too (the whole edit
        /// always has them; left off, the clips are as shot, to grade in
        /// the other editor).
        #[arg(long, requires = "clips")]
        graded: bool,
        /// Stitch from the low-resolution proxy (LRF): a quick, soft draft.
        #[arg(long)]
        draft: bool,
        /// Stitch on the CPU, the reference the GPU shader must match.
        /// Without a usable GPU this happens anyway.
        #[arg(long)]
        cpu: bool,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Stitch evenly spaced moments onto one contact-sheet PNG, each tile
    /// labelled with its time, to skim a clip. Tiles show the whole sphere,
    /// marked at yaw −90/0/90° and the horizon, unless a framing is given.
    ///
    /// Given a project (.json), one row per cut (more with --every), each
    /// tile framed as the export frames it and labelled with the cut's
    /// number and the clip time.
    ///
    /// Uses the proxy (LRF) unless --full. Stabilisation defaults to horizon.
    Preview {
        input: PathBuf,
        #[command(flatten)]
        framing: Framing,
        /// Start time in seconds.
        #[arg(short, long)]
        start: Option<f64>,
        /// Duration in seconds.
        #[arg(short, long)]
        duration: Option<f64>,
        /// Seconds between tiles; else 24 tiles, or the keyframes' times.
        #[arg(long)]
        every: Option<f64>,
        /// Tile width in pixels.
        #[arg(long, default_value_t = 640)]
        tile: usize,
        #[arg(long, default_value_t = 4)]
        columns: usize,
        #[arg(short, long, default_value = "preview.png")]
        output: PathBuf,
        /// Stitch from the full-resolution lens tracks instead of the proxy.
        #[arg(long)]
        full: bool,
        #[command(flatten)]
        opts: StitchOpts,
    },
    /// Summarise the clip over time as CSV, to find moments worth a look:
    /// the camera's heading and tilt, how fast it turns or shakes, and how
    /// loud it is.
    ///
    /// Given several clips or folders (every .OSV in them and the folders
    /// inside), writes a CSV per clip into the folder --output, named after
    /// the clip, a few clips at a time.
    Scan {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// The folder for a CSV per clip; one clip without it goes to the
        /// standard output.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Clips scanned at once into --output.
        #[arg(long, default_value_t = 4)]
        jobs: usize,
        /// Seconds per row.
        #[arg(long, default_value_t = 1.0)]
        every: f64,
        /// Add columns from the picture: sky (% of the upper half that is
        /// sky), skyline (its median elevation, degrees) and open (% of
        /// directions with a view out to the horizon). Stitches the proxy's
        /// keyframes small, which takes seconds to a minute where plain scan
        /// reads only telemetry.
        #[arg(long)]
        image: bool,
    },
    /// Track a coloured subject (a jacket, a tent) through a clip, as CSV for
    /// keyframes to follow ("follow": "track.csv"): per sample its direction
    /// in the camera (x, y, z), its yaw and pitch as a level view would look
    /// at it (directions in the world, like scan's heading), and its size in
    /// % of the sphere. Empty where it was lost.
    Track {
        input: PathBuf,
        /// The subject's colour, #rrggbb (from a preview or a photo of it).
        #[arg(long)]
        colour: String,
        /// How far the hue may differ, degrees either side.
        #[arg(long, default_value_t = 18.0)]
        tolerance: f64,
        /// Where the subject is at the start, as level yaw,pitch (it may not
        /// be the biggest patch of its colour); else it starts from that.
        #[arg(long, value_parser = parse_pair, allow_hyphen_values = true)]
        near: Option<(f64, f64)>,
        /// Start time in seconds.
        #[arg(short, long)]
        start: Option<f64>,
        /// Seconds to track; to the end if omitted.
        #[arg(short, long)]
        duration: Option<f64>,
        /// Seconds between samples.
        #[arg(long, default_value_t = 0.1)]
        every: f64,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Cache clips ahead of time, so they open at once later: each .OSV
    /// given, and every .OSV in the folders given (and folders inside).
    Add {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Read every clip afresh, even those already cached.
        #[arg(long)]
        force: bool,
    },
    /// Move the cache to another folder (a disk with room, say), and keep it
    /// there from now on; ROSMO360_CACHE, if set, still wins for a run.
    Move {
        #[arg(required_unless_present = "default")]
        folder: Option<PathBuf>,
        /// Back to the system's cache folder (~/.cache/rosmo360 on Linux).
        #[arg(long, conflicts_with = "folder")]
        default: bool,
    },
    /// Delete the cached entries: all of them, or those unused for a while.
    Clear {
        /// Only entries no clip has used for this many days.
        #[arg(long)]
        older_than: Option<f64>,
    },
}

/// Each path given that is a file, and every .OSV in the folders given (and
/// the folders inside), in name order.
fn osv_under(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    fn walk(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        if path.is_dir() {
            let mut inside: Vec<_> = std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?.flatten().map(|e| e.path()).collect();
            inside.sort();
            for p in inside {
                if p.is_dir() || p.extension().is_some_and(|e| e.eq_ignore_ascii_case("osv")) {
                    walk(&p, out)?;
                }
            }
        } else if path.exists() {
            out.push(path.to_owned());
        } else {
            bail!("{} does not exist", path.display());
        }
        Ok(())
    }
    let mut clips = Vec::new();
    for p in paths {
        walk(p, &mut clips)?;
    }
    Ok(clips)
}

/// `cache add`: caches each clip under `paths` that isn't yet (all of them
/// with `force`), saying how it went one line a clip. A clip that fails is
/// reported and skipped.
fn cache_clips(paths: &[PathBuf], force: bool) -> Result<()> {
    let clips = osv_under(paths)?;
    let dir = cache::dir().context("no cache folder (HOME is not set)")?;
    eprintln!("caching {} clips in {}", clips.len(), dir.display());
    let (mut added, mut failed) = (0, 0);
    for (i, clip) in clips.iter().enumerate() {
        let name = clip.file_name().map_or_else(|| clip.display().to_string(), |n| n.to_string_lossy().into_owned());
        let at = std::time::Instant::now();
        let was = !force && cache::has(clip);
        if force {
            cache::forget(clip)?;
        }
        match player::cache_clip(clip) {
            Ok(()) if was => println!("[{}/{}] {name}: already cached", i + 1, clips.len()),
            Ok(()) => {
                added += 1;
                println!("[{}/{}] {name}: cached in {:.0} s", i + 1, clips.len(), at.elapsed().as_secs_f64());
            }
            Err(e) => {
                failed += 1;
                println!("[{}/{}] {name}: failed: {e:#}", i + 1, clips.len());
            }
        }
    }
    eprintln!("{added} added, {} already there, {failed} failed", clips.len() - added - failed);
    Ok(())
}

fn parse_pair(s: &str) -> Result<(f64, f64), String> {
    let (a, b) = s.split_once(',').ok_or("give YAW,PITCH")?;
    Ok((a.trim().parse().map_err(|e| format!("{a:?}: {e}"))?, b.trim().parse().map_err(|e| format!("{b:?}: {e}"))?))
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum, serde::Serialize, serde::Deserialize)]
enum Resolution {
    #[value(name = "720p")]
    #[serde(rename = "720p")]
    P720,
    #[value(name = "1080p")]
    #[serde(rename = "1080p")]
    P1080,
    #[value(name = "1440p")]
    #[serde(rename = "1440p")]
    P1440,
    #[value(name = "2160p", alias = "4k")]
    #[serde(rename = "2160p")]
    P2160,
}

impl Resolution {
    const ALL: [Resolution; 4] = [Resolution::P720, Resolution::P1080, Resolution::P1440, Resolution::P2160];

    /// Pixels on the picture's short side.
    fn short_side(self) -> u32 {
        match self {
            Resolution::P720 => 720,
            Resolution::P1080 => 1080,
            Resolution::P1440 => 1440,
            Resolution::P2160 => 2160,
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}p", self.short_side())
    }
}

/// How `export` and `preview` frame the sphere: keyframes from a file or a
/// fixed view.
#[derive(Args)]
struct Framing {
    /// Keyframed views as JSON: {"aspect": "9:16", "keyframes": [{"t": 12,
    /// "yaw": 40, "pitch": -5, "fov": 80, "projection": "perspective", "ease":
    /// "smooth"}, ...]}. Keyframes may leave out what doesn't change.
    #[arg(long, conflicts_with = "view")]
    reframe: Option<PathBuf>,
    /// A fixed view: YAW,PITCH[,ROLL[,FOV]] in degrees.
    #[arg(long, value_parser = parse_view, allow_hyphen_values = true)]
    view: Option<Angles>,
    /// Projection for --view.
    #[arg(long, value_enum, default_value_t = Projection::Perspective, requires = "view")]
    projection: Projection,
}

impl Framing {
    fn reframe(&self) -> Result<Option<Reframe>> {
        if let Some(path) = &self.reframe {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let mut reframe: Reframe = serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
            // Tracks it follows are named relative to it, as in a project.
            let dir = path.parent().unwrap_or(Path::new(""));
            for track in reframe.keyframes.tracks_mut() {
                *track = dir.join(&*track);
            }
            return Ok(Some(reframe));
        }
        Ok(self.view.as_ref().map(|Angles(v)| {
            let home = View::home(self.projection);
            Reframe::fixed(View { yaw: v[0], pitch: v[1], roll: v.get(2).copied().unwrap_or(home.roll), fov: v.get(3).copied().unwrap_or(home.fov), ..home })
        }))
    }
}

/// --view's YAW,PITCH[,ROLL[,FOV]]. A newtype, since clap takes a bare
/// `Vec` for a flag given several times.
#[derive(Clone)]
struct Angles(Vec<f64>);

fn parse_view(s: &str) -> Result<Angles, String> {
    let v = s.split(',').map(|x| x.trim().parse::<f64>().map_err(|e| format!("{x:?}: {e}"))).collect::<Result<Vec<_>, _>>()?;
    if !(2..=4).contains(&v.len()) {
        return Err("expected YAW,PITCH[,ROLL[,FOV]]".into());
    }
    Ok(Angles(v))
}

fn parse_size(s: &str) -> Result<[u32; 2], String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    let (w, h): (u32, u32) = (w.parse().map_err(|e| format!("{e}"))?, h.parse().map_err(|e| format!("{e}"))?);
    if w < 16 || h < 16 || w % 2 == 1 || h % 2 == 1 {
        return Err("width and height must be even and at least 16".into());
    }
    Ok([w, h])
}

#[derive(Args, Clone)]
struct StitchOpts {
    /// Use this alternative calibration (see `rig`'s `variants`) instead of the default.
    #[arg(long)]
    variant: Option<usize>,
    /// Render only this lens (0 or 1), for checking overlap alignment.
    #[arg(long)]
    only: Option<usize>,
    /// Stabilisation mode, from the camera's attitude data [default: off
    /// for stitch and render, horizon for export and preview].
    #[arg(long, value_enum)]
    stabilize: Option<stab::Mode>,
    /// Heading smoothing window for `horizon`, in ms either side.
    #[arg(long, default_value_t = 500.0)]
    smooth: f64,
    /// How the IMU's axes sit relative to the rig's.
    #[arg(long, value_enum, default_value_t = stab::ImuAxes::X180)]
    imu_axes: stab::ImuAxes,
    /// Treat the attitude's world z as pointing down rather than up.
    #[arg(long)]
    world_z_down: bool,
    /// Skip rolling-shutter correction.
    #[arg(long)]
    no_rs: bool,
    /// Even out the lenses' exposure at the seam. Off by default: where the
    /// sun veils one lens it dulls the other to meet it.
    #[arg(long)]
    exposure: bool,
    /// Skip shifting the lenses to line up close subjects at the seam.
    #[arg(long)]
    no_align: bool,
    /// Blend everywhere instead of cutting where the lenses disagree.
    #[arg(long)]
    no_cut: bool,
}

impl Default for StitchOpts {
    fn default() -> Self {
        StitchOpts {
            variant: None,
            only: None,
            stabilize: None,
            smooth: 500.0,
            imu_axes: stab::ImuAxes::X180,
            world_z_down: false,
            no_rs: false,
            exposure: false,
            no_align: false,
            no_cut: false,
        }
    }
}

/// What to draw from the stitched sphere.
enum Shot {
    /// The whole sphere, this wide.
    Equirect(usize),
    View { camera: Box<view::Camera>, size: (usize, usize) },
}

/// Everything needed to turn decoded lens frames at a given time into a
/// stitched picture.
struct Pipeline {
    input: PathBuf,
    lenses: Vec<stitch::Lens>,
    tracks: Vec<u32>,
    stab: stab::Stabilizer,
    readout_ms: f64,
    fps: f64,
    only: Option<usize>,
    rs_steps: usize,
    seam: Option<exposure::Seam>,
    aligner: Option<align::Aligner>,
    cut: bool,
    hw: ffmpeg::Hwaccel,
    /// Decode this side-by-side proxy instead of the lens tracks.
    proxy: Option<PathBuf>,
    /// An ordinary video: no lenses, rig or attitude; drawn by cropping.
    flat: Option<ffmpeg::Flat>,
    /// Decode at this rate instead of the clip's own (an ordinary video in
    /// an edit at another rate); `fps` is then this.
    rate: Option<f64>,
    /// The colour profile it was filmed in (D-Log M wants a LUT).
    color_mode: dji::ColorMode,
    /// Tracks that keyframes follow, read once each (`None`: unreadable).
    followed: std::sync::Mutex<std::collections::HashMap<PathBuf, Option<std::sync::Arc<track::Track>>>>,
}

/// Seam state carried from frame to frame so it eases instead of flickering.
#[derive(Default)]
struct Carry {
    gains: Option<exposure::Gains>,
    field: Option<align::Field>,
}

impl Pipeline {
    /// `stabilize` is the mode when the options don't name one.
    fn new(input: &Path, opts: &StitchOpts, stabilize: stab::Mode) -> Result<Self> {
        Self::load(input, opts, stabilize, &|_| {})
    }

    /// `new`, telling `progress` how far reading the telemetry has got (0..1).
    fn load(input: &Path, opts: &StitchOpts, stabilize: stab::Mode, progress: &dyn Fn(f64)) -> Result<Self> {
        if let Some(flat) = ffmpeg::flat(input)? {
            return Self::load_flat(input, flat);
        }
        let telemetry = cache::telemetry(input, progress)?;
        let rig = telemetry.rig.context("no camera rig in file")?;
        let projection = match opts.variant {
            Some(i) => &rig.variants.get(i).context("no such variant")?.projection,
            None => &rig.projection,
        };
        let ProjectionKind::Rig { cameras, .. } = &projection.kind else {
            bail!("not a multi-lens rig");
        };
        let lenses = cameras.iter().map(stitch::Lens::new).collect::<Result<Vec<_>>>()?;
        let tracks = rig
            .pictures
            .iter()
            .map(|p| match p {
                RigPicture::VideoTrack(n) => Ok(*n),
                RigPicture::Sibling { .. } => bail!("multi-file rigs are not supported"),
            })
            .collect::<Result<Vec<_>>>()?;
        let mode = opts.stabilize.unwrap_or(stabilize);
        let stab = stab::Stabilizer::new(&telemetry.quats, mode, opts.imu_axes, if opts.world_z_down { 1.0 } else { -1.0 }, opts.smooth);
        if !stab.has_data() && mode != stab::Mode::Off {
            bail!("no attitude data to stabilise with");
        }
        let readout_ms = cameras[0].readout.map_or(0.0, |r| r.time_us / 1000.0);
        let rs_steps = if opts.no_rs || readout_ms == 0.0 || !stab.has_data() { 1 } else { 33 };
        let seam = if opts.exposure && opts.only.is_none() { exposure::Seam::new(&lenses) } else { None };
        let aligner = if opts.no_align || opts.only.is_some() { None } else { align::Aligner::new(&lenses) };
        let hw = ffmpeg::Hwaccel::pick(input, tracks[0])?;
        Ok(Pipeline {
            input: input.to_owned(),
            lenses,
            tracks,
            stab,
            readout_ms,
            fps: ffmpeg::frame_rate(input)?,
            only: opts.only,
            rs_steps,
            seam,
            aligner,
            cut: !opts.no_cut && opts.only.is_none(),
            hw,
            proxy: None,
            flat: None,
            rate: None,
            color_mode: telemetry.color_mode,
            followed: Default::default(),
        })
    }

    /// An ordinary video: nothing to stitch, steady or follow.
    fn load_flat(input: &Path, flat: ffmpeg::Flat) -> Result<Self> {
        Ok(Pipeline {
            input: input.to_owned(),
            lenses: Vec::new(),
            tracks: vec![flat.track],
            stab: stab::Stabilizer::new(&[], stab::Mode::Off, stab::ImuAxes::X180, -1.0, 0.0),
            readout_ms: 0.0,
            fps: ffmpeg::frame_rate(input)?,
            only: None,
            rs_steps: 1,
            seam: None,
            aligner: None,
            cut: false,
            hw: if flat.picture { ffmpeg::Hwaccel::Software } else { ffmpeg::Hwaccel::pick(input, flat.track)? },
            proxy: None,
            flat: Some(flat),
            rate: None,
            color_mode: dji::ColorMode::Normal,
            followed: Default::default(),
        })
    }

    /// Where `follow` points in the view at clip time `t` (seconds).
    fn aim(&self, follow: &view::Follow, t: f64) -> Option<(f64, f64)> {
        if self.flat.is_some() {
            return None;
        }
        match follow {
            view::Follow::Stick => self.stab.stick(t * 1000.0, self.readout_ms),
            view::Follow::Track(path) => {
                let track = self
                    .followed
                    .lock()
                    .unwrap()
                    .entry(path.clone())
                    .or_insert_with(|| track::Track::load(path).inspect_err(|e| eprintln!("{e:#}; keyframes following it look ahead")).ok().map(std::sync::Arc::new))
                    .clone()?;
                track.aim(&self.stab, t, self.readout_ms)
            }
        }
    }

    /// Decode from the clip's proxy from now on, if it has one.
    fn use_proxy(&mut self) {
        if self.flat.is_some() {
            return;
        }
        self.proxy = ffmpeg::proxy(&self.input);
        if self.proxy.is_none() {
            eprintln!("no proxy (LRF) next to the clip; using the full-resolution tracks");
        }
    }

    /// Rounds to the nearest frame so attitude lookups match decoded pictures.
    fn snap(&self, t: f64) -> f64 {
        (t * self.fps).round() / self.fps
    }

    /// Stitches one frame, measuring the seam afresh and easing towards it.
    fn render(&self, frames: &[stitch::Frame], t: f64, carry: &mut Carry, shot: &Shot) -> stitch::Frame {
        if self.flat.is_some() {
            let picture = &frames[0];
            return match *shot {
                // The whole picture, as the home view shows it.
                Shot::Equirect(width) => {
                    let size = (width, (width * picture.height / picture.width).max(2) / 2 * 2);
                    stitch::flat(picture, size, &view::View::home(view::Projection::Perspective).camera())
                }
                Shot::View { ref camera, size } => {
                    let mut frame = stitch::flat(picture, size, camera);
                    grade_frame(&mut frame, camera);
                    frame
                }
            };
        }
        let views = if self.stab.has_data() { self.stab.rig_from_view(t * 1000.0, self.readout_ms, self.rs_steps) } else { vec![stitch::IDENTITY] };
        let rgb = |i: usize, u: f64, v: f64| self.lenses[i].sample(frames, u, v).map(|c| c / 255.0);
        let exposure = self.seam.as_ref().map(|seam| {
            let now = seam.measure(|i, u, v| Some(rgb(i, u, v)));
            let g = carry.gains.get_or_insert(now);
            g.ease(&now, exposure::EASE);
            (seam, *g)
        });
        let align = self.aligner.as_ref().map(|aligner| {
            let now = aligner.measure(&aligner.strips(|i, u, v| Some(rgb(i, u, v).iter().sum::<f64>() / 3.0)), carry.field.as_ref());
            let f = carry.field.get_or_insert_with(|| now.clone());
            f.ease(&now, align::EASE);
            (aligner, f.clone())
        });
        let seam = stitch::Seam { exposure: exposure.as_ref().map(|(s, g)| (*s, g)), align: align.as_ref().map(|(a, f)| (*a, f)), cut: self.cut };
        match *shot {
            Shot::Equirect(width) => stitch::equirect(&self.lenses, frames, width, self.only, &views, &seam),
            Shot::View { ref camera, size: (w, h) } => {
                let ray = |x: f64, y: f64| stitch::apply(camera.user, camera.ray(w as f64 / h as f64, [x / w as f64 * 2.0 - 1.0, y / h as f64 * 2.0 - 1.0]));
                let mut frame = stitch::render(&self.lenses, frames, (w, h), ray, self.only, &views, &seam);
                grade_frame(&mut frame, camera);
                frame
            }
        }
    }

    /// Every lens's frame at `t`.
    fn stills(&self, t: f64) -> Result<Vec<stitch::Frame>> {
        if let Some(flat) = &self.flat {
            let (w, h) = flat.size();
            return Ok(vec![ffmpeg::still_out(&self.input, flat.track, t, self.hw, (w as usize, h as usize), flat.out(None, None))?]);
        }
        match &self.proxy {
            Some(proxy) => Ok(ffmpeg::split_proxy(&ffmpeg::still(proxy, 0, t, self.hw)?, &self.tracks)),
            None => self.tracks.iter().map(|&n| ffmpeg::still(&self.input, n, t, self.hw)).collect(),
        }
    }

    /// Says so when an export holds this clip's last frame for more than a
    /// frame or two: the cut runs past what decoded (a duration rounded up
    /// costs a frame), so its picture freezes.
    pub fn warn_held(&self, frames: usize) {
        if frames > 2 {
            eprintln!(
                "\nwarning: {} ran out {frames} frames ({:.2} s) before the cut's end; its last frame is held",
                self.input.display(),
                frames as f64 / self.fps
            );
        }
    }

    fn stream(&self, start: f64) -> Result<Stream> {
        if let Some(flat) = &self.flat {
            let (w, h) = flat.size();
            return Ok(Stream::Tracks(vec![ffmpeg::Decoder::with(&self.input, flat.track, start, self.hw, (w as usize, h as usize), flat.out(None, self.rate))?]));
        }
        Ok(match &self.proxy {
            // At the clip's rate: in the 100 fps mode the proxy is 25 fps,
            // and frames are timed n / fps.
            Some(proxy) => {
                let out = ffmpeg::Out { rate: Some(self.fps), ..Default::default() };
                Stream::Proxy(ffmpeg::Decoder::with(proxy, 0, start, self.hw, ffmpeg::dimensions(proxy, 0)?, out)?, self.tracks.clone())
            }
            None => Stream::Tracks(self.tracks.iter().map(|&n| ffmpeg::Decoder::new(&self.input, n, start, self.hw)).collect::<Result<_>>()?),
        })
    }

    /// The span to render: `start`/`duration` if given, else the keyframes'
    /// span, else from `start` to the end of the clip.
    fn span(&self, start: Option<f64>, duration: Option<f64>, reframe: Option<&Reframe>) -> Result<(f64, f64)> {
        let (start, end) = span(&self.input, start, duration, reframe)?;
        Ok((self.snap(start), end))
    }
}

/// The span of `input` to render: `start`/`duration` if given, else the
/// keyframes' span, else from `start` to the end of the clip.
fn span(input: &Path, start: Option<f64>, duration: Option<f64>, reframe: Option<&Reframe>) -> Result<(f64, f64)> {
    let keyed = reframe.and_then(Reframe::span);
    let start = start.or(keyed.map(|k| k.0)).unwrap_or(0.0);
    let end = match (duration, keyed) {
        (Some(d), _) => start + d,
        (None, Some((_, end))) if end > start => end,
        // A picture has no end of its own.
        _ => Some(ffmpeg::duration(input)?).filter(|&d| d < ffmpeg::PICTURE_MOST).unwrap_or(start + ffmpeg::PICTURE_SECONDS),
    };
    if end <= start {
        bail!("nothing to render between {start} and {end} s");
    }
    Ok((start, end))
}

/// Renders every cut of `project` back to back into one MP4 at `size`,
/// with their sound. Each clip is set up once however many cuts use it.
/// `progress(done, total)` hears of every frame; an error from it (a
/// cancel) stops the export and removes the unfinished file.
/// Sets up each cut's clip once per clip and stabilisation (the cut's own
/// wins over the command's): the pipelines, and per cut the index of its own.
fn cut_pipelines(project: &mut project::Project, opts: &StitchOpts, proxy: bool) -> Result<(Vec<Pipeline>, Vec<usize>)> {
    let mut pipelines: Vec<Pipeline> = Vec::new();
    let mut of_cut = Vec::with_capacity(project.cuts.len());
    for cut in &mut project.cuts {
        let mode = cut.stabilize.or(opts.stabilize).unwrap_or(stab::Mode::Horizon);
        let i = match pipelines.iter().position(|p| p.input == cut.input && p.stab.mode() == mode) {
            Some(i) => i,
            None => {
                let mut p = Pipeline::new(&cut.input, &StitchOpts { stabilize: Some(mode), ..opts.clone() }, mode)?;
                if proxy {
                    p.use_proxy();
                }
                pipelines.push(p);
                pipelines.len() - 1
            }
        };
        cut.flat = pipelines[i].flat.is_some();
        if let Err(e) = cut.resolve_lut() {
            eprintln!("warning: {e:#}; {} is left as filmed", cut.input.display());
        }
        of_cut.push(i);
    }
    // The edit runs at its first 360 clip's rate (its first video's if it
    // has none, else 30 fps); ordinary videos at another rate are decoded
    // at it, frames repeated or dropped, so they keep their speed and stay
    // in step, and pictures are repeated at it.
    let fps = pipelines.iter().find(|p| p.flat.is_none()).or(pipelines.iter().find(|p| p.flat.is_some_and(|f| !f.picture))).map_or(30.0, |p| p.fps);
    for p in pipelines.iter_mut().filter(|p| p.flat.is_some_and(|f| f.picture) || (p.flat.is_some() && (p.fps - fps).abs() > 0.01)) {
        p.rate = Some(fps);
        p.fps = fps;
    }
    Ok((pipelines, of_cut))
}

/// How and where `export` writes.
pub struct ExportOpts {
    pub size: (usize, usize),
    /// Stitch from the proxies: quick and soft.
    pub draft: bool,
    /// Stitch on the CPU, not the GPU.
    pub cpu: bool,
    pub format: ffmpeg::Format,
    pub target: Target,
}

pub enum Target {
    /// The whole edit in one file.
    One(PathBuf),
    /// A file per cut in `dir`, named `<name> - 001.<ext>` and on, for an
    /// external editor, and a WAV per music clip. Each runs `handles` seconds
    /// longer either side where its clip has the footage, for that editor's
    /// own transitions.
    Clips { dir: PathBuf, name: String, handles: f64, graded: bool },
}

/// Renders `project` as `out` says, with sound, and returns the files
/// written. Each clip is set up once however many cuts use it.
/// `progress(done, total)` hears of every frame; an error from it (a
/// cancel) stops the export and removes the unfinished file.
fn export(project: &project::Project, opts: &StitchOpts, out: &ExportOpts, progress: &mut dyn FnMut(usize, usize) -> Result<()>) -> Result<Vec<PathBuf>> {
    if project.cuts.is_empty() {
        bail!("nothing to export yet: the project has music but no video cuts");
    }
    let mut project = project.clone();
    let (pipelines, of_cut) = cut_pipelines(&mut project, opts, out.draft)?;
    let project = &project;
    // The shader always draws both lenses, so `--only` needs the CPU.
    let mut gpu = if out.cpu || opts.only.is_some() {
        None
    } else {
        // The Resolve format is 10-bit, so 16-bit frames from the GPU.
        match player::render::Gpu::new(out.format == ffmpeg::Format::Resolve) {
            Ok(gpu) => {
                eprintln!("stitching on {}", gpu.name);
                Some(gpu)
            }
            Err(e) => {
                eprintln!("stitching on the CPU: {e:#}");
                None
            }
        }
    };
    let clips: Vec<_> = if gpu.is_some() { pipelines.iter().map(player::render::Clip::new).collect::<Result<_>>()? } else { Vec::new() };
    let handles = match out.target {
        Target::One(_) => 0.0,
        Target::Clips { handles, .. } => handles.max(0.0),
    };
    // Per cut: its clip's pipeline, start (on a frame) and frame count.
    let mut plan = Vec::with_capacity(project.cuts.len());
    for (cut, &i) in project.cuts.iter().zip(&of_cut) {
        let p = &pipelines[i];
        let from = (cut.start - handles).max(0.0);
        let end = (cut.end + handles).min(ffmpeg::duration(&cut.input)?);
        if end <= from {
            bail!("the cut from {} s starts after {} ends", cut.start, cut.input.display());
        }
        // Count frames over the span asked for, not from the frame the start
        // snaps to: snapping 14 s forward to 14.014 left `-d 5` at 149 frames.
        plan.push((i, p.snap(from), ((end - from) * p.fps).round().max(1.0) as usize));
    }
    let fps = pipelines[0].fps;
    if pipelines.iter().any(|p| (p.fps - fps).abs() > 0.01) {
        eprintln!("the clips' frame rates differ; exporting at {fps:.3} fps");
    }
    // Frames each cut shares with the one before: its crossfade, in the
    // whole edit only (a file per cut is for another editor's own), and
    // never more than either cut has to give.
    let mut overlap = vec![0usize; plan.len()];
    if matches!(out.target, Target::One(_)) {
        for n in 1..plan.len() {
            let room = (plan[n - 1].2 - overlap[n - 1]).min(plan[n].2).saturating_sub(1);
            overlap[n] = ((project.cuts[n].crossfade * fps).round() as usize).min(room);
        }
    }
    // Each file and the cuts it holds.
    let files: Vec<(PathBuf, std::ops::Range<usize>)> = match &out.target {
        Target::One(path) => vec![(path.clone(), 0..plan.len())],
        Target::Clips { dir, name, .. } => {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            (0..plan.len()).map(|k| (dir.join(format!("{name} - {:03}.{}", k + 1, out.format.extension())), k..k + 1)).collect()
        }
    };
    let total = plan.iter().map(|p| p.2).sum();
    let mut done = 0;
    let mut written = Vec::with_capacity(files.len());
    for (path, cuts) in files {
        let sound: Vec<_> = cuts
            .clone()
            .zip(&plan[cuts.clone()])
            .map(|(n, &(i, start, frames))| {
                let crossfade = if n > cuts.start { overlap[n] as f64 / fps } else { 0.0 };
                ffmpeg::Sound { input: &pipelines[i].input, start, duration: Some(frames as f64 / fps), volume: &project.cuts[n].volume, crossfade }
            })
            .collect();
        // The whole edit gets its mix; a file per cut is for another editor,
        // so its sound is left as recorded and the music goes beside it.
        let length: f64 = cuts.clone().map(|n| (plan[n].2 - if n > cuts.start { overlap[n] } else { 0 }) as f64 / fps).sum();
        let mix = ffmpeg::Mix { sound_db: project.sound.volume, music: &project.music, length, fades: project.fades };
        let mix = matches!(out.target, Target::One(_)).then_some(&mix);
        let mut encoder = ffmpeg::Encoder::new(&path, out.size, fps, &sound, mix, out.format, out.format == ffmpeg::Format::Resolve && gpu.is_some())?;
        // Overlays over the whole edit only; a file per cut has none.
        let lanes = if matches!(out.target, Target::One(_)) { project.overlays.iter().chain(&project.text).collect() } else { Vec::new() };
        let mut burn = overlay::Burn::new(&mut encoder, lanes, out.size, fps, project.fades, project.duration())?;
        let mut sink = dissolve::Dissolve::new(&mut burn);
        for (n, &(i, start, frames)) in cuts.clone().zip(&plan[cuts.clone()]) {
            let tail = if n + 1 < cuts.end { overlap[n + 1] } else { 0 };
            let head = if n > cuts.start { overlap[n] } else { 0 };
            sink.cut(frames, head, tail)?;
            let size = out.size;
            let mut tick = |n: usize| progress(done + n, total);
            // Framed by its keyframes and graded by its effects (in the whole
            // edit, faded to black at its ends; clips graded only if asked).
            let (whole, graded) = match out.target {
                Target::One(_) => (true, true),
                Target::Clips { graded, .. } => (false, graded),
            };
            let camera = |t: f64| {
                let mut c = project::camera_at(&project.cuts, n, t, &|f, t| pipelines[i].aim(f, t));
                if !graded {
                    // Ungraded clips are for grading elsewhere: as filmed,
                    // so D-Log M stays log for the editor's own LUT.
                    c.grade = Default::default();
                    c.lut = None;
                } else if whole {
                    c.grade.black = project.black_at(n, t);
                }
                c
            };
            let result = match &mut gpu {
                Some(gpu) => player::render::encode(gpu, &pipelines[i], &clips[i], start, frames, &mut sink, camera, size, &mut tick),
                None => encode(&pipelines[i], start, Some(frames), &mut sink, |t| Shot::View { camera: Box::new(camera(t)), size }, &mut tick),
            };
            if let Err(e) = result {
                drop(sink);
                drop(burn);
                encoder.abort();
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            done += frames;
        }
        drop(sink);
        drop(burn);
        encoder.finish()?;
        written.push(path);
    }
    // Beside the clips, each music clip as it is in the song, for the other
    // editor to level and fade: with handles, but none of the mix.
    if let Target::Clips { dir, name, .. } = &out.target {
        let mut music: Vec<_> = project.music.clips.iter().collect();
        music.sort_by(|a, b| a.at.total_cmp(&b.at));
        for (k, clip) in music.into_iter().enumerate() {
            let path = dir.join(format!("{name} - music {:03}.wav", k + 1));
            let from = (clip.start - handles).max(0.0);
            let end = (clip.end + handles).min(ffmpeg::duration(&clip.input)?);
            ffmpeg::wav(&clip.input, from, end - from, &path)?;
            written.push(path);
        }
    }
    Ok(written)
}

/// Progress on the terminal: frames done, out of how many, and the rate.
fn print_progress() -> impl FnMut(usize, usize) -> Result<()> {
    let began = Instant::now();
    move |done, total| {
        if done.is_multiple_of(10) || done == total {
            eprint!("\rframe {done}/{total}  {:.2} fps", done as f64 / began.elapsed().as_secs_f64().max(1e-3));
        }
        if done == total {
            eprintln!();
        }
        Ok(())
    }
}

/// Decoded lens frames in order, from the lens tracks or the proxy.
enum Stream {
    Tracks(Vec<ffmpeg::Decoder>),
    Proxy(ffmpeg::Decoder, Vec<u32>),
}

impl Stream {
    /// One frame per lens; `None` once the clip ends.
    fn next(&mut self) -> Result<Option<Vec<stitch::Frame>>> {
        match self {
            Stream::Tracks(decoders) => decoders.iter_mut().map(|d| d.next()).collect::<Result<Option<Vec<_>>>>(),
            Stream::Proxy(decoder, tracks) => Ok(decoder.next()?.map(|f| ffmpeg::split_proxy(&f, tracks))),
        }
    }
}

/// The command line, as the player wants it when started as a macOS app:
/// Finder starts it with nothing (or, on some versions, only a `-psn_`
/// process serial number), which opens the player, as the AppImage's AppRun
/// does on Linux. From a terminal nothing still prints the help.
/// On Windows, double-clicked in Explorer (or a clip or project opened with
/// it) the same: the player, with the file if there is one.
fn app_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().filter(|a| !a.to_string_lossy().starts_with("-psn_")).collect();
    let in_app = std::env::current_exe().is_ok_and(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"));
    let explorer = from_explorer();
    if (cfg!(target_os = "macos") && in_app || explorer) && args.len() == 1 {
        args.push("play".into());
    } else if explorer && args.len() == 2 {
        args.insert(1, "play".into());
    }
    args
}

/// Whether Windows started us from Explorer rather than a terminal: then the
/// console it made is ours alone (a terminal shares its own), and it's
/// closed, so the player doesn't sit beside a black window. Errors then go
/// to a message box (`main`), there being nowhere to print them.
#[cfg(windows)]
fn from_explorer() -> bool {
    unsafe extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    static ALONE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALONE.get_or_init(|| {
        let mut ids = [0u32; 2];
        let alone = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), 2) } == 1;
        if alone {
            unsafe { FreeConsole() };
        }
        alone
    })
}

#[cfg(not(windows))]
fn from_explorer() -> bool {
    false
}

fn main() -> Result<()> {
    // Output piped into something that stopped reading (`| head`) is no
    // failure to report: the reader has what it wanted.
    match run() {
        Err(e) if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)) => Ok(()),
        Err(e) if from_explorer() => {
            rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("rOsmo360").set_description(format!("{e:#}")).show();
            Err(e)
        }
        r => r,
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse_from(app_args());
    if cli.no_cache {
        cache::disable();
    }
    match cli.cmd {
        Cmd::Cache { action: None } => cache::report()?,
        Cmd::Cache { action: Some(CacheAction::Clear { older_than }) } => cache::clear(older_than)?,
        Cmd::Cache { action: Some(CacheAction::Add { paths, force }) } => cache_clips(&paths, force)?,
        Cmd::Cache { action: Some(CacheAction::Move { folder, .. }) } => cache::move_to(folder)?,
        Cmd::Rig { input } => {
            needs_360(&input, "a camera rig")?;
            let rig = cache::telemetry(&input, &|_| {})?.rig.context("no camera rig in file")?;
            println!("{}", serde_json::to_string_pretty(&rig)?);
        }
        Cmd::Imu { input } => {
            needs_360(&input, "motion data")?;
            let imu = imu::read(&load_input(&input, &|_| {})?);
            println!("kind,t,w,x,y,z");
            for (t, q) in &imu.quats {
                println!("q,{t},{},{},{},{}", q[0], q[1], q[2], q[3]);
            }
            for (kind, series) in [("g", &imu.gyro), ("a", &imu.accel)] {
                for (t, v) in series {
                    println!("{kind},{t},,{},{},{}", v[0], v[1], v[2]);
                }
            }
        }
        Cmd::Info { input } => info(&input)?,
        Cmd::Check { project, fast_pan, opts } => check::check(&project, &opts, fast_pan)?,
        Cmd::Play { input, osv_size, hwaccel, stabilize, imu_axes, world_z_down } => {
            player::run(input, player::Launch { osv_size, hw: hwaccel, stabilize, imu_axes, world_z_down })?;
        }
        Cmd::Seam { input, start, duration, dump } => needs_360(&input, "a seam").and_then(|_| seam_report(&input, start, duration, dump.as_deref()))?,
        Cmd::Stitch { input, time, output, width, opts } => {
            let p = Pipeline::new(&input, &opts, stab::Mode::Off)?;
            let time = p.snap(time);
            let out = p.render(&p.stills(time)?, time, &mut Carry::default(), &Shot::Equirect(width));
            save_png(&output, &out)?;
            eprintln!("wrote {}", output.display());
        }
        Cmd::Render { input, start, duration, output, width, opts } => {
            let p = Pipeline::new(&input, &opts, stab::Mode::Off)?;
            let start = p.snap(start);
            let total = duration.map(|d| (d * p.fps).round() as usize);
            let mut encoder = ffmpeg::Encoder::new(&output, (width, width / 2), p.fps, &[ffmpeg::Sound { input: &input, start, duration, volume: &[], crossfade: 0.0 }], None, ffmpeg::Format::Mp4, false)?;
            let began = Instant::now();
            let mut tick = |n: usize| {
                if n.is_multiple_of(10) {
                    eprint!("\rframe {n}{}  {:.2} fps", total.map_or(String::new(), |t| format!("/{t}")), n as f64 / began.elapsed().as_secs_f64());
                }
                Ok(())
            };
            encode(&p, start, total, &mut encoder, |_| Shot::Equirect(width), &mut tick)?;
            eprintln!();
            encoder.finish()?;
            eprintln!("wrote {}", output.display());
        }
        Cmd::Export { input, framing, start, duration, resolution, aspect, size, output, format, clips, handles, graded, draft, cpu, opts } => {
            let name = input.file_stem().map_or_else(|| "export".to_owned(), |s| s.to_string_lossy().into_owned());
            let project = if is_project(&input) {
                if framing.reframe.is_some() || framing.view.is_some() || start.is_some() || duration.is_some() {
                    bail!("a project sets its own cuts and views; leave out --reframe, --view, -s and -d");
                }
                project::Project::load(&input)?
            } else {
                // One clip is a project of one cut.
                let reframe = framing.reframe()?.unwrap_or_else(|| Reframe::fixed(View::home(Projection::Perspective)));
                let (start, end) = span(&input, start, duration, Some(&reframe))?;
                project::Project { aspect: reframe.aspect, cuts: vec![project::Cut { input, start, end, keyframes: reframe.keyframes, stabilize: None, note: None, volume: Vec::new(), crossfade: 0.0, effects: Vec::new(), lut: None, old_lut_strength: None, flat: false, lut_applied: None }], ..Default::default() }
            };
            let aspect = aspect.or(project.aspect).unwrap_or(view::Aspect(16, 9));
            let [w, h] = size.unwrap_or_else(|| aspect.size(resolution.short_side())).map(|v| v as usize);
            let target = if clips {
                Target::Clips { dir: output.unwrap_or_else(|| PathBuf::from(&name)), name, handles, graded }
            } else {
                Target::One(output.unwrap_or_else(|| PathBuf::from(format!("clip.{}", format.extension()))))
            };
            let written = export(&project, &opts, &ExportOpts { size: (w, h), draft, cpu, format, target }, &mut print_progress())?;
            for path in &written {
                // The music beside clips is sound only.
                let size = if path.extension().is_some_and(|e| e == "wav") { String::new() } else { format!(" ({w}×{h})") };
                eprintln!("wrote {}{size}", path.display());
            }
            eprintln!("{} file(s), {:.2} s of edit", written.len(), project.duration());
        }
        Cmd::Preview { input, framing, start, duration, every, tile, columns, output, full, opts } if is_project(&input) => {
            if framing.reframe.is_some() || framing.view.is_some() || start.is_some() || duration.is_some() {
                bail!("a project sets its own cuts and views; leave out --reframe, --view, -s and -d");
            }
            let project = project::Project::load(&input)?;
            let (sheet, tiles) = preview_project(&project, &opts, every, tile, columns.max(1), !full)?;
            save_png(&output, &sheet)?;
            eprintln!("wrote {}: {tiles} tiles of {} cuts", output.display(), project.cuts.len());
        }
        Cmd::Preview { input, framing, start, duration, every, tile, columns, output, full, opts } => {
            let reframe = framing.reframe()?;
            let mut p = Pipeline::new(&input, &opts, stab::Mode::Horizon)?;
            if !full {
                p.use_proxy();
            }
            let times = match (every, &reframe) {
                (Some(every), _) => {
                    let (a, b) = p.span(start, duration, reframe.as_ref())?;
                    (0..).map(|n| a + n as f64 * every.max(1.0 / p.fps)).take_while(|&t| t < b).collect()
                }
                (None, Some(r)) if start.is_none() && duration.is_none() && r.keyframes.len() > 1 => r.keyframes.iter().map(|k| k.t).collect(),
                _ => {
                    let (a, b) = p.span(start, duration, None)?;
                    (0..24).map(|n| a + (n as f64 + 0.5) * (b - a) / 24.0).collect::<Vec<_>>()
                }
            };
            let shot = |t: f64| match &reframe {
                None => Shot::Equirect(tile),
                Some(r) => {
                    let view::Aspect(w, h) = r.aspect.unwrap_or(view::Aspect(16, 9));
                    Shot::View { camera: Box::new(r.camera_at(t, &|f, t| p.aim(f, t))), size: (tile, ((tile * h as usize / w as usize) / 2).max(8) * 2) }
                }
            };
            let tiles = times
                .par_iter()
                .map(|&t| {
                    let t = p.snap(t);
                    let mut f = p.render(&p.stills(t)?, t, &mut Carry::default(), &shot(t));
                    if reframe.is_none() {
                        sheet::equirect_grid(&mut f);
                    }
                    sheet::label(&mut f, 4, 4, &format!("{t:.1}s"));
                    Ok(f)
                })
                .collect::<Result<Vec<_>>>()?;
            save_png(&output, &sheet::compose(&tiles, columns))?;
            eprintln!("wrote {}: {} tiles at {}", output.display(), tiles.len(), times.iter().map(|t| format!("{:.1}", p.snap(*t))).collect::<Vec<_>>().join(" "));
        }
        Cmd::Scan { inputs, output, jobs, every, image } => match (&inputs[..], output) {
            ([input], None) if !input.is_dir() => needs_360(input, "motion data").and_then(|_| scan(input, every, image, &mut std::io::stdout().lock()))?,
            (_, None) => bail!("several clips (or a folder) need --output, a folder for a CSV per clip"),
            (_, Some(dir)) => scan_many(&inputs, &dir, jobs, every, image)?,
        },
        Cmd::Track { input, colour, tolerance, near, start, duration, every } => {
            needs_360(&input, "a sphere to track in")?;
            let near = near.map(|(yaw, pitch)| track::Near::Level(yaw, pitch));
            let (found, rows) = track_to(&input, &track::Colour::parse(&colour, tolerance)?, near, start, duration, every, &mut std::io::stdout().lock(), &mut |_, _| Ok(()))?;
            eprintln!("found the colour in {found} of {rows} samples");
        }
    }
    Ok(())
}

fn is_project(input: &Path) -> bool {
    input.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// A contact sheet of a project: per cut, `columns` moments spread across
/// it (or one every `every` seconds), framed as `export` frames them. Each
/// cut starts a row, headed by its edit time, number, clip and span
/// (`2:16 #23 0036 18.0-25.0`); each tile says its edit and clip time and
/// shows the overlays and text there, and with music the mix's waveform
/// runs under each row, lined up with the
/// tiles. Blank tiles fill out the last. Returns the sheet and how many
/// tiles it shows.
fn preview_project(project: &project::Project, opts: &StitchOpts, every: Option<f64>, tile: usize, columns: usize, proxy: bool) -> Result<(stitch::Frame, usize)> {
    let mut project = project.clone();
    let (pipelines, of_cut) = cut_pipelines(&mut project, opts, proxy)?;
    let project = &project;
    let view::Aspect(w, h) = project.aspect.unwrap_or(view::Aspect(16, 9));
    let size = (tile, ((tile * h as usize / w as usize) / 2).max(8) * 2);
    let offsets = project::offsets(&project.cuts);
    let music = sheet_music(project);
    let width = sheet::width(tile, columns);
    let mut shots = Vec::new();
    let mut bands = Vec::new();
    for (n, (cut, &i)) in project.cuts.iter().zip(&of_cut).enumerate() {
        let p = &pipelines[i];
        let len = cut.end - cut.start;
        let (times, step): (Vec<f64>, f64) = match every {
            Some(every) => {
                let every = every.max(1.0 / p.fps);
                ((0..).map(|k| cut.start + k as f64 * every).take_while(|&t| t < cut.end).collect(), every)
            }
            None => ((0..columns).map(|k| cut.start + (k as f64 + 0.5) * len / columns as f64).collect(), len / columns as f64),
        };
        let edit = |t: f64| offsets[n] + t - cut.start;
        for (r, row) in times.chunks(columns).enumerate() {
            let heading = if r == 0 {
                format!("{} #{} {} {:.1}-{:.1}", check::clock(offsets[n]), n + 1, clip_number(&cut.input), cut.start, cut.end)
            } else {
                format!("{} #{}", check::clock(edit(row[0])), n + 1)
            };
            // Each pixel across the row stands for the edit time between
            // its tiles' times, as far as the cut goes.
            let wave = music.as_ref().map(|level| {
                let at = |x: f64| edit(row[0]) + (x - sheet::tile_middle(tile, 0)) / (sheet::tile_middle(tile, 1) - sheet::tile_middle(tile, 0)).max(1.0) * step;
                (0..width)
                    .map(|x| {
                        let (a, b) = (at(x as f64), at(x as f64 + 1.0));
                        let (from, to) = (offsets[n], offsets[n] + len);
                        (b > from && a < to).then(|| {
                            let k = ((b - a) / player::music::WAVE_STEP).ceil().max(1.0) as usize;
                            (0..k).map(|j| level(a + (b - a) * j as f64 / k as f64)).fold(0.0, f32::max)
                        })
                    })
                    .collect()
            });
            bands.push(sheet::Band { heading, wave });
        }
        let row_end = times.len().div_ceil(columns) * columns;
        shots.extend(times.iter().map(|&t| Some((n, i, p.snap(t).clamp(cut.start, cut.end)))));
        shots.extend((times.len()..row_end).map(|_| None));
    }
    // Overlays and text as the export burns them in, so a title's place
    // shows on the sheet; the tiles leave out the edit's fades, so do they.
    let overlays = overlay::Overlays::new(project.overlays.iter().chain(&project.text).collect(), size, Default::default(), project.duration())?;
    let tiles = shots
        .par_iter()
        .map(|shot| {
            let Some((n, i, t)) = *shot else { return Ok(stitch::Frame { width: size.0, height: size.1, rgb: vec![0; size.0 * size.1 * 3], deep: false }) };
            let p = &pipelines[i];
            let mut f = p.render(&p.stills(t)?, t, &mut Carry::default(), &Shot::View { camera: Box::new(project::camera_at(&project.cuts, n, t, &|f, t| p.aim(f, t))), size });
            let cut = &project.cuts[n];
            let edit = offsets[n] + t - cut.start;
            overlays.draw(&mut f, edit);
            sheet::label(&mut f, 4, 4, &format!("{} {t:.1}s", check::clock(edit)));
            Ok(f)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((sheet::compose_rows(&tiles, columns, &bands), shots.iter().flatten().count()))
}

/// A clip's number for a sheet's headings, which draw only digits: the
/// last run of them in its name (`CAM_20260713153034_0037_D` → 0037).
fn clip_number(input: &Path) -> String {
    let stem = input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let digits: Vec<&str> = stem.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).collect();
    digits.last().map_or_else(String::new, |d| d.to_string())
}

/// The music's loudness at edit time, 0..1, as the export mixes it (each
/// song's peaks at its level, fades and crossfades), or `None` without
/// music. Songs that can't be read are left out, with a note.
fn sheet_music(project: &project::Project) -> Option<impl Fn(f64) -> f32 + '_> {
    let track = &project.music;
    if track.clips.is_empty() {
        return None;
    }
    let waves: Vec<_> = track.clips.iter().map(|c| player::music::Waveform::read(&c.input).inspect_err(|e| eprintln!("no waveform for {}: {e:#}", c.input.display())).ok()).collect();
    let end = project::edit_length(&project.cuts);
    Some(move |t: f64| {
        let mut loudest = 0.0f32;
        for (i, (clip, wave)) in track.clips.iter().zip(&waves).enumerate() {
            let Some(wave) = wave else { continue };
            if t < clip.at || t >= clip.ends() {
                continue;
            }
            let peak = wave.peaks.get(((clip.start + t - clip.at) / player::music::WAVE_STEP) as usize).copied().unwrap_or(0.0);
            let level = player::music::gain(track.level_at(t)) * player::music::envelope(clip, track.crossfades(i), t, end) as f32;
            loudest = loudest.max(peak * level);
        }
        loudest
    })
}

/// Stitches `total` frames (or to the end) from `start` into `encoder`,
/// framing each as `shot` says for its time. If the clip ends early, the
/// last frame is repeated up to `total`, so the sound stays in step.
fn encode(p: &Pipeline, start: f64, total: Option<usize>, encoder: &mut dyn ffmpeg::FrameSink, shot: impl Fn(f64) -> Shot, tick: &mut dyn FnMut(usize) -> Result<()>) -> Result<()> {
    let mut stream = p.stream(start)?;
    let mut carry = Carry::default();
    let mut last = None;
    let mut n = 0;
    while total.is_none_or(|total| n < total) {
        let Some(frames) = stream.next()? else { break };
        let t = start + n as f64 / p.fps;
        let frame = p.render(&frames, t, &mut carry, &shot(t));
        encoder.write(&frame)?;
        last = Some(frame);
        n += 1;
        tick(n)?;
    }
    if let Some(total) = total {
        // A cut with no frames would write none while its sound still
        // counted, and every later cut would run behind its sound.
        let last = last.context("no frames decoded")?;
        p.warn_held(total - n);
        for k in n..total {
            encoder.write(&last)?;
            tick(k + 1)?;
        }
    }
    Ok(())
}

/// Grades a rendered frame on the CPU, as the shader does on the GPU.
/// Its LUT, then its effects, as the shader's `finish` does on the GPU.
fn grade_frame(frame: &mut stitch::Frame, camera: &view::Camera) {
    let (grade, lut) = (&camera.grade, camera.lut.map(|id| (lut::get(id), camera.grade.lut.clamp(0.0, 1.0))));
    if grade.is_neutral() && lut.is_none() {
        return;
    }
    let (w, h) = (frame.width, frame.height);
    let aspect = w as f64 / h as f64;
    frame.rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for (x, px) in row.chunks_exact_mut(3).enumerate() {
            // Pixel centres, as the shader's fragment positions.
            let at = [(x as f64 + 0.5) / w as f64 * 2.0 - 1.0, (y as f64 + 0.5) / h as f64 * 2.0 - 1.0];
            let mut rgb = [px[0], px[1], px[2]].map(|v| v as f64 / 255.0);
            if let Some((lut, strength)) = &lut {
                rgb = lut.blend(rgb, *strength);
            }
            let out = if grade.is_neutral() { rgb } else { grade.apply(rgb, at, aspect) };
            for (p, v) in px.iter_mut().zip(out) {
                *p = (v * 255.0).round() as u8;
            }
        }
    });
}

/// Replaces `path` with `bytes` whole or not at all: written to a new file
/// beside it, synced, then renamed over it. Written in place, a crash or a
/// full disk mid-save left a project (or the settings) cut short. The new
/// file keeps the old one's permissions, and a symlink's target is what
/// gets replaced, not the link.
pub fn replace_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        // create_new: never through a link someone left at that name.
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        if let Ok(meta) = std::fs::metadata(&path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}

fn save_png(path: &Path, f: &stitch::Frame) -> Result<()> {
    image::save_buffer(path, &f.rgb, f.width as u32, f.height as u32, image::ColorType::Rgb8).with_context(|| format!("writing {}", path.display()))
}

/// Fails with a plain reason when `input` is an ordinary video, which has
/// no `what` (only an Osmo 360's .OSV does).
fn needs_360(input: &Path, what: &str) -> Result<()> {
    if ffmpeg::flat(input)?.is_some() {
        bail!("{} is an ordinary video or picture: it has no {what}, only an Osmo 360 clip (.OSV) does", input.display());
    }
    Ok(())
}

fn info(input: &Path) -> Result<()> {
    if let Some(flat) = ffmpeg::flat(input)? {
        let info = if flat.picture {
            serde_json::json!({ "input": input, "kind": "picture", "size": flat.size() })
        } else {
            serde_json::json!({
            "input": input,
            "kind": "video",
            "duration": ffmpeg::duration(input)?,
            "fps": ffmpeg::frame_rate(input)?,
            "size": flat.size(),
            "recorded": ffmpeg::creation_time(input)?,
            "audio": ffmpeg::has_audio(input)?,
            })
        };
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(());
    }
    let telemetry = cache::telemetry(input, &|_| {})?;
    let quats = telemetry.quats;
    let info = serde_json::json!({
        "input": input,
        "kind": "osv",
        "colour": telemetry.color_mode.name(),
        "proxy": ffmpeg::proxy(input),
        "duration": ffmpeg::duration(input)?,
        "fps": ffmpeg::frame_rate(input)?,
        "lens_size": ffmpeg::dimensions(input, 0)?,
        "recorded": ffmpeg::creation_time(input)?,
        "audio": ffmpeg::has_audio(input)?,
        "attitude": !quats.is_empty(),
    });
    println!("{}", serde_json::to_string_pretty(&info)?);
    Ok(())
}

/// Tracks `colour` through `input` from `start` for `duration` seconds (to
/// the end if `None`), writing `Cmd::Track`'s CSV to `out`. `progress(done,
/// total)` hears of each sample and cancels by failing. Returns how many of
/// how many samples found it.
pub fn track_to(
    input: &Path,
    colour: &track::Colour,
    near: Option<track::Near>,
    start: Option<f64>,
    duration: Option<f64>,
    every: f64,
    out: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(usize, usize) -> Result<()>,
) -> Result<(usize, usize)> {
    // Small, unstabilised (so its directions are the rig's) and from the
    // proxy: enough to find a jacket, and fast.
    const WIDTH: usize = 512;
    let mut p = Pipeline::new(input, &StitchOpts::default(), stab::Mode::Off)?;
    if !p.stab.has_data() {
        bail!("{} has no attitude data to track with", input.display());
    }
    p.use_proxy();
    let (start, end) = p.span(start, duration, None)?;
    let total = ((end - start) / every).ceil().max(1.0) as usize;
    let mut stream = p.stream(start)?;
    let mut carry = Carry::default();
    let mut last: Option<(f64, [f64; 3])> = near.map(|near| {
        let d = match near {
            track::Near::Level(yaw, pitch) => track::from_level(&p.stab, start * 1000.0, p.readout_ms, yaw, pitch),
            track::Near::Rig(d) => d,
        };
        (start, d)
    });
    let (mut next, mut n, mut found, mut rows) = (start, 0usize, 0usize, 0usize);
    writeln!(out, "t,x,y,z,yaw,pitch,size")?;
    while let Some(frames) = stream.next()? {
        let t = start + n as f64 / p.fps;
        n += 1;
        if t > end {
            break;
        }
        if t + 0.5 / p.fps < next {
            continue;
        }
        next += every;
        rows += 1;
        let pic = p.render(&frames, t, &mut carry, &Shot::Equirect(WIDTH));
        // Look where it was last, carried through the camera's turning since;
        // after a few seconds lost, anywhere again.
        let guess = last.filter(|(at, _)| t - at < 3.0).map(|(at, d)| p.stab.to_rig(t * 1000.0, p.stab.to_world(at * 1000.0, d)));
        match track::find(&pic.rgb, WIDTH, colour, guess) {
            Some((d, share)) => {
                let (yaw, pitch) = track::level_angles(&p.stab, t * 1000.0, p.readout_ms, d);
                writeln!(out, "{t:.3},{:.5},{:.5},{:.5},{yaw:.1},{pitch:.1},{:.3}", d[0], d[1], d[2], share * 100.0)?;
                last = Some((t, d));
                found += 1;
            }
            None => writeln!(out, "{t:.3},,,,,,")?,
        }
        progress(rows.min(total), total)?;
    }
    Ok((found, rows))
}

/// `scan` of each clip under `inputs` into `dir/<clip>.csv`, `jobs` at once,
/// a line each on how it went; a clip that fails is reported and skipped.
fn scan_many(inputs: &[PathBuf], dir: &Path, jobs: usize, every: f64, image: bool) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let clips = osv_under(inputs)?;
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    eprintln!("scanning {} clips into {}", clips.len(), dir.display());
    let (next, failed) = (AtomicUsize::new(0), AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..jobs.clamp(1, clips.len().max(1)) {
            s.spawn(|| {
                while let Some(clip) = clips.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let at = std::time::Instant::now();
                    let name = clip.file_stem().map_or_else(|| clip.display().to_string(), |n| n.to_string_lossy().into_owned());
                    let out = dir.join(format!("{name}.csv"));
                    // To a temporary file first, so a failed scan leaves no half CSV.
                    let tmp = dir.join(format!(".{name}.csv.part"));
                    let result = needs_360(clip, "motion data").and_then(|_| {
                        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
                        scan(clip, every, image, &mut w)?;
                        std::io::Write::flush(&mut w)?;
                        drop(w);
                        std::fs::rename(&tmp, &out)?;
                        Ok(())
                    });
                    match result {
                        Ok(()) => println!("{name}: {} in {:.0} s", out.display(), at.elapsed().as_secs_f64()),
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            failed.fetch_add(1, Ordering::Relaxed);
                            println!("{name}: failed: {e:#}");
                        }
                    }
                }
            });
        }
    });
    let failed = failed.into_inner();
    eprintln!("{} scanned, {failed} failed", clips.len() - failed);
    Ok(())
}

/// One CSV row per `every` seconds into `out`; see `Cmd::Scan` and the header.
fn scan(input: &Path, every: f64, image: bool, out: &mut dyn std::io::Write) -> Result<()> {
    let skies = if image { skies(input)? } else { Vec::new() };
    let quats = cache::telemetry(input, &|_| {})?.quats;
    let duration = ffmpeg::duration(input)?;
    // The proxy carries the same sound (decoded, sample for sample) at a
    // sixteenth of the file: reading the clip's own from a network share
    // made a folder of clips take minutes.
    let sound = ffmpeg::proxy(input).filter(|p| ffmpeg::has_audio(p).unwrap_or(false)).unwrap_or_else(|| input.to_owned());
    let levels = if ffmpeg::has_audio(&sound)? { ffmpeg::audio_levels(&sound, every)? } else { Vec::new() };
    let stab = (!quats.is_empty()).then(|| stab::Stabilizer::new(&quats, stab::Mode::Off, stab::ImuAxes::X180, -1.0, 0.0));
    // Turning is sampled every 20 ms: fine enough for hand shake, coarse
    // enough that the attitude's held-then-jumping samples average out.
    const STEP: f64 = 0.02;
    writeln!(out, "t,heading,tilt,turn,rotation,shake,loudness,peak{}", if image { ",sky,skyline,open" } else { "" })?;
    let mut heading = None::<f64>;
    let rows = (duration / every).ceil() as usize;
    for n in 0..rows {
        let t = n as f64 * every;
        let mut level = levels.get(n).map_or(String::from(","), |(rms, peak)| format!("{rms:.1},{peak:.1}"));
        if image {
            // The keyframe nearest the row's middle, if there's one in it.
            let mid = t + every / 2.0;
            match skies.iter().filter(|(at, _)| (at - mid).abs() <= every.max(1.0) / 2.0).min_by(|a, b| (a.0 - mid).abs().total_cmp(&(b.0 - mid).abs())) {
                Some((_, s)) => level += &format!(",{:.1},{:.1},{:.0}", s.sky, s.skyline, s.open),
                None => level += ",,,",
            }
        }
        let Some(stab) = &stab else {
            writeln!(out, "{t:.2},,,,,,{level}")?;
            continue;
        };
        let poses: Vec<_> = (0..=(every / STEP).round() as usize).map(|k| stab.pose((t + k as f64 * STEP) * 1000.0)).collect();
        let (first, last) = (&poses[0], &poses[poses.len() - 1]);
        let path: f64 = poses.windows(2).map(|w| w[0].turn_to(&w[1])).sum();
        // Unwrapped, so a full turn reads 360 rather than jumping back.
        let mut h = first.heading;
        if let Some(prev) = heading {
            h = prev + (h - prev + 540.0).rem_euclid(360.0) - 180.0;
        }
        heading = Some(h);
        let turn = (last.heading - first.heading + 540.0).rem_euclid(360.0) - 180.0;
        let rotation = path / every;
        let shake = (path - first.turn_to(last)) / every;
        writeln!(out, "{t:.2},{h:.1},{:.1},{:.1},{rotation:.1},{shake:.1},{level}", first.tilt, turn / every)?;
    }
    Ok(())
}

/// The sky around the camera at the proxy's keyframes (about every 0.7 s),
/// level. Keyframes alone decode fast; the nearest one stands for a row.
fn skies(input: &Path) -> Result<Vec<(f64, sky::Sky)>> {
    const WIDTH: usize = 360;
    const LENS: usize = 256;
    // The seam corrections measure at full strip size whatever the output's;
    // at 1° a pixel their few pixels at the seam don't change the sky.
    let opts = StitchOpts { no_rs: true, no_align: true, no_cut: true, ..StitchOpts::default() };
    let p = Pipeline::new(input, &opts, stab::Mode::Horizon)?;
    let proxy = ffmpeg::proxy(input).context("scan --image reads the LRF proxy beside the clip, and there is none")?;
    let mut skies = Vec::new();
    ffmpeg::key_frames(&proxy, 0, (LENS * p.tracks.len(), LENS), ffmpeg::Out::default(), |t, frame| {
        let frames = ffmpeg::split_proxy(&frame, &p.tracks);
        let pic = p.render(&frames, t, &mut Carry::default(), &Shot::Equirect(WIDTH));
        skies.push((t, sky::measure(&pic.rgb, WIDTH)));
        true
    })?;
    Ok(skies)
}

fn seam_report(input: &Path, start: f64, duration: f64, dump: Option<&Path>) -> Result<()> {
    let opts = StitchOpts { no_rs: true, ..StitchOpts::default() };
    let p = Pipeline::new(input, &opts, stab::Mode::Off)?;
    let aligner = p.aligner.as_ref().context("not a two-lens rig")?;
    let start = p.snap(start);
    let mut stream = p.stream(start)?;
    let (mut eased, mut prev_raw): (Option<align::Field>, Option<align::Field>) = (None, None);
    let mut sums = [0.0; 7];
    // The selfie stick runs down the rig's y axis.
    let stick = aligner.column([0.0, 1.0, 0.0]);
    let mut n = 0;
    let jitter = |a: &align::Field, b: &align::Field| a.shift.iter().zip(&b.shift).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / a.shift.len() as f64;
    println!("t,mismatch_none,mismatch_raw,mismatch_eased,jitter_raw_deg,jitter_eased_deg,stick_none,stick_eased");
    while n < (duration * p.fps).round() as usize {
        let Some(frames) = stream.next()? else { break };
        let strips = aligner.strips(|i, u, v| Some(p.lenses[i].sample(&frames, u, v).iter().sum::<f64>() / (3.0 * 255.0)));
        let raw = aligner.measure(&strips, eased.as_ref());
        let before = eased.clone();
        let e = eased.get_or_insert_with(|| raw.clone());
        e.ease(&raw, align::EASE);
        let row = [
            aligner.mismatch(&strips, &align::Field::default(), None),
            aligner.mismatch(&strips, &raw, None),
            aligner.mismatch(&strips, e, None),
            prev_raw.as_ref().map_or(0.0, |r| jitter(r, &raw).to_degrees()),
            before.as_ref().map_or(0.0, |b| jitter(b, e).to_degrees()),
            aligner.mismatch(&strips, &align::Field::default(), Some(stick)),
            aligner.mismatch(&strips, e, Some(stick)),
        ];
        println!("{:.3},{:.5},{:.5},{:.5},{:.4},{:.4},{:.5},{:.5}", start + n as f64 / p.fps, row[0], row[1], row[2], row[3], row[4], row[5], row[6]);
        for (s, v) in sums.iter_mut().zip(row) {
            *s += v;
        }
        if n == 0
            && let Some(dir) = dump
        {
            aligner.dump(&strips, &raw, dir)?;
            eprintln!("rig down is strip column {}, rig up column {}", aligner.column([0.0, 1.0, 0.0]), aligner.column([0.0, -1.0, 0.0]));
        }
        prev_raw = Some(raw);
        n += 1;
    }
    let m = sums.map(|s| s / n.max(1) as f64);
    eprintln!(
        "{n} frames: mismatch none {:.5} raw {:.5} eased {:.5}; jitter raw {:.4}° eased {:.4}°; stick zone none {:.5} eased {:.5}",
        m[0], m[1], m[2], m[3], m[4], m[5], m[6]
    );
    Ok(())
}

fn load_input(input: &Path, progress: &dyn Fn(f64)) -> Result<telemetry_parser::Input> {
    let mut f = File::open(input)?;
    let size = f.metadata()?.len() as usize;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok(telemetry_parser::Input::from_stream(&mut f, size, input, progress, cancel)?)
}
