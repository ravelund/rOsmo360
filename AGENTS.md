# rOsmo360: instructions for coding agents

Read this before changing anything. It's for AI coding agents (and the people driving them) helping build rOsmo360. `CLAUDE.md` is the maintainer's own working notes, full of paths and footage you don't have; use this file instead. Human contributors: see `CONTRIBUTING.md` too.

## What it is

A Rust replacement for DJI Studio's playback and editing of DJI Osmo 360 `.OSV` footage: dual-fisheye stitching with DJI's own calibration, gyro stabilisation, rolling-shutter correction, a free-look player and timeline editor (iced), and export. One binary, `rosmo360`: `rosmo360 play` is the GUI; the other subcommands (`export`, `stitch`, `preview`, `scan`, `check`, `info`, ...) are the CLI. Linux is the main platform, macOS (Apple Silicon) builds from source, Windows is in beta.

## Build and check

```sh
cargo build --release          # target/release/rosmo360
cargo test --release           # GPU tests need a real GPU adapter
cargo clippy --release         # no new warnings
```

Runtime needs `ffmpeg` and `ffprobe` (on the PATH, or in an `ffmpeg/` folder beside the binary) and, on Linux, ALSA. Cross-builds: `sh packaging/windows/build.sh` (docker, MinGW), `sh packaging/macos/build.sh` (on a Mac). Releases (`packaging/build-release.sh`), version numbers and tags are the maintainer's; don't bump them in a PR.

## The GPU and CPU paths must match

Everything that draws a picture exists twice: a GPU version (the player, and GPU export) and a CPU version (`export --cpu`, export without a usable GPU, thumbnails, `preview`, `stitch`, photo save). The CPU stitcher in `src/stitch.rs` is the ground truth; the shader must match it. **A change to one side is not finished until the other side does the same thing.** The pairs:

| CPU (Rust) | GPU (WGSL) |
| --- | --- |
| `stitch.rs`: lens projection (ThetaPoly + tangential), masks, feather, blend, rolling-shutter re-aim, `cut_share` | `player/shader.wgsl` |
| `exposure.rs` `Seam::gain` | `exposure_gain` |
| `align.rs` `Aligner::warp` | `align_warp` |
| `view.rs` `Camera::ray`, `Camera::flat_uv` | `screen_ray`, `flat_uv` |
| `grade.rs` `Grade::apply` | `grade` in `shader.wgsl`, `grade_by` in `player/grade.wgsl` (shared with `overlay.wgsl`) |
| `hdr.rs` `to_sdr` | `hdr_to_sdr` |
| `lut.rs` `Lut::blend` | `apply_lut` |
| `overlay.rs` `composite` | `player/overlay.wgsl` |
| `main.rs` `encode` (frame times, attitude, readout steps, last-frame padding) | `player/render.rs` `encode` |

How to check:

