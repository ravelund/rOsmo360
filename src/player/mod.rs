//! Real-time 360 player: an iced window with the stitched view, an open-file
//! dialog and playback controls. Free look on all three axes, stabilisation
//! and rolling-shutter correction run in the view's shader.

mod audio;
mod edit;
mod export;
mod gpu;
mod audio_panel;
mod export_panel;
mod handles;
mod info;
mod settings_panel;
mod meter;
mod playback;
pub mod render;
mod settings;
mod source;
mod thumbs;
mod effects;
pub mod music;
mod timeline;
mod divider;

use crate::ffmpeg::{self, Hwaccel};
use crate::stab::{ImuAxes, Mode, Stabilizer};
use crate::project::{Cut, Project};
use crate::stitch::M3;
use crate::view::{Camera, Ease, Follow, Keyframes, Projection, View};
use anyhow::{Context, Result};
use gpu::{Rig, Shown};
use iced::keyboard::{self, Key, key::Named};
use iced::mouse;
use iced::widget::{button, canvas, center, column, progress_bar, container, image, pick_list, responsive, row, shader, slider, space, stack, svg, text, toggler, tooltip};
use iced::window;
use iced::{Alignment, Element, Length, Point, Rectangle, Size, Subscription, Task, Theme};
use playback::Playback;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct Media {
    pub input: PathBuf,
    /// Side-by-side proxy, if the clip has one.
    pub proxy: Option<PathBuf>,
    /// Stream the OSV at this size instead of the proxy.
    pub osv_size: Option<u32>,
    /// Pixels square of the OSV's lens tracks.
    pub osv_full: u32,
    pub hw: Hwaccel,
    pub has_audio: bool,
    pub rig: Arc<Rig>,
    pub stab: Stabilizer,
    pub readout_ms: f64,
    pub fps: f64,
    pub duration: f64,
    /// An ordinary video (`ffmpeg::flat`), not a 360 clip.
    pub flat: Option<ffmpeg::Flat>,
    /// The colour profile it was filmed in: D-Log M gets its LUT.
    pub color_mode: crate::dji::ColorMode,
}

impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media").field("input", &self.input).finish_non_exhaustive()
    }
}

/// What the command line asked for; what it leaves out comes from the
/// remembered settings.
pub struct Launch {
    pub osv_size: Option<u32>,
    pub hw: Option<Hwaccel>,
    pub stabilize: Option<Mode>,
    pub imu_axes: ImuAxes,
    pub world_z_down: bool,
}

/// Settings for loading and playing clips; the GUI updates them so the
/// next clip opens the way the last one was left.
#[derive(Clone)]
pub struct Options {
    pub osv_size: Option<u32>,
    /// Forced decoder, or the one found by probing the first Osmo clip.
    pub hw: Option<Hwaccel>,
    /// `hw` was asked for (`--hwaccel`), not found.
    pub hw_forced: bool,
    pub stabilize: Mode,
    pub imu_axes: ImuAxes,
    pub world_z_down: bool,
}

/// Caches `input` the way opening it in the player would: its telemetry and
/// timeline thumbnails (`rosmo360 cache add`).
pub fn cache_clip(input: &Path) -> Result<()> {
    let opts = Options { osv_size: None, hw: Some(Hwaccel::Software), hw_forced: true, stabilize: Mode::Horizon, imu_axes: ImuAxes::X180, world_z_down: false };
    thumbs::make(&Media::load(input.to_owned(), &opts, &|_| {})?, |_, _| true)
}

pub use settings::config_dir;

impl Media {
    /// `progress` hears how far reading the clip's telemetry has got (0..1),
    /// which is most of the time a clip takes to load.
    fn load(input: PathBuf, opts: &Options, progress: &dyn Fn(f64)) -> Result<Media> {
        let stitch = crate::StitchOpts { stabilize: Some(opts.stabilize), imu_axes: opts.imu_axes, world_z_down: opts.world_z_down, ..Default::default() };
        let p = crate::Pipeline::load(&input, &stitch, opts.stabilize, progress)?;
        let proxy = if p.flat.is_some() { None } else { ffmpeg::proxy(&input) };
        // An ordinary video or picture has its own (`Pipeline::load_flat`):
        // a GPU decoder takes no JPEG, and a picture loaded first made every
        // later OSV decode in software, crawling at 1920 px.
        let hw = match opts.hw {
            _ if p.flat.is_some() && !opts.hw_forced => p.hw,
            Some(hw) => hw,
            None => {
                let hw = Hwaccel::pick(&input, p.tracks[0])?;
                eprintln!("decoding with {hw:?}");
                hw
            }
        };
        Ok(Media {
            duration: ffmpeg::duration(&input)?,
            osv_full: match p.flat {
                Some(f) => f.size().0.max(f.size().1),
                None => ffmpeg::dimensions(&input, p.tracks[0])?.0 as u32,
            },
            has_audio: ffmpeg::has_audio(&input)?,
            proxy,
            osv_size: opts.osv_size,
            hw,
            rig: Arc::new(match p.flat {
                Some(flat) => Rig::flat(p.tracks[0], flat.hdr),
                None => Rig::new(p.lenses, p.tracks, false),
            }),
            stab: p.stab,
            readout_ms: p.readout_ms,
            fps: p.fps,
            flat: p.flat,
            color_mode: p.color_mode,
            input,
        })
    }
}

pub fn run(inputs: Vec<PathBuf>, launch: Launch) -> Result<()> {
    // Finding the system's fonts takes a moment: done ahead, the font list
    // opens at once.
    std::thread::spawn(crate::text::families);
    let settings = settings::Settings::load();
    let opts = Options {
        osv_size: launch.osv_size.or(settings.osv_size),
        hw: launch.hw,
        hw_forced: launch.hw.is_some(),
        stabilize: launch.stabilize.unwrap_or(settings.stabilize),
        imu_axes: launch.imu_axes,
        world_z_down: launch.world_z_down,
    };
    iced::application(
        move || {
            let mut app = App::new(opts.clone(), settings.clone());
            let is_project = |p: &PathBuf| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
            let task = match inputs.iter().find(|p| is_project(p)) {
                Some(project) => app.update(Message::ProjectPicked(Some(project.clone()))),
                None => {
                    app.queue.extend(inputs.iter().map(|p| (p.clone(), true)));
                    app.open_next()
                }
            };
            (app, task)
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(|app: &App| app.theme.clone())
    .window(window::Settings {
        // Wide enough for the top bar and the transport row (at 1280 the
        // ends of both were cut off).
        size: iced::Size::new(1640.0, 900.0),
        // Closing asks first when the edit has unsaved changes.
        exit_on_close_request: false,
        // Wayland matches the window to its menu entry and icon by this id: in a
        // Flatpak that's the Flatpak's own (io.github.ravelund.rOsmo360).
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: std::env::var("FLATPAK_ID").unwrap_or_else(|_| NAME.to_owned()),
            ..Default::default()
        },
        // Windows takes the title bar's and taskbar's icon from the window,
        // not the .exe's resource (which Explorer shows).
        #[cfg(windows)]
        icon: window::icon::from_file_data(include_bytes!("../../packaging/rosmo360.png"), None).ok(),
        ..Default::default()
    })
    .run()
    .context("running the player window")
}

/// What "save" writes: the view as framed, or the whole sphere.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Saving {
    View,
    Sphere,
}

/// Longest side of a saved view, and width of a saved sphere: the lenses'
/// full resolution holds about this much detail.
const SAVE_VIEW_PX: f32 = 3840.0;
const SAVE_SPHERE_PX: usize = 7680;

/// The panels that dock at the right of the view, one at a time.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Effects,
    Export,
    Audio,
}

/// The shape of the view, and so of a saved view: free fills the window.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Aspect {
    Free,
    Ratio(u16, u16),
}

impl Aspect {
    const ALL: [Aspect; 6] = [Aspect::Free, Aspect::Ratio(16, 9), Aspect::Ratio(9, 16), Aspect::Ratio(4, 3), Aspect::Ratio(1, 1), Aspect::Ratio(2, 1)];

    /// Width over height, if locked.
    fn ratio(self) -> Option<f32> {
        match self {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(w as f32 / h as f32),
        }
    }
}

impl fmt::Display for Aspect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Aspect::Free => f.write_str("Free"),
            Aspect::Ratio(w, h) => write!(f, "{w}:{h}"),
        }
    }
}

/// What the player streams while playing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Source {
    Proxy,
    Osv(u32),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Proxy => f.write_str("Proxy (LRF)"),
            Source::Osv(s) => write!(f, "OSV {s} px"),
        }
    }
}

/// What waits on the edit being saved or let go.
#[derive(Debug, Clone, Copy)]
enum Then {
    Close,
    OpenProject,
    NewProject,
}

/// An answer to "save changes first?".
#[derive(Debug, Clone, Copy)]
enum Choice {
    Save,
    Discard,
    Cancel,
}

/// The playing cut's clip: its playback plus the frame the view draws.
struct Clip {
    playback: Playback,
    shown: Option<Arc<Shown>>,
}

#[derive(Debug, Clone)]
enum Message {
    Open,
    /// Clips to add to the edit, in order.
    Picked(Vec<PathBuf>),
    OpenProject,
    ProjectPicked(Option<PathBuf>),
    /// The window's close button was pressed.
    CloseRequested,
    /// The answer to "stop the export (or tracking) and quit?".
    StopAndClose(bool),
    /// The answer to "save changes first?" before doing `.0`.
    Confirmed(Then, Choice),
    /// Save the project; to a new file if true or never saved.
    SaveProject(bool),
    SaveProjectTo(Option<PathBuf>),
    Resolution(crate::Resolution),
    /// Export each cut to its own file.
    ExportClips(bool),
    /// Grade those files with the cuts' effects.
    ExportGraded(bool),
    ExportFormat(ffmpeg::Format),
    /// Open or close the Export panel.
    ExportPanel,
    /// Open or close the Audio panel.
    AudioPanel,
    /// Open or close the Settings panel.
    SettingsPanel,
    AutoKey(bool),
    /// The divider above the transport row is this many pixels below
    /// where it was pressed (the lanes shrink), was let go, or was
    /// double-clicked (back to 1).
    SplitDrag(f32),
    SplitDone,
    SplitReset,
    Theme(Theme),
    TenBit(bool),
    /// Add a `.cube` to the LUT library (a file dialog), the file picked,
    /// take one out, and choose the one D-Log M clips get.
    AddLut,
    LutPicked(Option<PathBuf>),
    RemoveLut(String),
    DlogMLut(effects::LutChoice),
    /// A drag on the view let go.
    LookDone,
    /// Open the Info panel on a file on the timeline (right-click).
    Info(info::Of),
    /// What was read about a file for the Info panel.
    InfoRead(PathBuf, Result<Arc<info::File>, String>),
    InfoClose,
    /// Show a file in the file manager.
    Reveal(PathBuf),
    CopyPath(PathBuf),
    /// Export the edit: ask where (a file, or a folder for the clips), then
    /// render in the background.
    Export,
    ExportTo(Option<PathBuf>),
    /// Fraction of the export done.
    ExportProgress(f32),
    /// The files written.
    ExportDone(Result<Vec<PathBuf>, String>),
    ExportCancel,
    /// Starts (or stops) drawing a box over a subject to track its colour.
    PickColour,
    /// Pick a song for the music track.
    AddMusic,
    MusicPicked(Option<PathBuf>),
    /// A song's waveform, read off the UI thread.
    Waveform(PathBuf, Result<Arc<music::Waveform>, String>),
    /// An overlay's picture ready (by `Item::key`), and the font its text
    /// missed.
    OverlayPicture(String, Result<(Arc<crate::overlay::Picture>, Option<String>), String>),
    /// Add a text overlay at the playhead.
    AddText,
    /// Add pictures to the overlay lane at the playhead.
    AddOverlay,
    OverlayPicked(Vec<PathBuf>),
    OverlaySelect(Option<crate::overlay::ItemRef>),
    /// An overlay dragged to start at edit time `at`.
    OverlayMove(crate::overlay::ItemRef, f64),
    /// An overlay's in (`true`) or out edge dragged to own time `t`.
    OverlayTrim(crate::overlay::ItemRef, bool, f64),
    /// To an overlay keyframe's edit time.
    OverlayJump(f64),
    /// An overlay's effect keyframe (`true`) or motion keyframe dragged to
    /// another time in the item's own.
    OverlayKeyMove(crate::overlay::ItemRef, bool, usize, f64),
    SelectMusic(usize),
    /// Music clip `i` dragged to start at this edit time; with Shift held
    /// (`true`) it may overlap a neighbour, crossfading.
    MusicMove(usize, f64, bool),
    /// Music clip `i`'s in (`true`) or out edge dragged to this song time,
    /// with Shift held or not.
    MusicTrim(usize, bool, f64, bool),
    /// A music volume point added at this edit time and level (dB).
    VolumeAdd(f64, f64),
    VolumeSelect(usize),
    /// Music volume point `i` dragged to this edit time and level.
    VolumeMove(usize, f64, f64),
    /// A sound point on cut `cut` at this clip time and level (dB).
    SoundKeyAdd(usize, f64, f64),
    SoundKeySelect(usize, usize),
    /// Cut `cut`'s sound point dragged to this clip time and level.
    SoundKeyMove(usize, usize, f64, f64),
    /// The mix: the clips' sound and the music track, in dB.
    SoundLevel(f64),
    MusicLevel(f64),
    /// A level slider let go.
    LevelDone,
    /// Start an empty edit (asking first if there are unsaved changes).
    NewProject,
    /// A redraw while clips load, for the loading bar.
    LoadingTick,
    /// The box being drawn, in fractions of the view.
    PickArea(Rectangle),
    /// The box drawn, and the view's width over its height.
    AreaPicked(Rectangle, f64),
    TrackProgress(f32),
    TrackCancel,
    Tracked(Result<TrackDone, String>),
    /// Never cloned in flight, so the clip can be taken back out of the `Arc`.
    Loaded(Arc<Result<Media, String>>),
    Tick,
    Key(Key, keyboard::Modifiers),
    Toggle,
    Scrub(f64),
    ScrubDone,
    /// Add a keyframe with the current view at the playhead, or update the
    /// one there.
    KeyAdd,
    /// Delete the keyframe at the playhead.
    KeyDelete,
    /// Zoom the timeline by a factor, keeping edit time `.1` (the playhead
    /// if `None`) where it is on screen.
    TimelineZoom(f64, Option<f64>),
    /// Zoom the timeline to slider position `.0` (0 whole edit, 1 closest).
    TimelineZoomTo(f64),
    /// Scroll the timeline so it starts at edit time `.0`.
    TimelinePan(f64),
    /// A thumbnail of clip `.0` at clip time `.1`.
    Thumb(PathBuf, f64, image::Handle),
    /// How the keyframe at the playhead moves on to the next.
    KeyEase(Ease),
    KeyFollow(Aiming),
    /// Jump to keyframe `.1` of cut `.0`.
    KeyJump(usize, usize),
    /// Drag keyframe `.1` of cut `.0` to clip time `.2`.
    KeyMove(usize, usize, f64),
    Select(usize),
    /// Drag cut `.0`'s in (`.1` true) or out point to clip time `.2`.
    Trim(usize, bool, f64),
    /// Move cut `.0` to position `.1`.
    MoveCut(usize, usize),
    /// Split the cut at the playhead.
    Split,
    /// Cut `i`'s crossfade from the cut before dragged to this many seconds.
    Crossfade(usize, f64),
    /// Give the selected (or playing) cut a 1 s crossfade, or take its away.
    ToggleCrossfade,
    /// From the Effects panel.
    Effect(effects::EffectMsg),
    /// Delete the selected cut, or the selected music clip.
    DeleteCut,
    Undo,
    Redo,
    Look { yaw: f64, pitch: f64, roll: f64 },
    /// Scroll notches; positive zooms out.
    Zoom(f64),
    Fov(f64),
    Stab(Mode),
    Projection(Projection),
    Source(Source),
    RollingShutter(bool),
    FullOnPause(bool),
    Mute,
    Volume(f32),
    Aspect(Aspect),
    /// The 360 view's size on screen, so a saved view keeps its shape.
    ViewSize(Size),
    Save(Saving),
    SaveTo(Saving, Option<PathBuf>),
    Saved(Result<PathBuf, String>),
    ResetView,
    Fullscreen,
}

struct App {
    opts: Options,
    /// Tracks that keyframes follow, read once each (`None`: unreadable).
    followed: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<Arc<crate::track::Track>>>>,
    /// The colour theme (the Settings panel's; Kanagawa Dragon unless chosen).
    theme: Theme,
    logo: image::Handle,
    /// Every clip opened so far, ready for the cuts that use it.
    library: Vec<Media>,
    /// Each clip's timeline thumbnails by clip time, as they arrive.
    thumbs: std::collections::HashMap<PathBuf, Vec<(f64, image::Handle)>>,
    /// Songs' waveforms, and the music clip selected on its lane.
    waveforms: std::collections::HashMap<PathBuf, Arc<music::Waveform>>,
    /// The mix's level as it plays.
    meter: meter::Meter,
    /// The overlays' pictures, scaled for the view: `None` while loading.
    overlay_pictures: std::collections::HashMap<String, Option<(u64, Arc<crate::overlay::Picture>)>>,
    /// Their timeline thumbnails.
    overlay_thumbs: std::collections::HashMap<String, iced::widget::image::Handle>,
    /// The selected text overlay's words as edited in the panel, and which
    /// overlay they're of.
    text_content: iced::widget::text_editor::Content,
    text_of: Option<crate::overlay::ItemRef>,

    /// The overlay or text item selected, in the lane shown of its track.
    selected_overlay: Option<crate::overlay::ItemRef>,
    selected_music: Option<usize>,
    /// The music volume point selected, and the cut sound point: (cut,
    /// point).
    selected_volume: Option<usize>,
    selected_sound: Option<(usize, usize)>,
    music: music::Player,
    /// Through a crossfade, the other cut of it alongside: its index and
    /// clip. Usually the next one, coming in over the playing cut; while
    /// paused with the incoming cut selected for editing, that one is the
    /// current cut and this is the one before (`follow_crossfade`).
    partner: Option<(usize, Clip)>,
    edit: edit::Edit,
    /// The cut being played, and the one selected on the timeline.
    current: usize,
    selected: Option<usize>,
    /// Clips waiting to be loaded, in order, and whether each is added as a
    /// cut (opened) or only loaded for a project's cuts.
    queue: std::collections::VecDeque<(PathBuf, bool)>,
    /// Whether the clip loading now is added as a cut.
    loading_adds: bool,
    /// The next cut's playback, primed in the last `PREROLL` seconds of the
    /// one playing, with the cut it was made for: its clip and in point, so
    /// one made stale by an edit is never used.
    preroll: Option<(usize, PathBuf, f64, Playback)>,
    /// Clips loaded since the queue was last empty, and how far the one
    /// loading now has got (an f64's bits, set from its loading thread):
    /// together with the queue, a project's loading progress.
    loaded: usize,
    loading_part: Arc<std::sync::atomic::AtomicU64>,
    /// The project file, once opened or saved, and the edit version saved.
    project: Option<PathBuf>,
    saved_version: u64,
    /// The aspect lock when last saved: it's in the project, so changing it
    /// is a change to save.
    saved_aspect: Aspect,
    /// What to do once the project has been saved.
    after_save: Option<Then>,
    /// Quitting once the export or tracking stopped for it has cleaned up.
    closing: bool,
    /// Exports are this many pixels on the short side.
    resolution: crate::Resolution,
    /// Export each cut to its own file, and in which format.
    export_clips: bool,
    export_graded: bool,
    export_format: ffmpeg::Format,
    /// A running export: its cancel flag and how far it has got.
    exporting: Option<(Arc<std::sync::atomic::AtomicBool>, f32)>,
    /// A trim or keyframe drag has saved the edit for undo already.
    dragging: bool,
    /// Timeline zoom (1 shows the whole edit) and the edit time at its left
    /// edge; `timeline_window` keeps them in range.
    timeline_zoom: f64,
    timeline_start: f64,
    clip: Option<Clip>,
    /// The picture as it was drawn when the playhead crossed into another
    /// cut, kept until that cut's first frame arrives. Drawing the old frame
    /// with the new cut's stabilisation and keyframes showed it at a wrong
    /// angle for a moment, like a ghost of the previous clip.
    held: Option<Sphere>,
    loading: Option<PathBuf>,
    error: Option<String>,
    /// The free-look view: what is shown unless following keyframes.
    look: View,
    /// The free look kept for the other kind of clip (360 or ordinary
    /// video), swapped in when the playing cut is of that kind: angles mean
    /// another thing there, and a little planet's would push a video's
    /// picture off screen.
    other_look: View,
    /// The cut (index and clip) looked around in by hand since the view last
    /// followed keyframes: one without keyframes of its own keeps that view
    /// (`keep_hand_view`).
    hand_cut: Option<(usize, PathBuf)>,
    /// Whether `look` belongs to ordinary videos.
    look_flat: bool,
    /// Show the keyframes' view rather than `look`. Playing, seeking and
    /// keyframe edits turn it on; looking around by hand turns it off.
    follow: bool,
    rolling_shutter: bool,
    /// Show a full-resolution frame when paused.
    full_on_pause: bool,
    /// Looking around while paused keys the view at the playhead (the
    /// Settings panel).
    auto_key: bool,
    /// The cut and clip time automatic keyframing is writing to, so one
    /// look-around there is one undo step; `None` once it's over (the mouse
    /// let go, or anything else done).
    auto_key_at: Option<(usize, f64)>,
    /// The timeline lanes' scale as the divider has it now, and as last let
    /// go, which is what's saved: a drag would otherwise write the settings
    /// file at every step.
    split: f32,
    split_kept: f32,
    /// The Settings panel is open (at the left).
    settings_open: bool,
    /// 10-bit tracks decode at 10 bits (`source::deep`).
    ten_bit: bool,
    /// The library LUT new D-Log M cuts get, and the library's LUTs.
    dlog_m_lut: Option<String>,
    luts: Vec<String>,
    fullscreen: bool,
    volume: f32,
    muted: bool,
    view_size: Size,
    aspect: Aspect,
    /// What the last save did, or that one is running.
    save_status: Option<String>,
    /// Picking a colour to track: `Some(None)` until a box is drawn, then
    /// the box in fractions of the view.
    picking: Option<Option<Rectangle>>,
    /// The box picks a white balance, not a colour to track.
    picking_white: bool,
    /// The Effects panel shows beside the view.
    /// The panel docked at the right of the view, if one is open.
    side: Option<Side>,
    /// The Info panel (at the left, in the Text panel's place).
    info: Option<info::Info>,
    /// A running colour track: its cancel flag and progress, 0..1.
    tracking: Option<(Arc<std::sync::atomic::AtomicBool>, f32)>,
    /// The settings as last written, to save only when they change.
    saved_settings: settings::Settings,
}

impl App {
    fn new(opts: Options, saved: settings::Settings) -> Self {
        audio::set_listening(if saved.muted { 0.0 } else { saved.volume });
        source::set_deep(saved.ten_bit);
        App {
            opts,
            followed: Default::default(),
            theme: settings_panel::theme_named(&saved.theme),
            ten_bit: saved.ten_bit,
            dlog_m_lut: saved.dlog_m_lut.clone(),
            luts: crate::lut::library_names(),
            logo: image::Handle::from_bytes(LOGO),
            library: Vec::new(),
            thumbs: Default::default(),
            waveforms: Default::default(),
            meter: Default::default(),
            overlay_pictures: Default::default(),
            overlay_thumbs: Default::default(),
            text_content: Default::default(),
            text_of: None,

            selected_overlay: None,
            selected_music: None,
            selected_volume: None,
            selected_sound: None,
            music: Default::default(),
            partner: None,
            edit: edit::Edit::default(),
            current: 0,
            selected: None,
            dragging: false,
            timeline_zoom: 1.0,
            timeline_start: 0.0,
            queue: Default::default(),
            loading_adds: true,
            loaded: 0,
            loading_part: Arc::default(),
            preroll: None,
            project: None,
            saved_version: 0,
            saved_aspect: Aspect::Free,
            after_save: None,
            closing: false,
            resolution: saved.resolution,
            export_clips: saved.export_clips,
            export_graded: saved.export_graded,
            export_format: saved.export_format,
            exporting: None,
            clip: None,
            held: None,
            loading: None,
            error: None,
            look: View::default(),
            other_look: View::home(Projection::Perspective),
            hand_cut: None,
            look_flat: false,
            follow: true,
            rolling_shutter: saved.rolling_shutter,
            full_on_pause: saved.full_on_pause,
            auto_key: saved.auto_key,
            auto_key_at: None,
            split: saved.timeline_scale,
            split_kept: saved.timeline_scale,
            settings_open: false,
            fullscreen: false,
            volume: saved.volume,
            muted: saved.muted,
            view_size: Size::new(16.0, 9.0),
            aspect: saved.aspect.map_or(Aspect::Free, |[w, h]| Aspect::Ratio(w, h)),
            save_status: None,
            picking: None,
            picking_white: false,
            side: None,
            info: None,
            tracking: None,
            saved_settings: saved,
        }
    }

    /// The settings to remember, as they are now.
    fn settings(&self) -> settings::Settings {
        settings::Settings {
            stabilize: self.opts.stabilize,
            osv_size: self.opts.osv_size,
            rolling_shutter: self.rolling_shutter,
            full_on_pause: self.full_on_pause,
            volume: self.volume,
            muted: self.muted,
            resolution: self.resolution,
            aspect: match self.aspect {
                Aspect::Free => None,
                Aspect::Ratio(w, h) => Some([w, h]),
            },
            export_clips: self.export_clips,
            export_graded: self.export_graded,
            export_format: self.export_format,
            auto_key: self.auto_key,
            theme: self.theme.to_string(),
            ten_bit: self.ten_bit,
            dlog_m_lut: self.dlog_m_lut.clone(),
            timeline_scale: self.split_kept,
        }
    }

    /// Handles `message`, then saves the settings if it changed any.
    fn update(&mut self, message: Message) -> Task<Message> {
        let before = if self.look_flat { (self.other_look, self.look) } else { (self.look, self.other_look) };
        // What may look around (a key may turn the view); a let-go drag or
        // anything else done ends a look-around's undo step. Frames and
        // the view's size go by without ending it.
        let looking = matches!(message, Message::Look { .. } | Message::Zoom(_) | Message::Fov(_) | Message::ResetView | Message::Key(..));
        if !looking && !matches!(message, Message::Tick | Message::LoadingTick | Message::ViewSize(_)) {
            self.auto_key_at = None;
        }
        let task = self.handle(message);
        if looking {
            self.auto_key(before);
        }
        self.keep_hand_view(before);
        let task = Task::batch([task, self.load_overlay_pictures()]);
        self.settle_look();
        self.settle_text();
        self.follow_crossfade();
        self.follow_music();
        let now = self.settings();
        if now != self.saved_settings {
            now.save();
            self.saved_settings = now;
        }
        task
    }

    /// Keeps the music in step with the edit: playing where playback is, at
    /// its level, or stopped while playback is.
    fn follow_music(&mut self) {
        let (playing, t) = match &self.clip {
            // Only while the clock runs: waiting for its first frame it stands
            // still, and music started then took it as drift and restarted
            // over and over until the picture came.
            Some(c) if self.current < self.edit.cuts.len() => (c.playback.running(), self.edit.edit_time(self.current, c.playback.now())),
            _ => (false, 0.0),
        };
        self.music.follow(&self.edit.music, t, self.edit.duration(), playing);
        self.meter.follow(playing);
        // The cut's own volume points move its level as it plays, and
        // through a crossfade the two cuts trade places, equal power.
        let shares = self.crossfade_shares();
        let quarter = |x: f64| (x.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2).sin() as f32;
        let v = self.clip_volume() * shares.map_or(1.0, |(mine, _)| quarter(mine));
        if let Some(c) = &mut self.clip {
            c.playback.set_volume(v);
        }
        if let (Some((_, theirs)), Some((i, inc))) = (shares, &self.partner) {
            let points = self.edit.cuts.get(*i).map_or(0.0, |c| crate::project::volume_at(&c.volume, inc.playback.now()));
            let v = music::gain(self.edit.sound.volume + points) * quarter(theirs);
            if let Some((_, inc)) = &mut self.partner {
                inc.playback.set_volume(v);
            }
        }
    }

    /// Keeps the cut crossfading in alongside the playing one wherever
    /// playback is in a crossfade (played into, sought, scrubbed or paused
    /// there): at the matching time in its own clip, playing or paused with
    /// it. Elsewhere there's none.
    fn follow_crossfade(&mut self) {
        // Played, the outgoing cut leads again, as `tick` expects.
        if self.clip.as_ref().is_some_and(|c| c.playback.playing) && self.partner.as_ref().is_some_and(|(j, _)| j + 1 == self.current) {
            self.swap_partner();
            if let Some(c) = &mut self.clip {
                c.playback.scrub_end();
                c.playback.play();
            }
        }
        let want = (|| {
            let (cut, t) = self.clip_now()?;
            let playing = self.clip.as_ref()?.playback.playing;
            // The outgoing cut playing or paused in its crossfade: the next
            // cut alongside.
            if let Some(from) = self.edit.crossfade_from(self.current)
                && t >= from
                && t < cut.end
            {
                let next = &self.edit.cuts[self.current + 1];
                return Some((self.current + 1, next.input.clone(), next.start + (t - from), playing));
            }
            // The incoming cut paused in it (taken over to edit): the one
            // before alongside, shown when it's not selected.
            if !playing && self.current > 0 && t < cut.start + cut.crossfade {
                let before = &self.edit.cuts[self.current - 1];
                return Some((self.current - 1, before.input.clone(), before.end - cut.crossfade + (t - cut.start), false));
            }
            None
        })();
        let Some((i, input, t, playing)) = want else {
            self.partner = None;
            return;
        };
        match &mut self.partner {
            Some((j, inc)) if *j == i && inc.playback.media.input == input => {
                let p = &mut inc.playback;
                // Primed to catch up: started as the edit gets there.
                if let Some(at) = p.go_at {
                    if !playing {
                        p.go_at = None;
                        p.pause();
                    } else if t >= at - 0.5 / p.media.fps {
                        p.go();
                    }
                    return;
                }
                // Played from paused, primed ahead as a resync is: played
                // from its paused frame, it came up behind and froze for the
                // 1-2 s of a resync.
                if p.playing != playing {
                    if !playing {
                        p.pause();
                    } else {
                        inc.playback = p.primed_again(t + Self::catch_up(p.startup()));
                        return;
                    }
                }
                // Playing, the two clocks run side by side, once it's running:
                // waiting for its first frame (a second or more for the OSV)
                // its clock stands still, and taking that for drift restarted
                // it over and over. Drifted, it's primed again further on, by
                // its decoder's start-up and some: jumped to where the edit
                // was, it came up as far behind as its start took and jumped
                // again, showing nothing (OSV 1920 px, 1.1-1.4 s). Paused, it
                // follows the playhead as the playing cut does when scrubbed:
                // a frame at a time, skipping those overtaken (a jump each
                // time started decoders faster than they ended).
                if playing {
                    if p.running() && (p.now() - t).abs() > 0.25 {
                        inc.playback = p.primed_again(t + Self::catch_up(p.startup()));
                    }
                } else if (p.target() - t).abs() > 1.0 / p.media.fps {
                    p.scrub(t);
                }
            }
            // Played into: the cut readied ahead, if it was.
            _ if playing && t - self.edit.cuts[i].start < 0.25 && self.preroll.as_ref().is_some_and(|(j, inp, at, _)| (*j, inp, *at) == (i, &input, self.edit.cuts[i].start)) => {
                let (_, _, _, mut primed) = self.preroll.take().unwrap();
                primed.go();
                self.partner = Some((i, Clip { playback: primed, shown: None }));
            }
            _ => {
                let Some(media) = self.library.iter().find(|m| m.input == input) else { return };
                let mut media = media.clone();
                media.osv_size = self.opts.osv_size;
                media.stab.set_mode(self.edit.cuts[i].stabilize.unwrap_or(self.opts.stabilize));
                // Playing, primed ahead to start as the edit gets there, as
                // a resync is.
                let playback = if playing {
                    let mut p = Playback::primed(media, t + Self::catch_up(0.0), 0.0, self.full_on_pause);
                    p.go_at = Some(p.target());
                    p
                } else {
                    let mut p = Playback::new(media, 0.0, self.full_on_pause);
                    p.jump(t, false);
                    p
                };
                self.partner = Some((i, Clip { playback, shown: None }));
            }
        }
    }

    /// How far ahead of the edit a crossfade's partner is primed to catch
    /// up, for a decoder that took `startup` seconds to start last time:
    /// with room for it to take longer (it varied 1.1-1.4 s), at least half
    /// a second.
    fn catch_up(startup: f64) -> f64 {
        // Not measured yet: about an OSV's at 1920 px.
        let startup = if startup > 0.0 { startup } else { 1.0 };
        (startup * 1.5).max(0.5)
    }

    /// The clips' sound in the mix: its level and the playing cut's volume
    /// points where it is (the listening volume is `audio::set_listening`'s).
    fn clip_volume(&self) -> f32 {
        let points = match (&self.clip, self.edit.cuts.get(self.current)) {
            (Some(c), Some(cut)) => crate::project::volume_at(&cut.volume, c.playback.now()),
            _ => 0.0,
        };
        music::gain(self.edit.sound.volume + points)
    }

    /// Reads `path`'s waveform for the music lane, unless it has it.
    fn load_waveform(&self, path: &Path) -> Task<Message> {
        if self.waveforms.contains_key(path) {
            return Task::none();
        }
        let path = path.to_owned();
        Task::perform(
            blocking({
                let path = path.clone();
                move || music::Waveform::read(&path).map(Arc::new).map_err(|e| format!("{e:#}"))
            }),
            move |r| Message::Waveform(path.clone(), r),
        )
    }

    /// What the speakers get: nothing while muted, so unmuting restores
    /// the level that was set.
    fn volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Adds the next queued clip: straight away if it is in the library
    /// already, else once it has loaded.
    fn open_next(&mut self) -> Task<Message> {
        // A project's first cut plays once its clip is in, while the rest load.
        if self.clip.is_none() && self.edit.cuts.first().is_some_and(|c| self.library.iter().any(|m| m.input == c.input)) {
            let start = self.edit.cuts[0].start;
            self.follow = true;
            self.goto(0, start, false);
        }
        while let Some((path, add)) = self.queue.pop_front() {
            match self.library.iter().find(|m| m.input == path).map(|m| m.duration) {
                Some(duration) if add => self.add_cut(path, duration),
                Some(_) => {}
                None => {
                    self.loading_adds = add;
                    return self.open(path);
                }
            }
        }
        self.loaded = 0;
        Task::none()
    }

    /// The edit as a project file holds it.
    fn to_project(&self) -> Project {
        let aspect = match self.aspect {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(crate::view::Aspect(w.into(), h.into())),
        };
        // The music in edit order, as it plays (the lane keeps the order
        // they were added and split in).
        let mut music = self.edit.music.clone();
        music.clips.sort_by(|a, b| a.at.total_cmp(&b.at));
        let mut cuts = self.edit.cuts.clone();
        // Cuts with no keyframes of their own nor any before them show the
        // free look; the file alone would frame them straight ahead (and an
        // ordinary video whole). Each kind's look is held as a keyframe at
        // the in point of its first such cut, so the export, the CLI and the
        // next open frame them as seen (later 360 cuts carry it on, ordinary
        // videos don't carry keyframes, so each of those gets one).
        let (look_360, look_flat) = if self.look_flat { (self.other_look, self.look) } else { (self.look, self.other_look) };
        let hold = |cut: &mut Cut, look: View| {
            if cut.keyframes.is_empty() && look != View::home(Projection::Perspective) {
                cut.keyframes.set(cut.start, look, 0.0, &|_, _| None);
            }
        };
        if let Some(cut) = cuts.iter_mut().find(|c| !c.flat) {
            hold(cut, look_360);
        }
        for cut in cuts.iter_mut().filter(|c| c.flat) {
            hold(cut, look_flat);
        }
        Project { aspect, cuts, sound: self.edit.sound, music, fades: self.edit.fades, overlays: self.edit.overlays.clone(), text: self.edit.texts.clone() }
    }