- The tests `gpu_hdr_matches_the_cpu`, `gpu_lut_matches_the_cpu`, `gpu_overlays_match_the_cpu`, `deep_render_keeps_ten_bits` and `dither_keeps_a_smooth_ramp` (in `player/render.rs`) render on the GPU and compare. Add one like them when you add something drawn.
- Export the same clip or project both ways and compare:

  ```sh
  rosmo360 export clip.OSV -s 10 -d 3 -o gpu.mp4
  rosmo360 export clip.OSV -s 10 -d 3 --cpu -o cpu.mp4
  ffmpeg -i gpu.mp4 -i cpu.mp4 -lavfi psnr -f null -
  ```

  An Osmo clip lands around 39-42 dB (the GPU decodes 8-bit NV12, the CPU ffmpeg's RGB; then x264). Ordinary videos ~36 dB, pictures ~32 (NV12 halves the colour resolution). Clearly below that, or a frame count that differs, means the paths disagree.

The same rule holds wider: the **player and the export** must frame, grade and time a cut alike, and so must every export route (MP4, `--format resolve`, `--clips`, `--draft`, from the player's Export panel and from the CLI). If you change how a cut is framed, check them all.

## Things that are easy to break

- **Portability.** Code must build and work on Linux, macOS and Windows. No Linux-only paths, commands or APIs without a `cfg` fallback; config and cache folders from the platform; ffmpeg found by `ffmpeg::tool`, never assumed at `/usr/bin`.
- **ffmpeg is a separate program.** We talk to it over pipes, never link its libraries: that keeps rOsmo360 MIT/Apache. Never require a nonfree ffmpeg build.
- **wgpu comes from iced.** Use `iced::wgpu`; never add `wgpu` as a direct dependency, or the versions split.
- **The clip cache** (`src/cache.rs`): if your change alters what a cache entry holds or how it's made (layout, a telemetry-parser update touching the rig or attitude, thumbnails), bump `cache::FORMAT`. Otherwise leave it, so users keep their cache.
- **Project files are a public format** (`src/project.rs`, `view.rs`, `overlay.rs`, `grade.rs`; documented in the README). Older projects must open and look exactly as before: new fields need a default and are written only when they differ from it. Same for `settings.json`.
- **10-bit and HDR.** Frames may be 8-bit NV12 or 10-bit P010, video or full range, BT.709, BT.601 or BT.2020 HLG/PQ. Test what you change at both depths (Settings → 10-bit, or `ROSMO360_10BIT=1`).
- **Frame rates.** Clips at 30, 60 and 100 fps can share one edit, whose rate is its first Osmo clip's. Frames are timed n / fps; every decode is converted to the clip's or edit's rate (`ffmpeg::Out::rate`). Check a mixed-rate edit's frame count when touching decode or export.
- **The UI stays clean.** No help-text lines in bars or panels: explanations go in hover hints, Settings notes or the user guide. The app never mentions DJI Studio by name.
- **iced quirks.** Keyboard shortcuts only see keys no widget used. Within one canvas layer images draw over shapes, which is why the timeline is two stacked canvases.

## Facts not to re-derive

These took real footage and long measurement to establish. Don't "fix" them without the same evidence.

- **Calibration** comes from telemetry-parser's `util::camera_rig()` (DJI's default calibration, masks and blend ramps). A hand decode of the calibration slots is wrong.
- **IMU axes:** the attitude is world_from_imu with world +z up, and the IMU sits a half turn about rig x (`ImuAxes::X180`). telemetry-parser's identity is wrong for this camera.
- **Frame timing:** a frame's timestamp is the start of its readout (~18.3 ms top-down); `rig_from_view` reads the attitude from t to t + readout. Centring it made the picture swim.
- **Geometry:** rig frame x right, y down, z forward; `Lens::rotation` is rig_from_cam; equirect direction `(cos φ sin λ, −sin φ, cos φ cos λ)`.
- **The `.LRF` proxy** is 2048×1024 H.264: left half lens 0, right half lens 1, each the whole circle at 1024², unstitched. 100 fps clips have 1920² lenses and a 25 fps proxy.
- **Hardware decode** through the ffmpeg CLI: `-hwaccel X -hwaccel_output_format X`, then `hwdownload,format=F` with exactly one format. That chain fails loudly when the GPU can't decode, which `Hwaccel::pick` relies on. Filters after `hwdownload`, not before.
- **Projection:** perspective and little planet are one family, s = sin θ / (d + cos θ); d is `View::correction` (0 perspective, 1 stereographic).
- **Seam work already tried and dropped** (no visible gain, or worse): a per-frame optimal seam path, a wider alignment search, contrast/offset veil matching, two-band blending, whole-lens exposure. Exposure matching is off by default on purpose. What's left needs per-pixel optical flow.

## Testing without the maintainer's footage

There's no test footage in the repo, and there won't be: footage is personal. Use your own Osmo 360 clips (keep the `.LRF` beside the `.OSV`); ordinary videos and pictures can be made with ffmpeg (`testsrc2`, `smptebars`). Never commit footage, exports, screenshots of people, or files from outside the repo.

Useful checks:

- Stitch geometry: `rosmo360 stitch clip.OSV -t 10 --only 0 -o a.png` and `--only 1`; content must line up across the overlap at longitude ±90°.
- Seam: `rosmo360 seam clip.OSV -s 10 -d 3`; compare mismatch and eased jitter before and after.
- Stabilisation: `--stabilize lock` through a fast pan; a distant object (or the sun) must stay put. `horizon` keeps the horizon level at large tilts.
- Exports run several GB of memory at 1080p from the full-res OSV.

## Code style

- Match the surrounding code: `anyhow` errors, short doc comments that say *why*, no dead code, no debug output left in.
- Prose (docs, UI text, comments) uses British spelling ("colour", "stabilise") and plain words.
- Commit messages explain the reasoning and the evidence (what was measured, on what), not just the change.
- When a change is visible to users, update `docs/user-guide.md` and the README in the same PR.
- Ask (in an issue) before adding a heavy dependency; never one under the GPL or AGPL in the binary.

## Layout

- `src/main.rs`: CLI, `Pipeline` (rig, lenses, stabiliser, fps per clip), CPU `encode` and `export`.
- `src/stitch.rs`: the CPU stitcher (ground truth). `ring.rs`: seam coordinates. `exposure.rs`, `align.rs`: per-frame seam corrections.
- `src/stab.rs`: stabilisation modes. `imu.rs`: reads the attitude. `view.rs`: views, keyframes, `Camera`.
- `src/project.rs`: projects (cuts, sound, music). `overlay.rs`, `text.rs`: overlay and text tracks. `grade.rs`, `lut.rs`, `hdr.rs`: colour. `dissolve.rs`: crossfades in export.
- `src/ffmpeg.rs`: every ffmpeg/ffprobe call, decode options, hardware decoder probing. `cache.rs`: the clip cache. `dji.rs`: DJI's metadata.
- `src/track.rs`, `sky.rs`, `check.rs`, `sheet.rs`: colour tracking, `scan --image`, `check`, `preview` sheets.
- `src/player/`: the iced app (`mod.rs`), GPU drawing (`gpu.rs`, `shader.wgsl`, `grade.wgsl`, `overlay.wgsl`), GPU export (`render.rs`), decoding (`source.rs`, `playback.rs`), sound (`audio.rs`, `music.rs`), the timeline (`timeline.rs`, `edit.rs`, `thumbs.rs`) and the panels.
- `packaging/`: release builds for Linux (tarball, AppImage, Flatpak), Windows and macOS. `docs/`: the user guide.