    /// Renders the edit to `path` on its own thread with the export the
    /// command line uses, at the chosen resolution, in the aspect lock's
    /// shape (16:9 when free), stabilised as the player is.
    fn start_export(&mut self, path: PathBuf) -> Task<Message> {
        use iced::futures::{SinkExt, StreamExt};
        let project = self.to_project();
        let [w, h] = project.aspect.unwrap_or(crate::view::Aspect(16, 9)).size(self.resolution.short_side());
        let target = if self.export_clips {
            crate::Target::Clips { dir: path, name: self.export_name(), handles: 0.0, graded: self.export_graded }
        } else {
            crate::Target::One(path.with_extension(self.export_format.extension()))
        };
        let out = crate::ExportOpts { size: (w as usize, h as usize), draft: false, cpu: false, format: self.export_format, target };
        let opts = crate::StitchOpts {
            stabilize: Some(self.opts.stabilize),
            imu_axes: self.opts.imu_axes,
            world_z_down: self.opts.world_z_down,
            no_rs: !self.rolling_shutter,
            ..Default::default()
        };
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.exporting = Some((cancel.clone(), 0.0));
        self.save_status = None;
        let work = iced::stream::channel(8, async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
            std::thread::spawn(move || {
                let mut progress = |done: usize, total: usize| {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("cancelled");
                    }
                    if done.is_multiple_of(5) || done == total {
                        let _ = tx.unbounded_send(Message::ExportProgress(done as f32 / total.max(1) as f32));
                    }
                    Ok(())
                };
                let result = crate::export(&project, &opts, &out, &mut progress);
                let _ = tx.unbounded_send(Message::ExportDone(result.map_err(|e| format!("{e:#}"))));
            });
            while let Some(m) = rx.next().await {
                if output.send(m).await.is_err() {
                    break;
                }
            }
        });
        Task::run(work, |m| m)
    }

    /// What exports are named after: the project, else the first clip.
    fn export_base(&self) -> &Path {
        self.project.as_deref().unwrap_or(&self.edit.cuts[0].input)
    }

    fn export_name(&self) -> String {
        self.export_base().file_stem().map_or_else(|| "export".to_owned(), |s| s.to_string_lossy().into_owned())
    }

    /// Tracks `colour` from the playhead to the end of the cut on a thread,
    /// starting from `dir` (where the picked box was, in the rig), into a
    /// CSV beside the project (or the clip, before it is saved).
    fn start_track(&mut self, colour: crate::track::Colour, hex: String, dir: [f64; 3]) -> Task<Message> {
        use iced::futures::{SinkExt, StreamExt};
        let Some((cut, t)) = self.playing_cut() else { return Task::none() };
        let (input, end, i) = (cut.input.clone(), cut.end, self.current);
        let folder = self.project.as_deref().unwrap_or(&input).parent().unwrap_or(Path::new("")).to_owned();
        let stem = input.file_stem().map_or_else(|| "clip".to_owned(), |s| s.to_string_lossy().into_owned());
        let path = folder.join(format!("{stem}-colour-{t:.1}s.csv"));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.tracking = Some((cancel.clone(), 0.0));
        let work = iced::stream::channel(8, async move |mut output: iced::futures::channel::mpsc::Sender<Message>| {
            let (tx, mut rx) = iced::futures::channel::mpsc::unbounded();
            std::thread::spawn(move || {
                let mut progress = |done: usize, total: usize| {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        anyhow::bail!("cancelled");
                    }
                    let _ = tx.unbounded_send(Message::TrackProgress(done as f32 / total.max(1) as f32));
                    Ok(())
                };
                let result = (|| {
                    use std::io::Write;
                    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?);
                    let near = Some(crate::track::Near::Rig(dir));
                    let (found, rows) = crate::track_to(&input, &colour, near, Some(t), Some(end - t), 0.1, &mut file, &mut progress)?;
                    file.flush()?;
                    Ok::<_, anyhow::Error>(TrackDone { path: path.clone(), cut: i, t, found, rows, colour: hex })
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&path);
                }
                let _ = tx.unbounded_send(Message::Tracked(result.map_err(|e| format!("{e:#}"))));
            });
            while let Some(m) = rx.next().await {
                if output.send(m).await.is_err() {
                    break;
                }
            }
        });
        Task::run(work, |m| m)
    }

    /// Makes the keyframe where a finished track starts follow it, with the
    /// subject in the middle of the view.
    fn follow_track(&mut self, done: TrackDone) {
        self.save_status = Some(format!("Tracked {}: found in {} of {} samples", done.colour, done.found, done.rows));
        if done.cut >= self.edit.cuts.len() {
            return;
        }
        self.followed.borrow_mut().remove(&done.path);
        let follow = Follow::Track(done.path);
        let here = done.cut == self.current;
        let mut aims = if here { self.aims(done.t) } else { Vec::new() };
        let subject = if here { self.aim(&follow, done.t) } else { None };
        aims.push((follow.clone(), subject));
        let mut view = if here { self.shown_view() } else { View::home(Projection::Perspective) };
        if let Some((yaw, pitch)) = subject {
            (view.yaw, view.pitch) = (yaw, pitch);
        }
        let aim = |f: &Follow, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1);
        let tolerance = self.tolerance();
        self.edit.checkpoint();
        let keyframes = &mut self.edit.cuts[done.cut].keyframes;
        let k = keyframes.set(done.t, view, tolerance, &aim);
        keyframes.set_follow(k, Some(follow), &aim);
        self.follow = true;
    }

    /// The median colour inside `area` (fractions of the view, `aspect`
    /// wide over tall) of the frame on screen, and the rig direction of its
    /// middle. Sampled from the lens pictures as the shader maps them.
    fn sample_area(&self, area: Rectangle, aspect: f64) -> Option<([f64; 3], [f64; 3])> {
        const N: usize = 16;
        let c = self.clip.as_ref()?;
        let frame = c.shown.as_ref()?.frame();
        let rig = &c.playback.media.rig;
        let camera = self.camera();
        let views = c.playback.rig_from_view(self.rolling_shutter);
        let rig_from_view = views[views.len() / 2];
        let dir = |fx: f64, fy: f64| crate::stitch::apply(rig_from_view, crate::stitch::apply(camera.user, camera.ray(aspect, [2.0 * fx - 1.0, 2.0 * fy - 1.0])));
        let mut channels: [Vec<f64>; 3] = Default::default();
        // An ordinary video or picture has no lenses: its frame's one
        // picture, cropped as the shader crops it. The eyedropper used to
        // find nothing there and silently do nothing (the user's Nikon clip).
        let flat = c.playback.media.flat;
        let picture = frame.pictures.first().map_or(1.0, |p| p.width as f64 / p.height as f64);
        for gy in 0..N {
            for gx in 0..N {
                let fx = area.x as f64 + (gx as f64 + 0.5) / N as f64 * area.width as f64;
                let fy = area.y as f64 + (gy as f64 + 0.5) / N as f64 * area.height as f64;
                if let Some(flat) = flat {
                    if let Some([u, v]) = camera.flat_uv(aspect, picture, [2.0 * fx - 1.0, 2.0 * fy - 1.0])
                        && let Some(rgb) = frame.flat_rgb_at(u, v, flat.hdr)
                    {
                        for k in 0..3 {
                            channels[k].push(rgb[k]);
                        }
                    }
                    continue;
                }
                let d = dir(fx, fy);
                // The lens that weighs most there, as the blend would.
                let best = rig.lenses.iter().filter_map(|l| l.project(d).map(|h| (l, h))).max_by(|a, b| (a.0.blend.weight(a.1.2) * a.1.3).total_cmp(&(b.0.blend.weight(b.1.2) * b.1.3)));
                if let Some((lens, (u, v, _, _))) = best
                    && let Some(rgb) = frame.rgb_at(lens, rig.tracks[lens.input], u, v)
                {
                    for k in 0..3 {
                        channels[k].push(rgb[k]);
                    }
                }
            }
        }
        if channels[0].is_empty() {
            return None;
        }
        let median = channels.map(|mut c| {
            c.sort_by(f64::total_cmp);
            c[c.len() / 2]
        });
        Some((median, dir(area.x as f64 + area.width as f64 / 2.0, area.y as f64 + area.height as f64 / 2.0)))
    }

    /// Export button, resolution and options, or a running export's progress.
    fn export_controls(&self) -> Element<'_, Message> {
        if let Some((_, done)) = &self.tracking {
            return row![
                text(format!("Tracking {:.0}%", done * 100.0)),
                progress_bar(0.0..=1.0, *done).length(140).girth(8),
                tip(icon_button(CANCEL, Some(Message::TrackCancel)), text("Stop tracking"), tooltip::Position::Bottom),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into();
        }
        match &self.exporting {
            Some((_, done)) => row![
                text(format!("Exporting {:.0}%", done * 100.0)),
                progress_bar(0.0..=1.0, *done).length(140).girth(8),
                tip(icon_button(CANCEL, Some(Message::ExportCancel)), text("Stop the export"), tooltip::Position::Bottom),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            None => row![
                tip(
                    labelled("Aspect", pick_list(&Aspect::ALL[..], Some(self.aspect), Message::Aspect)),
                    text("The shape of the view, of saved views and of the export (16:9 when free)"),
                    tooltip::Position::Bottom,
                ),
                tip(
                    icon_button(EXPORT, (!self.edit.cuts.is_empty()).then_some(Message::ExportPanel)),
                    text(if self.side == Some(Side::Export) { "Close the Export panel (Ctrl+E)" } else { "Export: resolution, format and clips, then the edit with sound (Ctrl+E)" }),
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        }
    }

    /// Asks whether to save the edit before doing `then`.
    fn confirm(&self, then: Then) -> Task<Message> {
        let name = self.project.as_ref().and_then(|p| p.file_name()).map_or_else(|| "this edit".to_owned(), |n| n.to_string_lossy().into_owned());
        let ask = rfd::AsyncMessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Unsaved changes")
            .set_description(format!("Save the changes to {name} first?"))
            .set_buttons(rfd::MessageButtons::YesNoCancelCustom("Save".into(), "Don't save".into(), "Cancel".into()));
        Task::perform(ask.show(), move |answer| {
            // Backends report custom buttons by label, some as yes/no.
            let choice = match answer {
                rfd::MessageDialogResult::Yes => Choice::Save,
                rfd::MessageDialogResult::No => Choice::Discard,
                rfd::MessageDialogResult::Custom(label) if label == "Save" => Choice::Save,
                rfd::MessageDialogResult::Custom(label) if label == "Don't save" => Choice::Discard,
                _ => Choice::Cancel,
            };
            Message::Confirmed(then, choice)
        })
    }

    /// An export or colour track is running on its thread.
    fn busy(&self) -> bool {
        self.exporting.is_some() || self.tracking.is_some()
    }

    /// Asks whether to stop the running export or tracking and quit:
    /// quitting under it left a half-written video, and the crossfade's
    /// temporary frames (GBs at 2160p) in the temp folder.
    fn confirm_stop(&self) -> Task<Message> {
        let what = if self.exporting.is_some() { "An export" } else { "Colour tracking" };
        let ask = rfd::AsyncMessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Still working")
            .set_description(format!("{what} is still running. Stop it and quit?"))
            .set_buttons(rfd::MessageButtons::OkCancelCustom("Stop and quit".into(), "Keep working".into()));
        Task::perform(ask.show(), |answer| {
            Message::StopAndClose(matches!(answer, rfd::MessageDialogResult::Ok) || matches!(&answer, rfd::MessageDialogResult::Custom(l) if l == "Stop and quit"))
        })
    }

    fn cancel_work(&self) {
        for (cancel, _) in self.exporting.iter().chain(&self.tracking) {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Goes on quitting once the work stopped for it has finished
    /// (cleaning up after itself), asking about unsaved changes as usual.
    fn close_when_stopped(&mut self) -> Task<Message> {
        if self.closing && !self.busy() {
            self.closing = false;
            return self.update(Message::CloseRequested);
        }
        Task::none()
    }

    /// Does what waited on the edit being saved or let go.
    fn proceed(&mut self, then: Then) -> Task<Message> {
        match then {
            Then::Close => iced::exit(),
            Then::OpenProject => self.update(Message::OpenProject),
            Then::NewProject => self.update(Message::NewProject),
        }
    }

    /// Opens `side` at the right (closing whichever was there), or closes it.
    fn toggle_side(&mut self, side: Side) {
        self.side = if self.side == Some(side) { None } else { Some(side) };
    }

    fn unsaved(&self) -> bool {
        (self.edit.version != self.saved_version || self.aspect != self.saved_aspect) && self.has_edit()
    }

    /// Whether there is anything to save: cuts, or music to fill.
    fn has_edit(&self) -> bool {
        !self.edit.cuts.is_empty() || !self.edit.music.clips.is_empty()
    }

    /// Parses the clip and probes the decoder off the UI thread.
    fn open(&mut self, path: PathBuf) -> Task<Message> {
        self.loading = Some(path.clone());
        self.error = None;
        let opts = self.opts.clone();
        let part = self.loading_part.clone();
        part.store(0f64.to_bits(), std::sync::atomic::Ordering::Relaxed);
        let progress = move |f: f64| part.store(f.clamp(0.0, 1.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
        Task::perform(blocking(move || Media::load(path, &opts, &progress).map_err(|e| format!("{e:#}"))), |r| Message::Loaded(Arc::new(r)))
    }

    fn title(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // A dot marks unsaved changes, as in many editors.
        let project = match &self.project {
            Some(p) => format!(" · {}{}", name(p), if self.unsaved() { " •" } else { "" }),
            None if self.unsaved() => " · unsaved edit •".to_owned(),
            None => String::new(),
        };
        match (&self.loading, &self.clip) {
            (Some(_), _) => format!("{NAME}{project} · loading {:.0}%", self.load_progress() * 100.0),
            (None, Some(c)) => format!("{NAME}{project} · {}", name(&c.playback.media.input)),
            (None, None) => format!("{NAME}{project}"),
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let keys = keyboard::listen().filter_map(|e| match e {
            keyboard::Event::KeyPressed { key, modifiers, .. } => Some(Message::Key(key, modifiers)),
            _ => None,
        });
        // The crossfade's partner too: paused, its frame came in only with
        // the next event.
        let busy = self.clip.iter().chain(self.partner.as_ref().map(|(_, c)| c)).any(|c| c.playback.busy());
        let close = window::close_requests().map(|_| Message::CloseRequested);
        if busy {
            Subscription::batch([keys, close, window::frames().map(|_| Message::Tick)])
        } else if self.loading.is_some() {
            // Only to redraw the loading bar as it fills.
            Subscription::batch([keys, close, window::frames().map(|_| Message::LoadingTick)])
        } else {
            Subscription::batch([keys, close])
        }
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Open => {
                let dir = self.clip.as_ref().and_then(|c| c.playback.media.input.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .set_title("Add clips")
                            .add_filter(
                                "Videos and pictures",
                                &[
                                    "OSV", "osv", "mp4", "MP4", "mov", "MOV", "mkv", "MKV", "m4v", "M4V", "avi", "AVI", "webm", "WEBM", "mts", "MTS", "jpg", "JPG", "jpeg", "JPEG", "png", "PNG", "webp", "WEBP", "bmp", "BMP", "tif", "TIF", "tiff", "TIFF",
                                ],
                            )
                            .add_filter("Osmo 360 video", &["OSV", "osv"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_files().await.unwrap_or_default().iter().map(|f| f.path().to_owned()).collect()
                    },
                    Message::Picked,
                );
            }
            Message::Picked(paths) => {
                self.queue.extend(paths.into_iter().map(|p| (p, true)));
                if self.loading.is_none() {
                    return self.open_next();
                }
            }
            Message::Loaded(result) => {
                self.loading = None;
                self.loaded += 1;
                match Arc::into_inner(result).expect("loaded clip is not shared") {
                    Ok(media) => {
                        if media.flat.is_none() {
                            self.opts.hw = Some(media.hw);
                        }
                        let (input, duration) = (media.input.clone(), media.duration);
                        let thumbs = (!self.thumbs.contains_key(&input)).then(|| Task::run(thumbs::stream(media.clone()), |m| m));
                        self.thumbs.entry(input.clone()).or_default();
                        self.library.push(media);
                        self.mark_flat();
                        if self.loading_adds {
                            self.add_cut(input, duration);
                        }
                        return Task::batch([self.open_next(), thumbs.unwrap_or_else(Task::none)]);
                    }
                    Err(e) => self.error = Some(e),
                }
                return self.open_next();
            }
            Message::OpenProject if self.unsaved() => return self.confirm(Then::OpenProject),
            Message::NewProject if self.unsaved() => return self.confirm(Then::NewProject),
            Message::NewProject => {
                // Back to the start screen with an empty edit. Loaded clips
                // stay in the library, so adding them again is instant; one
                // still loading finishes there without becoming a cut.
                self.edit.replace(Vec::new(), Default::default(), Default::default(), Default::default(), Vec::new(), Vec::new());
                self.hand_cut = None;
                self.info = None;
                self.selected_music = None;
                self.selected_volume = None;
                self.selected_sound = None;
                self.saved_version = self.edit.version;
                self.saved_aspect = self.aspect;
                self.project = None;
                self.clip = None;
                self.held = None;
                self.current = 0;
                self.selected = None;
                self.queue.clear();
                self.loading_adds = false;
                self.error = None;
                self.save_status = None;
                self.timeline_zoom = 1.0;
                self.timeline_start = 0.0;
            }
            Message::CloseRequested if self.busy() => return self.confirm_stop(),
            Message::StopAndClose(stop) => {
                if stop {
                    self.cancel_work();
                    self.closing = self.busy();
                    if !self.closing {
                        return self.update(Message::CloseRequested);
                    }
                    self.save_status = Some("Stopping, then quitting…".to_owned());
                }
            }
            Message::CloseRequested if self.unsaved() => return self.confirm(Then::Close),
            Message::CloseRequested => return iced::exit(),
            Message::Confirmed(then, choice) => match choice {
                Choice::Save => {
                    self.after_save = Some(then);
                    return self.update(Message::SaveProject(false));
                }
                Choice::Discard => {
                    // Let the changes go: the next save starts fresh.
                    self.saved_version = self.edit.version;
                    self.saved_aspect = self.aspect;
                    return self.proceed(then);
                }
                Choice::Cancel => {}
            },
            Message::OpenProject => {
                let dir = self.project.as_ref().and_then(|p| p.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Open project").add_filter("rOsmo360 project", &["json"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_file().await.map(|f| f.path().to_owned())
                    },
                    Message::ProjectPicked,
                );
            }
            Message::ProjectPicked(None) => {}
            Message::SaveProjectTo(None) => self.after_save = None,
            Message::ProjectPicked(Some(path)) => {
                let project = match Project::load(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        self.error = Some(format!("{e:#}"));
                        return Task::none();
                    }
                };
                // Cuts of clips that are gone can't play; say so and leave them out.
                let (mut cuts, missing): (Vec<_>, Vec<_>) = project.cuts.into_iter().partition(|c| c.input.exists());
                // Each cut gets a mode of its own, so changing one leaves the rest.
                for c in &mut cuts {
                    c.stabilize.get_or_insert(self.opts.stabilize);
                }
                self.error = (!missing.is_empty()).then(|| {
                    let names: Vec<_> = missing.iter().map(|c| c.input.display().to_string()).collect();
                    format!("Left out cuts of missing clips: {}", names.join(", "))
                });
                self.aspect = project.aspect.map_or(Aspect::Free, |a| Aspect::Ratio(a.0 as u16, a.1 as u16));
                self.clip = None;
                self.current = 0;
                self.selected = None;
                // Music that's gone is left out the same way.
                let mut music = project.music;
                let (here, gone): (Vec<_>, Vec<_>) = music.clips.into_iter().partition(|c| c.input.exists());
                music.clips = here;
                if !gone.is_empty() {
                    let names: Vec<_> = gone.iter().map(|c| c.input.display().to_string()).collect();
                    let said = self.error.take().map_or(String::new(), |e| e + ". ");
                    self.error = Some(format!("{said}Left out missing music: {}", names.join(", ")));
                }
                // And overlay pictures.
                let (mut overlays, texts) = (project.overlays, project.text);
                let mut lost = Vec::new();
                for lane in &mut overlays {
                    let (here, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut lane.items).into_iter().partition(|it| it.text.is_some() || it.input.exists());
                    lane.items = here;
                    lost.extend(gone.into_iter().map(|it| it.input.display().to_string()));
                }
                if !lost.is_empty() {
                    let said = self.error.take().map_or(String::new(), |e| e + ". ");
                    self.error = Some(format!("{said}Left out missing overlays: {}", lost.join(", ")));
                }
                let waves: Vec<_> = music.clips.iter().map(|c| self.load_waveform(&c.input)).collect();
                self.queue.extend(cuts.iter().map(|c| (c.input.clone(), false)));
                let complete = missing.is_empty() && gone.is_empty() && lost.is_empty();
                self.edit.replace(cuts, music, project.sound, project.fades, overlays, texts);
                self.hand_cut = None;
                self.info = None;
                self.mark_flat();
                self.saved_version = if complete { self.edit.version } else { 0 };
                self.saved_aspect = self.aspect;
                self.project = Some(path);
                self.selected_music = None;
                self.selected_volume = None;
                self.selected_sound = None;
                let next = if self.loading.is_none() { self.open_next() } else { Task::none() };
                return Task::batch(waves.into_iter().chain([next]));
            }
            Message::SaveProject(as_new) => {
                if !self.has_edit() {
                    return Task::none();
                }
                if let (Some(path), false) = (&self.project, as_new) {
                    return self.update(Message::SaveProjectTo(Some(path.clone())));
                }
                // Named after the first clip, or the song a project starts from.
                let first = self.edit.cuts.first().map(|c| &c.input).or(self.edit.music.clips.first().map(|c| &c.input)).unwrap();
                let dir = self.project.as_ref().unwrap_or(first).parent().map(Path::to_owned);
                let name = self.project.as_ref().and_then(|p| p.file_name()).map_or_else(
                    || format!("{}_edit.json", first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()),
                    |n| n.to_string_lossy().into_owned(),
                );
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Save project").add_filter("rOsmo360 project", &["json"]).set_file_name(name);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|f| f.path().to_owned())
                    },
                    Message::SaveProjectTo,
                );
            }
            Message::Resolution(r) => self.resolution = r,
            Message::ExportClips(on) => self.export_clips = on,
            Message::ExportGraded(on) => self.export_graded = on,
            Message::ExportFormat(f) => self.export_format = f,
            Message::Info(of) => {
                let path = match of {
                    info::Of::Cut(i) => self.edit.cuts.get(i).map(|c| c.input.clone()),
                    info::Of::Music(i) => self.edit.music.clips.get(i).map(|m| m.input.clone()),
                    info::Of::Overlay(r) => self.edit.item(r).filter(|it| it.text.is_none()).map(|it| it.input.clone()),
                };
                let Some(path) = path else { return Task::none() };
                self.settings_open = false;
                // Read once per file: another cut of it shows at once.
                let file = self.info.take().filter(|i| i.path == path).and_then(|i| i.file).filter(|f| f.is_ok());
                let read = file.is_none();
                self.info = Some(info::Info { of, path: path.clone(), file });
                if read {
                    let song = matches!(of, info::Of::Music(_));
                    let p = path.clone();
                    return Task::perform(blocking(move || info::read(&p, song).map(Arc::new).map_err(|e| format!("{e:#}"))), move |r| Message::InfoRead(path.clone(), r));
                }
            }
            Message::InfoRead(path, file) => {
                if let Some(info) = &mut self.info
                    && info.path == path
                {
                    info.file = Some(file);
                }
            }
            Message::InfoClose => self.info = None,
            Message::Reveal(path) => {
                std::thread::spawn(move || {
                    if let Err(e) = info::reveal(&path) {
                        eprintln!("couldn't show {}: {e:#}", path.display());
                    }
                });
            }
            Message::CopyPath(path) => {
                self.save_status = Some("Copied the file's path".to_owned());
                return iced::clipboard::write(path.display().to_string());
            }
            Message::ExportPanel => self.toggle_side(Side::Export),
            Message::AudioPanel => self.toggle_side(Side::Audio),
            Message::SettingsPanel => {
                self.settings_open = !self.settings_open;
                if self.settings_open {
                    self.info = None;
                }
            }
            Message::AutoKey(on) => self.auto_key = on,
            Message::SplitDrag(dy) => self.split = (self.split_kept - dy / timeline::SCALABLE).clamp(*timeline::SCALES.start(), *timeline::SCALES.end()),
            Message::SplitDone => self.split_kept = self.split,
            Message::SplitReset => (self.split, self.split_kept) = (1.0, 1.0),
            Message::TenBit(on) => {
                self.ten_bit = on;
                source::set_deep(on);
            }
            Message::AddLut => {
                return Task::perform(
                    async { rfd::AsyncFileDialog::new().set_title("Add a LUT").add_filter("3D LUT", &["cube", "CUBE"]).pick_file().await.map(|f| f.path().to_owned()) },
                    Message::LutPicked,
                );
            }
            Message::LutPicked(None) => {}
            Message::LutPicked(Some(path)) => match crate::lut::add_to_library(&path) {
                Ok(name) => {
                    self.luts = crate::lut::library_names();
                    self.save_status = Some("LUT added".to_owned());
                    // DJI's LUT added with no choice made yet: D-Log M
                    // clips get it, those in the edit too.
                    let squashed = name.to_lowercase().replace([' ', '-', '_'], "");
                    if self.dlog_m_lut.is_none() && squashed.contains("dlogm") {
                        self.dlog_m_lut = Some(name);
                        self.lut_dlog_m_cuts();
                    }
                    self.mark_flat();
                }
                Err(e) => self.error = Some(format!("Couldn't add the LUT: {e:#}")),
            },
            Message::RemoveLut(name) => {
                if let Err(e) = crate::lut::remove_from_library(&name) {
                    self.error = Some(format!("{e:#}"));
                }
                if self.dlog_m_lut.as_ref() == Some(&name) {
                    self.dlog_m_lut = None;
                }
                self.luts = crate::lut::library_names();
                self.mark_flat();
            }
            Message::DlogMLut(choice) => self.dlog_m_lut = choice.name(),
            Message::LookDone => {}
            Message::Export => {
                if self.edit.cuts.is_empty() || self.exporting.is_some() {
                    return Task::none();
                }
                let dir = self.export_base().parent().map(Path::to_owned);
                let (clips, ext) = (self.export_clips, self.export_format.extension());
                let name = format!("{}_{}.{ext}", self.export_name(), self.resolution);
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new();
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        if clips {
                            dialog.set_title("Export the cuts as clips, into the folder").pick_folder().await.map(|f| f.path().to_owned())
                        } else {
                            let kind = if ext == "mov" { "QuickTime video" } else { "MP4 video" };
                            dialog.set_title("Export video").add_filter(kind, &[ext]).set_file_name(name).save_file().await.map(|f| f.path().to_owned())
                        }
                    },
                    Message::ExportTo,
                );
            }
            Message::ExportTo(None) => {}
            Message::ExportTo(Some(path)) => return self.start_export(path),
            Message::ExportProgress(f) => {
                if let Some((_, done)) = &mut self.exporting {
                    *done = f;
                }
            }
            Message::ExportCancel => {
                if let Some((cancel, _)) = &self.exporting {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::Effect(e) => self.effect_message(e),
            Message::PickColour if self.on_flat() => {}
            Message::PickColour => {
                self.picking_white = false;
                if self.picking.take().is_none() && self.clip.is_some() {
                    self.with_playback(Playback::pause);
                    self.picking = Some(None);
                    self.save_status = Some("Draw a box over the subject to track its colour (Esc cancels)".to_owned());
                } else {
                    self.save_status = None;
                }
            }
            Message::PickArea(area) => {
                if self.picking.is_some() {
                    self.picking = Some(Some(area));
                }
            }
            Message::AreaPicked(area, aspect) => {
                self.picking = None;
                self.save_status = None;
                if area.width < 0.004 || area.height < 0.004 {
                    return Task::none();
                }
                if std::mem::take(&mut self.picking_white) {
                    if let Some((rgb, _)) = self.sample_area(area, aspect) {
                        self.white_balance(rgb);
                    }
                } else if let Some((rgb, dir)) = self.sample_area(area, aspect) {
                    match crate::track::Colour::from_rgb(rgb, 18.0) {
                        Ok(colour) => return self.start_track(colour, crate::track::hex(rgb), dir),
                        Err(e) => self.error = Some(format!("{e:#}")),
                    }
                }
            }
            Message::TrackProgress(f) => {
                if let Some((_, done)) = &mut self.tracking {
                    *done = f;
                }
            }
            Message::TrackCancel => {
                if let Some((cancel, _)) = &self.tracking {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::Tracked(result) => {
                let cancelled = self.tracking.take().is_some_and(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed));
                match result {
                    Ok(done) => self.follow_track(done),
                    Err(_) if cancelled => self.save_status = Some("Tracking cancelled".to_owned()),
                    Err(e) => self.error = Some(format!("Tracking failed: {e}")),
                }
                return self.close_when_stopped();
            }
            Message::ExportDone(result) => {
                let cancelled = self.exporting.take().is_some_and(|(c, _)| c.load(std::sync::atomic::Ordering::Relaxed));
                match result {
                    Ok(paths) => {
                        let name = |p: &PathBuf| p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                        self.save_status = Some(match paths.as_slice() {
                            [one] => format!("Exported {}", name(one)),
                            [first, ..] => format!("Exported {} clips to {}", paths.len(), first.parent().map_or_else(String::new, |d| d.display().to_string())),
                            [] => "Exported nothing".to_owned(),
                        });
                    }
                    Err(_) if cancelled => self.save_status = Some("Export cancelled".to_owned()),
                    Err(e) => self.error = Some(format!("Export failed: {e}")),
                }
                return self.close_when_stopped();
            }
            Message::SaveProjectTo(Some(path)) => match self.to_project().save(&path) {
                Ok(()) => {
                    self.saved_version = self.edit.version;
                    self.saved_aspect = self.aspect;
                    // No "Saved …" (user: just annoying): the title's unsaved
                    // mark going is enough.
                    self.save_status = None;
                    self.project = Some(path);
                    if let Some(then) = self.after_save.take() {
                        return self.proceed(then);
                    }
                }
                Err(e) => {
                    self.after_save = None;
                    self.error = Some(format!("Saving the project failed: {e:#}"));
                }
            },
            Message::LoadingTick => {}
            Message::Tick => {
                self.tick();
                if self.clip.as_ref().is_some_and(|c| c.playback.playing) {
                    self.reveal_playhead();
                }
            }
            Message::Key(key, mods) => {
                let task = self.key(key, mods);
                // Keys can seek out of sight (arrows, [ ]): follow.
                self.reveal_playhead();
                return task;
            }
            Message::TimelineZoom(factor, anchor) => self.zoom_timeline(self.timeline_zoom * factor, anchor),
            Message::TimelineZoomTo(v) => self.zoom_timeline(self.timeline_most_zoom().powf(v), None),
            Message::TimelinePan(start) => self.timeline_start = start,
            Message::Toggle => {
                self.follow = true;
                let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                if !playing && self.position() >= self.edit.duration() - 1.5 / self.fps() {
                    // From the end of the edit, play again from the start.
                    let start = self.edit.cuts.first().map_or(0.0, |c| c.start);
                    self.goto(0, start, true);
                } else {
                    self.with_playback(Playback::toggle);
                }
            }
            Message::Scrub(t) => {
                self.follow = true;
                // In an overlap, the selected cut's time, if it's one of the two.
                let selected = self.selected.filter(|&i| i < self.edit.cuts.len()).and_then(|i| {
                    let (from, c) = (self.edit.offset(i), &self.edit.cuts[i]);
                    (t >= from && t < from + c.len()).then(|| (i, c.start + t - from))
                });
                if let Some((i, t)) = selected.or_else(|| self.edit.locate(t)) {
                    self.scrub_to(i, t);
                }
            }
            Message::ScrubDone => {
                self.dragging = false;
                self.with_playback(Playback::scrub_end);
                if let Some((_, inc)) = &mut self.partner {
                    inc.playback.scrub_end();
                }
            }
            Message::Select(i) => {
                // The other cut of a crossfade selected while paused: it
                // takes the overlap over, to be framed there.
                if self.partner.as_ref().is_some_and(|(j, _)| *j == i) && self.clip.as_ref().is_some_and(|c| !c.playback.playing) {
                    self.swap_partner();
                }
                self.selected = Some(i);
                self.selected_music = None;
                self.selected_volume = None;
                self.selected_sound = None;
                self.selected_overlay = None;
            }
            Message::AddOverlay => {
                let dir = self.project.as_ref().and_then(|p| p.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .set_title("Add overlay pictures")
                            .add_filter("Pictures", &["png", "PNG", "jpg", "JPG", "jpeg", "JPEG", "webp", "WEBP", "bmp", "BMP", "tif", "TIF", "tiff", "TIFF"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_files().await.unwrap_or_default().iter().map(|f| f.path().to_owned()).collect()
                    },
                    Message::OverlayPicked,
                );
            }
            Message::OverlayPicked(paths) if !paths.is_empty() => {
                self.edit.checkpoint();
                // One after another from the playhead, each where it fits.
                let mut at = self.position();
                let lane = self.edit.lane_mut(crate::overlay::Track::Overlay);
                for input in paths {
                    lane.items.push(crate::overlay::Item { input, text: None, at, start: 0.0, end: ffmpeg::PICTURE_SECONDS, motion: Vec::new(), effects: Vec::new() });
                    let i = lane.items.len() - 1;
                    lane.items[i].at = lane.place(i, at);
                    at = lane.items[i].ends();
                    self.selected_overlay = Some((crate::overlay::Track::Overlay, i));
                }
                self.selected_music = None;
                self.selected_volume = None;
                self.selected_sound = None;
            }
            Message::OverlayPicked(_) => {}
            Message::OverlaySelect(i) => {
                // Typing in another overlay is a change of its own for undo.
                self.dragging = false;
                self.selected_overlay = i;
                if i.is_some() {
                    self.selected_music = None;
                    self.selected_volume = None;
                    self.selected_sound = None;
                }
            }
            Message::OverlayMove((track, i), at) => {
                if self.edit.item((track, i)).is_some() {
                    self.begin_drag();
                    let lane = self.edit.lane_mut(track);
                    lane.items[i].at = lane.place(i, at);
                }
            }
            Message::OverlayTrim((track, i), start, t) => {
                if self.edit.item((track, i)).is_some() {
                    self.begin_drag();
                    let lane = self.edit.lane_mut(track);
                    let (lo, hi) = lane.room(i);
                    let item = &mut lane.items[i];
                    if start {
                        // Its picture stays where it is on the edit; a
                        // picture has nothing before its own start.
                        let earliest = (item.start - (item.at - lo)).max(0.0);
                        let t = t.clamp(earliest, item.end - edit::MIN_CUT);
                        item.at += t - item.start;
                        item.start = t;
                    } else {
                        let latest = (item.start + (hi - item.at)).min(item.start + ffmpeg::PICTURE_MOST);
                        item.end = t.clamp(item.start + edit::MIN_CUT, latest);
                    }
                }
            }
            Message::OverlayJump(at) => {
                self.follow = true;
                if let Some((i, t)) = self.edit.locate(at) {
                    self.goto(i, t, false);
                }
            }
            Message::OverlayKeyMove(r, effects, key, t) => {
                self.follow = true;
                self.begin_drag();
                let gap = 1.0 / self.fps();
                let Some(item) = self.edit.item_mut(r) else { return Task::none() };
                let local = if effects {
                    crate::view::retime(&mut item.effects, key, t, gap);
                    item.effects.get(key).map(|k| k.t)
                } else {
                    crate::view::retime(&mut item.motion, key, t, gap);
                    item.motion.get(key).map(|k| k.t)
                };
                // Follow the mark, so what it holds shows as it moves.
                if let Some(at) = local.map(|t| item.at + t - item.start)
                    && let Some((i, t)) = self.edit.locate(at)
                {
                    self.scrub_to(i, t);
                }
            }
            Message::AddMusic => {
                let dir = self.project.as_ref().and_then(|p| p.parent().map(Path::to_owned));
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new()
                            .set_title("Add music")
                            .add_filter("Music", &["mp3", "MP3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma"]);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.pick_file().await.map(|f| f.path().to_owned())
                    },
                    Message::MusicPicked,
                );
            }
            Message::MusicPicked(None) => {}
            Message::MusicPicked(Some(path)) => {
                // Its length comes with the waveform; until then a stand-in
                // that the waveform corrects.
                let duration = ffmpeg::duration(&path).unwrap_or(60.0);
                self.edit.checkpoint();
                // After the music already there, whole.
                let at = self.edit.music.end();
                self.edit.music.clips.push(crate::project::MusicClip { input: path.clone(), at, start: 0.0, end: duration, fade_in: 0.0, fade_out: 0.0 });
                self.selected_music = Some(self.edit.music.clips.len() - 1);
                self.selected = None;
                return self.load_waveform(&path);
            }
            Message::OverlayPicture(key, Ok((picture, missing))) => {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                if let Some(font) = missing {
                    self.error = Some(format!("The font {font} isn't on this computer; its text shows in Inter"));
                }
                self.overlay_thumbs.insert(key.clone(), overlay_thumb(&picture));
                self.overlay_pictures.insert(key, Some((NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), picture)));
            }
            Message::OverlayPicture(key, Err(e)) => {
                self.error = Some(format!("Couldn't read the overlay {}: {e}", key.trim_start_matches("file:")));
            }
            Message::AddText => {
                self.edit.checkpoint();
                let at = self.position();
                let lane = self.edit.lane_mut(crate::overlay::Track::Text);
                lane.items.push(crate::overlay::Item { input: PathBuf::new(), text: Some(Default::default()), at, start: 0.0, end: ffmpeg::PICTURE_SECONDS, motion: Vec::new(), effects: Vec::new() });
                let i = lane.items.len() - 1;
                lane.items[i].at = lane.place(i, at);
                self.selected_overlay = Some((crate::overlay::Track::Text, i));
                self.selected_music = None;
                self.selected_volume = None;
                self.selected_sound = None;
                // Its words and look are set in the text panel, open now.
                self.text_of = None;
            }
            Message::Waveform(path, Ok(wave)) => {
                self.waveforms.insert(path, wave);
            }
            Message::Waveform(path, Err(e)) => self.error = Some(format!("Can't read {}: {e}", path.display())),
            Message::SelectMusic(i) => {
                self.selected_music = Some(i);
                self.selected_overlay = None;
                self.selected = None;
                self.selected_volume = None;
                self.selected_sound = None;
            }
            Message::VolumeAdd(t, db) => {
                self.begin_drag();
                let keys = &mut self.edit.music.keyframes;
                let i = keys.partition_point(|k| k.t <= t);
                keys.insert(i, crate::project::VolumeKey { t, volume: db });
                self.select_volume(i);
            }
            Message::VolumeSelect(i) => self.select_volume(i),
            Message::SoundKeyAdd(cut, t, db) => {
                let Some(c) = self.edit.cuts.get(cut) else { return Task::none() };
                let t = t.clamp(c.start, c.end);
                self.begin_drag();
                let keys = &mut self.edit.cuts[cut].volume;
                let i = keys.partition_point(|k| k.t <= t);
                keys.insert(i, crate::project::VolumeKey { t, volume: db });
                self.select_sound(cut, i);
            }
            Message::SoundKeySelect(cut, i) => self.select_sound(cut, i),
            Message::SoundKeyMove(cut, i, t, db) => {
                let Some(c) = self.edit.cuts.get(cut) else { return Task::none() };
                let (start, end) = (c.start, c.end);
                self.begin_drag();
                let keys = &mut self.edit.cuts[cut].volume;
                // Within the cut and between its neighbours.
                let lo = i.checked_sub(1).map_or(start, |j| keys[j].t + 0.01).max(start);
                let hi = keys.get(i + 1).map_or(end, |k| k.t - 0.01).min(end);
                if let Some(k) = keys.get_mut(i) {
                    *k = crate::project::VolumeKey { t: t.clamp(lo, hi.max(lo)), volume: db };
                }
            }
            Message::VolumeMove(i, t, db) => {
                self.begin_drag();
                let keys = &mut self.edit.music.keyframes;
                // Between its neighbours: points keep their order.
                let lo = i.checked_sub(1).map_or(0.0, |j| keys[j].t + 0.01);
                let hi = keys.get(i + 1).map_or(f64::INFINITY, |k| k.t - 0.01);
                if let Some(k) = keys.get_mut(i) {
                    *k = crate::project::VolumeKey { t: t.clamp(lo, hi.max(lo)), volume: db };
                }
            }
            Message::MusicMove(i, want, overlap) => {
                self.begin_drag();
                if i < self.edit.music.clips.len() {
                    let m = &self.edit.music;
                    let c = &m.clips[i];
                    let len = c.end - c.start;
                    // A plain drag never overlaps: it stops edge to edge, or
                    // hops past. With Shift it slides in as far as it may.
                    let at = if overlap || m.crossfades(i) != (0.0, 0.0) {
                        crate::project::Music::reach(c.at, want.max(0.0), |at| m.fits(i, at, len, overlap))
                    } else {
                        m.place(i, want)
                    };
                    self.edit.music.clips[i].at = at;
                }
            }
            Message::MusicTrim(i, in_edge, t, overlap) => {
                self.begin_drag();
                if i >= self.edit.music.clips.len() {
                    return Task::none();
                }
                let song = self.waveforms.get(&self.edit.music.clips[i].input).map_or(f64::INFINITY, |w| w.duration);
                let m = &self.edit.music;
                let c = &m.clips[i];
                // Song time to edit time on this clip: the song stays put on
                // the edit, so trimming its start moves where the clip
                // begins, not what plays when. Within the song and the edit,
                // and into a neighbour only with Shift.
                let to_edit = c.at - c.start;
                let (start, end) = (c.start, c.end);
                if in_edge {
                    let want = t.max(start - c.at).max(0.0).min(end - edit::MIN_CUT);
                    let start = crate::project::Music::reach(start, want, |s| m.fits(i, s + to_edit, end - s, overlap));
                    let c = &mut self.edit.music.clips[i];
                    c.at = start + to_edit;
                    c.start = start;
                } else {
                    let want = t.min(song).max(start + edit::MIN_CUT);
                    let end = crate::project::Music::reach(end, want, |e| m.fits(i, c.at, e - start, overlap));
                    self.edit.music.clips[i].end = end;
                }
            }
            Message::SoundLevel(db) => {
                self.begin_drag();
                self.edit.sound.volume = db;
                let v = self.clip_volume();
                self.with_playback(|p| p.set_volume(v));
            }
            Message::MusicLevel(db) => {
                self.begin_drag();
                self.edit.music.volume = db;
            }
            Message::LevelDone => self.dragging = false,
            Message::Trim(i, start, t) => {
                let Some(clip_len) = self.edit.cuts.get(i).and_then(|c| self.library.iter().find(|m| m.input == c.input)).map(|m| m.duration) else {
                    return Task::none();
                };
                self.begin_drag();
                self.edit.trim(i, start, t, clip_len);
                let c = &self.edit.cuts[i];
                // Show the frame at the edge being trimmed.
                let t = if start { c.start } else { c.end - 1.0 / self.fps() };
                self.follow = true;
                self.scrub_to(i, t);
            }
            Message::MoveCut(from, to) => {
                if to != from && to != from + 1 {
                    self.edit.checkpoint();
                    self.selected_sound = None;
                    let at = |i: usize| {
                        let landed = if to > from { to - 1 } else { to };
                        if i == from {
                            return landed;
                        }
                        let j = if i > from { i - 1 } else { i };
                        if j >= landed { j + 1 } else { j }
                    };
                    self.edit.move_cut(from, to);
                    self.current = at(self.current);
                    self.selected = Some(at(from));
                }
            }
            Message::Crossfade(i, seconds) => {
                self.begin_drag();
                self.edit.set_crossfade(i, seconds);
            }
            Message::ToggleCrossfade => {
                let i = self.selected.unwrap_or(self.current);
                let Some(c) = self.edit.cuts.get(i) else { return Task::none() };
                let to = if c.crossfade > 0.0 { 0.0 } else { 1.0 };
                self.edit.checkpoint();
                self.edit.set_crossfade(i, to);
            }
            Message::Split if self.selected_music.is_some() => {
                let i = self.selected_music.unwrap();
                self.edit.checkpoint();
                if self.edit.music.split(i, self.position(), edit::MIN_CUT) {
                    self.selected_music = Some(i + 1);
                } else {
                    self.edit.undo();
                }
            }
            Message::Split => {
                let Some((_, t)) = self.playing_cut() else { return Task::none() };
                let i = self.current;
                self.edit.checkpoint();
                if self.edit.split(i, t) {
                    // The playhead is now at the start of the second half.
                    self.current = i + 1;
                    self.selected = Some(i + 1);
                } else {
                    self.edit.undo();
                }
            }
            Message::DeleteCut if self.selected_overlay.is_some() => {
                let (track, i) = self.selected_overlay.take().unwrap();
                if self.edit.item((track, i)).is_some() {
                    self.edit.checkpoint();
                    self.edit.lane_mut(track).items.remove(i);
                }
            }
            Message::DeleteCut if self.selected_sound.is_some() => {
                let (cut, i) = self.selected_sound.take().unwrap();
                if self.edit.cuts.get(cut).is_some_and(|c| i < c.volume.len()) {
                    self.edit.checkpoint();
                    self.edit.cuts[cut].volume.remove(i);
                }
            }
            Message::DeleteCut if self.selected_volume.is_some() => {
                let i = self.selected_volume.take().unwrap();
                if i < self.edit.music.keyframes.len() {
                    self.edit.checkpoint();
                    self.edit.music.keyframes.remove(i);
                }
            }
            // With a song selected, delete that, not the cut under the
            // playhead (Shift+Del used to, being the key for cuts).
            Message::DeleteCut if self.selected_music.is_some() => {
                let i = self.selected_music.take().unwrap();
                if i < self.edit.music.clips.len() {
                    self.edit.checkpoint();
                    self.edit.music.clips.remove(i);
                }
            }
            Message::DeleteCut => {
                let Some(i) = self.selected.or(self.clip.as_ref().map(|_| self.current)) else { return Task::none() };
                if i >= self.edit.cuts.len() {
                    return Task::none();
                }
                self.edit.checkpoint();
                self.edit.remove(i);
                self.after_edit(self.edit.offset(i.min(self.edit.cuts.len())));
            }
            Message::Undo | Message::Redo => {
                let at = self.position();
                let changed = if matches!(message, Message::Undo) { self.edit.undo() } else { self.edit.redo() };
                if changed {
                    self.mark_flat();
                    self.after_edit(at);
                }
            }
            Message::KeyAdd => {
                if self.playing_cut().is_some() {
                    self.edit.checkpoint();
                    self.key_view();
                }
            }
            Message::KeyDelete => {
                if let Some(k) = self.current_key() {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].keyframes.remove(k);
                }
            }
            Message::Thumb(input, t, handle) => {
                let thumbs = self.thumbs.entry(input).or_default();
                let i = thumbs.partition_point(|(at, _)| *at < t);
                thumbs.insert(i, (t, handle));
            }
            Message::KeyEase(ease) => {
                if let Some(k) = self.current_key()
                    && self.edit.cuts[self.current].keyframes[k].ease != ease
                {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].keyframes.set_ease(k, ease);
                    self.follow = true;
                }
            }
            Message::KeyFollow(_) | Message::Stab(_) | Message::Projection(_) if self.on_flat() => {}
            Message::KeyFollow(aiming) => {
                let (Some(k), Some((_, t))) = (self.current_key(), self.playing_cut()) else { return Task::none() };
                if Aiming::of(&self.edit.cuts[self.current].keyframes[k].follow) == aiming {
                    return Task::none();
                }
                let follow = match aiming {
                    Aiming::Fixed => None,
                    Aiming::Stick => Some(Follow::Stick),
                    // A track comes from picking its colour; the keyframe
                    // here follows it once tracked.
                    Aiming::Colour => return self.update(Message::PickColour),
                };
                let aims = self.aims(t);
                self.edit.checkpoint();
                self.edit.cuts[self.current].keyframes.set_follow(k, follow, &|f, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1));
                self.follow = true;
            }
            Message::KeyJump(i, k) => {
                self.follow = true;
                self.selected = Some(i);
                if let Some(t) = self.edit.cuts.get(i).and_then(|c| c.keyframes.get(k)).map(|k| k.t) {
                    let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                    self.goto(i, t, playing);
                }
            }
            Message::KeyMove(i, k, t) => {
                self.follow = true;
                self.begin_drag();
                let gap = 1.0 / self.fps();
                let Some(cut) = self.edit.cuts.get_mut(i) else { return Task::none() };
                cut.keyframes.retime(k, t, gap);
                let t = cut.keyframes[k].t;
                self.scrub_to(i, t);
            }
            Message::Look { yaw, pitch, roll } => {
                let look = self.free_look();
                look.yaw += yaw.to_degrees();
                look.pitch += pitch.to_degrees();
                look.roll += roll.to_degrees();
            }
            Message::Zoom(deg) => self.free_look().zoom_by(deg),
            Message::Fov(deg) => self.free_look().fov = deg,
            Message::Stab(mode) => {
                // The playing cut's, and the one new cuts get.
                self.opts.stabilize = mode;
                if self.edit.cuts.get(self.current).is_some_and(|c| c.stabilize != Some(mode)) {
                    self.edit.checkpoint();
                    self.edit.cuts[self.current].stabilize = Some(mode);
                }
                self.with_playback(|p| p.media.stab.set_mode(mode));
            }
            Message::Projection(p) => {
                let look = self.free_look();
                look.projection = p;
                look.reset();
            }
            Message::Source(s) => {
                let size = match s {
                    Source::Proxy => None,
                    Source::Osv(size) => Some(size),
                };
                self.opts.osv_size = size;
                self.with_playback(|p| p.set_osv_size(size));
            }
            Message::RollingShutter(on) => self.rolling_shutter = on,
            Message::FullOnPause(on) => {
                self.full_on_pause = on;
                self.with_playback(|p| p.set_full_on_pause(on));
            }
            Message::ViewSize(size) => self.view_size = size,
            Message::Aspect(a) => self.aspect = a,
            Message::Theme(theme) => self.theme = theme,
            Message::Save(what) => {
                let Some(c) = &self.clip else { return Task::none() };
                let media = &c.playback.media;
                let stem = media.input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let name = format!("{stem}_{:.2}s_{}.png", c.playback.shown_pts, if what == Saving::View { "view" } else { "360" });
                let dir = media.input.parent().map(Path::to_owned);
                return Task::perform(
                    async move {
                        let mut dialog = rfd::AsyncFileDialog::new().set_title("Save photo").add_filter("PNG image", &["png"]).set_file_name(name);
                        if let Some(dir) = dir {
                            dialog = dialog.set_directory(dir);
                        }
                        dialog.save_file().await.map(|f| f.path().to_owned())
                    },
                    move |path| Message::SaveTo(what, path),
                );
            }
            Message::SaveTo(_, None) => {}
            Message::SaveTo(what, Some(path)) => {
                let Some(c) = &self.clip else { return Task::none() };
                let media = &c.playback.media;
                let shot = match what {
                    Saving::Sphere => export::Shot::Sphere { width: SAVE_SPHERE_PX },
                    Saving::View => {
                        let k = SAVE_VIEW_PX / self.view_size.width.max(self.view_size.height).max(1.0);
                        // Even sizes keep image tools that halve chroma happy.
                        let px = |v: f32| ((v * k / 2.0).round() as usize).max(1) * 2;
                        export::Shot::View {
                            camera: Box::new(self.camera()),
                            size: (px(self.view_size.width), px(self.view_size.height)),
                        }
                    }
                };
                let request = export::Request {
                    input: media.input.clone(),
                    flat: media.flat,
                    hw: media.hw,
                    rig: media.rig.clone(),
                    stab: media.stab.clone(),
                    readout_ms: media.readout_ms,
                    t: c.playback.shown_pts,
                    rolling_shutter: self.rolling_shutter,
                    shot,
                    path,
                };
                self.save_status = Some("Saving photo…".to_owned());
                return Task::perform(blocking(move || export::save(request).map_err(|e| format!("{e:#}"))), Message::Saved);
            }
            Message::Saved(result) => match result {
                Ok(_) => self.save_status = None,
                Err(e) => {
                    self.save_status = None;
                    self.error = Some(format!("Saving failed: {e}"));
                }
            },
            Message::Mute => {
                self.muted = !self.muted;
                audio::set_listening(self.volume());
            }
            Message::Volume(v) => {
                self.volume = v;
                self.muted = false;
                audio::set_listening(self.volume());
            }
            Message::ResetView => self.free_look().reset(),
            Message::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen { window::Mode::Fullscreen } else { window::Mode::Windowed };
                return window::latest().and_then(move |id| window::set_mode(id, mode));
            }
        }
        Task::none()
    }

    /// Half a frame: how near the playhead must be to sit on a keyframe.
    fn tolerance(&self) -> f64 {
        self.clip.as_ref().map_or(0.0, |c| 0.5 / c.playback.media.fps)
    }

    /// The playing cut, and the playhead's clip time. That is where a seek
    /// went even before its frame has decoded, and within the cut: in points
    /// fall between frames, so the frame shown can be a hair before it, and a
    /// keyframe there would be outside the cut, never drawn.
    fn playing_cut(&self) -> Option<(&Cut, f64)> {
        let cut = self.edit.cuts.get(self.current)?;
        Some((cut, self.clip.as_ref()?.playback.target().clamp(cut.start, cut.end)))
    }

    fn fps(&self) -> f64 {
        self.clip.as_ref().map_or(30.0, |c| c.playback.media.fps)
    }

    /// The playhead in edit time.
    fn position(&self) -> f64 {
        match &self.clip {
            Some(c) if self.current < self.edit.cuts.len() => {
                // In points fall between frames, so the frame shown can be a
                // hair before the cut.
                let cut = &self.edit.cuts[self.current];
                self.edit.edit_time(self.current, c.playback.position().clamp(cut.start, cut.end))
            }
            _ => 0.0,
        }
    }

    /// The keyframe at the shown frame, if there is one.
    fn current_key(&self) -> Option<usize> {
        let (cut, t) = self.playing_cut()?;
        cut.keyframes.near(t, self.tolerance())
    }

    /// The keyframes framing the playing cut at its clip time `t`, as
    /// `export` frames it: a cut without any holds the edit's last view.
    fn keyframed(&self, t: f64) -> Option<(&Keyframes, f64)> {
        crate::project::keyframed(&self.edit.cuts, self.current, t)
    }

    /// Where `follow` points in the playing clip's view at its time `t`.
    fn aim(&self, follow: &Follow, t: f64) -> Option<(f64, f64)> {
        self.aim_in(&self.clip.as_ref()?.playback.media, follow, t)
    }

    /// Where `follow` points in `media`'s clip at `t`.
    fn aim_in(&self, media: &Media, follow: &Follow, t: f64) -> Option<(f64, f64)> {
        match follow {
            Follow::Stick => media.stab.stick(t * 1000.0, media.readout_ms),
            Follow::Track(path) => {
                let track = self
                    .followed
                    .borrow_mut()
                    .entry(path.clone())
                    .or_insert_with(|| crate::track::Track::load(path).inspect_err(|e| eprintln!("{e:#}")).ok().map(Arc::new))
                    .clone()?;
                track.aim(&media.stab, t, media.readout_ms)
            }
        }
    }

    /// Where everything a keyframe could follow points at clip time `t`
    /// (the stick and the edit's tracks), for keyframe edits, which can't
    /// look while the edit is borrowed.
    fn aims(&self, t: f64) -> Vec<(Follow, Option<(f64, f64)>)> {
        let mut follows = vec![Follow::Stick];
        for f in self.edit.cuts.iter().flat_map(|c| c.keyframes.iter()).filter_map(|k| k.follow.clone()) {
            if !follows.contains(&f) {
                follows.push(f);
            }
        }
        follows.into_iter().map(|f| (f.clone(), self.aim(&f, t))).collect()
    }

    /// The view on screen, as angles.
    fn shown_view(&self) -> View {
        match self.playing_cut() {
            Some((_, t)) if self.follow => self.keyframed(t).and_then(|(k, at)| k.view_at(at, &crate::project::held(&|f, t| self.aim(f, t), at == t, t))).unwrap_or(self.look),
            _ => self.look,
        }
    }

    /// The view on screen, ready to draw: mid-morph between perspective and
    /// little planet when following keyframes.
    fn camera(&self) -> Camera {
        let camera = match self.playing_cut() {
            Some((_, t)) if self.follow => self.keyframed(t).and_then(|(k, at)| k.camera_at(at, &crate::project::held(&|f, t| self.aim(f, t), at == t, t))).unwrap_or_else(|| self.look.camera()),
            _ => self.look.camera(),
        };
        let grade = self.playing_cut().map(|(_, t)| self.grade(self.current, t)).unwrap_or_default();
        Camera { grade, lut: self.edit.cuts.get(self.current).and_then(|c| c.lut_applied), ..camera }
    }

    /// Cut `i`'s colour effects at clip time `t`, faded to black where the
    /// edit's fades are.
    fn grade(&self, i: usize, t: f64) -> crate::grade::Grade {
        let Some(cut) = self.edit.cuts.get(i) else { return Default::default() };
        let mut grade = crate::grade::grade_at(&cut.effects, t);
        grade.black = self.edit.fades.black(self.edit.edit_time(i, t), self.edit.duration());
        grade
    }

    /// Starts getting each overlay picture the edit uses that isn't ready
    /// yet, at the view's quality: a file read (4096 px on its long side at
    /// most), text drawn (for a 1440 px high frame). Those no longer used
    /// (text since retyped) are let go.
    fn load_overlay_pictures(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        let items: Vec<_> = self.edit.lanes().flat_map(|l| &l.items).cloned().collect();
        let keys: std::collections::HashSet<String> = items.iter().map(crate::overlay::Item::key).collect();
        self.overlay_pictures.retain(|k, _| keys.contains(k));
        self.overlay_thumbs.retain(|k, _| keys.contains(k));
        for item in items {
            let key = item.key();
            if self.overlay_pictures.contains_key(&key) {
                continue;
            }
            self.overlay_pictures.insert(key.clone(), None);
            tasks.push(Task::perform(
                blocking(move || item.picture(4096, 1440).map(|(p, missing)| (Arc::new(p), missing)).map_err(|e| format!("{e:#}"))),
                move |r| Message::OverlayPicture(key.clone(), r),
            ));
        }
        Task::batch(tasks)
    }

    /// Fills the panel's text box with the selected text overlay's words
    /// when the selection moves to another one.
    fn settle_text(&mut self) {
        let words = self.selected_overlay.and_then(|r| self.edit.item(r)?.text.as_ref().map(|t| (r, t.content.clone())));
        match words {
            Some((i, content)) if self.text_of != Some(i) || self.text_content.text().trim_end_matches('\n') != content => {
                self.text_content = iced::widget::text_editor::Content::with_text(&content);
                self.text_of = Some(i);
            }
            Some(_) => {}
            None => self.text_of = None,
        }
    }

    /// The overlays showing at the playhead, lowest first, as the export
    /// burns them in.
    fn overlay_draws(&self) -> Vec<gpu::OverlayDraw> {
        let t = self.position();
        let black = self.edit.fades.black(t, self.edit.duration());
        self.edit
            .lanes()
            .filter_map(|lane| lane.at(t).map(|(_, it)| it))
            .filter_map(|item| {
                let (id, picture) = self.overlay_pictures.get(&item.key())?.clone()?;
                let (place, grade) = crate::overlay::look(item, t);
                Some(gpu::OverlayDraw { id, picture, place, grade, black })
            })
            .collect()
    }

    /// The overlays showing at the playhead for the view's handles, while
    /// paused (playing, a drag would fight the motion keyframes).
    fn overlay_handles(&self) -> handles::Handles {
        let t = self.position();
        let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
        let items = [crate::overlay::Track::Overlay, crate::overlay::Track::Text]
            .into_iter()
            .filter(|_| !playing)
            .filter_map(|track| {
                let (i, item) = self.edit.lane(track)?.at(t)?;
                let (_, picture) = self.overlay_pictures.get(&item.key())?.clone()?;
                Some(((track, i), crate::overlay::look(item, t).0, picture))
            })
            .collect();
        handles::Handles { items, selected: self.selected_overlay }
    }

    /// The timeline's blocks for `track`'s lane.
    fn overlay_blocks(&self, track: crate::overlay::Track) -> Vec<timeline::OverlayBlock> {
        self.edit.lane(track).map_or_else(Vec::new, |lane| {
            lane.items
                .iter()
                .map(|it| timeline::OverlayBlock {
                    label: it.name(),
                    from: it.at,
                    start: it.start,
                    len: it.end - it.start,
                    effects: it.effects.iter().map(|k| k.t).collect(),
                    motion: it.motion.iter().map(|k| (k.t, k.ease)).collect(),
                    thumb: self.overlay_thumbs.get(&it.key()).cloned(),
                })
                .collect()
        })
    }

    /// The 360 view as it should be drawn now, once a frame is decoded.
    fn sphere(&self) -> Option<Sphere> {
        let c = self.clip.as_ref()?;
        Some(Sphere {
            rig: c.playback.media.rig.clone(),
            shown: c.shown.clone()?,
            camera: self.camera(),
            rig_from_view: c.playback.rig_from_view(self.rolling_shutter),
            fov_deg: self.shown_view().fov,
            picking: self.picking.is_some(),
            incoming: self.partner_layer(),
            overlays: self.overlay_draws(),
        })
    }

    /// The other cut of a crossfade, as the view draws it over the current one.
    fn partner_layer(&self) -> Option<(Arc<Rig>, Arc<Shown>, Camera, Vec<M3>, f32)> {
        // Paused with the playing cut selected, it's shown on its own, to
        // frame it.
        if self.selected == Some(self.current) && self.clip.as_ref().is_some_and(|c| !c.playback.playing) {
            return None;
        }
        let (i, inc) = self.partner.as_ref()?;
        let shown = inc.shown.clone()?;
        let p = &inc.playback;
        let t = p.target();
        // As `camera` frames the playing cut: by its keyframes (or those it
        // holds from before), else the view as it is.
        let keyed = || crate::project::keyframed(&self.edit.cuts, *i, t).and_then(|(k, at)| k.camera_at(at, &crate::project::held(&|f, t| self.aim_in(&p.media, f, t), at == t, t)));
        // The free look of the partner's own kind: a 360 view would push an
        // ordinary video's picture off screen, and the other way round.
        let look = if self.edit.cuts.get(*i).is_some_and(|c| c.flat) == self.look_flat { self.look } else { self.other_look };
        let camera = if self.follow { keyed().unwrap_or_else(|| look.camera()) } else { look.camera() };
        let camera = Camera { grade: self.grade(*i, t), lut: self.edit.cuts.get(*i).and_then(|c| c.lut_applied), ..camera };
        Some((p.media.rig.clone(), shown, camera, p.rig_from_view(self.rolling_shutter), self.crossfade_shares()?.1 as f32))
    }

    /// Where playback is in the playing cut, by the clock `tick` changes
    /// cuts by: playing, the clock (the frame shown lags it a little, and
    /// going by that dropped a crossfade `tick` had just begun); paused,
    /// where it's been put.
    fn clip_now(&self) -> Option<(&Cut, f64)> {
        let cut = self.edit.cuts.get(self.current)?;
        let p = &self.clip.as_ref()?.playback;
        Some((cut, if p.playing { p.now() } else { p.target() }.clamp(cut.start, cut.end)))
    }

    /// Through a crossfade, the current cut's share of the blend and its
    /// partner's (linear, adding to 1): the outgoing cut's falls as the
    /// incoming one's rises.
    fn crossfade_shares(&self) -> Option<(f64, f64)> {
        let (cut, t) = self.clip_now()?;
        let (j, _) = self.partner.as_ref()?;
        let incoming = if *j == self.current + 1 {
            let from = self.edit.crossfade_from(self.current)?;
            (t >= from && t < cut.end).then(|| (t - from) / (cut.end - from))?
        } else {
            (*j + 1 == self.current && t < cut.start + cut.crossfade).then(|| (t - cut.start) / cut.crossfade)?
        };
        Some(if *j == self.current + 1 { (1.0 - incoming, incoming) } else { (incoming, 1.0 - incoming) })
    }

    /// Makes the partner the current cut and the current one its partner:
    /// to edit the other cut of a crossfade, or to play it as it plays.
    fn swap_partner(&mut self) {
        let Some((j, other)) = self.partner.take() else { return };
        if let Some(mine) = self.clip.replace(other) {
            self.partner = Some((self.current, mine));
        }
        self.current = j;
    }

    /// Adds the whole clip as a cut at the end of the edit and selects it.
    /// The first clip starts playing, as opening a clip always did; later
    /// ones leave playback where it is.
    fn add_cut(&mut self, input: PathBuf, duration: f64) {
        self.edit.checkpoint();
        // A picture has no length of its own: a few seconds, trimmed at will.
        let picture = self.library.iter().find(|m| m.input == input).and_then(|m| m.flat).is_some_and(|f| f.picture);
        let end = if picture { ffmpeg::PICTURE_SECONDS } else { duration };
        let lut = self.auto_lut(&input);
        self.edit.cuts.push(Cut { input, start: 0.0, end, keyframes: Default::default(), stabilize: Some(self.opts.stabilize), note: None, volume: Vec::new(), crossfade: 0.0, effects: Vec::new(), lut, old_lut_strength: None, flat: false, lut_applied: None });
        self.mark_flat();
        let i = self.edit.cuts.len() - 1;
        self.selected = Some(i);
        if self.clip.is_none() {
            self.follow = true;
            self.goto(i, 0.0, true);
        }
    }

    /// Shows cut `i` at clip time `t`, playing or paused. A cut of another
    /// clip gets a fresh playback of it from the library; the last frame
    /// stays on screen until the new one is decoded.
    fn goto(&mut self, i: usize, t: f64, play: bool) {
        let Some(cut) = self.edit.cuts.get(i) else { return };
        let t = t.clamp(cut.start, cut.end);
        let other_clip = self.clip.as_ref().is_none_or(|c| c.playback.media.input != cut.input);
        if (i != self.current || other_clip) && self.held.is_none() {
            self.held = self.sphere();
        }
        match &mut self.clip {
            Some(c) if !other_clip => c.playback.jump(t, play),
            _ => {
                let Some(media) = self.library.iter().find(|m| m.input == cut.input) else { return };
                let mut media = media.clone();
                media.osv_size = self.opts.osv_size;
                let mut playback = Playback::new(media, self.clip_volume(), self.full_on_pause);
                playback.jump(t, play);
                self.clip = Some(Clip { playback, shown: None });
            }
        }
        self.current = i;
        let mode = self.edit.cuts[i].stabilize.unwrap_or(self.opts.stabilize);
        self.with_playback(|p| p.media.stab.set_mode(mode));
        self.settle_look();
    }

    /// Makes `look` the free look of the playing cut's kind. Run after every
    /// message, as the current cut changes in many places (playing on at an
    /// out point, a crossfade, a split, undo): swapped only in `goto`, a 360
    /// view made after playing into a 360 cut became the ordinary video's.
    fn settle_look(&mut self) {
        let flat = self.edit.cuts.get(self.current).is_some_and(|c| c.flat);
        if flat != self.look_flat {
            std::mem::swap(&mut self.look, &mut self.other_look);
            self.look_flat = flat;
        }
    }

    /// Whether the playing clip is an ordinary video: no stabilisation,
    /// projection, stick or colour track there.
    fn on_flat(&self) -> bool {
        self.clip.as_ref().is_some_and(|c| c.playback.media.flat.is_some())
    }

    /// Marks each cut whose clip has loaded as an ordinary video or not;
    /// the views of the two kinds are kept apart (`project::keyframed`).
    fn mark_flat(&mut self) {
        let mut missing = Vec::new();
        for cut in &mut self.edit.cuts {
            if let Some(m) = self.library.iter().find(|m| m.input == cut.input) {
                cut.flat = m.flat.is_some();
            }
            // Its LUT loaded too; a missing one leaves the cut as filmed.
            if cut.resolve_lut().is_err() {
                missing.extend(cut.lut.clone().filter(|n| !missing.contains(n)));
            }
        }
        if !missing.is_empty() {
            self.error = Some(format!("Missing LUT, shown as filmed (add it in Settings): {}", missing.join(", ")));
        }
    }

    /// The LUT a newly added clip gets: the D-Log M one for D-Log M.
    fn auto_lut(&self, input: &Path) -> Option<String> {
        let media = self.library.iter().find(|m| m.input == input)?;
        (media.color_mode == crate::dji::ColorMode::DLogM).then(|| self.dlog_m_lut.clone()).flatten()
    }

    /// Gives every D-Log M cut without a LUT the D-Log M one, one undo step.
    fn lut_dlog_m_cuts(&mut self) {
        let todo: Vec<usize> = (0..self.edit.cuts.len()).filter(|&i| self.edit.cuts[i].lut.is_none() && self.auto_lut(&self.edit.cuts[i].input).is_some()).collect();
        if todo.is_empty() {
            return;
        }
        self.edit.checkpoint();
        for i in todo {
            self.edit.cuts[i].lut = self.auto_lut(&self.edit.cuts[i].input.clone());
        }
    }

    /// Follows a drag (the playhead, a trim, a keyframe) to clip time `t`
    /// of cut `i`, switching clips if the drag crossed into another one.
    fn scrub_to(&mut self, i: usize, t: f64) {
        if i != self.current || self.clip.is_none() {
            self.goto(i, t, false);
        }
        self.with_playback(|p| p.scrub(t));
    }

    /// Saves the edit for undo once per drag.
    /// Selects music volume point `i`, and nothing else.
    fn select_volume(&mut self, i: usize) {
        self.selected_volume = Some(i);
        self.selected_music = None;
        self.selected = None;
        self.selected_sound = None;
    }

    /// Selects cut `cut`'s sound point `i`, and nothing else.
    fn select_sound(&mut self, cut: usize, i: usize) {
        self.selected_sound = Some((cut, i));
        self.selected_volume = None;
        self.selected_music = None;
        self.selected = None;
    }

    fn begin_drag(&mut self) {
        if !self.dragging {
            self.edit.checkpoint();
            self.dragging = true;
        }
    }

    /// After cuts were removed or restored: back to edit time `at`, or the
    /// start screen if no cut is left.
    fn after_edit(&mut self, at: f64) {
        self.selected = self.selected.filter(|&i| i < self.edit.cuts.len());
        // Its cut may have moved or gone.
        self.selected_sound = None;
        match self.edit.locate(at.min(self.edit.duration())) {
            Some((i, t)) => {
                // The playback may be of a clip no longer at this index.
                if self.edit.cuts[i].input != self.clip.as_ref().map_or_else(PathBuf::new, |c| c.playback.media.input.clone()) {
                    self.clip = None;
                }
                self.goto(i, t, false);
            }
            None => {
                self.clip = None;
                self.current = 0;
            }
        }
    }

    /// The view to change by hand: stops following keyframes, carrying on
    /// from the view they show now.
    fn free_look(&mut self) -> &mut View {
        if self.follow {
            self.look = self.shown_view();
            self.follow = false;
            self.hand_cut = self.edit.cuts.get(self.current).map(|c| (self.current, c.input.clone()));
        }
        &mut self.look
    }

    /// Once the view follows keyframes again (play, a seek, another cut),
    /// a cut looked around in by hand that has no keyframes of its own keeps
    /// what was shown as a keyframe at its in point (one undo step), so it
    /// doesn't share the free look with other cuts or lose it at the next
    /// play. Made then rather than at the first move, which would keep only
    /// the start of a drag. `before` is the free look of each kind (360,
    /// flat) as it was.
    fn keep_hand_view(&mut self, before: (View, View)) {
        if !self.follow {
            return;
        }
        let Some((i, input)) = self.hand_cut.take() else { return };
        let Some(cut) = self.edit.cuts.get(i).filter(|c| c.input == input && c.keyframes.is_empty()) else { return };
        let look = if cut.flat { before.1 } else { before.0 };
        if look == View::home(Projection::Perspective) && cut.flat {
            return;
        }
        self.edit.checkpoint();
        let cut = &mut self.edit.cuts[i];
        let start = cut.start;
        cut.keyframes.set(start, look, 0.0, &|_, _| None);
    }

    /// Keys the view shown at the playhead on the playing cut, or updates
    /// the keyframe there; the view then follows the keyframes again.
    fn key_view(&mut self) {
        let (view, tolerance) = (self.shown_view(), self.tolerance());
        let Some((_, t)) = self.playing_cut() else { return };
        let aims = self.aims(t);
        if let Some(cut) = self.edit.cuts.get_mut(self.current) {
            cut.keyframes.set(t, view, tolerance, &|f, _| aims.iter().find(|a| &a.0 == f).and_then(|a| a.1));
            self.follow = true;
        }
    }

    /// With Auto keyframe on: a look around while paused (`before` is each
    /// kind's free look as it was) keys the view at the playhead, as K does.
    /// While playing a drag would leave a keyframe each frame, so looking
    /// around stays free there.
    fn auto_key(&mut self, before: (View, View)) {
        let now = if self.look_flat { (self.other_look, self.look) } else { (self.look, self.other_look) };
        if !self.auto_key || self.follow || now == before || self.clip.as_ref().is_none_or(|c| c.playback.playing) {
            return;
        }
        let Some((_, t)) = self.playing_cut() else { return };
        if self.auto_key_at != Some((self.current, t)) {
            self.edit.checkpoint();
            self.auto_key_at = Some((self.current, t));
        }
        self.key_view();
    }

    /// Jumps to the nearest keyframe before (`-1`) or after (`1`) the playhead.
    fn key_step(&mut self, dir: f64) -> Task<Message> {
        let tolerance = self.tolerance();
        let Some((cut, now)) = self.playing_cut() else { return Task::none() };
        let within = |k: &&crate::view::Keyframe| k.t >= cut.start - tolerance && k.t <= cut.end + tolerance;
        let keys: Vec<_> = cut.keyframes.iter().enumerate().filter(|(_, k)| within(k)).collect();
        let next = if dir < 0.0 { keys.iter().rev().find(|(_, k)| k.t < now - tolerance) } else { keys.iter().find(|(_, k)| k.t > now + tolerance) };
        let current = self.current;
        next.map(|(i, _)| *i).map_or(Task::none(), |i| self.update(Message::KeyJump(current, i)))
    }

    fn with_playback(&mut self, f: impl FnOnce(&mut Playback)) {
        if let Some(c) = &mut self.clip {
            f(&mut c.playback);
            self.error = c.playback.error.clone();
        }
    }

    fn tick(&mut self) {
        let volume = self.clip_volume();
        if let Some((_, inc)) = &mut self.partner
            && inc.playback.go_at.is_none()
            && let Some((frame, fix)) = inc.playback.tick()
        {
            inc.shown = Some(Arc::new(Shown::new(frame, fix)));
        }
        let Some(c) = &mut self.clip else { return };
        if let Some((frame, fix)) = c.playback.tick() {
            c.shown = Some(Arc::new(Shown::new(frame, fix)));
            self.held = None;
        }
        self.error = c.playback.error.clone();
        let Some(cut) = self.edit.cuts.get(self.current) else { return };
        let p = &c.playback;
        // A cut running to its clip's end has paused there (`ended`), and
        // still moves on.
        if !p.playing && !p.ended {
            self.preroll = None;
            return;
        }
        let next = self.current + 1;
        let next_cut = self.edit.cuts.get(next);
        let loaded = |input: &PathBuf| self.library.iter().find(|m| &m.input == input);
        // A next cut that starts where this one ends in the same clip (after
        // a split) just plays on: no seek, nothing to prime.
        let carries_on = next_cut.is_some_and(|n| n.input == cut.input && (n.start - cut.end).abs() < 1.5 / p.media.fps && n.crossfade == 0.0);
        // Where the next cut comes in: this one's end, or before it by the
        // crossfade.
        let change = self.edit.crossfade_from(self.current).unwrap_or(cut.end);
        // In the last `PREROLL` seconds, ready the next cut so the change is
        // instant.
        if p.now() >= change - PREROLL
            && !carries_on
            && self.partner.as_ref().is_none_or(|(i, _)| *i != next)
            && let Some(n) = next_cut
            && self.preroll.as_ref().is_none_or(|(i, input, start, _)| (*i, input, *start) != (next, &n.input, n.start))
            && let Some(media) = loaded(&n.input)
        {
            let mut media = media.clone();
            media.osv_size = self.opts.osv_size;
            media.stab.set_mode(n.stabilize.unwrap_or(self.opts.stabilize));
            let primed = Playback::primed(media, n.start, volume, self.full_on_pause);
            self.preroll = Some((next, n.input.clone(), n.start, primed));
        }
        // Its sound starts a device latency early, so it's heard from the
        // moment this cut's ends (that latency is otherwise a gap, and the
        // picture waits through it to stay in sync).
        if let Some((i, input, at, primed)) = &mut self.preroll
            && next_cut.is_some_and(|n| (*i, &*input, *at) == (next, &n.input, n.start))
            && primed.sound_lead().is_some_and(|lead| p.now() >= change - lead)
        {
            primed.start_sound();
        }
        // A crossfade starts: the next cut plays on alongside, drawn over
        // this one, until this one ends.
        if change < cut.end
            && p.now() >= change
            && self.partner.as_ref().is_none_or(|(i, _)| *i != next)
            && let Some((i, input, at, mut primed)) = self.preroll.take()
        {
            if next_cut.is_some_and(|n| (i, &input, at) == (next, &n.input, n.start)) {
                primed.go();
                self.partner = Some((next, Clip { playback: primed, shown: None }));
            }
        }
        // At a cut's out point (or the clip's end), play on into the next cut.
        if p.now() < cut.end && !p.ended {
            return;
        }
        match next_cut {
            None if p.playing => c.playback.pause(),
            None => {}
            // Its clip is still loading: wait at the out point.
            Some(n) if loaded(&n.input).is_none() => {
                if p.playing {
                    c.playback.pause();
                    self.save_status = Some(format!("Cut {} is still loading", next + 1));
                }
            }
            Some(_) if carries_on && !p.ended => {
                self.current = next;
                let mode = self.edit.cuts[next].stabilize.unwrap_or(self.opts.stabilize);
                self.with_playback(|p| p.media.stab.set_mode(mode));
            }
            // Out of a crossfade: the cut that came in plays on.
            Some(_) if self.partner.as_ref().is_some_and(|(i, _)| *i == next) => {
                let (_, mut incoming) = self.partner.take().unwrap();
                if incoming.playback.go_at.is_some() {
                    incoming.playback.go();
                }
                self.clip = Some(incoming);
                self.current = next;
                self.preroll = None;
            }
            Some(n) => {
                let start = n.start;
                match self.preroll.take() {
                    Some((i, input, at, mut primed)) if (i, &input, at) == (next, &n.input, n.start) => {
                        // The last frame stays up until the primed one's first.
                        if self.held.is_none() {
                            self.held = self.sphere();
                        }
                        primed.go();
                        self.clip = Some(Clip { playback: primed, shown: None });
                        self.current = next;
                    }
                    _ => self.goto(next, start, true),
                }
            }
        }
    }

    /// The stretch of edit time the timeline shows: (start, span).
    fn timeline_window(&self) -> (f64, f64) {
        let d = self.edit.frame();
        let span = (d / self.timeline_zoom).max(TIMELINE_SPAN.min(d));
        (self.timeline_start.clamp(0.0, (d - span).max(0.0)), span)
    }

    /// The closest zoom: `TIMELINE_SPAN` seconds across.
    fn timeline_most_zoom(&self) -> f64 {
        (self.edit.frame() / TIMELINE_SPAN).max(1.0)
    }

    /// Zooms the timeline keeping `anchor` in place: by default the playhead
    /// if it is in sight, else the middle.
    fn zoom_timeline(&mut self, zoom: f64, anchor: Option<f64>) {
        let (start, span) = self.timeline_window();
        let position = self.position();
        let anchor = anchor.unwrap_or(if position >= start && position <= start + span { position } else { start + span / 2.0 });
        self.timeline_zoom = zoom.clamp(1.0, self.timeline_most_zoom());
        let (_, new_span) = self.timeline_window();
        self.timeline_start = anchor - (anchor - start) / span.max(1e-9) * new_span;
        self.timeline_start = self.timeline_window().0;
    }

    /// Scrolls the timeline to keep the playhead in sight, a page at a time.
    fn reveal_playhead(&mut self) {
        let (start, span) = self.timeline_window();
        let position = self.position();
        if position < start || position > start + span {
            self.timeline_start = (position - span * 0.05).max(0.0);
        }
        self.timeline_start = self.timeline_window().0;
    }

    fn key(&mut self, key: Key, mods: keyboard::Modifiers) -> Task<Message> {
        let step = 5.0;
        match key.as_ref() {
            Key::Named(Named::Escape) if self.picking.is_some() => return self.update(Message::PickColour),
            Key::Named(Named::Escape) if self.fullscreen => return self.update(Message::Fullscreen),
            Key::Named(Named::Space) => self.with_playback(Playback::toggle),
            Key::Named(Named::ArrowLeft) if mods.shift() => self.free_look().yaw -= 2.0 * step,
            Key::Named(Named::ArrowRight) if mods.shift() => self.free_look().yaw += 2.0 * step,
            Key::Named(arrow @ (Named::ArrowLeft | Named::ArrowRight)) => {
                self.follow = true;
                let t = self.position() + if arrow == Named::ArrowLeft { -5.0 } else { 5.0 };
                if let Some((i, t)) = self.edit.locate(t.max(0.0)) {
                    let playing = self.clip.as_ref().is_some_and(|c| c.playback.playing);
                    self.goto(i, t, playing);
                }
            }
            Key::Named(Named::ArrowUp) => self.free_look().pitch += step,
            Key::Named(Named::ArrowDown) => self.free_look().pitch -= step,
            Key::Named(Named::Home) => self.free_look().reset(),
            Key::Named(Named::Delete | Named::Backspace) if mods.shift() || self.selected_music.is_some() || self.selected_volume.is_some() || self.selected_sound.is_some() || self.selected_overlay.is_some() => {
                return self.update(Message::DeleteCut);
            }
            Key::Named(Named::Delete | Named::Backspace) => return self.update(Message::KeyDelete),
            Key::Character(c) => match c {
                "o" | "O" if mods.command() && mods.shift() => return self.update(Message::OpenProject),
                "n" | "N" if mods.command() => return self.update(Message::NewProject),
                "o" if mods.command() => return self.update(Message::Open),
                "s" | "S" if mods.command() => return self.update(Message::SaveProject(mods.shift())),
                "z" | "Z" if mods.command() && mods.shift() => return self.update(Message::Redo),
                "z" if mods.command() => return self.update(Message::Undo),
                "y" if mods.command() => return self.update(Message::Redo),
                "e" | "E" if mods.command() => return self.update(Message::ExportPanel),
                "," if mods.command() => return self.update(Message::SettingsPanel),
                "c" | "C" => return self.update(Message::Split),
                "q" | "Q" => self.free_look().roll -= step,
                "e" | "E" => self.free_look().roll += step,
                "=" | "+" if mods.command() => return self.update(Message::TimelineZoom(1.5, None)),
                "-" if mods.command() => return self.update(Message::TimelineZoom(1.0 / 1.5, None)),
                "0" if mods.command() => return self.update(Message::TimelineZoomTo(0.0)),
                "0" => self.free_look().reset(),
                // On the music: a volume point at the playhead, on the line.
                "k" | "K" if self.selected_music.is_some() || self.selected_volume.is_some() => {
                    let t = self.position();
                    let db = crate::project::volume_at(&self.edit.music.keyframes, t);
                    self.edit.checkpoint();
                    let keys = &mut self.edit.music.keyframes;
                    match keys.iter().position(|k| (k.t - t).abs() < 0.01) {
                        Some(i) => self.select_volume(i),
                        None => {
                            let i = keys.partition_point(|k| k.t <= t);
                            keys.insert(i, crate::project::VolumeKey { t, volume: db });
                            self.select_volume(i);
                        }
                    }
                }
                // A cut's sound point selected: another at the playhead.
                "k" | "K" if self.selected_sound.is_some() => {
                    let Some((_, t)) = self.playing_cut() else { return Task::none() };
                    let cut = self.current;
                    let db = crate::project::volume_at(&self.edit.cuts[cut].volume, t);
                    self.edit.checkpoint();
                    let keys = &mut self.edit.cuts[cut].volume;
                    match keys.iter().position(|k| (k.t - t).abs() < 0.01) {
                        Some(i) => self.select_sound(cut, i),
                        None => {
                            let i = keys.partition_point(|k| k.t <= t);
                            keys.insert(i, crate::project::VolumeKey { t, volume: db });
                            self.select_sound(cut, i);
                        }
                    }
                }
                "k" | "K" => return self.update(Message::KeyAdd),
                "j" | "J" => {
                    if let Some(k) = self.current_key() {
                        let ease = self.edit.cuts[self.current].keyframes[k].ease;
                        let i = Ease::ALL.iter().position(|&e| e == ease).unwrap_or(0);
                        return self.update(Message::KeyEase(Ease::ALL[(i + 1) % Ease::ALL.len()]));
                    }
                }
                "[" => return self.key_step(-1.0),
                "]" => return self.key_step(1.0),
                "," | "." => {
                    self.follow = true;
                    let frames = if c == "," { -1.0 } else { 1.0 };
                    self.with_playback(|p| p.step(frames));
                }
                "+" | "=" => self.free_look().zoom_by(-1.0),
                "-" => self.free_look().zoom_by(1.0),
                "s" | "S" => {
                    let mode = self.clip.as_ref().map_or(self.opts.stabilize, |c| c.playback.media.stab.mode());
                    let next = match mode {
                        Mode::Off => Mode::Horizon,
                        Mode::Horizon => Mode::Level,
                        Mode::Level => Mode::Lock,
                        Mode::Lock => Mode::Off,
                    };
                    return self.update(Message::Stab(next));
                }
                "g" | "G" => self.effect_message(effects::EffectMsg::Toggle),
                "r" | "R" => self.rolling_shutter = !self.rolling_shutter,
                "m" | "M" => return self.update(Message::Mute),
                "p" | "P" => {
                    let i = Projection::ALL.iter().position(|&p| p == self.shown_view().projection).unwrap_or(0);
                    return self.update(Message::Projection(Projection::ALL[(i + 1) % Projection::ALL.len()]));
                }
                "f" | "F" => return self.update(Message::Fullscreen),
                _ => {}
            },
            _ => {}
        }
        Task::none()
    }

    /// The logo shown while a clip loads or decodes its first frame.
    fn logo(&self) -> Element<'_, Message> {
        image(self.logo.clone()).width(Length::Fixed(420.0)).into()
    }

    /// Clips loaded and to load since the queue was last empty: a clip
    /// that several cuts use loads once.
    fn load_total(&self) -> usize {
        let waiting: std::collections::HashSet<&PathBuf> =
            self.queue.iter().map(|(p, _)| p).filter(|p| self.loading.as_ref() != Some(*p) && !self.library.iter().any(|m| &m.input == *p)).collect();
        self.loaded + waiting.len() + usize::from(self.loading.is_some())
    }

    /// How far loading the queued clips has got, 0..1.
    fn load_progress(&self) -> f32 {
        let part = f64::from_bits(self.loading_part.load(std::sync::atomic::Ordering::Relaxed));
        ((self.loaded as f64 + part) / self.load_total().max(1) as f64) as f32
    }

    /// A bar for loading clips, rather than their names going by.
    fn loading_bar(&self, length: f32) -> Element<'_, Message> {
        let total = self.load_total();
        let label = if total > 1 { format!("Loading clip {} of {total}", self.loaded + 1) } else { "Loading…".to_owned() };
        row![text(label), progress_bar(0.0..=1.0, self.load_progress()).length(length).girth(8)].spacing(8).align_y(Alignment::Center).into()
    }

    fn view(&self) -> Element<'_, Message> {
        let open_hint = if self.edit.cuts.is_empty() { "Open clips (Ctrl+O)" } else { "Add clips to the edit as cuts (Ctrl+O)" };
        let error = self.error.as_ref().map(|e| text(e.clone()).style(text::danger));
        let Some(clip) = &self.clip else {
            // Music first: the song is chosen and sets the length to fill.
            let song = self.edit.music.clips.first().map(|c| {
                let name = c.input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                format!("Music: {name} ({}). Add clips to fill it.", clock(self.edit.music.end()))
            });
            let status: Element<'_, Message> = match (&self.loading, song) {
                (Some(_), _) => self.loading_bar(280.0),
                (None, Some(song)) => text(song).into(),
                (None, None) => text("Open DJI Osmo 360 .OSV clips, other videos or pictures, or a saved project, to start. Or start from a song.").into(),
            };
            let idle = self.loading.is_none();
            let buttons = row![
                labelled_button(ADD_CLIP, "Open clips…", idle.then_some(Message::Open)),
                labelled_button(OPEN_PROJECT, "Open project…", idle.then_some(Message::OpenProject)),
                labelled_button(ADD_MUSIC, "Add music…", idle.then_some(Message::AddMusic)),
            ]
            .spacing(12);
            let start = center(column![self.logo(), status, buttons].push(error).spacing(16).align_x(Alignment::Center));
            return column![start, credit()].into();
        };
        let p = &clip.playback;
        let media = &p.media;

        let quality = p.quality();
        let showing = if quality.starts_with("loading") {
            status_icon(SHOWING_LOADING, format!("Showing the playback picture, {quality}"))
        } else if quality == "proxy" {
            status_icon(SHOWING_PROXY, "Showing the proxy (LRF): quick to decode, less detail".to_owned())
        } else {
            status_icon(SHOWING_FULL, format!("Showing the {}", if quality == "full res" { "OSV at full resolution".to_owned() } else { quality.clone() }))
        };
        let decode = match media.hw {
            Hwaccel::Software => status_icon(DECODE_CPU, "Decoding in software: no GPU decoder worked here".to_owned()),
            hw => status_icon(DECODE_GPU, format!("Decoding on the GPU with {}", match hw {
                Hwaccel::Vulkan => "Vulkan Video",
                Hwaccel::Vaapi => "VA-API",
                Hwaccel::VideoToolbox => "VideoToolbox",
                Hwaccel::D3d11va => "Direct3D 11",
                _ => "CUDA",
            })),
        };
        // One line: wrapped, a long one stood the whole top bar on end.
        let message = self.save_status.as_ref().map(|s| text(s.clone()).style(text::secondary).wrapping(text::Wrapping::None));
        let status: Element<'_, Message> = match &self.loading {
            Some(_) => self.loading_bar(140.0),
            None => row![].push(message).push(showing).push(decode).spacing(10).align_y(Alignment::Center).into(),
        };
        let shown_view = self.shown_view();
        let (lo, hi) = shown_view.fov_range();
        let mut sources = Vec::new();
        if media.proxy.is_some() {
            sources.push(Source::Proxy);
        }
        sources.push(Source::Osv(1920.min(media.osv_full)));
        sources.push(Source::Osv(media.osv_full));
        sources.dedup();
        let source = match (media.osv_size, &media.proxy) {
            (None, Some(_)) => Source::Proxy,
            (size, _) => Source::Osv(size.unwrap_or(1920)),
        };
        let top = row![
            tip(
                icon_button(SETTINGS, Some(Message::SettingsPanel)),
                text(if self.settings_open { "Close the Settings panel (Ctrl+,)" } else { "Settings (Ctrl+,)" }),
                tooltip::Position::Bottom,
            ),
            tip(icon_button(NEW_PROJECT, Some(Message::NewProject)), text("Start a new, empty project (Ctrl+N)"), tooltip::Position::Bottom),
            tip(icon_button(ADD_CLIP, Some(Message::Open)), text(open_hint), tooltip::Position::Bottom),
            tip(
                icon_button(ADD_MUSIC, Some(Message::AddMusic)),
                text(if self.edit.music.clips.is_empty() { "Add music under the edit" } else { "Add more music, after the last song" }),
                tooltip::Position::Bottom,
            ),
            tip(icon_button(ADD_OVERLAY, Some(Message::AddOverlay)), text("Add pictures over the picture, at the playhead (PNG transparency shows through)"), tooltip::Position::Bottom),
            tip(icon_button(ADD_TEXT, Some(Message::AddText)), text("Add text over the picture, at the playhead (typed in the Text panel at the left)"), tooltip::Position::Bottom),
            tip(icon_button(OPEN_PROJECT, Some(Message::OpenProject)), text("Open a saved edit (Ctrl+Shift+O)"), tooltip::Position::Bottom),
            tip(
                icon_button(SAVE, self.unsaved().then_some(Message::SaveProject(false))),
                text("Save the edit: cuts, keyframes and shape (Ctrl+S)"),
                tooltip::Position::Bottom,
            ),
            tip(
                icon_button(SAVE_AS, (!self.edit.cuts.is_empty()).then_some(Message::SaveProject(true))),
                text("Save the edit as a new project file (Ctrl+Shift+S)"),
                tooltip::Position::Bottom,
            ),
            self.export_controls(),
        ]
        // How a 360 clip is steadied and drawn; an ordinary video or a
        // picture has neither, so nothing shows.
        .push(media.flat.is_none().then(|| labelled("Stabilisation", pick_list(&Mode::ALL[..], Some(media.stab.mode()), Message::Stab))))
        .push(media.flat.is_none().then(|| labelled("Projection", pick_list(&Projection::ALL[..], Some(shown_view.projection), Message::Projection))))
        .push(media.flat.is_none().then(|| {
            row![
                tip(labelled("Source", pick_list(sources, Some(source), Message::Source)), text("Playback source: the proxy is quick, the OSV sharper"), tooltip::Position::Bottom),
                tip(
                    toggler(self.full_on_pause).label("Full res").on_toggle(Message::FullOnPause),
                    text("Full res when paused: a paused frame is decoded from the OSV at full size; off, it stays at the playback source's quality"),
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
        }))
        .push(row![
            space().width(Length::Fill),
            // Beside what's playing: the transport row is full at the
            // window's default width, and cut it off.
            tip(
                row![
                    canvas(self.meter.view()).width(meter::WIDTH).height(meter::HEIGHT),
                    // The loudest since play started, red past 0 dBFS.
                    text(self.meter.loudest.map_or("–".to_string(), |l| if l > -48.0 { format!("{l:+.1}") } else { "–".to_string() }))
                        .size(12)
                        .width(34)
                        .color_maybe(self.meter.loudest.filter(|&l| l >= 0.0).map(|_| iced::Color::from_rgb8(0xe5, 0x3e, 0x3e))),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
                text("The mix as it plays, left and right: peak (faint) and RMS, red past 0 dBFS; the number is the loudest peak since play started, dBFS. It sums the clip sound and music at their levels, as the export mixes them, whatever the speakers are set to."),
                tooltip::Position::Bottom,
            ),
            tip(
                icon_button(if self.fullscreen { FULLSCREEN_EXIT } else { FULLSCREEN }, Some(Message::Fullscreen)),
                text(if self.fullscreen { "Leave fullscreen (F)" } else { "Fullscreen (F)" }),
                tooltip::Position::Bottom,
            ),
            status
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .width(Length::Fill))
        .spacing(12)
        .padding([6, 10])
        .align_y(Alignment::Center);

        let view: Element<'_, Message> = match self.held.clone().or_else(|| self.sphere()) {
            Some(sphere) => {
                let (aspect, picking) = (self.aspect, self.picking);
                let handles = self.overlay_handles();
                // A locked aspect is the largest box of that shape that fits.
                responsive(move |room| {
                    let (w, h) = match aspect.ratio() {
                        None => (room.width, room.height),
                        Some(r) => if room.width / room.height > r { (room.height * r, room.height) } else { (room.width, room.width / r) },
                    };
                    let sphere = shader(sphere.clone()).width(w).height(h);
                    // The box's layer is there from the start of picking: adding it
                    // mid-drag would rebuild the view's state and lose the drag.
                    // The handles' layer is always there for the same reason.
                    match picking {
                        Some(area) => center(stack![sphere, canvas(PickBox(area)).width(w).height(h)]).into(),
                        None => center(stack![sphere, canvas(handles.clone()).width(w).height(h)]).into(),
                    }
                })
                .into()
            }
            None => center(column![self.logo(), text("Decoding…")].spacing(16).align_x(Alignment::Center)).into(),
        };
        let view = container(view).width(Length::Fill).height(Length::Fill).style(|_| container::background(iced::Color::BLACK));
        let view: Element<'_, Message> = match self.side {
            Some(Side::Effects) => row![view, self.effects_panel()].into(),
            Some(Side::Export) => row![view, self.export_panel()].into(),
            Some(Side::Audio) => row![view, self.audio_panel()].into(),
            None => view.into(),
        };
        // A text item's words and look, at the left while it's selected.
        // The Info and Settings panels win the left side over the Text panel
        // while open.
        let view: Element<'_, Message> = match self.info_panel().or_else(|| self.settings_panel()).or_else(|| self.text_panel()) {
            Some(panel) => row![panel, view].into(),
            None => view,
        };

        let current = self.current_key();
        let aiming = current.map(|k| Aiming::of(&self.edit.cuts[self.current].keyframes[k].follow));
        let tolerance = self.tolerance();
        let offsets = crate::project::offsets(&self.edit.cuts);
        let blocks = self
            .edit
            .cuts
            .iter()
            .zip(offsets)
            .map(|(c, from)| {
                let keys = c.keyframes.iter().enumerate().filter(|(_, k)| k.t >= c.start - tolerance && k.t <= c.end + tolerance).map(|(i, k)| (i, k.t, k.ease, k.follow.is_some())).collect();
                // Only those in the cut, and one either side for its ends.
                let thumbs = self.thumbs.get(&c.input).map_or_else(Vec::new, |all| {
                    let from = all.partition_point(|(t, _)| *t < c.start).saturating_sub(1);
                    let to = (all.partition_point(|(t, _)| *t <= c.end) + 1).min(all.len());
                    all[from..to].to_vec()
                });
                timeline::Block { labels: cut_labels(c, self.opts.stabilize), from, len: c.end - c.start, start: c.start, keys, sound: c.volume.clone(), thumbs, crossfade: c.crossfade, effects: c.effects.iter().map(|k| k.t).collect() }
            })
            .collect();
        let (timeline_start, timeline_span) = self.timeline_window();
        let music = self
            .edit
            .music
            .clips
            .iter()
            .map(|c| timeline::MusicBlock {
                label: c.input.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned()),
                from: c.at,
                start: c.start,
                len: c.end - c.start,
                wave: self.waveforms.get(&c.input).cloned(),
            })
            .collect();
        let timeline = timeline::Timeline {
            blocks,
            music,
            selected_music: self.selected_music,
            volume: self.edit.music.keyframes.clone(),
            selected_volume: self.selected_volume,
            selected_sound: self.selected_sound,
            video: self.edit.duration(),
            duration: self.edit.frame(),
            start: timeline_start,
            span: timeline_span,
            position: self.position(),
            selected: self.selected,
            current: current.map(|k| (self.current, k)),
            overlays: self.overlay_blocks(crate::overlay::Track::Overlay),
            texts: self.overlay_blocks(crate::overlay::Track::Text),
            selected_overlay: self.selected_overlay,
            rows: timeline::Rows::new(self.split),
        }
        .view();
        // With a song selected, C splits that instead.
        let music = self.selected_music.and_then(|i| self.edit.music.clips.get(i));
        let point = self.selected_volume.is_some() || self.selected_sound.is_some();
        // Any cut but the first can crossfade in from the one before.
        let crossfade_cut = Some(self.selected.unwrap_or(self.current)).filter(|&i| i > 0).and_then(|i| self.edit.cuts.get(i));
        let can_split = match music {
            Some(c) => self.position() > c.at + edit::MIN_CUT && self.position() < c.ends() - edit::MIN_CUT,
            None => self.edit.cuts.get(self.current).is_some_and(|c| p.target() > c.start + edit::MIN_CUT && p.target() < c.end - edit::MIN_CUT),
        };
        let transport = row![
            tip(
                icon_button(if p.playing { PAUSE } else { PLAY }, Some(Message::Toggle)),
                text(if p.playing { "Pause (Space)" } else { "Play (Space)" }),
                tooltip::Position::Top,
            ),
            tip(
                icon_button(SAVE_VIEW, (!p.playing).then_some(Message::Save(Saving::View))),
                text("Save what you see as a PNG (pause first)"),
                tooltip::Position::Top,
            ),
            tip(
                icon_button(SAVE_SPHERE, (!p.playing).then_some(Message::Save(Saving::Sphere))),
                text("Save the whole sphere as an equirect PNG (pause first)"),
                tooltip::Position::Top,
            ),
            // A fixed width: the digits' widths differ in most fonts (macOS's
            // system font especially), and the row after it jittered as it ran.
            text(format!("{} / {}", clock(self.position()), clock(self.edit.duration()))).width(170).center(),
            tip(
                icon_button(KEY_ADD, Some(Message::KeyAdd)),
                text(if current.is_some() { "Update this keyframe with the view (K)" } else { "Add a keyframe with this view (K)" }),
                tooltip::Position::Top,
            ),
            tip(icon_button(KEY_DELETE, current.map(|_| Message::KeyDelete)), text("Delete this keyframe (Del)"), tooltip::Position::Top),
            tip(
                pick_list(&Ease::ALL[..], current.map(|k| self.edit.cuts[self.current].keyframes[k].ease), Message::KeyEase).placeholder("Ease").width(96),
                text("How this keyframe moves on to the next (J cycles)"),
                // Below: the list opens upwards and the hint would cover it.
                tooltip::Position::Bottom,
            ),
            tip(
                pick_list(&Aiming::ALL[..], aiming.filter(|_| !self.on_flat()), Message::KeyFollow).placeholder(if self.on_flat() { "—" } else { "Follow" }).width(140),
                text("What this keyframe looks at: fixed angles, the selfie stick (whoever holds it), or a colour track (pick a new one by drawing a box over the subject)"),
                tooltip::Position::Bottom,
            ),
            tip(
                icon_button(PICK, (self.clip.is_some() && self.tracking.is_none() && !self.on_flat()).then_some(Message::PickColour)),
                text("Track a colour: draw a box over a subject (a jacket, a tent); the keyframe here then follows it"),
                tooltip::Position::Top,
            ),
            tip(labelled("FOV", slider(lo..=hi, shown_view.fov, Message::Fov).width(110)), text("Field of view: how wide the view is (scroll on the view)"), tooltip::Position::Top),
            tip(icon_button(RESET_VIEW, Some(Message::ResetView)), text("Reset the view: straight ahead, level (0)"), tooltip::Position::Top),
            tip(
                icon_button(EFFECTS, Some(Message::Effect(effects::EffectMsg::Toggle))),
                text(if self.side == Some(Side::Effects) { "Close the Effects panel (G)" } else { "Effects: exposure, colour and tone of the cut, keyframed; fades for the edit (G)" }),
                tooltip::Position::Top,
            ),
            tip(
                icon_button(CROSSFADE, crossfade_cut.map(|_| Message::ToggleCrossfade)),
                text(match crossfade_cut {
                    Some(c) if c.crossfade > 0.0 => "Take away this cut's crossfade (Shift-drag its start edge to change its length)",
                    _ => "Crossfade into this cut from the one before, 1 s (Shift-drag its start edge to change its length)",
                }),
                tooltip::Position::Top,
            ),
            tip(icon_button(SPLIT, can_split.then_some(Message::Split)), text(if music.is_some() { "Split the selected music at the playhead (C)" } else { "Split the cut at the playhead (C)" }), tooltip::Position::Top),
            tip(
                icon_button(DELETE_CUT, (self.selected.is_some() || music.is_some() || point || self.selected_overlay.is_some()).then_some(Message::DeleteCut)),
                text(if point {
                    "Delete the selected volume point (Del)"
                } else if music.is_some() {
                    "Delete the selected music (Del)"
                } else {
                    "Delete the selected cut (Shift+Del)"
                }),
                tooltip::Position::Top,
            ),
            tip(icon_button(UNDO, Some(Message::Undo)), text("Undo (Ctrl+Z)"), tooltip::Position::Top),
            tip(icon_button(REDO, Some(Message::Redo)), text("Redo (Ctrl+Shift+Z)"), tooltip::Position::Top),
            tip(icon_button(ZOOM_OUT, Some(Message::TimelineZoom(1.0 / 1.5, None))), text("Zoom the timeline out (Ctrl+−, or scroll on it)"), tooltip::Position::Top),
            tip(
                slider(0.0..=1.0, self.timeline_zoom.ln() / self.timeline_most_zoom().ln().max(1e-9), Message::TimelineZoomTo).step(0.001).width(90),
                text("Timeline zoom"),
                tooltip::Position::Top,
            ),
            tip(icon_button(ZOOM_IN, Some(Message::TimelineZoom(1.5, None))), text("Zoom the timeline in (Ctrl+=, or scroll on it)"), tooltip::Position::Top),
            tip(icon_button(ZOOM_FIT, Some(Message::TimelineZoomTo(0.0))), text("Show the whole edit (Ctrl+0)"), tooltip::Position::Top),
            space().width(Length::Fill),
            tip(
                icon_button(if self.muted { SPEAKER_MUTED } else { SPEAKER }, Some(Message::AudioPanel)),
                text(if self.side == Some(Side::Audio) { "Close the Audio panel" } else { "Audio: listening volume and the mix levels (M mutes)" }),
                tooltip::Position::Top,
            ),
        ]
        .spacing(12)
        .padding([4, 10])
        .align_y(Alignment::Center);

        let timeline = container(timeline).padding([2, 10]);
        let divider = tip(
            canvas(divider::Divider).width(Length::Fill).height(divider::HEIGHT),
            text("Drag to give the timeline or the picture more room (double-click: as it was)"),
            tooltip::Position::Top,
        );
        column![top, view, divider, transport, timeline].push(error.map(|e| container(e).padding([0, 10]))).push(credit()).into()
    }
}

/// The tool's name: Osmo 360, with an r for its Rust core.
pub const NAME: &str = "rOsmo360";

const LOGO: &[u8] = include_bytes!("../../vibewithravelund-lowres-corrected.jpg");

/// Who made it, and how: credit to the author and to the AI-assisted way it
/// was built.
fn credit<'a>() -> Element<'a, Message> {
    container(text(concat!("rOsmo360 ", env!("CARGO_PKG_VERSION"), " · Made by Vibing with Ravelund · 2026")).size(12).style(text::secondary)).width(Length::Fill).align_x(Alignment::Center).padding([2, 10]).into()
}

/// Edit icons: scissors (split), a bin (delete cut), arrows (undo, redo).
/// Two film frames and a plus: give every cut this look.
const TO_ALL_CUTS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="11" height="8" rx="1"/><rect x="6" y="9" width="11" height="8" rx="1"/><path d="M9 9v2M13 9v2M20 15v6M17 18h6"/></svg>"#;
/// Three sliders (the Effects panel).
const EFFECTS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 4h-7M10 4H3M21 12h-9M8 12H3M21 20h-5M12 20H3M14 2v4M8 10v4M16 18v4"/></svg>"#;
/// Two strips crossing: one picture dissolving into the next.
const CROSSFADE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="6" width="12" height="12" rx="2"/><rect x="10" y="6" width="12" height="12" rx="2" stroke-dasharray="2 2"/></svg>"#;
const SPLIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M20 4 8.1 15.9M14.5 14.5 20 20M8.1 8.1 12 12"/></svg>"#;
const DELETE_CUT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/></svg>"#;
const UNDO: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14 4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11"/></svg>"#;
/// The next cut is primed this many seconds before the playing one ends:
/// enough for its decoder to open and seek. From a network share that took
/// 0.2-1.6 s; at 1 s ahead the slower ones still stalled the change.
const PREROLL: f64 = 3.0;

/// The timeline zooms in until this many seconds fill it.
const TIMELINE_SPAN: f64 = 2.0;

/// Timeline zoom icons: a magnifier with a minus or plus, and arrows out to
/// both ends (fit).
const ZOOM_OUT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round"><circle cx="10" cy="10" r="7"/><path d="m21 21-6-6M7 10h6"/></svg>"#;
const ZOOM_IN: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round"><circle cx="10" cy="10" r="7"/><path d="m21 21-6-6M7 10h6M10 7v6"/></svg>"#;
const ZOOM_FIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 5v14M21 5v14M7 12h10M10 9l-3 3 3 3M14 9l3 3-3 3"/></svg>"#;
const REDO: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 14 5-5-5-5"/><path d="M20 9H9.5a5.5 5.5 0 0 0 0 11H13"/></svg>"#;

/// A short name for a clip on the timeline: DJI's clip number (the digits
/// in CAM_20260713153034_0037_D), else the file name.
fn clip_name(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    stem.split('_').rev().find(|p| p.len() == 4 && p.bytes().all(|b| b.is_ascii_digit())).map_or(stem.clone(), str::to_owned)
}

/// Keyframe icons: a diamond with a plus (add or update) or a cross (delete).
const PICK: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m2 22 1-1h3l9-9M3 21v-3l9-9"/><path d="m15 6 3.4-3.4a2.1 2.1 0 1 1 3 3L18 9l.4.4a2.1 2.1 0 1 1-3 3l-3.8-3.8a2.1 2.1 0 1 1 3-3z"/></svg>"#;
const KEY_ADD: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z" fill="white"/><path d="M19 15v8M15 19h8" stroke-linecap="round"/></svg>"#;
const KEY_DELETE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linejoin="round"><path d="M10 2 18 10 10 18 2 10z"/><path d="m16 16 6 6M22 16l-6 6" stroke-linecap="round"/></svg>"#;

/// File icons: a page with a plus (new project), a film strip with a plus
/// (open or add clips), a note with a plus (add music), an open folder (open
/// a project), a diskette (save) and one with a pencil (save as).
const ADD_MUSIC: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M9 18V5l10-2v9"/><circle cx="6" cy="18" r="3"/><path d="M19 15v6M16 18h6"/></svg>"#;
const NEW_PROJECT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7z"/><path d="M14 2v4a2 2 0 0 0 2 2h4M9 15h6M12 18v-6"/></svg>"#;
const ADD_OVERLAY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="14" height="14" rx="2"/><circle cx="8" cy="8" r="1.5"/><path d="M17 12l-4-4-8 8"/><path d="M20 15v6M17 18h6"/></svg>"#;
const ADD_TEXT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7V4h12v3M10 4v16M7 20h6"/><path d="M19 13v8M15 17h8"/></svg>"#;
const ADD_CLIP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="14" height="18" rx="2"/><path d="M6 3v18M12 3v18M2 8h4M2 16h4M12 8h4M12 16h4M20 9v6M17 12h6"/></svg>"#;
const OPEN_PROJECT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2"/></svg>"#;
const SAVE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"/><path d="M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7M7 3v4a1 1 0 0 0 1 1h7"/></svg>"#;
const SAVE_AS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h10.2a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V10"/><path d="M7 3v4a1 1 0 0 0 1 1h7M7 21v-7a1 1 0 0 1 1-1h4"/><path d="M21.4 15.6a1.98 1.98 0 0 0-2.8-2.8L14 17.4V20.2h2.8z"/></svg>"#;
/// An arrow out of a tray (export) and a crossed circle (cancel a running job).
const EXPORT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 15V3M7 8l5-5 5 5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/></svg>"#;
const SETTINGS: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/></svg>"#;
const FOLDER: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>"#;
const COPY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/></svg>"#;
const CANCEL: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="m15 9-6 6M9 9l6 6"/></svg>"#;
/// Transport: play, pause, a camera (save the view) and a globe (save the
/// whole sphere).
const PLAY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M7 4v16l13-8z" fill="white"/></svg>"#;
const PAUSE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="4" width="4" height="16" rx="1" fill="white"/><rect x="14" y="4" width="4" height="16" rx="1" fill="white"/></svg>"#;
const SAVE_VIEW: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14.5 4h-5L7 7H4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-3z"/><circle cx="12" cy="13" r="3"/></svg>"#;
const SAVE_SPHERE: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/></svg>"#;
/// View: turn back (reset the view) and corners out or in (fullscreen).
const RESET_VIEW: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/></svg>"#;
const FULLSCREEN: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M3 16v3a2 2 0 0 0 2 2h3M16 21h3a2 2 0 0 0 2-2v-3"/></svg>"#;
const FULLSCREEN_EXIT: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3v3a2 2 0 0 1-2 2H3M21 8h-3a2 2 0 0 1-2-2V3M3 16h3a2 2 0 0 1 2 2v3M16 21v-3a2 2 0 0 1 2-2h3"/></svg>"#;

/// Status: what the view shows (the proxy, the OSV or full res, a full-res
/// frame on its way) and how it decodes (on the GPU, or in software).
const SHOWING_PROXY: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2"/><path d="M7 3v18M17 3v18M3 7.5h4M3 12h18M3 16.5h4M17 7.5h4M17 16.5h4"/></svg>"#;
const SHOWING_FULL: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/></svg>"#;
const SHOWING_LOADING: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M5 22h14M5 2h14M17 22v-4.2a2 2 0 0 0-.6-1.4L12 12l-4.4 4.4a2 2 0 0 0-.6 1.4V22M7 2v4.2a2 2 0 0 0 .6 1.4L12 12l4.4-4.4a2 2 0 0 0 .6-1.4V2"/></svg>"#;
const DECODE_GPU: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2 3 14h9l-1 8 10-12h-9z"/></svg>"#;
const DECODE_CPU: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6" rx="1"/><path d="M9 1v3M15 1v3M9 20v3M15 20v3M20 9h3M20 14h3M1 9h3M1 14h3"/></svg>"#;

/// A hint on hover, on a box of its own so it reads over the picture.
fn tip<'a>(content: impl Into<Element<'a, Message>>, hint: iced::widget::Text<'a>, position: tooltip::Position) -> Element<'a, Message> {
    tooltip(content, container(hint).padding([4, 8]).style(container::rounded_box), position).into()
}

fn icon_button<'a>(icon: &'static [u8], on_press: Option<Message>) -> Element<'a, Message> {
    button(svg(svg::Handle::from_memory(icon)).width(20).height(20)).padding(5).on_press_maybe(on_press).into()
}

/// A mix level for its slider: the bottom of the range is silence.
fn db_label(db: f64) -> String {
    if db <= crate::project::OFF_DB { "off".to_owned() } else { format!("{db:+.1} dB") }
}

/// A status icon, dimmed towards the background so it stays out of the
/// way, with what it means on hover.
fn status_icon<'a>(icon: &'static [u8], hint: String) -> Element<'a, Message> {
    let dim = svg(svg::Handle::from_memory(icon)).width(18).height(18).style(|theme: &Theme, _| svg::Style {
        color: Some(iced::Color { a: 0.45, ..theme.palette().text }),
    });
    tip(dim, text(hint), tooltip::Position::Bottom)
}

/// An icon with its words, for the start screen, which has the room.
fn labelled_button<'a>(icon: &'static [u8], label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    button(row![svg(svg::Handle::from_memory(icon)).width(20).height(20), text(label)].spacing(8).align_y(Alignment::Center))
        .padding([6, 12])
        .on_press_maybe(on_press)
        .into()
}

/// Speaker icons for the mute button, drawn in the button's text colour.
const SPEAKER: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="M15.5 8.5a5 5 0 0 1 0 7"/><path d="M19 5a10 10 0 0 1 0 14"/></svg>"#;
const SPEAKER_MUTED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11 5 6 9H2v6h4l5 4z" fill="white"/><path d="m22 9-6 6"/><path d="m16 9 6 6"/></svg>"#;


fn labelled<'a>(label: &'a str, widget: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![text(label), widget.into()].spacing(6).align_y(Alignment::Center).into()
}

/// A finished colour track: its CSV, where it starts (cut and clip time),
/// how many samples found the colour, and the colour.
#[derive(Debug, Clone)]
pub struct TrackDone {
    path: PathBuf,
    cut: usize,
    t: f64,
    found: usize,
    rows: usize,
    colour: String,
}

/// The box being drawn to pick a colour, over the view.
struct PickBox(Option<Rectangle>);

impl canvas::Program<Message> for PickBox {
    type State = ();

    fn draw(&self, _: &(), renderer: &iced::Renderer, _: &Theme, bounds: Rectangle, _: mouse::Cursor) -> Vec<canvas::Geometry> {
        let Some(r) = &self.0 else { return vec![] };
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let rect = canvas::Path::rectangle(Point::new(r.x * bounds.width, r.y * bounds.height), Size::new(r.width * bounds.width, r.height * bounds.height));
        frame.fill(&rect, iced::Color { a: 0.15, ..iced::Color::WHITE });
        frame.stroke(&rect, canvas::Stroke::default().with_width(2.0).with_color(iced::Color::WHITE));
        vec![frame.into_geometry()]
    }
}

/// The Follow picker's choices for a keyframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aiming {
    Fixed,
    Stick,
    /// Any colour track: choosing it on a keyframe that follows none draws
    /// a box to pick one.
    Colour,
}

impl Aiming {
    const ALL: [Aiming; 3] = [Aiming::Fixed, Aiming::Stick, Aiming::Colour];

    fn of(follow: &Option<Follow>) -> Aiming {
        match follow {
            None => Aiming::Fixed,
            Some(Follow::Stick) => Aiming::Stick,
            Some(Follow::Track(_)) => Aiming::Colour,
        }
    }
}

impl std::fmt::Display for Aiming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Aiming::Fixed => f.write_str("Fixed"),
            Aiming::Stick => f.write_str("Stick"),
            Aiming::Colour => f.write_str("Colour track"),
        }
    }
}

/// A cut's names on the timeline, longest first, for the block to show the
/// first that fits: its clip, length, stabilisation when that isn't horizon
/// (the usual), and note.
fn cut_labels(c: &Cut, default: Mode) -> Vec<String> {
    let mut label = format!("{} · {:.1} s", clip_name(&c.input), c.end - c.start);
    match c.stabilize.unwrap_or(default) {
        Mode::Horizon => {}
        Mode::Off => label += " · unstabilised",
        Mode::Level => label += " · level",
        Mode::Lock => label += " · world lock",
    }
    match c.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => vec![format!("{label} · {note}"), label],
        None => vec![label],
    }
}

fn clock(t: f64) -> String {
    format!("{}:{:05.2}", (t / 60.0) as u32, t % 60.0)
}

/// A small picture of an overlay for its timeline block: 48 px high,
/// over a dark checker so transparency shows.
fn overlay_thumb(picture: &crate::overlay::Picture) -> iced::widget::image::Handle {
    let h = 48usize;
    let w = (picture.width * h / picture.height.max(1)).clamp(1, 4 * h);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (x * picture.width / w, y * picture.height / h);
            let p = &picture.rgba[(sy * picture.width + sx) * 4..][..4];
            let under = if (x / 6 + y / 6) % 2 == 0 { 60.0 } else { 90.0 };
            let a = p[3] as f32 / 255.0;
            rgba.extend([0, 1, 2].map(|c| (p[c] as f32 + under * (1.0 - a)).min(255.0) as u8));
            rgba.push(255);
        }
    }
    iced::widget::image::Handle::from_rgba(w as u32, h as u32, rgba)
}

/// Runs `f` on its own thread so blocking work (parsing, ffprobe) stays off
/// the executor.
fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl Future<Output = T> {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    async move { rx.await.expect("loader thread panicked") }
}

/// The 360 view widget: draws the shader primitive and turns drags and the
/// scroll wheel into look and zoom messages.
#[derive(Clone)]
struct Sphere {
    rig: Arc<Rig>,
    shown: Arc<Shown>,
    camera: Camera,
    rig_from_view: Vec<M3>,
    fov_deg: f64,
    /// Drags draw a box to pick a colour instead of looking around.
    picking: bool,
    /// Through a crossfade, the next cut drawn over: its rig, frame,
    /// framing, readout views and how far in it is (0..1).
    incoming: Option<(Arc<Rig>, Arc<Shown>, Camera, Vec<M3>, f32)>,
    overlays: Vec<gpu::OverlayDraw>,
}

#[derive(Default)]
struct Drag {
    button: Option<mouse::Button>,
    last: Option<Point>,
    /// The size last reported to the app.
    size: Option<Size>,
    /// Where a colour-picking box started, in the view.
    pick: Option<Point>,
}

impl Sphere {
    /// While picking a colour, a drag draws a box: the app hears of it as it
    /// grows and once it's drawn, in fractions of the view.
    fn pick(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        let at = || cursor.position().map(|p| Point::new((p.x - bounds.x).clamp(0.0, bounds.width), (p.y - bounds.y).clamp(0.0, bounds.height)));
        let area = |a: Point, b: Point| {
            Rectangle::new(Point::new(a.x.min(b.x) / bounds.width, a.y.min(b.y) / bounds.height), Size::new((a.x - b.x).abs() / bounds.width, (a.y - b.y).abs() / bounds.height))
        };
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                cursor.position_over(bounds)?;
                drag.pick = at();
                Some(shader::Action::capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let (a, b) = (drag.pick?, at()?);
                Some(shader::Action::publish(Message::PickArea(area(a, b))).and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let (a, b) = (drag.pick.take()?, at()?);
                Some(shader::Action::publish(Message::AreaPicked(area(a, b), (bounds.width / bounds.height) as f64)).and_capture())
            }
            _ => None,
        }
    }

    /// Drags look around, the wheel zooms.
    fn input(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        if self.picking {
            return self.pick(drag, event, bounds, cursor);
        }
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(b @ (mouse::Button::Left | mouse::Button::Right))) => {
                drag.last = Some(cursor.position_over(bounds)?);
                drag.button = Some(*b);
                Some(shader::Action::capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(_)) if drag.button.is_some() => {
                drag.button = None;
                Some(shader::Action::publish(Message::LookDone).and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let (button, last) = (drag.button?, drag.last?);
                drag.last = Some(*position);
                // Grab-style: the picture follows the pointer.
                let k = self.fov_deg.to_radians() / bounds.height as f64;
                let (dx, dy) = ((position.x - last.x) as f64, (position.y - last.y) as f64);
                let look = match button {
                    mouse::Button::Right => Message::Look { yaw: 0.0, pitch: 0.0, roll: dx * k },
                    _ => Message::Look { yaw: -dx * k, pitch: dy * k, roll: 0.0 },
                };
                Some(shader::Action::publish(look).and_capture())
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                cursor.position_over(bounds)?;
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y as f64,
                    mouse::ScrollDelta::Pixels { y, .. } => *y as f64 / 40.0,
                };
                Some(shader::Action::publish(Message::Zoom(-lines)).and_capture())
            }
            _ => None,
        }
    }
}

impl shader::Program<Message> for Sphere {
    type State = Drag;
    type Primitive = gpu::Primitive;

    fn update(&self, drag: &mut Drag, event: &iced::Event, bounds: Rectangle, cursor: mouse::Cursor) -> Option<shader::Action<Message>> {
        let action = self.input(drag, event, bounds, cursor);
        if action.is_none() && drag.size != Some(bounds.size()) {
            drag.size = Some(bounds.size());
            return Some(shader::Action::publish(Message::ViewSize(bounds.size())));
        }
        action
    }

    fn draw(&self, _: &Drag, _: mouse::Cursor, _: Rectangle) -> gpu::Primitive {
        gpu::Primitive {
            rig: self.rig.clone(),
            shown: self.shown.clone(),
            camera: self.camera,
            rig_from_view: self.rig_from_view.clone(),
            incoming: self.incoming.clone().map(|(rig, shown, camera, rig_from_view, mix)| gpu::Incoming { rig, shown, camera, rig_from_view, mix }),
            overlays: self.overlays.clone(),
        }
    }

    fn mouse_interaction(&self, drag: &Drag, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        if self.picking && cursor.is_over(bounds) {
            mouse::Interaction::Crosshair
        } else if drag.button.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}
